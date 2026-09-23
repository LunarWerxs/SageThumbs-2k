#![cfg(test)]

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
fn read_line_capped_rejects_an_oversized_line_even_when_the_newline_arrives_in_the_same_chunk() {
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
    let resp =
        handle(&json!({ "jsonrpc": "1.0", "id": 42, "method": "ping" })).expect("must be answered");
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
        let req = json!({ "jsonrpc": "2.0", "id": 20, "method": "tools/call", "params": params });
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
        st2k_codecs::decode::limits::MAX_DIM
    );
}
