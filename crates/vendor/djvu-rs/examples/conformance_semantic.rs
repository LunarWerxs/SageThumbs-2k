//! Semantic conformance comparison against DjVuLibre's `djvused`.
//!
//! Emits JSONL records for text, text hierarchy, and annotations on every page,
//! plus document-level bookmarks, metadata, and component-directory planes.
//! Signatures deliberately ignore printer-only formatting while retaining
//! semantic content.

use std::path::Path;
use std::process::{Command, ExitCode};

use djvu_rs::DjVuDocument;
use djvu_rs::djvu_document::DjVuBookmark;
use djvu_rs::metadata::DjVuMetadata;
use djvu_rs::text::{TextLayer, TextZone};
use serde_json::json;

fn text_signature(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn bookmarks_value(bookmarks: &[DjVuBookmark]) -> serde_json::Value {
    serde_json::Value::Array(
        bookmarks
            .iter()
            .map(|bookmark| {
                json!({
                    "title": text_signature(&bookmark.title),
                    "url": text_signature(&bookmark.url),
                    "children": bookmarks_value(&bookmark.children),
                })
            })
            .collect(),
    )
}

fn metadata_signature(meta: &DjVuMetadata) -> String {
    let mut pairs = Vec::new();
    let push = |pairs: &mut Vec<(String, String)>, key: &str, value: &Option<String>| {
        if let Some(value) = value {
            pairs.push((key.to_string(), text_signature(value)));
        }
    };
    push(&mut pairs, "title", &meta.title);
    push(&mut pairs, "author", &meta.author);
    push(&mut pairs, "subject", &meta.subject);
    push(&mut pairs, "publisher", &meta.publisher);
    push(&mut pairs, "year", &meta.year);
    push(&mut pairs, "keywords", &meta.keywords);
    for (key, value) in &meta.extra {
        pairs.push((key.to_ascii_lowercase(), text_signature(value)));
    }
    pairs.sort_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(&right.1)));
    pairs
        .into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn djvused_metadata_signature(raw: &str) -> String {
    let mut pairs = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, rest)) = line.split_once('\t') else {
            continue;
        };
        let value = rest.trim().trim_matches('"');
        pairs.push((key.to_ascii_lowercase(), text_signature(value)));
    }
    pairs.sort_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(&right.1)));
    pairs
        .into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn directory_signature(entries: &[(char, String)]) -> String {
    entries
        .iter()
        .map(|(kind, id)| format!("{kind}\t{id}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn djvused_directory_signature(raw: &str) -> String {
    let mut entries = Vec::new();
    for line in raw.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        // Formats:
        //   "1 P 4799 boy.djvu"  or  "I 938 dict0006.iff"  or  "T <thumbnails>"
        if parts.len() >= 3
            && parts[1].len() == 1
            && parts[1].chars().all(|c| c.is_ascii_alphabetic())
        {
            let kind = parts[1].chars().next().unwrap_or('?');
            let id = parts[parts.len() - 1].to_string();
            entries.push((kind, id));
        } else if parts.len() >= 2
            && parts[0].len() == 1
            && parts[0].chars().all(|c| c.is_ascii_alphabetic())
        {
            let kind = parts[0].chars().next().unwrap_or('?');
            let id = parts[parts.len() - 1].to_string();
            entries.push((kind, id));
        }
    }
    directory_signature(&entries)
}

fn zone_kind_name(kind: &djvu_rs::text::TextZoneKind) -> &'static str {
    use djvu_rs::text::TextZoneKind::*;
    match kind {
        Page => "page",
        Column => "column",
        Region => "region",
        Para => "para",
        Line => "line",
        Word => "word",
        Character => "char",
    }
}

fn zone_signature(zone: &TextZone) -> serde_json::Value {
    json!({
        "kind": zone_kind_name(&zone.kind),
        "text": text_signature(&zone.text),
        "children": zone.children.iter().map(zone_signature).collect::<Vec<_>>(),
    })
}

fn text_hierarchy_signature(layer: Option<&TextLayer>) -> String {
    let Some(layer) = layer else {
        return "empty".into();
    };
    if layer.zones.is_empty() {
        return "empty".into();
    }
    // Match DjVuLibre's empty page tree used by print-txt.
    if layer.zones.len() == 1
        && layer.zones[0].children.is_empty()
        && text_signature(&layer.zones[0].text).is_empty()
    {
        return "empty".into();
    }
    serde_json::Value::Array(layer.zones.iter().map(zone_signature).collect()).to_string()
}

fn parse_print_txt_node(
    tokens: &[OutlineToken],
    cursor: &mut usize,
) -> Result<serde_json::Value, String> {
    if tokens.get(*cursor) != Some(&OutlineToken::Left) {
        return Err("print-txt node must start with '('".into());
    }
    *cursor += 1;
    let OutlineToken::Atom(kind) = tokens.get(*cursor).ok_or("missing print-txt kind")? else {
        return Err("print-txt kind must be an atom".into());
    };
    *cursor += 1;
    // Skip the four coordinate numbers when present.
    while matches!(tokens.get(*cursor), Some(OutlineToken::Atom(value)) if value.parse::<i64>().is_ok())
    {
        *cursor += 1;
    }
    let mut text = String::new();
    let mut children = Vec::new();
    while tokens.get(*cursor) != Some(&OutlineToken::Right) {
        match tokens.get(*cursor) {
            Some(OutlineToken::Left) => children.push(parse_print_txt_node(tokens, cursor)?),
            Some(OutlineToken::String(value)) => {
                if !text.is_empty() {
                    text.push(' ');
                }
                text.push_str(&text_signature(value));
                *cursor += 1;
            }
            Some(OutlineToken::Atom(_)) => *cursor += 1,
            Some(OutlineToken::Right) | None => break,
        }
    }
    if tokens.get(*cursor) != Some(&OutlineToken::Right) {
        return Err("print-txt node missing ')'".into());
    }
    *cursor += 1;
    Ok(json!({
        "kind": kind,
        "text": text_signature(&text),
        "children": children,
    }))
}

fn djvused_text_hierarchy_signature(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "(page 0 0 0 0 \"\")" {
        return Ok("empty".into());
    }
    let tokens = outline_tokens(trimmed)?;
    let mut cursor = 0;
    let root = parse_print_txt_node(&tokens, &mut cursor)?;
    if cursor != tokens.len() {
        return Err("print-txt has trailing tokens".into());
    }
    if root
        .get("children")
        .and_then(|value| value.as_array())
        .is_some_and(Vec::is_empty)
        && root.get("text").and_then(|value| value.as_str()) == Some("")
        && root.get("kind").and_then(|value| value.as_str()) == Some("page")
    {
        return Ok("empty".into());
    }
    Ok(serde_json::Value::Array(vec![root]).to_string())
}

#[derive(Debug, PartialEq)]
enum OutlineToken {
    Left,
    Right,
    Atom(String),
    String(String),
}

/// Tokenize djvused S-expressions, including octal byte escapes.
fn outline_tokens(raw: &str) -> Result<Vec<OutlineToken>, String> {
    let bytes = raw.as_bytes();
    let mut output = Vec::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
            continue;
        }
        if bytes[cursor] == b'(' {
            output.push(OutlineToken::Left);
            cursor += 1;
            continue;
        }
        if bytes[cursor] == b')' {
            output.push(OutlineToken::Right);
            cursor += 1;
            continue;
        }
        if bytes[cursor] != b'"' {
            let start = cursor;
            while cursor < bytes.len()
                && !bytes[cursor].is_ascii_whitespace()
                && !matches!(bytes[cursor], b'(' | b')')
            {
                cursor += 1;
            }
            output.push(OutlineToken::Atom(
                String::from_utf8_lossy(&bytes[start..cursor]).into_owned(),
            ));
            continue;
        }
        cursor += 1;
        let mut value = Vec::new();
        while cursor < bytes.len() && bytes[cursor] != b'"' {
            if bytes[cursor] == b'\\' && cursor + 1 < bytes.len() {
                let escaped = bytes[cursor + 1];
                if escaped.is_ascii_digit() {
                    let mut octal = 0u32;
                    let mut count = 0;
                    while count < 3
                        && cursor + 1 + count < bytes.len()
                        && bytes[cursor + 1 + count].is_ascii_digit()
                    {
                        octal = octal * 8 + u32::from(bytes[cursor + 1 + count] - b'0');
                        count += 1;
                    }
                    value.push((octal & 0xff) as u8);
                    cursor += 1 + count;
                } else {
                    value.push(match escaped {
                        b'n' => b'\n',
                        b'r' => b'\r',
                        b't' => b'\t',
                        other => other,
                    });
                    cursor += 2;
                }
            } else {
                value.push(bytes[cursor]);
                cursor += 1;
            }
        }
        if cursor >= bytes.len() {
            return Err("unterminated string in djvused outline".into());
        }
        cursor += 1;
        output.push(OutlineToken::String(
            String::from_utf8_lossy(&value).into_owned(),
        ));
    }
    Ok(output)
}

fn parse_outline_node(
    tokens: &[OutlineToken],
    cursor: &mut usize,
) -> Result<serde_json::Value, String> {
    if tokens.get(*cursor) != Some(&OutlineToken::Left) {
        return Err("bookmark node must start with '('".into());
    }
    *cursor += 1;
    let OutlineToken::String(title) = tokens.get(*cursor).ok_or("missing bookmark title")? else {
        return Err("bookmark title must be a string".into());
    };
    *cursor += 1;
    let OutlineToken::String(url) = tokens.get(*cursor).ok_or("missing bookmark URL")? else {
        return Err("bookmark URL must be a string".into());
    };
    *cursor += 1;
    let mut children = Vec::new();
    while tokens.get(*cursor) == Some(&OutlineToken::Left) {
        children.push(parse_outline_node(tokens, cursor)?);
    }
    if tokens.get(*cursor) != Some(&OutlineToken::Right) {
        return Err("bookmark node missing ')'".into());
    }
    *cursor += 1;
    Ok(json!({
        "title": text_signature(title),
        "url": text_signature(url),
        "children": children,
    }))
}

fn djvused_bookmarks_value(raw: &str) -> Result<serde_json::Value, String> {
    if raw.trim().is_empty() {
        return Ok(json!([]));
    }
    let tokens = outline_tokens(raw)?;
    let mut cursor = 0;
    if tokens.get(cursor) != Some(&OutlineToken::Left)
        || tokens.get(cursor + 1) != Some(&OutlineToken::Atom("bookmarks".into()))
    {
        return Err("djvused outline missing bookmarks root".into());
    }
    cursor += 2;
    let mut bookmarks = Vec::new();
    while tokens.get(cursor) == Some(&OutlineToken::Left) {
        bookmarks.push(parse_outline_node(&tokens, &mut cursor)?);
    }
    if tokens.get(cursor) != Some(&OutlineToken::Right) || cursor + 1 != tokens.len() {
        return Err("djvused outline has malformed trailing tokens".into());
    }
    Ok(serde_json::Value::Array(bookmarks))
}

fn djvused(path: &Path, command: &str) -> Result<String, String> {
    let output = Command::new("djvused")
        .arg(path)
        .arg("-e")
        .arg(command)
        .output()
        .map_err(|error| format!("cannot run djvused: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "djvused failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn emit(file: &Path, page: usize, plane: &str, ours: String, theirs: String) -> bool {
    let matches = ours == theirs;
    println!(
        "{}",
        json!({
            "file": file.to_string_lossy(),
            "page": page,
            "plane": plane,
            "status": if matches { "match" } else { "diverge" },
            "ours": ours,
            "djvulibre": theirs,
        })
    );
    matches
}

fn process(path: &Path, max_pages: usize) -> Result<bool, String> {
    let bytes = std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let document = DjVuDocument::parse(&bytes).map_err(|error| error.to_string())?;
    let mut all_match = true;

    let theirs_bookmarks = djvused_bookmarks_value(&djvused(path, "print-outline")?)?;
    all_match &= emit(
        path,
        0,
        "bookmarks",
        bookmarks_value(document.bookmarks()).to_string(),
        theirs_bookmarks.to_string(),
    );

    let ours_meta = document
        .metadata()
        .map_err(|error| error.to_string())?
        .as_ref()
        .map(metadata_signature)
        .unwrap_or_default();
    // DjVuLibre print-meta exposes annotation-carried file metadata. The corpus
    // fixtures have neither METa nor print-meta payloads today; both empty → match.
    let theirs_meta = djvused_metadata_signature(&djvused(path, "print-meta")?);
    all_match &= emit(path, 0, "metadata", ours_meta, theirs_meta);

    let mut ours_dir: Vec<(char, String)> = document
        .component_directory()
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|entry| (entry.kind, entry.id))
        .collect();
    if ours_dir.is_empty() {
        let fallback = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        ours_dir.push(('P', fallback));
    }
    let theirs_dir = djvused_directory_signature(&djvused(path, "ls")?);
    all_match &= emit(path, 0, "dirm", directory_signature(&ours_dir), theirs_dir);

    let page_count = if max_pages == 0 {
        document.page_count()
    } else {
        document.page_count().min(max_pages)
    };
    for page_index in 0..page_count {
        let page = document
            .page(page_index)
            .map_err(|error| error.to_string())?;
        let ours_text = page
            .text()
            .map_err(|error| error.to_string())?
            .map(|value| text_signature(&value))
            .unwrap_or_default();
        let theirs_text = text_signature(&djvused(
            path,
            &format!("select {}; print-pure-txt", page_index + 1),
        )?);
        all_match &= emit(path, page_index, "text", ours_text, theirs_text);

        let ours_hierarchy = text_hierarchy_signature(
            page.text_layer()
                .map_err(|error| error.to_string())?
                .as_ref(),
        );
        let theirs_hierarchy = djvused_text_hierarchy_signature(&djvused(
            path,
            &format!("select {}; print-txt", page_index + 1),
        )?)?;
        all_match &= emit(
            path,
            page_index,
            "text_hierarchy",
            ours_hierarchy,
            theirs_hierarchy,
        );

        let ours_areas = page
            .annotations()
            .map_err(|error| error.to_string())?
            .map(|(_, areas)| areas.len())
            .unwrap_or(0);
        let theirs_annotations = djvused(path, &format!("select {}; print-ant", page_index + 1))?;
        let theirs_areas = theirs_annotations.matches("(maparea").count();
        all_match &= emit(
            path,
            page_index,
            "annotations",
            ours_areas.to_string(),
            theirs_areas.to_string(),
        );
    }
    Ok(all_match)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut files = Vec::new();
    let mut max_pages = 0;
    let mut index = 0;
    while index < args.len() {
        if args[index] == "--max-pages" {
            index += 1;
            max_pages = args
                .get(index)
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
        } else {
            files.push(args[index].clone());
        }
        index += 1;
    }
    if files.is_empty() {
        eprintln!("usage: conformance_semantic <file.djvu> [...]");
        return ExitCode::from(2);
    }
    let mut success = true;
    for file in files {
        match process(Path::new(&file), max_pages) {
            // Content divergences are valid JSONL results. The report builder
            // must see them so it can publish a FAIL dashboard and artifact.
            Ok(_) => {}
            Err(error) => {
                eprintln!("semantic conformance failed for {file}: {error}");
                success = false;
            }
        }
    }
    if success {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_signature_preserves_word_boundaries() {
        assert_eq!(text_signature(" a\n bc\t"), "a bc");
        assert_ne!(text_signature("a bc"), text_signature("ab c"));
    }

    #[test]
    fn outline_signature_preserves_tree_and_pairing() {
        let nested = r##"(bookmarks ("A" "#1" ("B" "#2")))"##;
        let siblings = r##"(bookmarks ("A" "#1") ("B" "#2"))"##;
        let swapped = r##"(bookmarks ("#1" "A" ("B" "#2")))"##;
        let value = djvused_bookmarks_value(nested).unwrap();
        assert_ne!(value, djvused_bookmarks_value(siblings).unwrap());
        assert_ne!(value, djvused_bookmarks_value(swapped).unwrap());
    }

    #[test]
    fn directory_parser_accepts_page_and_include_rows() {
        let raw = "     I      938  dict0006.iff\n   1 P     1411  p0001.djvu\n";
        assert_eq!(
            djvused_directory_signature(raw),
            "I\tdict0006.iff\nP\tp0001.djvu"
        );
    }
}
