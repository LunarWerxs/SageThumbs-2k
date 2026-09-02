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

/// Dispatch one parsed message. Returns `Some(response)` for a request (has an
/// `id`), `None` for a notification (no `id`) or a no-reply method.
fn handle(req: &Value) -> Option<Value> {
    // A JSON-RPC batch array or a bare scalar isn't an object, so `Value::get` (which only
    // resolves string keys on `Object`) silently returns `None` for both "id" and "method"
    // below — that used to fall to the wildcard arm's `id.map(...)`, which is `None` too, so
    // `serve()` wrote nothing back and the caller hung waiting for a reply that never came.
    // Answer immediately instead: id is unknowable for a non-object request, so it's null.
    if !req.is_object() {
        return Some(error_resp(Value::Null, -32600, "Invalid Request"));
    }
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
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
            "description": "Compress an image to a target file size → a '(compressed)' JPEG sibling at or under the limit (quality binary-search, then downscale if needed).",
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
            "description": "Combine one or more images into a single PDF (one image per page). Refuses to overwrite an existing file at 'output' unless its extension is .pdf.",
            "inputSchema": { "type": "object", "properties": {
                "output": str_prop("destination .pdf path"),
                "inputs": { "type": "array", "items": { "type": "string" }, "description": "image paths, in page order" }
            }, "required": ["output", "inputs"] }
        },
        {
            "name": "cbz",
            "description": "Combine one or more images into a single CBZ (comic-book zip) archive, natural-sorted, with a ComicInfo.xml sidecar. Refuses to overwrite an existing file at 'output' unless its extension is .cbz.",
            "inputSchema": { "type": "object", "properties": {
                "output": str_prop("destination .cbz path"),
                "inputs": { "type": "array", "items": { "type": "string" }, "description": "image paths, in page order" }
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
            "description": "Bulk-process many files/folders in one process: thumbnail, convert, or read info (dimensions/EXIF/audio tags, as one JSON array) for every supported file found. Each input directory is scanned ONE level deep unless 'recurse' is true.",
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
                "sizes": { "type": "array", "items": { "type": "integer" }, "description": "edge sizes in px to build (default 96,256,768 — Explorer's Medium/Large/Extra-large buckets)" },
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
    let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let empty = json!({});
    let args = params.get("arguments").unwrap_or(&empty);

    // Reject a UNC path anywhere in the arguments before EITHER of the two dispatch paths
    // below ever sees them — a same-desktop or prompt-injected caller could otherwise force
    // an SMB/NTLM handshake against an attacker-controlled path.
    if let Some(bad) = find_unc_arg(args) {
        return result(
            id,
            json!({ "content": [{ "type": "text", "text": format!("UNC paths are not accepted: {bad}") }], "isError": true }),
        );
    }

    // `view` returns an IMAGE content block (base64 PNG) so the agent can SEE the file —
    // handled before the text-returning dispatch below.
    if name == "view" {
        let Some(input) = args.get("input").and_then(|v| v.as_str()) else {
            return result(
                id,
                json!({ "content": [{ "type": "text", "text": "missing string argument 'input'" }], "isError": true }),
            );
        };
        // Clamp to the decoder's own bomb-guard ceiling — 0 stays 0 ("full size", the
        // documented sentinel `cli::view_png` already handles); anything above the ceiling
        // is clamped rather than reaching the decoder unbounded.
        let size = clamp_requested_size(args.get("size").and_then(|v| v.as_u64()).unwrap_or(512));
        return match cli::view_png(input, size) {
            Ok(png) => {
                // `view` has no output-size cap, unlike the strict inbound
                // `MAX_MSG_BYTES` — a legitimate large image (or `size: 0`, "full size")
                // can base64-encode into tens-to-hundreds of MB written into ONE JSON-RPC
                // line with nothing warning the caller. Refuse rather than write it.
                const MAX_VIEW_PNG_BYTES: usize = 24 * 1024 * 1024;
                if png.len() > MAX_VIEW_PNG_BYTES {
                    return result(
                        id,
                        json!({ "content": [{ "type": "text", "text": format!(
                            "decoded image is {} MB, over the {}-MB view limit — pass a smaller 'size'",
                            png.len() / (1024 * 1024), MAX_VIEW_PNG_BYTES / (1024 * 1024)
                        ) }], "isError": true }),
                    );
                }
                result(
                    id,
                    json!({ "content": [{ "type": "image", "data": STANDARD.encode(&png), "mimeType": "image/png" }], "isError": false }),
                )
            }
            Err(msg) => result(
                id,
                json!({ "content": [{ "type": "text", "text": msg }], "isError": true }),
            ),
        };
    }

    match dispatch_tool(name, args) {
        Ok(text) => result(
            id,
            json!({ "content": [{ "type": "text", "text": text }], "isError": false }),
        ),
        Err(msg) => result(
            id,
            json!({ "content": [{ "type": "text", "text": msg }], "isError": true }),
        ),
    }
}

/// Collect the string array at `k`. `Err` when the key is present as an array but carries a
/// non-string element (before this fix, such an element was silently DROPPED — a mixed-type
/// `inputs` array built a PDF/CBZ/batch with fewer pages/files than requested and reported
/// success); missing/non-array/absent stays `Ok(vec![])`, same as before.
fn want_str_array(args: &Value, k: &str) -> Result<Vec<String>, String> {
    let Some(v) = args.get(k) else {
        return Ok(Vec::new());
    };
    let Some(a) = v.as_array() else {
        return Ok(Vec::new());
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
    let want = |k: &str| args.get(k).and_then(|v| v.as_str()).map(|s| s.to_string());
    let need = |k: &str| want(k).ok_or_else(|| format!("missing string argument '{k}'"));
    let u64_or = |k: &str, d: u64| args.get(k).and_then(|v| v.as_u64()).unwrap_or(d);
    let q = u64_or("quality", 90).clamp(1, 100) as u8;
    let wq = args
        .get("webp_quality")
        .and_then(|v| v.as_u64())
        .map(|w| w.clamp(1, 100) as u8);
    cli::convert(
        &need("input")?,
        &need("output")?,
        q,
        wq,
        cli::parse_resize(want("resize").as_deref())?,
    )
}

/// `pdf`: an output path plus the input file list.
fn dispatch_pdf(args: &Value) -> Result<String, String> {
    let need = |k: &str| {
        args.get(k)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| format!("missing string argument '{k}'"))
    };
    let output = need("output")?;
    refuse_foreign_overwrite(&output, &["pdf"])?;
    cli::pdf(&output, &want_str_array(args, "inputs")?)
}

/// `cbz`: same shape as `pdf`, writing a comic-book zip instead.
fn dispatch_cbz(args: &Value) -> Result<String, String> {
    let need = |k: &str| {
        args.get(k)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| format!("missing string argument '{k}'"))
    };
    let output = need("output")?;
    refuse_foreign_overwrite(&output, &["cbz"])?;
    cli::cbz(&output, &want_str_array(args, "inputs")?)
}

/// `batch`: an operation name over the input file list, plus the same
/// output/size/format/quality/resize options `thumbnail`/`convert` take individually.
fn dispatch_batch(args: &Value) -> Result<String, String> {
    let want = |k: &str| args.get(k).and_then(|v| v.as_str()).map(|s| s.to_string());
    let need = |k: &str| want(k).ok_or_else(|| format!("missing string argument '{k}'"));
    let u64_or = |k: &str, d: u64| args.get(k).and_then(|v| v.as_u64()).unwrap_or(d);
    let recurse = args
        .get("recurse")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    cli::batch(
        &need("op")?,
        &want_str_array(args, "inputs")?,
        recurse,
        want("out").as_deref(),
        clamp_requested_size(u64_or("size", 256)),
        want("to").as_deref(),
        u64_or("quality", 90).clamp(1, 100) as u8,
        cli::parse_resize(want("resize").as_deref())?,
    )
}

/// `prebuild`: fill Explorer's thumbnail cache for whole folders.
fn dispatch_prebuild(args: &Value) -> Result<String, String> {
    let inputs = want_str_array(args, "inputs")?;
    if inputs.is_empty() {
        return Err("missing or empty array argument 'inputs'".to_string());
    }
    let recurse = args
        .get("recurse")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let sizes: Vec<u32> = args
        .get("sizes")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_u64())
                .map(saturating_u32)
                .collect::<Vec<u32>>()
        })
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| crate::prebuild::DEFAULT_SIZES.to_vec());
    let rebuild_all = args
        .get("rebuild_all")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let jobs = args
        .get("jobs")
        .and_then(|v| v.as_u64())
        .map(|j| j as usize)
        .unwrap_or(3);
    cli::prebuild(&inputs, recurse, sizes, rebuild_all, jobs)
}

/// Map a tool name + arguments to a [`crate::cli`] verb. `Err` = a tool error
/// (bad/missing args or the verb failing), surfaced to the agent as text.
fn dispatch_tool(name: &str, args: &Value) -> Result<String, String> {
    let want = |k: &str| args.get(k).and_then(|v| v.as_str()).map(|s| s.to_string());
    let need = |k: &str| want(k).ok_or_else(|| format!("missing string argument '{k}'"));
    let u32_or =
        |k: &str, d: u64| clamp_requested_size(args.get(k).and_then(|v| v.as_u64()).unwrap_or(d));

    match name {
        "thumbnail" => cli::thumbnail(&need("input")?, &need("output")?, u32_or("size", 256)),
        "convert" => dispatch_convert(args),
        "compress" => cli::compress(&need("input")?, cli::parse_size(&need("max_size")?)?),
        "rotate" => cli::rotate(&need("input")?, &need("by")?),
        "strip" => cli::strip_meta(&need("input")?),
        "ocr" => cli::ocr(&need("input")?),
        "pdf" => dispatch_pdf(args),
        "cbz" => dispatch_cbz(args),
        "info" => cli::info(&need("input")?, true),
        "formats" => Ok(cli::list_formats(true)),
        "doctor" => Ok(crate::doctor::report(want("file").as_deref())),
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
