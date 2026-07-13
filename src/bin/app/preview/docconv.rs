//! Load-time document→markdown converters for the Quick preview viewer.
//!
//! CSV/TSV and Jupyter notebooks render THROUGH the existing markdown pipeline: the loader
//! converts them to synthesized markdown once (per load), and the viewer gets the GitHub-grid
//! table / rendered-cells view for free — outline sidebar, links, code highlighting included.
//! Conversion is bounded (row/cell caps with a truncation note), read-only, and lossy by
//! design (it's a PREVIEW, not an editor).

/// A converted document: the synthesized markdown plus any notebook cell attachments
/// (image bytes that live base64-encoded INSIDE the file — the loader pre-decodes them into
/// the viewer's image cache under their `attachment:<cell>/<name>` keys).
pub(super) struct Converted {
    pub md: String,
    pub attachments: Vec<(String, Vec<u8>)>,
}

/// Convert `text` (the raw file, already read + size-capped) for `ext`. `None` = not a
/// convertible type (plain markdown flows through untouched).
pub(super) fn to_markdown(ext: &str, text: &str) -> Option<Converted> {
    match ext {
        "csv" => Some(Converted { md: delimited_table(text, sniff_delim(text)), attachments: Vec::new() }),
        "tsv" => Some(Converted { md: delimited_table(text, b'\t'), attachments: Vec::new() }),
        "ipynb" => Some(ipynb_md(text)),
        _ => None,
    }
}

// ---- CSV / TSV ----------------------------------------------------------------------------

const MAX_ROWS: usize = 1000;
const MAX_COLS: usize = 64;

/// Pick the delimiter a `.csv` actually uses (comma, but European Excel exports use `;` and
/// some tools tab): whichever occurs most in the first non-empty line, outside quotes.
fn sniff_delim(text: &str) -> u8 {
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let mut counts = [0usize; 3]; // , ; \t
    let mut in_q = false;
    for b in line.bytes() {
        match b {
            b'"' => in_q = !in_q,
            b',' if !in_q => counts[0] += 1,
            b';' if !in_q => counts[1] += 1,
            b'\t' if !in_q => counts[2] += 1,
            _ => {}
        }
    }
    match counts.iter().enumerate().max_by_key(|(_, c)| **c) {
        Some((1, c)) if *c > 0 => b';',
        Some((2, c)) if *c > 0 => b'\t',
        _ => b',',
    }
}

/// RFC-4180-ish parse (quoted fields, `""` escapes, embedded delimiters/newlines) into a GFM
/// pipe table: first record = header row, capped rows/cols with a truncation note.
fn delimited_table(text: &str, delim: u8) -> String {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut in_q = false;
    let mut total_rows = 0usize;
    let b = text.as_bytes();
    let mut i = 0;
    let push_field = |row: &mut Vec<String>, field: &mut String| {
        if row.len() < MAX_COLS {
            row.push(std::mem::take(field));
        } else {
            field.clear();
        }
    };
    while i < b.len() {
        let c = b[i];
        if in_q {
            if c == b'"' {
                if b.get(i + 1) == Some(&b'"') {
                    field.push('"');
                    i += 2;
                    continue;
                }
                in_q = false;
            } else {
                // multibyte-safe: push the raw byte run for this char
                let ch_len = utf8_len(c);
                field.push_str(std::str::from_utf8(&b[i..(i + ch_len).min(b.len())]).unwrap_or("\u{FFFD}"));
                i += ch_len;
                continue;
            }
        } else if c == b'"' && field.is_empty() {
            in_q = true;
        } else if c == delim {
            push_field(&mut row, &mut field);
        } else if c == b'\n' || c == b'\r' {
            if c == b'\r' && b.get(i + 1) == Some(&b'\n') {
                i += 1;
            }
            if !field.is_empty() || !row.is_empty() {
                push_field(&mut row, &mut field);
                total_rows += 1;
                if rows.len() <= MAX_ROWS {
                    rows.push(std::mem::take(&mut row));
                } else {
                    row.clear();
                }
            }
        } else {
            let ch_len = utf8_len(c);
            field.push_str(std::str::from_utf8(&b[i..(i + ch_len).min(b.len())]).unwrap_or("\u{FFFD}"));
            i += ch_len;
            continue;
        }
        i += 1;
    }
    if !field.is_empty() || !row.is_empty() {
        push_field(&mut row, &mut field);
        total_rows += 1;
        if rows.len() <= MAX_ROWS {
            rows.push(row);
        }
    }
    if rows.is_empty() {
        return "*(empty file)*".to_string();
    }

    let ncols = rows.iter().map(Vec::len).max().unwrap_or(1).max(1);
    let cell = |s: &str| {
        // Escape the pipe-table structural chars AND markdown inline syntax: a CSV cell is DATA,
        // not authored markdown — without this, a hostile spreadsheet cell like
        // `[click me](https://evil)` renders as a live styled link (spoofing; review finding,
        // 2026-07-13). Backslash-escapes are consumed by the parser, so normal text is unchanged.
        let mut e = String::with_capacity(s.len());
        for ch in s.chars() {
            match ch {
                '\\' | '|' | '[' | ']' | '`' | '*' | '_' | '!' | '<' => {
                    e.push('\\');
                    e.push(ch);
                }
                '\r' | '\n' => e.push(' '),
                _ => e.push(ch),
            }
        }
        e
    };
    let mut out = String::with_capacity(text.len() + rows.len() * 4);
    for (ri, r) in rows.iter().enumerate() {
        out.push('|');
        for ci in 0..ncols {
            out.push(' ');
            out.push_str(&cell(r.get(ci).map(String::as_str).unwrap_or("")));
            out.push_str(" |");
        }
        out.push('\n');
        if ri == 0 {
            out.push('|');
            for _ in 0..ncols {
                out.push_str(" --- |");
            }
            out.push('\n');
        }
    }
    if total_rows > rows.len() {
        // count DATA rows (the first record is the header)
        out.push_str(&format!(
            "\n*Showing the first {} of {} rows.*\n",
            rows.len().saturating_sub(1),
            total_rows.saturating_sub(1)
        ));
    }
    out
}

/// Byte-length of the UTF-8 char starting with `b` (1 for ASCII/continuation garbage).
fn utf8_len(b: u8) -> usize {
    match b {
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => 1,
    }
}

// ---- Jupyter notebook -----------------------------------------------------------------------

const MAX_CELLS: usize = 300;
const MAX_OUTPUT_CHARS: usize = 8_000; // per cell, keeps a runaway training log readable
const MAX_ATTACHMENTS: usize = 64; // bound the pasted-image decode work per notebook

/// Notebook JSON -> markdown: markdown cells verbatim, code cells as fenced blocks in the
/// kernel language, text outputs as plain fences, rich outputs as an italic marker. Cell
/// `attachments` (base64 images referenced as `![](attachment:name)`) are decoded out and
/// namespaced per cell so the loader can render them inline. A file that doesn't parse as a
/// notebook renders as a highlighted JSON block instead.
fn ipynb_md(text: &str) -> Converted {
    let plain = |md: String| Converted { md, attachments: Vec::new() };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return plain(format!("```json\n{text}\n```"));
    };
    let Some(cells) = v.get("cells").and_then(|c| c.as_array()) else {
        return plain(format!("```json\n{text}\n```"));
    };
    let lang = v
        .pointer("/metadata/kernelspec/language")
        .or_else(|| v.pointer("/metadata/language_info/name"))
        .and_then(|l| l.as_str())
        .unwrap_or("python")
        .to_string();

    let mut out = String::with_capacity(text.len() / 2);
    let mut attachments: Vec<(String, Vec<u8>)> = Vec::new();
    for (idx, cell) in cells.iter().take(MAX_CELLS).enumerate() {
        let kind = cell.get("cell_type").and_then(|t| t.as_str()).unwrap_or("");
        let src = join_source(cell.get("source"));
        match kind {
            "markdown" => {
                // Each cell's `attachment:` namespace is private, so prefix both the in-markdown
                // refs and the decoded cache keys with the cell index (`c{idx}/attachment:NAME`)
                // to keep identical names across cells from clashing. The rewritten src stays a
                // colon-scheme local ref (never `http(s)`/`//`/`data:`), so it renders as an
                // inline image whose bytes the loader pre-seeds into the image cache.
                let rewritten = src.replace("](attachment:", &format!("](c{idx}/attachment:"));
                if attachments.len() < MAX_ATTACHMENTS {
                    collect_attachments(cell, idx, &mut attachments);
                }
                out.push_str(rewritten.trim_end());
                out.push_str("\n\n");
            }
            "code" => {
                if !src.trim().is_empty() {
                    out.push_str(&format!("```{lang}\n{}\n```\n\n", src.trim_end()));
                }
                if let Some(outputs) = cell.get("outputs").and_then(|o| o.as_array()) {
                    for o in outputs {
                        push_output(&mut out, o);
                    }
                }
            }
            "raw" if !src.trim().is_empty() => {
                out.push_str(&format!("```\n{}\n```\n\n", src.trim_end()));
            }
            _ => {}
        }
    }
    if cells.len() > MAX_CELLS {
        out.push_str(&format!("*Showing the first {MAX_CELLS} of {} cells.*\n", cells.len()));
    }
    let md = if out.trim().is_empty() {
        "*(empty notebook)*".to_string()
    } else {
        out
    };
    Converted { md, attachments }
}

/// Decode a markdown cell's `attachments` object into `(key, bytes)` pairs, where `key` matches
/// the rewritten `c{idx}/attachment:NAME` src the markdown now references. Each attachment is
/// `{ "name": { "image/png": "base64…", … } }`; we take the first `image/*` MIME.
fn collect_attachments(cell: &serde_json::Value, idx: usize, out: &mut Vec<(String, Vec<u8>)>) {
    use base64::Engine;
    let Some(atts) = cell.get("attachments").and_then(|a| a.as_object()) else {
        return;
    };
    for (name, mimes) in atts {
        if out.len() >= MAX_ATTACHMENTS {
            break;
        }
        let Some(map) = mimes.as_object() else { continue };
        let Some(b64) = map
            .iter()
            .find(|(mime, _)| mime.starts_with("image/"))
            .and_then(|(_, v)| v.as_str())
        else {
            continue;
        };
        // Jupyter wraps base64 payloads with newlines; strip all whitespace before decoding.
        let cleaned: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
        if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(cleaned.as_bytes()) {
            if !bytes.is_empty() {
                out.push((format!("c{idx}/attachment:{name}"), bytes));
            }
        }
    }
}

/// Notebook `source`/text fields are either one string or an array of line strings.
fn join_source(v: Option<&serde_json::Value>) -> String {
    match v {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(a)) => {
            a.iter().filter_map(|s| s.as_str()).collect::<String>()
        }
        _ => String::new(),
    }
}

/// One notebook output object -> markdown (bounded).
fn push_output(out: &mut String, o: &serde_json::Value) {
    let kind = o.get("output_type").and_then(|t| t.as_str()).unwrap_or("");
    let text = match kind {
        "stream" => join_source(o.get("text")),
        "execute_result" | "display_data" => {
            let data = o.get("data");
            let plain = join_source(data.and_then(|d| d.get("text/plain")));
            if plain.is_empty() {
                let has_img = data
                    .and_then(|d| d.as_object())
                    .is_some_and(|m| m.keys().any(|k| k.starts_with("image/")));
                if has_img {
                    out.push_str("*[image output]*\n\n");
                }
                return;
            }
            plain
        }
        "error" => {
            let tb = o
                .get("traceback")
                .and_then(|t| t.as_array())
                .map(|a| a.iter().filter_map(|s| s.as_str()).collect::<Vec<_>>().join("\n"))
                .unwrap_or_default();
            strip_ansi(&tb)
        }
        _ => return,
    };
    let t = text.trim_end();
    if t.is_empty() {
        return;
    }
    let clipped: String = t.chars().take(MAX_OUTPUT_CHARS).collect();
    out.push_str("```\n");
    out.push_str(&clipped);
    if clipped.len() < t.len() {
        out.push_str("\n…");
    }
    out.push_str("\n```\n\n");
}

/// Drop ANSI escape sequences (Jupyter tracebacks are full of `\x1b[0;31m` colouring).
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                // consume to the terminating letter (inclusive)
                for t in chars.by_ref() {
                    if t.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_basic_and_quotes() {
        let md = delimited_table("a,b\n\"x,1\",\"say \"\"hi\"\"\"\n", b',');
        assert!(md.contains("| a | b |"));
        assert!(md.contains("| x,1 | say \"hi\" |"));
        assert!(md.contains("| --- | --- |"));
    }

    #[test]
    fn csv_pipe_escape_and_crlf() {
        let md = delimited_table("h1,h2\r\nval|ue,two\r\n", b',');
        assert!(md.contains("val\\|ue"));
    }

    #[test]
    fn csv_sniffs_semicolon() {
        assert_eq!(sniff_delim("a;b;c\n1;2;3"), b';');
        assert_eq!(sniff_delim("a,b\n"), b',');
    }

    #[test]
    fn ipynb_cells_render() {
        let nb = r##"{"cells":[
            {"cell_type":"markdown","source":["# Title\n","body"]},
            {"cell_type":"code","source":"print(1)","outputs":[
                {"output_type":"stream","text":["1\n"]}]}
        ],"metadata":{"language_info":{"name":"python"}}}"##;
        let md = ipynb_md(nb).md;
        assert!(md.contains("# Title"));
        assert!(md.contains("```python\nprint(1)\n```"));
        assert!(md.contains("```\n1\n```"));
    }

    #[test]
    fn ipynb_garbage_falls_back_to_json_block() {
        assert!(ipynb_md("not json").md.starts_with("```json"));
    }

    #[test]
    fn ipynb_attachment_extracted_and_ref_rewritten() {
        use base64::Engine;
        // "hi" base64 stands in for image bytes; the test only checks plumbing, not decoding.
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"hi");
        let nb = format!(
            r##"{{"cells":[{{"cell_type":"markdown","source":["![fig](attachment:img.png)"],"attachments":{{"img.png":{{"image/png":"{b64}"}}}}}}]}}"##
        );
        let conv = ipynb_md(&nb);
        // the ref is namespaced to the cell, and the decoded key matches it
        assert!(conv.md.contains("](c0/attachment:img.png)"), "md was: {}", conv.md);
        assert_eq!(conv.attachments.len(), 1);
        assert_eq!(conv.attachments[0].0, "c0/attachment:img.png");
        assert_eq!(conv.attachments[0].1, b"hi");
    }

    #[test]
    fn ansi_stripped() {
        assert_eq!(strip_ansi("\u{1b}[0;31mred\u{1b}[0m plain"), "red plain");
    }
}
