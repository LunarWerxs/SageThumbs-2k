//! Minimal MCP (Model Context Protocol) server over stdio — `st2k --mcp`.
//!
//! **Not a daemon.** An MCP client (Claude Desktop, an IDE agent, …) spawns this
//! as a child process, exchanges newline-delimited JSON-RPC 2.0 messages over
//! stdin/stdout, and terminates it when the client closes. Every tool just calls
//! the same [`crate::cli`] verbs the command line uses, so an agent gets the
//! bundled offline image engine (decode 178 formats, convert, rotate, strip,
//! OCR, PDF, info) with zero extra installs.
//!
//! The transport is the MCP stdio framing: one JSON-RPC message per line, no
//! embedded newlines (serde_json::to_string never emits any).

use std::io::{BufRead, Write};

use serde_json::{json, Value};

use crate::cli;
use crate::verbs::Resize;

/// MCP protocol revision we implement (the stable 2024-11-05 spec).
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Read JSON-RPC messages from stdin and reply on stdout until EOF (the client
/// closing its end). Locks both streams for the process lifetime — fine for a
/// dedicated child server.
pub fn serve() -> std::io::Result<()> {
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break; // EOF: client closed the pipe
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
        "instructions": "Offline image toolbox: decode ~178 formats, convert, rotate/flip, strip metadata, OCR, combine to PDF, and read image info. All tools take local file paths."
    })
}

/// The tool catalog (name + description + JSON-Schema for arguments).
fn tool_defs() -> Value {
    let str_prop = |desc: &str| json!({ "type": "string", "description": desc });
    json!([
        {
            "name": "thumbnail",
            "description": "Render any supported image (~178 formats Windows often can't, incl. HEIC/RAW/PSD/ebook covers) to an image file, capped to a max long-edge size.",
            "inputSchema": { "type": "object", "properties": {
                "input": str_prop("path to the source image"),
                "output": str_prop("path to write; output format is taken from this extension (.png/.jpg/…)"),
                "size": { "type": "integer", "description": "max long-edge in px (default 256; 0 = full size)" }
            }, "required": ["input", "output"] }
        },
        {
            "name": "convert",
            "description": "Convert an image to another format (format from the output extension), with optional quality and resize.",
            "inputSchema": { "type": "object", "properties": {
                "input": str_prop("source image path"),
                "output": str_prop("destination path; format from its extension"),
                "quality": { "type": "integer", "description": "encoder quality 1-100 (JPEG; default 90)" },
                "resize": str_prop("optional 'WxH' (fit, no upscale) or 'N%' (scale)")
            }, "required": ["input", "output"] }
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
            "description": "Losslessly strip EXIF/IPTC/XMP metadata from a JPEG or PNG in place (keeps the ICC color profile; no pixel re-encode).",
            "inputSchema": { "type": "object", "properties": {
                "input": str_prop("JPEG or PNG path")
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
            "description": "Combine one or more images into a single PDF (one image per page).",
            "inputSchema": { "type": "object", "properties": {
                "output": str_prop("destination .pdf path"),
                "inputs": { "type": "array", "items": { "type": "string" }, "description": "image paths, in page order" }
            }, "required": ["output", "inputs"] }
        },
        {
            "name": "info",
            "description": "Read an image's dimensions and EXIF camera/date/GPS. Returns JSON.",
            "inputSchema": { "type": "object", "properties": {
                "input": str_prop("image path")
            }, "required": ["input"] }
        },
        {
            "name": "formats",
            "description": "List every supported input format (extension, category, description). Returns JSON.",
            "inputSchema": { "type": "object", "properties": {} }
        }
    ])
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

    match dispatch_tool(name, args) {
        Ok(text) => result(id, json!({ "content": [{ "type": "text", "text": text }], "isError": false })),
        Err(msg) => result(id, json!({ "content": [{ "type": "text", "text": msg }], "isError": true })),
    }
}

/// Map a tool name + arguments to a [`crate::cli`] verb. `Err` = a tool error
/// (bad/missing args or the verb failing), surfaced to the agent as text.
fn dispatch_tool(name: &str, args: &Value) -> Result<String, String> {
    let want = |k: &str| args.get(k).and_then(|v| v.as_str()).map(|s| s.to_string());
    let need = |k: &str| want(k).ok_or_else(|| format!("missing string argument '{k}'"));
    let u64_or = |k: &str, d: u64| args.get(k).and_then(|v| v.as_u64()).unwrap_or(d);

    match name {
        "thumbnail" => cli::thumbnail(&need("input")?, &need("output")?, u64_or("size", 256) as u32),
        "convert" => {
            let q = u64_or("quality", 90).clamp(1, 100) as u8;
            cli::convert(&need("input")?, &need("output")?, q, parse_resize(want("resize").as_deref())?)
        }
        "rotate" => cli::rotate(&need("input")?, &need("by")?),
        "strip" => cli::strip_meta(&need("input")?),
        "ocr" => cli::ocr(&need("input")?),
        "pdf" => {
            let inputs: Vec<String> = args
                .get("inputs")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                .unwrap_or_default();
            cli::pdf(&need("output")?, &inputs)
        }
        "info" => cli::info(&need("input")?, true),
        "formats" => Ok(cli::list_formats(true)),
        other => Err(format!("unknown tool '{other}'")),
    }
}

/// Parse the optional `resize` argument ("WxH" fit, or "N%" scale) — same syntax
/// as the `st2k convert --resize` flag.
fn parse_resize(v: Option<&str>) -> Result<Resize, String> {
    let Some(v) = v.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(Resize::None);
    };
    if let Some(p) = v.strip_suffix('%') {
        let pct: u32 = p.trim().parse().map_err(|_| format!("bad percent '{v}'"))?;
        return Ok(Resize::Percent(pct.clamp(1, 1000)));
    }
    let (w, h) = v.split_once(['x', 'X']).ok_or_else(|| format!("bad resize '{v}' (use WxH or N%)"))?;
    let w: u32 = w.trim().parse().map_err(|_| format!("bad width in '{v}'"))?;
    let h: u32 = h.trim().parse().map_err(|_| format!("bad height in '{v}'"))?;
    Ok(Resize::Fit(w.max(1), h.max(1)))
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
        for v in ["thumbnail", "convert", "rotate", "strip", "ocr", "pdf", "info", "formats"] {
            assert!(names.contains(&v), "tools/list missing '{v}'");
        }
        // Every tool carries an object input schema.
        assert!(tools.iter().all(|t| t["inputSchema"]["type"] == json!("object")));
    }

    #[test]
    fn notification_gets_no_response() {
        let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        assert!(handle(&note).is_none(), "notifications must not be answered");
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
        assert!(text.trim_start().starts_with('['), "formats should be a JSON array");
        assert!(text.contains("\"ext\":\"png\""), "should list png");
    }

    #[test]
    fn tools_call_thumbnail_runs_the_verb() {
        let dir = std::env::temp_dir().join("st2k_mcp");
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("in.png");
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(300, 200)).save(&src).unwrap();
        let out = dir.join("out.png");

        let req = json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {
            "name": "thumbnail",
            "arguments": { "input": src.to_str().unwrap(), "output": out.to_str().unwrap(), "size": 64 }
        }});
        let resp = handle(&req).unwrap();
        assert_eq!(resp["result"]["isError"], json!(false), "got {resp}");
        assert!(out.exists(), "thumbnail tool should have written the output");
        let d = image::open(&out).unwrap();
        assert!(d.width() <= 64 && d.height() <= 64);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tools_call_missing_arg_is_tool_error() {
        let req = json!({ "jsonrpc": "2.0", "id": 5, "method": "tools/call",
            "params": { "name": "thumbnail", "arguments": { "input": "x.png" } } });
        let resp = handle(&req).unwrap();
        assert_eq!(resp["result"]["isError"], json!(true), "missing 'output' is a tool error");
    }
}
