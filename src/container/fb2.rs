//! FictionBook 2 cover extraction (ported from DarkThumbs' fb.cpp):
//!   <coverpage><image l:href="#ID"/></coverpage>  ->  <binary id="ID">BASE64</binary>
//! FB2 is frequently windows-1251, so the encoding is sniffed from the XML prolog.

use base64::Engine;

/// True if `bytes` looks like a FictionBook XML document.
pub fn looks_like_fb2(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(2048)];
    contains_ci(head, b"<fictionbook")
}

pub fn extract(bytes: &[u8]) -> Option<Vec<u8>> {
    // The whole document is decoded to a String to parse it, so bound the input
    // like every other container path (zip/7z/rar all cap at MAX_COVER). Real
    // FB2 books are KB–low-MB; 64 MiB is very generous and keeps the transient
    // allocation sane. Oversized files just fall back to the default icon.
    if bytes.len() as u64 > super::MAX_COVER.saturating_mul(2) {
        return None;
    }
    let text = decode_xml(bytes);
    let cover_id = coverpage_id(&text)?;
    let b64 = binary_by_id(&text, &cover_id)?;
    let cleaned: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
    let out = base64::engine::general_purpose::STANDARD.decode(cleaned.as_bytes()).ok()?;
    (out.len() as u64 <= super::MAX_COVER).then_some(out)
}

/// Decode the document to a String, honoring the prolog's declared encoding
/// (FB2 is commonly windows-1251), defaulting to UTF-8.
fn decode_xml(bytes: &[u8]) -> String {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(256)]);
    let label = head
        .find("encoding=\"")
        .and_then(|p| {
            let p = p + 10;
            head.get(p..).and_then(|r| r.find('"')).map(|e| &head[p..p + e])
        })
        .unwrap_or("utf-8");
    let enc = encoding_rs::Encoding::for_label(label.as_bytes()).unwrap_or(encoding_rs::UTF_8);
    enc.decode(bytes).0.into_owned()
}

/// The binary id referenced by `<coverpage>`'s image href (leading '#' stripped).
fn coverpage_id(text: &str) -> Option<String> {
    let cp = text.find("<coverpage")?;
    let rest = text.get(cp..)?;
    let hp = rest.find("href=\"")? + 6;
    let he = rest.get(hp..)?.find('"')? + hp;
    let id = rest.get(hp..he)?.trim_start_matches('#');
    (!id.is_empty()).then(|| id.to_string())
}

/// The base64 payload of `<binary id="ID" ...>...</binary>`.
fn binary_by_id(text: &str, id: &str) -> Option<String> {
    let needle = format!("id=\"{id}\"");
    let mut from = 0usize;
    loop {
        let p = text.get(from..)?.find(&needle)? + from;
        let lt = text.get(..p)?.rfind('<')?;
        if text.get(lt..)?.starts_with("<binary") {
            let gt = text.get(p..)?.find('>')? + p + 1;
            let end = text.get(gt..)?.find("</binary>")? + gt;
            return Some(text.get(gt..end)?.to_string());
        }
        from = p + needle.len();
    }
}

fn contains_ci(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || hay.len() < needle.len() {
        return false;
    }
    hay.windows(needle.len()).any(|w| w.eq_ignore_ascii_case(needle))
}
