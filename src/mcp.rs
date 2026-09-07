//! Minimal MCP (Model Context Protocol) server over stdio — `st2k --mcp`.
//!
//! **Not a daemon.** An MCP client (Claude Desktop, an IDE agent, …) spawns this
//! as a child process, exchanges newline-delimited JSON-RPC 2.0 messages over
//! stdin/stdout, and terminates it when the client closes. Every tool just calls
//! the same [`crate::cli`] verbs the command line uses, so an agent gets the
//! bundled offline image engine (decode all registered formats, convert, rotate, strip,
//! OCR, PDF, info) with zero extra installs.
//!
//! The transport is the MCP stdio framing: one JSON-RPC message per line, no
//! embedded newlines (serde_json::to_string never emits any).

use std::io::{BufRead, Write};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::{json, Value};

use crate::cli;
use crate::formats;

/// MCP protocol revision we implement (the stable 2024-11-05 spec).
const PROTOCOL_VERSION: &str = "2024-11-05";

/// The only JSON-RPC version this server speaks. Spelled once so the envelope check and the
/// replies cannot drift apart.
const JSONRPC_VERSION: &str = "2.0";

/// `BufRead::read_line` with a ceiling: reads one `\n`-terminated line into `line`, returning the
/// bytes consumed, or `Ok(0)` on EOF **or** once the line exceeds `max` (the caller treats both as
/// "stop"). Byte-oriented so an over-long line is abandoned without ever materializing.
fn read_line_capped<R: BufRead>(
    reader: &mut R,
    line: &mut String,
    max: usize,
) -> std::io::Result<usize> {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        // Consume out of the buffer in whole chunks (not byte at a time) — the newline scan is
        // the same work `read_line` does, just with a ceiling on how much we're willing to keep.
        let (done, used) = {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                (true, 0) // EOF
            } else if let Some(i) = available.iter().position(|&b| b == b'\n') {
                buf.extend_from_slice(&available[..=i]);
                (true, i + 1)
            } else {
                buf.extend_from_slice(available);
                (false, available.len())
            }
        };
        reader.consume(used);
        // Check the cap BEFORE the done/break check: a chunk that pushes `buf` past `max`
        // can be the very chunk that also carries the terminating newline, and checking
        // order previously let `done` short-circuit past the size check on that same
        // iteration — accepting (and parsing) an oversized line instead of dropping it.
        if buf.len() > max {
            return Ok(0); // oversized message — give up on this stream
        }
        if done {
            break;
        }
    }
    if buf.is_empty() {
        return Ok(0);
    }
    let n = buf.len();
    // Invalid UTF-8 isn't valid JSON either; hand the lossy form to the parser, which
    // answers with a proper JSON-RPC parse error.
    line.push_str(&String::from_utf8_lossy(&buf));
    Ok(n)
}

/// Read JSON-RPC messages from stdin and reply on stdout until EOF (the client
/// closing its end). Locks both streams for the process lifetime — fine for a
/// dedicated child server.
pub fn serve() -> std::io::Result<()> {
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    /// Cap on one JSON-RPC message. `read_line` grows its `String` until it sees a newline, so a
    /// client (or anything else wired to our stdin) that streams megabytes without one would grow
    /// the buffer without bound. Real requests are a few KB; a `view`/`compress` reply is big but
    /// that's the OUTPUT side. Over the cap we drop the connection rather than keep buffering.
    const MAX_MSG_BYTES: usize = 8 * 1024 * 1024;

    let mut line = String::new();
    loop {
        line.clear();
        if read_line_capped(&mut reader, &mut line, MAX_MSG_BYTES)? == 0 {
            break; // EOF: client closed the pipe, or a message blew the cap
        }
        // Trim whitespace AND a stray UTF-8 BOM (`U+FEFF`) — some clients/shells
        // prepend one to the stream, and Rust's `trim()` doesn't treat it as
        // whitespace, so it would otherwise poison the first message.
        let trimmed = line.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(trimmed) {
            Ok(req) => {
                if let Some(resp) = handle(&req) {
                    write_msg(&mut out, &resp)?;
                }
            }
            // Malformed JSON: JSON-RPC parse error, id unknowable → null.
            Err(_) => write_msg(&mut out, &error_resp(Value::Null, -32700, "parse error"))?,
        }
    }
    Ok(())
}

/// A request envelope that has been CHECKED, not merely read (2026-09-05 audit, F20).
///
/// The old dispatcher took `req.get("id").cloned()` and `req.get("method").and_then(as_str)
/// .unwrap_or("")` straight off the raw value, so it never looked at `jsonrpc` at all and
/// could not tell a malformed member from a missing one: a `{"jsonrpc":"1.0","method":"ping"}`
/// was answered as a perfectly good ping, and `"method": 7` became the empty string and came
/// back as "method not found: ", which names nothing.
struct Envelope<'a> {
    /// `None` only when the `id` member is genuinely ABSENT, which is what makes a message a
    /// notification. A present `"id": null` is a request that spelled its id as null (the
    /// spec discourages it but allows it), and is answered with a null id.
    id: Option<Value>,
    method: &'a str,
}

/// Validate the envelope before anything is dispatched. `Err` is the ready-made reply.
///
/// These are TRANSPORT faults, so they come back as JSON-RPC `error` objects with -32600
/// Invalid Request, never as a tool result with `isError` (which is reserved for a tool that
/// ran and could not do the job). The id is echoed when it was itself well formed, so a
/// client can match the rejection to what it sent; it is null when the id could not be
/// determined, which is what the spec asks for.
fn parse_envelope(req: &Value) -> Result<Envelope<'_>, Value> {
    // A JSON-RPC batch array or a bare scalar isn't an object, so `Value::get` (which only
    // resolves string keys on `Object`) silently returns `None` for both "id" and "method".
    // That used to fall to the wildcard arm's `id.map(...)`, which is `None` too, so
    // `serve()` wrote nothing back and the caller hung waiting for a reply that never came.
    // Answer immediately instead: id is unknowable for a non-object request, so it's null.
    if !req.is_object() {
        return Err(error_resp(Value::Null, -32600, "Invalid Request"));
    }
    // Read the id FIRST so every rejection below can name it. Only the three types the spec
    // allows count; an array/object/boolean id is a request we cannot correlate a reply to.
    let id = match req.get("id") {
        None => None,
        Some(v @ (Value::String(_) | Value::Number(_) | Value::Null)) => Some(v.clone()),
        Some(other) => {
            return Err(invalid_request(
                Value::Null,
                &format!("\"id\" must be a string, a number or null, found {other}"),
            ))
        }
    };
    let echo = id.clone().unwrap_or(Value::Null);
    match req.get("jsonrpc") {
        Some(Value::String(v)) if v == JSONRPC_VERSION => {}
        None => {
            return Err(invalid_request(
                echo,
                "\"jsonrpc\": \"2.0\" is required on every message",
            ))
        }
        Some(other) => {
            return Err(invalid_request(
                echo,
                &format!("\"jsonrpc\" must be the string \"2.0\", found {other}"),
            ))
        }
    }
    match req.get("method") {
        Some(Value::String(m)) => Ok(Envelope { id, method: m }),
        None => Err(invalid_request(echo, "\"method\" is required")),
        Some(other) => Err(invalid_request(
            echo,
            &format!("\"method\" must be a string, found {other}"),
        )),
    }
}

fn invalid_request(id: Value, why: &str) -> Value {
    error_resp(id, -32600, &format!("Invalid Request: {why}"))
}

/// Dispatch one parsed message. Returns `Some(response)` for a request (has an
/// `id`), `None` for a notification (no `id`) or a no-reply method.
fn handle(req: &Value) -> Option<Value> {
    let env = match parse_envelope(req) {
        Ok(env) => env,
        // A malformed message is answered even when it looks like a notification: nothing
        // about it can be trusted, including the absence of an id.
        Err(resp) => return Some(resp),
    };
    let id = env.id;
    let method = env.method;
    match method {
        "initialize" => Some(result(id?, initialize_result())),
        "tools/list" => Some(result(id?, json!({ "tools": tool_defs() }))),
        "tools/call" => Some(tools_call(id?, req.get("params"))),
        "ping" => Some(result(id?, json!({}))),
        // Notifications we simply acknowledge by ignoring.
        m if m.starts_with("notifications/") => None,
        // Unknown request → method-not-found; unknown notification → ignore.
        _ => id.map(|id| error_resp(id, -32601, &format!("method not found: {method}"))),
    }
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "sagethumbs2k", "version": env!("CARGO_PKG_VERSION") },
        "instructions": format!("Offline image toolbox: decode {} formats, convert, rotate/flip, strip metadata, OCR, combine to PDF, and read image info. All tools take local file paths.", formats::FORMATS.len())
    })
}

/// The tool catalog (name + description + JSON-Schema for arguments).
fn tool_defs() -> Value {
    let str_prop = |desc: &str| json!({ "type": "string", "description": desc });
    let thumbnail_desc = format!(
        "Render any supported image ({} formats Windows often can't, incl. HEIC/RAW/PSD/ebook covers) to an image file, capped to a max long-edge size.",
        formats::FORMATS.len()
    );
    let view_desc = format!(
        "Decode a file and RETURN IT AS AN IMAGE you can see directly — for any of the {} supported formats Windows often can't open (HEIC/RAW/PSD/ebook & comic covers/CAD previews/audio cover art/…). Use it to look at, describe, caption, OCR-by-eye, or analyze a file's visual content. Returns an image content block, not a file path.",
        formats::FORMATS.len()
    );
    json!([
        {
            "name": "thumbnail",
            "description": thumbnail_desc,
            "inputSchema": { "type": "object", "properties": {
                "input": str_prop("path to the source image"),
                "output": str_prop("path to write; output format is taken from this extension (.png/.jpg/…)"),
                "size": { "type": "integer", "description": "max long-edge in px (default 256; 0 = full size)" }
            }, "required": ["input", "output"] }
        },
        {
            "name": "view",
            "description": view_desc,
            "inputSchema": { "type": "object", "properties": {
                "input": str_prop("path to the source file"),
                "size": { "type": "integer", "description": "max long-edge in px (default 512)" }
            }, "required": ["input"] }
        },
        {
            "name": "convert",
            "description": "Convert an image to another format (format from the output extension), with optional quality and resize.",
            "inputSchema": { "type": "object", "properties": {
                "input": str_prop("source image path"),
                "output": str_prop("destination path; format from its extension"),
                "quality": { "type": "integer", "description": "encoder quality 1-100 (JPEG; default 90)" },
                "webp_quality": { "type": "integer", "description": "1-100 → lossy WebP at this quality (only for .webp output; omit for lossless WebP)" },
                "resize": str_prop("optional 'WxH' (fit, no upscale) or 'N%' (scale)")
            }, "required": ["input", "output"] }
        },
        {
            "name": "compress",
            "description": "Compress an image to a target file size → a '(compressed)' JPEG sibling at or under the limit (quality binary-search, then downscale if needed). Success means at or under the limit: a limit the search cannot meet fails, writes nothing, and the error names the smallest size reachable, so ask again with at least that.",
            "inputSchema": { "type": "object", "properties": {
                "input": str_prop("source image path"),
                "max_size": str_prop("target size, e.g. '1MB', '500KB', or a byte count")
            }, "required": ["input", "max_size"] }
        },
        {
            "name": "rotate",
            "description": "Rotate or flip an image, writing a new '(edited)' sibling file (never re-compresses the original in place).",
            "inputSchema": { "type": "object", "properties": {
                "input": str_prop("source image path"),
                "by": { "type": "string", "enum": ["right", "left", "180", "fliph", "flipv"], "description": "transform to apply" }
            }, "required": ["input", "by"] }
        },
        {
            "name": "strip",
            // Synced to strip.rs's real match arms (jpg/jpeg/jpe/jfif, png, webp, svg/svgz,
            // heic/heif/hif/avif) — this used to say "JPEG or PNG" only, understating what an
            // agent could actually call it on.
            "description": "Losslessly strip EXIF/IPTC/XMP metadata from a JPEG, PNG, WebP, SVG/SVGZ, or HEIC/HEIF/AVIF file in place (keeps the ICC color profile where present; no pixel re-encode).",
            "inputSchema": { "type": "object", "properties": {
                "input": str_prop("JPEG/PNG/WebP/SVG(Z)/HEIC/HEIF/AVIF path")
            }, "required": ["input"] }
        },
        {
            "name": "ocr",
            "description": "Recognize text in an image and return it (Windows OCR; needs a language pack installed).",
            "inputSchema": { "type": "object", "properties": {
                "input": str_prop("image path")
            }, "required": ["input"] }
        },
        {
            "name": "pdf",
            "description": "Combine one or more images into a single PDF (one image per page). 'output' must be a .pdf path and must not be one of the inputs (any spelling, case or hard link of an input is refused before anything is written). Returns JSON: {output, status: 'ok'|'partial', requested, combined, omitted: [{input, cause: 'unreadable'|'undecodable'|'unencodable', detail}]}; an input that cannot be used is left out and listed under 'omitted' unless 'strict' is true, in which case the call fails and writes nothing.",
            "inputSchema": { "type": "object", "properties": {
                "output": str_prop("destination .pdf path"),
                "inputs": { "type": "array", "items": { "type": "string" }, "description": "image paths, in page order" },
                "strict": { "type": "boolean", "description": "fail and write nothing if any input would be left out (default false = build from the usable inputs and list the rest)" }
            }, "required": ["output", "inputs"] }
        },
        {
            "name": "cbz",
            "description": "Combine one or more images into a single CBZ (comic-book zip) archive, natural-sorted, with a ComicInfo.xml sidecar. 'output' must be a .cbz path and must not be one of the inputs (any spelling, case or hard link of an input is refused before anything is written). Returns the same JSON as 'pdf' ({output, status, requested, combined, omitted[]}); an input that cannot be read is left out and listed under 'omitted' unless 'strict' is true, in which case the call fails and writes nothing.",
            "inputSchema": { "type": "object", "properties": {
                "output": str_prop("destination .cbz path"),
                "inputs": { "type": "array", "items": { "type": "string" }, "description": "image paths, in page order" },
                "strict": { "type": "boolean", "description": "fail and write nothing if any input would be left out (default false = build from the usable inputs and list the rest)" }
            }, "required": ["output", "inputs"] }
        },
        {
            "name": "info",
            "description": "Read an image's dimensions, bit depth, DPI and EXIF camera/date/GPS — or, for an audio file (mp3/flac/wma/dsf/…), its artist/album/title/track/genre/year/duration/bitrate tags. Returns JSON.",
            "inputSchema": { "type": "object", "properties": {
                "input": str_prop("image or audio file path")
            }, "required": ["input"] }
        },
        {
            "name": "formats",
            "description": "List every supported input format (extension, category, description). Returns JSON.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "doctor",
            "description": "Read-only self-check: is the shell extension registered, loadable, and enabled? Diagnoses \"why aren't thumbnails showing\". Returns a paste-ready text report with a FIX per finding; optionally probes one file's decode as well.",
            "inputSchema": { "type": "object", "properties": {
                "file": str_prop("optional path to also probe (does this ONE file decode)")
            } }
        },
        {
            "name": "batch",
            "description": "Bulk-process many files/folders in one process: thumbnail, convert, or read info (dimensions/EXIF/audio tags, as one JSON array) for every supported file found. Each input directory is scanned ONE level deep unless 'recurse' is true. For thumbnail/convert the result is JSON: {status, requested, succeeded, failed, skipped_offline, results[]}, one entry per input carrying {input, output, status, cause, detail, elapsed_ms} - so a failed file is retryable by path and its 'cause' says whether it was unreadable, undecodable, unencodable or unwritable. Partial success is deliberate: a file that fails does not stop the rest. Every input failing is a tool error naming each one.",
            "inputSchema": { "type": "object", "properties": {
                "op": { "type": "string", "enum": ["thumbnail", "convert", "info"], "description": "operation to run on every input" },
                "inputs": { "type": "array", "items": { "type": "string" }, "description": "file and/or folder paths" },
                "recurse": { "type": "boolean", "description": "walk input directories recursively (default false = one level deep)" },
                "out": str_prop("output directory (default: alongside each source file; ignored for info)"),
                "size": { "type": "integer", "description": "thumbnail max long-edge in px (default 256; ignored for convert/info)" },
                "to": str_prop("output extension, required when op is \"convert\""),
                "quality": { "type": "integer", "description": "encoder quality 1-100 (default 90; ignored for info)" },
                "resize": str_prop("optional 'WxH' (fit, no upscale) or 'N%' (scale), convert only")
            }, "required": ["op", "inputs"] }
        },
        {
            "name": "prebuild",
            "description": "Pre-build Explorer's thumbnail cache for whole folders (so browsing them later is instant). Refuses to run elevated (the cache is per-user). Returns a built/cached/failed summary.",
            "inputSchema": { "type": "object", "properties": {
                "inputs": { "type": "array", "items": { "type": "string" }, "description": "file and/or folder paths" },
                "recurse": { "type": "boolean", "description": "walk input directories recursively (default false = one level deep)" },
                "sizes": { "type": "array", "items": { "type": "integer", "minimum": 1 }, "minItems": 1, "description": "edge sizes in px to build (omit for the default 96,256,768, Explorer's Medium/Large/Extra-large buckets). If given it must be a non-empty array of whole numbers above 0; every element is checked and one bad element fails the call, rather than being dropped" },
                "rebuild_all": { "type": "boolean", "description": "skip the already-cached probe and rebuild every file (default false)" },
                "jobs": { "type": "integer", "description": "worker threads (default 3)" }
            }, "required": ["inputs"] }
        },
        {
            "name": "register_status",
            "description": "Portable build only: report whether Explorer thumbnails are currently registered for this user.",
            "inputSchema": { "type": "object", "properties": {} }
        }
    ])
}

/// True for a UNC path — `\\server\share\...` or its extended-length spelling
/// `\\?\UNC\server\share\...` — which starts an SMB negotiation (and, by default, an NTLM
/// handshake) merely by being opened, driveable by a prompt-injected tool argument.
/// `\\?\C:\...` (the extended-length LOCAL form) is NOT UNC and stays usable.
fn is_unc_path(p: &str) -> bool {
    // Windows' path parser turns a leading `//` into `\\` before the redirector sees it, so
    // a forward-slash spelling reaches the network exactly like the backslash one.
    let p = p.trim_start().replace('/', "\\");
    let p = p.as_str();
    match p.strip_prefix(r"\\?\") {
        // `get(..4)` rather than a byte-range index: a panicking slice on a non-char-
        // boundary is exactly what this crate's `unwrap_used`/`expect_used` deny exists to
        // rule out for a hostile/malformed path, and `get` degrades to `None` (not a UNC
        // match) instead of aborting the process.
        Some(rest) => rest
            .get(..4)
            .is_some_and(|s| s.eq_ignore_ascii_case(r"UNC\")),
        None => p.starts_with(r"\\"),
    }
}

/// Walk every string value in `args` — including array elements, so `pdf`/`batch`'s
/// `inputs` list is covered without a second copy of this check — and return the first
/// one that is a UNC path. Centralised as ONE call at the top of [`tools_call`] rather
/// than per-tool/per-field, so a future path-taking argument on either surface (`view` or
/// `dispatch_tool`) is covered automatically instead of needing its own copy.
fn find_unc_arg(v: &Value) -> Option<&str> {
    match v {
        Value::String(s) if is_unc_path(s) => Some(s.as_str()),
        Value::Array(a) => a.iter().find_map(find_unc_arg),
        Value::Object(o) => o.values().find_map(find_unc_arg),
        _ => None,
    }
}

/// Run a `tools/call`: validate params, invoke the verb, wrap the text result.
/// Tool-level failures are reported as a result with `isError: true` (per MCP),
/// not as a JSON-RPC error — those are reserved for protocol faults.
fn tools_call(id: Value, params: Option<&Value>) -> Value {
    let Some(params) = params else {
        return error_resp(id, -32602, "missing params");
    };
    // The `params` SHAPE is part of the transport contract, so a malformed one is -32602
    // rather than a tool result: a missing `name` used to read as the empty string and come
    // back as the tool error "unknown tool ''", which describes the caller's typo as a
    // failure of a tool that does not exist. `arguments` is optional (a tool can take none)
    // but must be an object when supplied, since every validator below indexes it by key.
    if !params.is_object() {
        return error_resp(id, -32602, "params must be an object");
    }
    let Some(name) = params.get("name").and_then(|n| n.as_str()) else {
        return error_resp(
            id,
            -32602,
            "'name' must be a string naming the tool to call",
        );
    };
    let empty = json!({});
    let args = match params.get("arguments") {
        None | Some(Value::Null) => &empty,
        Some(v) if v.is_object() => v,
        Some(_) => return error_resp(id, -32602, "'arguments' must be an object"),
    };

    // Reject a UNC path anywhere in the arguments before EITHER of the two dispatch paths
    // below ever sees them — a same-desktop or prompt-injected caller could otherwise force
    // an SMB/NTLM handshake against an attacker-controlled path.
    if let Some(bad) = find_unc_arg(args) {
        return tool_error(id, format!("UNC paths are not accepted: {bad}"));
    }

    // `view` returns an IMAGE content block (base64 PNG) so the agent can SEE the file —
    // handled before the text-returning dispatch below.
    if name == "view" {
        return match view_png_bytes(args) {
            Ok(png) => result(
                id,
                json!({ "content": [{ "type": "image", "data": STANDARD.encode(&png), "mimeType": "image/png" }], "isError": false }),
            ),
            Err(msg) => tool_error(id, msg),
        };
    }

    match dispatch_tool(name, args) {
        Ok(text) => result(
            id,
            json!({ "content": [{ "type": "text", "text": text }], "isError": false }),
        ),
        Err(msg) => tool_error(id, msg),
    }
}

/// A tool that RAN and could not do the job. Per MCP this is a result with `isError: true`,
/// not a JSON-RPC error: those are reserved for transport faults (a malformed envelope or
/// malformed `params`), so a client can tell "I sent you nonsense" from "your file is
/// unreadable". Both halves of that split are exercised by the envelope tests below.
fn tool_error(id: Value, message: String) -> Value {
    result(
        id,
        json!({ "content": [{ "type": "text", "text": message }], "isError": true }),
    )
}

/// `view`'s decode, size cap included, as a plain `Result` so the argument validators can be
/// the same ones every other tool uses. Split out of [`tools_call`] when those validators
/// landed (2026-09-05 audit, F20); the caps and messages are unchanged.
fn view_png_bytes(args: &Value) -> Result<Vec<u8>, String> {
    let input = need_str(args, "input")?;
    // Clamp to the decoder's own bomb-guard ceiling. 0 stays 0 ("full size", the
    // documented sentinel `cli::view_png` already handles); anything above the ceiling
    // is clamped rather than reaching the decoder unbounded.
    let size = want_size(args, "size", 512)?;
    let png = cli::view_png(&input, size)?;
    // `view` has no output-size cap, unlike the strict inbound `MAX_MSG_BYTES`: a
    // legitimate large image (or `size: 0`, "full size") can base64-encode into
    // tens-to-hundreds of MB written into ONE JSON-RPC line with nothing warning the
    // caller. Refuse rather than write it.
    const MAX_VIEW_PNG_BYTES: usize = 24 * 1024 * 1024;
    if png.len() > MAX_VIEW_PNG_BYTES {
        return Err(format!(
            "decoded image is {} MB, over the {}-MB view limit, pass a smaller 'size'",
            png.len() / (1024 * 1024),
            MAX_VIEW_PNG_BYTES / (1024 * 1024)
        ));
    }
    Ok(png)
}

// ---- argument validators (2026-09-05 audit, F20) ------------------------------------
//
// One place where a tool argument becomes a Rust value, so every tool answers a malformed
// argument the same way. The rule, in one line: an ABSENT optional keeps its documented
// default, a SUPPLIED value of the wrong type or shape is an error naming the argument,
// never a silent substitution. The old accessors (`args.get(k).and_then(Value::as_u64)
// .unwrap_or(d)`) could not tell "you left `size` out" from "you sent `size: \"big\"`" or
// `size: -1`, and ran all three at the default, so the reply described work the caller had
// not asked for. These are tool-argument faults, so they surface as `isError` results.
//
// A JSON `null` counts as ABSENT rather than invalid: clients spell an omitted optional
// that way, and every accessor these replace already read it as absent.

/// The value at `k` if it was really supplied, `None` for absent or an explicit `null`.
fn present<'a>(args: &'a Value, k: &str) -> Option<&'a Value> {
    match args.get(k) {
        None | Some(Value::Null) => None,
        Some(v) => Some(v),
    }
}

/// A supplied string argument, `None` when absent.
fn want_str(args: &Value, k: &str) -> Result<Option<String>, String> {
    match present(args, k) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(other) => Err(format!("'{k}' must be a string, found {other}")),
    }
}

/// A required string argument. The message is the one this server has always used for a
/// missing one, so an agent that learned it keeps reading the same sentence.
fn need_str(args: &Value, k: &str) -> Result<String, String> {
    want_str(args, k)?.ok_or_else(|| format!("missing string argument '{k}'"))
}

/// A supplied whole number, `None` when absent. A negative number, a fractional one and a
/// non-number type are all refused: `as_u64` answered `None` to all three, which is exactly
/// what made them indistinguishable from absent.
fn want_u64(args: &Value, k: &str) -> Result<Option<u64>, String> {
    match present(args, k) {
        None => Ok(None),
        Some(v) => v
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("'{k}' must be a whole number 0 or greater, found {v}")),
    }
}

/// A supplied boolean, `None` when absent.
fn want_bool(args: &Value, k: &str) -> Result<Option<bool>, String> {
    match present(args, k) {
        None => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(other) => Err(format!("'{k}' must be true or false, found {other}")),
    }
}

/// A pixel size argument with its documented default. The CLAMP is deliberately kept: an
/// out-of-range NUMBER is a request this server has always answered by bounding it (see
/// [`clamp_requested_size`], and `0` still means "full size"), which is a documented
/// behaviour rather than a silent type substitution. What changes is that `"big"` or `-1`
/// is now an error instead of quietly becoming `default`.
fn want_size(args: &Value, k: &str, default: u64) -> Result<u32, String> {
    Ok(clamp_requested_size(want_u64(args, k)?.unwrap_or(default)))
}

/// An encoder-quality argument, clamped to the advertised 1-100 for the same reason.
fn want_quality(args: &Value, k: &str, default: u64) -> Result<u8, String> {
    Ok(want_u64(args, k)?.unwrap_or(default).clamp(1, 100) as u8)
}

/// Collect the string array at `k`. `Err` when the key is present as an array but carries a
/// non-string element (before this fix, such an element was silently DROPPED — a mixed-type
/// `inputs` array built a PDF/CBZ/batch with fewer pages/files than requested and reported
/// success), and `Err` when it is present as something other than an array at all (which
/// used to read as an empty list, so `"inputs": "a.png"` became "you gave me no inputs").
/// Absent stays `Ok(vec![])`, same as before.
fn want_str_array(args: &Value, k: &str) -> Result<Vec<String>, String> {
    let Some(v) = present(args, k) else {
        return Ok(Vec::new());
    };
    let Some(a) = v.as_array() else {
        return Err(format!("'{k}' must be an array of strings, found {v}"));
    };
    a.iter()
        .map(|x| {
            x.as_str()
                .map(String::from)
                .ok_or_else(|| format!("'{k}' must be an array of strings; found {x}"))
        })
        .collect()
}

/// Refuse to let a write tool clobber a file that already EXISTS at `output` when its
/// extension isn't one this tool produces. `pdf`/`cbz` write raw bytes to whatever path
/// they're given with no extension check at all (unlike `thumbnail`/`convert`, which
/// already refuse an unrecognized output extension before writing anything) — so a
/// prompt-injected `output` could otherwise silently overwrite any file the process
/// account can write, regardless of what it actually was. Never blocks writing a NEW path.
fn refuse_foreign_overwrite(output: &str, produced_exts: &[&str]) -> Result<(), String> {
    let p = std::path::Path::new(output);
    if !p.is_file() {
        return Ok(());
    }
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if produced_exts.iter().any(|e| e.eq_ignore_ascii_case(&ext)) {
        return Ok(());
    }
    Err(format!(
        "refusing to overwrite existing file '{output}': its extension \".{ext}\" is not one this tool writes ({})",
        produced_exts.join("/")
    ))
}

/// `convert`: input/output paths, JPEG/WebP quality, and an optional resize spec.
fn dispatch_convert(args: &Value) -> Result<String, String> {
    let q = want_quality(args, "quality", 90)?;
    // `webp_quality` has no default on purpose: absent means "lossless WebP", so it must stay
    // an Option rather than collapsing into a number.
    let wq = want_u64(args, "webp_quality")?.map(|w| w.clamp(1, 100) as u8);
    cli::convert(
        &need_str(args, "input")?,
        &need_str(args, "output")?,
        q,
        wq,
        cli::parse_resize(want_str(args, "resize")?.as_deref())?,
    )
}

/// The `pdf`/`cbz` omission policy from the tool arguments (2026-09-05 audit, F31): the
/// result is always the machine-readable JSON here (an agent reads it, not a person), and
/// `"strict": true` turns a partial result into a refusal that writes nothing. Both tools
/// go through `cli::pdf`/`cli::cbz`, so the alias and extension checks (F30) and the
/// per-input omission report are exactly the CLI's.
fn combine_opts(args: &Value) -> Result<cli::CombineOpts, String> {
    Ok(cli::CombineOpts {
        strict: want_bool(args, "strict")?.unwrap_or(false),
        json: true,
    })
}

/// `pdf`: an output path plus the input file list.
fn dispatch_pdf(args: &Value) -> Result<String, String> {
    let output = need_str(args, "output")?;
    refuse_foreign_overwrite(&output, &["pdf"])?;
    cli::pdf(
        &output,
        &want_str_array(args, "inputs")?,
        combine_opts(args)?,
    )
}

/// `cbz`: same shape as `pdf`, writing a comic-book zip instead.
fn dispatch_cbz(args: &Value) -> Result<String, String> {
    let output = need_str(args, "output")?;
    refuse_foreign_overwrite(&output, &["cbz"])?;
    cli::cbz(
        &output,
        &want_str_array(args, "inputs")?,
        combine_opts(args)?,
    )
}

/// `batch`: an operation name over the input file list, plus the same
/// output/size/format/quality/resize options `thumbnail`/`convert` take individually.
///
/// The result is always the machine-readable report (2026-09-05 audit, F11), for the same
/// reason `pdf`/`cbz` hardwire their `json` here: an agent reads it, not a person, and a
/// "9/12 succeeded" it cannot map back to file names leaves it nothing to retry. `info`
/// already answered this tool in JSON, so the whole tool is now one shape.
fn dispatch_batch(args: &Value) -> Result<String, String> {
    cli::batch(
        &need_str(args, "op")?,
        &want_str_array(args, "inputs")?,
        want_bool(args, "recurse")?.unwrap_or(false),
        want_str(args, "out")?.as_deref(),
        want_size(args, "size", 256)?,
        want_str(args, "to")?.as_deref(),
        want_quality(args, "quality", 90)?,
        cli::parse_resize(want_str(args, "resize")?.as_deref())?,
        true,
    )
}

/// The `prebuild` size list, under the ONE policy `prebuild::parse_size_list` states and the
/// CLI's `--size` also goes through (2026-09-05 audit, F12/F20). Absent (or `null`) means the
/// default buckets; a SUPPLIED list is validated element by element and the first bad element
/// fails the call before any cache work starts. Elements are handed over as their compact
/// JSON text, so a `"96"` STRING reaches the shared parser as the wrong-typed thing it is
/// rather than being dropped, and an empty array is refused rather than quietly becoming the
/// default. This function is the JSON front end of that policy, not a second copy of it.
fn prebuild_sizes(args: &Value) -> Result<Vec<u32>, String> {
    let Some(v) = present(args, "sizes") else {
        return Ok(crate::prebuild::DEFAULT_SIZES.to_vec());
    };
    let Some(elements) = v.as_array() else {
        return Err(format!(
            "'sizes' must be an array of whole numbers, found {v}"
        ));
    };
    let texts: Vec<String> = elements.iter().map(Value::to_string).collect();
    crate::prebuild::parse_size_list("'sizes'", texts.iter().map(String::as_str))
}

/// `prebuild`: fill Explorer's thumbnail cache for whole folders.
fn dispatch_prebuild(args: &Value) -> Result<String, String> {
    let inputs = want_str_array(args, "inputs")?;
    if inputs.is_empty() {
        return Err("missing or empty array argument 'inputs'".to_string());
    }
    let sizes = prebuild_sizes(args)?;
    // `jobs` is clamped to 1..=4 by `prebuild::run` itself, so a 0 or a silly large number is
    // still a request that can be honoured; only a non-number is refused.
    let jobs = want_u64(args, "jobs")?.unwrap_or(3);
    cli::prebuild(
        &inputs,
        want_bool(args, "recurse")?.unwrap_or(false),
        sizes,
        want_bool(args, "rebuild_all")?.unwrap_or(false),
        usize::try_from(jobs).unwrap_or(usize::MAX),
    )
}

/// Map a tool name + arguments to a [`crate::cli`] verb. `Err` = a tool error
/// (bad/missing args or the verb failing), surfaced to the agent as text.
fn dispatch_tool(name: &str, args: &Value) -> Result<String, String> {
    match name {
        "thumbnail" => cli::thumbnail(
            &need_str(args, "input")?,
            &need_str(args, "output")?,
            want_size(args, "size", 256)?,
        ),
        "convert" => dispatch_convert(args),
        "compress" => cli::compress(
            &need_str(args, "input")?,
            cli::parse_size(&need_str(args, "max_size")?)?,
        ),
        "rotate" => cli::rotate(&need_str(args, "input")?, &need_str(args, "by")?),
        "strip" => cli::strip_meta(&need_str(args, "input")?),
        "ocr" => cli::ocr(&need_str(args, "input")?),
        "pdf" => dispatch_pdf(args),
        "cbz" => dispatch_cbz(args),
        "info" => cli::info(&need_str(args, "input")?, true),
        "formats" => Ok(cli::list_formats(true)),
        "doctor" => Ok(crate::doctor::report(want_str(args, "file")?.as_deref())),
        "batch" => dispatch_batch(args),
        "prebuild" => dispatch_prebuild(args),
        "register_status" => cli::register_portable(false, true),
        other => Err(format!("unknown tool '{other}'")),
    }
}

/// Clamp a JSON `u64` size argument into `u32`, saturating rather than truncating. A plain
/// `as u32` cast WRAPS at 2^32 (`u32::MAX as u64 + 1` overflows back to 0), and 0 already
/// means something to every size-taking tool here ("full size, no downscale") — so a
/// client that sent an out-of-range size would silently get the opposite of what it asked
/// for instead of a large-but-sane clamp.
fn saturating_u32(v: u64) -> u32 {
    v.min(u32::MAX as u64) as u32
}

/// [`saturating_u32`], additionally clamped to the decoder's own bomb-guard ceiling
/// — `0` is left alone (every size-taking tool here treats it as "full size", a
/// documented sentinel, not a request for `MAX_DIM`). A very large explicit `size` used to
/// reach `decode::pdf_raster_edge` (whose only bound is a FLOOR at 1024, no ceiling) and
/// request a multi-billion-pixel raster; `pdf.rs`'s own doc notes it accepts a leaked
/// worker "in a disposable [dllhost/prevhost] host" — the MCP server is not disposable.
fn clamp_requested_size(v: u64) -> u32 {
    let v = saturating_u32(v);
    if v == 0 {
        0
    } else {
        v.min(crate::decode::limits::MAX_DIM)
    }
}

fn result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_resp(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn write_msg(out: &mut impl Write, msg: &Value) -> std::io::Result<()> {
    let s = serde_json::to_string(msg)?;
    out.write_all(s.as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_advertises_tools() {
        let req = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} });
        let resp = handle(&req).unwrap();
        assert_eq!(resp["id"], json!(1));
        assert_eq!(resp["result"]["protocolVersion"], json!(PROTOCOL_VERSION));
        assert!(resp["result"]["capabilities"]["tools"].is_object());
        assert_eq!(resp["result"]["serverInfo"]["name"], json!("sagethumbs2k"));
    }

    #[test]
    fn tools_list_has_all_verbs() {
        let req = json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" });
        let resp = handle(&req).unwrap();
        let tools = resp["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        for v in [
            "thumbnail",
            "convert",
            "rotate",
            "strip",
            "ocr",
            "pdf",
            "cbz",
            "info",
            "formats",
            "doctor",
            "batch",
            "prebuild",
            "register_status",
        ] {
            assert!(names.contains(&v), "tools/list missing '{v}'");
        }
        // Every tool carries an object input schema.
        assert!(tools
            .iter()
            .all(|t| t["inputSchema"]["type"] == json!("object")));
    }

    #[test]
    fn notification_gets_no_response() {
        let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        assert!(
            handle(&note).is_none(),
            "notifications must not be answered"
        );
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let req = json!({ "jsonrpc": "2.0", "id": 9, "method": "bogus/thing" });
        let resp = handle(&req).unwrap();
        assert_eq!(resp["error"]["code"], json!(-32601));
    }

    #[test]
    fn tools_call_formats_returns_json_text() {
        let req = json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": { "name": "formats", "arguments": {} } });
        let resp = handle(&req).unwrap();
        assert_eq!(resp["result"]["isError"], json!(false));
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text.trim_start().starts_with('['),
            "formats should be a JSON array"
        );
        assert!(text.contains("\"ext\":\"png\""), "should list png");
    }

    #[test]
    fn tools_call_thumbnail_runs_the_verb() {
        let dir = std::env::temp_dir().join(format!("st2k_mcp_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("in.png");
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(300, 200))
            .save(&src)
            .unwrap();
        let out = dir.join("out.png");

        let req = json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {
            "name": "thumbnail",
            "arguments": { "input": src.to_str().unwrap(), "output": out.to_str().unwrap(), "size": 64 }
        }});
        let resp = handle(&req).unwrap();
        assert_eq!(resp["result"]["isError"], json!(false), "got {resp}");
        assert!(
            out.exists(),
            "thumbnail tool should have written the output"
        );
        let d = image::open(&out).unwrap();
        assert!(d.width() <= 64 && d.height() <= 64);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tools_call_missing_arg_is_tool_error() {
        let req = json!({ "jsonrpc": "2.0", "id": 5, "method": "tools/call",
            "params": { "name": "thumbnail", "arguments": { "input": "x.png" } } });
        let resp = handle(&req).unwrap();
        assert_eq!(
            resp["result"]["isError"],
            json!(true),
            "missing 'output' is a tool error"
        );
    }

    #[test]
    fn read_line_capped_rejects_an_oversized_line_even_when_the_newline_arrives_in_the_same_chunk()
    {
        // A `BufReader` over a `Cursor` presents the WHOLE remaining slice in a single
        // `fill_buf` call when it fits the default internal buffer — the exact case where
        // the newline and the size overshoot land in the same chunk. `if done { break; }`
        // used to run before the size check, so this oversized line was accepted instead
        // of dropped.
        let data = b"123456789012345\n".to_vec(); // 16 bytes, well past the 10-byte cap
        let mut reader = std::io::BufReader::new(std::io::Cursor::new(data));
        let mut line = String::new();
        let n = read_line_capped(&mut reader, &mut line, 10).unwrap();
        assert_eq!(n, 0, "an oversized line must be dropped, not accepted");
        assert!(line.is_empty(), "no partial line should have been kept");
    }

    #[test]
    fn a_json_rpc_batch_array_gets_an_invalid_request_error_instead_of_silence() {
        // `Value::get` only resolves string keys on `Object`, so an array or bare scalar
        // used to make both "id" and "method" read as absent/empty, falling through to the
        // wildcard arm's `id.map(...)` — `None` for a `None` id — and `serve()` wrote
        // nothing back. The caller would hang waiting for a reply that never arrives.
        let req = json!([{ "jsonrpc": "2.0", "id": 1, "method": "ping" }]);
        let resp = handle(&req).expect("a non-object request must still get a reply");
        assert_eq!(resp["error"]["code"], json!(-32600));
        assert_eq!(resp["id"], Value::Null);
    }

    /// 2026-09-05 audit, F20: the envelope is CHECKED, not merely read. Each row is (the
    /// message, the JSON-RPC error code it must come back with). Before this, `jsonrpc` was
    /// never looked at, so row 1 was dispatched as a perfectly good ping, and a non-string
    /// `method` became `""` and came back as "method not found: ", naming nothing.
    #[test]
    fn a_malformed_envelope_is_an_invalid_request_and_is_never_dispatched() {
        let cases: &[(&str, Value)] = &[
            (
                "wrong jsonrpc version",
                json!({ "jsonrpc": "1.0", "id": 1, "method": "ping" }),
            ),
            (
                "jsonrpc as a number",
                json!({ "jsonrpc": 2.0, "id": 1, "method": "ping" }),
            ),
            ("jsonrpc absent", json!({ "id": 1, "method": "ping" })),
            (
                "id as an array",
                json!({ "jsonrpc": "2.0", "id": [1], "method": "ping" }),
            ),
            (
                "id as an object",
                json!({ "jsonrpc": "2.0", "id": {"n": 1}, "method": "ping" }),
            ),
            (
                "id as a boolean",
                json!({ "jsonrpc": "2.0", "id": true, "method": "ping" }),
            ),
            (
                "method as a number",
                json!({ "jsonrpc": "2.0", "id": 1, "method": 7 }),
            ),
            (
                "method as an array",
                json!({ "jsonrpc": "2.0", "id": 1, "method": ["ping"] }),
            ),
            ("method absent", json!({ "jsonrpc": "2.0", "id": 1 })),
            (
                "a wrong-version NOTIFICATION is still malformed",
                json!({ "jsonrpc": "1.0", "method": "notifications/initialized" }),
            ),
        ];
        for (why, req) in cases {
            let resp = handle(req).unwrap_or_else(|| panic!("{why}: must get a reply"));
            assert_eq!(resp["error"]["code"], json!(-32600), "{why}: got {resp}");
            assert!(
                resp["result"].is_null(),
                "{why}: a rejected envelope must not be dispatched, got {resp}"
            );
        }
    }

    /// The other half of the same rule: a WELL-FORMED envelope is answered exactly as it was
    /// before, so this is a tightening and not a behaviour change. A notification (no `id`)
    /// gets no reply, a request does, and the reply echoes the id it was sent with.
    #[test]
    fn a_well_formed_envelope_still_dispatches() {
        assert!(
            handle(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).is_none(),
            "a valid notification must not be answered"
        );
        assert!(
            handle(&json!({ "jsonrpc": "2.0", "method": "ping" })).is_none(),
            "a request-only method arriving without an id is a notification: no reply"
        );
        for id in [json!(1), json!("abc"), json!(-4), Value::Null] {
            let resp = handle(&json!({ "jsonrpc": "2.0", "id": id.clone(), "method": "ping" }))
                .expect("a request must be answered");
            assert_eq!(resp["id"], id, "the reply must echo the id it was sent");
            assert!(resp["result"].is_object(), "got {resp}");
            assert_eq!(resp["jsonrpc"], json!(JSONRPC_VERSION));
        }
        // A rejected envelope echoes a well-formed id too, so a client can match the
        // rejection to what it sent.
        let resp = handle(&json!({ "jsonrpc": "1.0", "id": 42, "method": "ping" }))
            .expect("must be answered");
        assert_eq!(resp["id"], json!(42));
    }

    /// `tools/call`'s params are transport shape, so a malformed one is a JSON-RPC error
    /// (-32602), NOT a tool result. Keeping the two apart is what lets a client tell "I sent
    /// you nonsense" from "your file is unreadable"; a missing `name` used to be reported as
    /// the tool error "unknown tool ''".
    #[test]
    fn malformed_tools_call_params_are_transport_errors_not_tool_errors() {
        for params in [
            json!("formats"),
            json!([{ "name": "formats" }]),
            json!({ "arguments": {} }),
            json!({ "name": 7 }),
            json!({ "name": "formats", "arguments": [] }),
            json!({ "name": "formats", "arguments": "none" }),
        ] {
            let req =
                json!({ "jsonrpc": "2.0", "id": 20, "method": "tools/call", "params": params });
            let resp = handle(&req).expect("must be answered");
            assert_eq!(
                resp["error"]["code"],
                json!(-32602),
                "params {params}: {resp}"
            );
            assert!(resp["result"].is_null(), "params {params}: {resp}");
        }
        // Absent `arguments` is fine for a tool that takes none, and stays a normal result.
        let req = json!({ "jsonrpc": "2.0", "id": 21, "method": "tools/call",
            "params": { "name": "formats" } });
        let resp = handle(&req).expect("must be answered");
        assert_eq!(resp["result"]["isError"], json!(false), "got {resp}");
    }

    /// The argument validators (F20) and the shared size-list policy (F12) at the JSON front
    /// end. Each row is (tool, arguments, a fragment the refusal must name). All of these
    /// used to run at a DEFAULT and report success for work nobody asked for.
    #[test]
    fn a_malformed_supplied_argument_is_refused_instead_of_becoming_a_default() {
        let cases: &[(&str, Value, &str)] = &[
            // A scalar where an array is required: read as "no inputs at all" before.
            (
                "pdf",
                json!({ "output": "o.pdf", "inputs": "a.png" }),
                "array",
            ),
            ("batch", json!({ "op": "info", "inputs": 5 }), "array"),
            // Mixed-type list.
            (
                "pdf",
                json!({ "output": "o.pdf", "inputs": ["a.png", 5] }),
                "array of strings",
            ),
            // Wrong scalar types.
            (
                "thumbnail",
                json!({ "input": "a.png", "output": "b.png", "size": "big" }),
                "'size'",
            ),
            (
                "thumbnail",
                json!({ "input": "a.png", "output": "b.png", "size": -1 }),
                "'size'",
            ),
            (
                "thumbnail",
                json!({ "input": "a.png", "output": "b.png", "size": 64.5 }),
                "'size'",
            ),
            (
                "thumbnail",
                json!({ "input": 7, "output": "b.png" }),
                "'input'",
            ),
            (
                "batch",
                json!({ "op": "info", "inputs": ["a.png"], "recurse": "yes" }),
                "'recurse'",
            ),
            (
                "convert",
                json!({ "input": "a.png", "output": "b.jpg", "quality": "high" }),
                "'quality'",
            ),
            (
                "pdf",
                json!({ "output": "o.pdf", "inputs": ["a.png"], "strict": 1 }),
                "'strict'",
            ),
            // The size list, one policy with the CLI: mixed types, invalid text, an empty
            // list, zero, a negative and an overflow.
            (
                "prebuild",
                json!({ "inputs": ["."], "sizes": [96, "typo", 768] }),
                "element 2",
            ),
            (
                "prebuild",
                json!({ "inputs": ["."], "sizes": [96, "256"] }),
                "element 2",
            ),
            (
                "prebuild",
                json!({ "inputs": ["."], "sizes": [] }),
                "no sizes given",
            ),
            (
                "prebuild",
                json!({ "inputs": ["."], "sizes": [0] }),
                "element 1",
            ),
            (
                "prebuild",
                json!({ "inputs": ["."], "sizes": [96, 0] }),
                "element 2",
            ),
            (
                "prebuild",
                json!({ "inputs": ["."], "sizes": [-96] }),
                "element 1",
            ),
            (
                "prebuild",
                json!({ "inputs": ["."], "sizes": [4294967296u64] }),
                "too large",
            ),
            (
                "prebuild",
                json!({ "inputs": ["."], "sizes": 96 }),
                "'sizes'",
            ),
            (
                "prebuild",
                json!({ "inputs": ["."], "jobs": "many" }),
                "'jobs'",
            ),
        ];
        for (tool, args, fragment) in cases {
            let req = json!({ "jsonrpc": "2.0", "id": 22, "method": "tools/call",
                "params": { "name": tool, "arguments": args } });
            let resp = handle(&req).expect("must be answered");
            assert_eq!(
                resp["result"]["isError"],
                json!(true),
                "{tool} {args}: a malformed argument is a TOOL error, got {resp}"
            );
            let text = resp["result"]["content"][0]["text"].as_str().unwrap_or("");
            assert!(
                text.contains(*fragment),
                "{tool} {args}: message must name {fragment:?}, got {text:?}"
            );
        }
    }

    /// The distinction the validators exist for: ABSENT keeps the documented default, and
    /// only a SUPPLIED value can be malformed. A `null` is how clients spell an omitted
    /// optional, so it counts as absent, exactly as the accessors it replaced read it.
    #[test]
    fn an_absent_optional_still_takes_its_default() {
        assert_eq!(want_u64(&json!({}), "size").unwrap(), None);
        assert_eq!(
            want_u64(&json!({ "size": Value::Null }), "size").unwrap(),
            None
        );
        assert_eq!(want_u64(&json!({ "size": 64 }), "size").unwrap(), Some(64));
        assert!(want_u64(&json!({ "size": "64" }), "size").is_err());
        assert_eq!(want_size(&json!({}), "size", 256).unwrap(), 256);
        assert_eq!(want_str(&json!({}), "out").unwrap(), None);
        assert_eq!(want_bool(&json!({}), "recurse").unwrap(), None);
        // The size list: absent means the documented buckets, and only those.
        assert_eq!(
            prebuild_sizes(&json!({})).unwrap(),
            crate::prebuild::DEFAULT_SIZES.to_vec()
        );
        assert_eq!(
            prebuild_sizes(&json!({ "sizes": Value::Null })).unwrap(),
            crate::prebuild::DEFAULT_SIZES.to_vec()
        );
        assert_eq!(
            prebuild_sizes(&json!({ "sizes": [96, 512] })).unwrap(),
            vec![96, 512]
        );
    }

    #[test]
    fn saturating_u32_clamps_instead_of_wrapping_at_the_u32_boundary() {
        assert_eq!(saturating_u32(0), 0);
        assert_eq!(saturating_u32(512), 512);
        assert_eq!(saturating_u32(u32::MAX as u64), u32::MAX);
        // The bug this guards against: a plain `as u32` cast wraps 2^32 back to 0, which
        // `view`/`thumbnail` both read as "full size" instead of an out-of-range request.
        assert_eq!(saturating_u32(u32::MAX as u64 + 1), u32::MAX);
        assert_eq!(saturating_u32((u32::MAX as u64) * 3), u32::MAX);
    }

    #[test]
    fn tools_call_doctor_returns_a_text_report() {
        let req = json!({ "jsonrpc": "2.0", "id": 6, "method": "tools/call",
            "params": { "name": "doctor", "arguments": {} } });
        let resp = handle(&req).unwrap();
        assert_eq!(resp["result"]["isError"], json!(false), "got {resp}");
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(!text.is_empty(), "doctor must return a non-empty report");
    }

    #[test]
    fn tools_call_register_status_runs_without_error() {
        let req = json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/call",
            "params": { "name": "register_status", "arguments": {} } });
        let resp = handle(&req).unwrap();
        assert_eq!(resp["result"]["isError"], json!(false), "got {resp}");
    }

    #[test]
    fn tools_call_batch_missing_op_is_a_tool_error() {
        let req = json!({ "jsonrpc": "2.0", "id": 8, "method": "tools/call",
            "params": { "name": "batch", "arguments": { "inputs": ["x.png"] } } });
        let resp = handle(&req).unwrap();
        assert_eq!(
            resp["result"]["isError"],
            json!(true),
            "missing 'op' is a tool error"
        );
    }

    /// A UNC path anywhere in the arguments — bare `\\server\share\...` or the
    /// extended-length `\\?\UNC\server\share\...` spelling — must be refused before any
    /// tool touches it, since merely opening one starts an SMB (and, by default, NTLM)
    /// negotiation. An extended-length LOCAL path (`\\?\C:\...`) must NOT be refused.
    #[test]
    fn unc_paths_are_rejected_in_both_view_and_dispatch_tool() {
        assert!(is_unc_path(r"\\attacker\share\x.jpg"));
        assert!(is_unc_path(r"\\?\UNC\attacker\share\x.jpg"));
        assert!(!is_unc_path(r"\\?\C:\local\path.jpg"));
        assert!(!is_unc_path(r"C:\local\path.jpg"));

        let req = json!({ "jsonrpc": "2.0", "id": 10, "method": "tools/call", "params": {
            "name": "view", "arguments": { "input": r"\\attacker\share\x.jpg" } } });
        let resp = handle(&req).unwrap();
        assert_eq!(resp["result"]["isError"], json!(true), "got {resp}");

        // Also covered inside an ARRAY argument (pdf/batch's `inputs`), not just a bare
        // string field — `find_unc_arg` walks arrays, so this must be caught too.
        let req = json!({ "jsonrpc": "2.0", "id": 11, "method": "tools/call", "params": {
            "name": "pdf", "arguments": { "output": "out.pdf", "inputs": [r"\\attacker\share\x.jpg"] } } });
        let resp = handle(&req).unwrap();
        assert_eq!(resp["result"]["isError"], json!(true), "got {resp}");
    }

    /// The `want_str_array` half: a non-string element in an `inputs` array must ERROR,
    /// not be silently dropped — before this fix, `["a.png", 5, "b.png"]` quietly became
    /// `["a.png", "b.png"]`, e.g. building a PDF with fewer pages than requested while
    /// still reporting success.
    #[test]
    fn want_str_array_errors_on_a_non_string_element_instead_of_dropping_it() {
        let args = json!({ "inputs": ["a.png", 5, "b.png"] });
        let err = want_str_array(&args, "inputs").unwrap_err();
        assert!(err.contains("inputs"));

        // Still fine when every element really is a string, or the key is absent.
        let args = json!({ "inputs": ["a.png", "b.png"] });
        assert_eq!(
            want_str_array(&args, "inputs").unwrap(),
            vec!["a.png".to_string(), "b.png".to_string()]
        );
        assert_eq!(
            want_str_array(&json!({}), "inputs").unwrap(),
            Vec::<String>::new()
        );
    }

    /// The overwrite half: `pdf` must refuse to clobber a file that already exists at
    /// `output` when its extension isn't `.pdf` — the concrete gap: `combine_to_pdf` writes
    /// raw PDF bytes to whatever path it's given with no extension check of its own.
    #[test]
    fn pdf_tool_refuses_to_overwrite_a_foreign_extension() {
        let dir = std::env::temp_dir().join(format!("st2k_mcp_overwrite_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let victim = dir.join("important.docx");
        std::fs::write(&victim, b"not actually a docx, just needs to exist").unwrap();

        let err = refuse_foreign_overwrite(victim.to_str().unwrap(), &["pdf"]).unwrap_err();
        assert!(err.contains("docx"));

        // A NEW path (nothing there yet) must never be blocked.
        let new_path = dir.join("brand_new.pdf");
        assert!(refuse_foreign_overwrite(new_path.to_str().unwrap(), &["pdf"]).is_ok());
        // An EXISTING file with the tool's own extension must never be blocked either —
        // overwriting a same-purpose file is the whole point of the `output` argument.
        let existing_pdf = dir.join("existing.pdf");
        std::fs::write(&existing_pdf, b"pdf bytes").unwrap();
        assert!(refuse_foreign_overwrite(existing_pdf.to_str().unwrap(), &["pdf"]).is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The `cbz` tool must exist end-to-end through the JSON-RPC surface, mirroring
    /// `pdf`'s own coverage — this was the exact gap the review found (PDF had a CLI/MCP
    /// front door, CBZ never did).
    #[test]
    fn tools_call_cbz_runs_the_verb() {
        let dir = std::env::temp_dir().join(format!("st2k_mcp_cbz_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("p1.png");
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(8, 8))
            .save(&a)
            .unwrap();
        let out = dir.join("out.cbz");

        let req = json!({ "jsonrpc": "2.0", "id": 12, "method": "tools/call", "params": {
            "name": "cbz",
            "arguments": { "output": out.to_str().unwrap(), "inputs": [a.to_str().unwrap()] }
        }});
        let resp = handle(&req).unwrap();
        assert_eq!(resp["result"]["isError"], json!(false), "got {resp}");
        assert!(out.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 2026-09-05 audit, F30 + F31 over the JSON-RPC surface. A `pdf` call with one good,
    /// one corrupt and one missing input used to answer `isError: false` with the bare output
    /// path. It now answers a JSON object whose `status` is `partial` and whose `omitted`
    /// list names each unusable input with a distinct cause; `"strict": true` fails and
    /// writes nothing; an `output` that IS one of the inputs (a PDF re-combined over itself,
    /// which the extension guard cannot see) fails with the source byte-identical. Against
    /// the pre-fix code the first assertion on `status` fails (the text is not JSON) and the
    /// alias call succeeds.
    #[test]
    fn tools_call_pdf_reports_omissions_honours_strict_and_refuses_an_alias() {
        let dir = std::env::temp_dir().join(format!("st2k_mcp_partial_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let good = dir.join("good.png");
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(8, 8))
            .save(&good)
            .unwrap();
        let corrupt = dir.join("corrupt.png");
        std::fs::write(&corrupt, b"not a png").unwrap();
        let missing = dir.join("missing.png");
        let inputs = json!([
            good.to_str().unwrap(),
            corrupt.to_str().unwrap(),
            missing.to_str().unwrap()
        ]);
        let call = |id: u64, arguments: Value| {
            let req = json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {
                "name": "pdf", "arguments": arguments } });
            handle(&req).unwrap()
        };
        let text_of = |resp: &Value| {
            resp["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .to_string()
        };

        let out = dir.join("out.pdf");
        let resp = call(
            20,
            json!({ "output": out.to_str().unwrap(), "inputs": inputs }),
        );
        assert_eq!(resp["result"]["isError"], json!(false), "got {resp}");
        let v: Value = serde_json::from_str(&text_of(&resp)).expect("the pdf tool returns JSON");
        assert_eq!(v["status"], "partial", "{v}");
        assert_eq!(v["requested"], 3);
        assert_eq!(v["combined"], 1);
        let causes: Vec<(&str, &str)> = v["omitted"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| (o["input"].as_str().unwrap(), o["cause"].as_str().unwrap()))
            .collect();
        assert!(
            causes.contains(&(corrupt.to_str().unwrap(), "undecodable")),
            "{v}"
        );
        assert!(
            causes.contains(&(missing.to_str().unwrap(), "unreadable")),
            "{v}"
        );
        assert!(out.exists());

        let strict_out = dir.join("strict.pdf");
        let resp = call(
            21,
            json!({ "output": strict_out.to_str().unwrap(), "inputs": inputs, "strict": true }),
        );
        assert_eq!(resp["result"]["isError"], json!(true), "got {resp}");
        assert!(text_of(&resp).contains("strict"), "got {resp}");
        assert!(!strict_out.exists(), "strict must write nothing");

        // Same-type alias: the finished PDF as both an input and the output.
        let before = std::fs::read(&out).unwrap();
        let resp = call(
            22,
            json!({ "output": out.to_str().unwrap(),
            "inputs": [good.to_str().unwrap(), out.to_str().unwrap()] }),
        );
        assert_eq!(resp["result"]["isError"], json!(true), "got {resp}");
        assert!(text_of(&resp).contains("same file"), "got {resp}");
        assert_eq!(
            std::fs::read(&out).unwrap(),
            before,
            "the source PDF was modified"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The `prebuild` tool must exist and reach `cli::prebuild` — checked against the
    /// elevation guard's error text rather than actually filling the thumbnail cache (this
    /// test process is not guaranteed to run un-elevated), the same way `cli.rs`'s own
    /// prebuild tests avoid depending on the live shell.
    #[test]
    fn tools_call_prebuild_reaches_cli_prebuild() {
        let dir = std::env::temp_dir().join(format!("st2k_mcp_prebuild_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let req = json!({ "jsonrpc": "2.0", "id": 13, "method": "tools/call", "params": {
            "name": "prebuild", "arguments": { "inputs": [dir.to_str().unwrap()] } } });
        let resp = handle(&req).unwrap();
        // Either outcome proves the tool reached `cli::prebuild` rather than "unknown tool":
        // a real (un-elevated) run succeeds, an elevated test process gets that guard's error.
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            !text.contains("unknown tool"),
            "prebuild tool must be wired up, got {text}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A request far above the decoder's own ceiling must be clamped down to it, not
    /// forwarded as-is — `0` ("full size") must be left alone.
    #[test]
    fn clamp_requested_size_bounds_to_max_dim_but_leaves_zero_alone() {
        assert_eq!(clamp_requested_size(0), 0);
        assert_eq!(clamp_requested_size(500), 500);
        assert_eq!(
            clamp_requested_size(50_000_000),
            crate::decode::limits::MAX_DIM
        );
    }
}
