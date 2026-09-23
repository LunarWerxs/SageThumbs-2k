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
use st2k_base::formats;

mod tool_defs;
use tool_defs::tool_defs;

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
        let done = read_chunk(reader, &mut buf)?;
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

/// Append the next chunk from `reader` to `buf`, returning `true` once the line is complete:
/// the chunk carrying its `\n` has been consumed, or the reader hit EOF (a final line with no
/// newline). `false` means the chunk had no newline and the next one must be read.
fn read_chunk<R: BufRead>(reader: &mut R, buf: &mut Vec<u8>) -> std::io::Result<bool> {
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
    Ok(done)
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
    while serve_one(&mut reader, &mut out, &mut line, MAX_MSG_BYTES)? {}
    Ok(())
}

/// Read, parse and answer ONE JSON-RPC message on `reader`/`out`; returns `false` when the
/// stream ended (EOF, or a message past `max`) so `serve` stops, `true` to keep reading.
fn serve_one<R: BufRead, W: Write>(
    reader: &mut R,
    out: &mut W,
    line: &mut String,
    max: usize,
) -> std::io::Result<bool> {
    line.clear();
    if read_line_capped(reader, line, max)? == 0 {
        return Ok(false); // EOF: client closed the pipe, or a message blew the cap
    }
    // Trim whitespace AND a stray UTF-8 BOM (`U+FEFF`) — some clients/shells
    // prepend one to the stream, and Rust's `trim()` doesn't treat it as
    // whitespace, so it would otherwise poison the first message.
    let trimmed = line.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
    if trimmed.is_empty() {
        return Ok(true);
    }
    match serde_json::from_str::<Value>(trimmed) {
        Ok(req) => {
            if let Some(resp) = handle(&req) {
                write_msg(out, &resp)?;
            }
        }
        // Malformed JSON: JSON-RPC parse error, id unknowable → null.
        Err(_) => write_msg(out, &error_resp(Value::Null, -32700, "parse error"))?,
    }
    Ok(true)
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
        false,
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
mod tests;
