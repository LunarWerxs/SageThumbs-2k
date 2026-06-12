//! OpenDocument (.odt/.ods/.odp…) and OOXML PowerPoint (.pptx/.pptm/.potx)
//! thumbnails. Both are ZIP packages carrying a ready-made preview image we just
//! extract — same pattern as ebook covers, no rendering.
//!
//! - ODF: a spec-mandated `Thumbnails/thumbnail.png` (OASIS ODF Part 2).
//! - OOXML: a `docProps/thumbnail.*` part referenced from `_rels/.rels`.
//!   PowerPoint embeds one; Word/Excel only when the user opts in, so it's
//!   commonly absent there — we return None and the shell shows the icon.

use super::zipfmt::{read_named, Zip};

pub enum Kind {
    Odf,
    Ooxml,
}

/// Identify an Office package by its signature entries, or None.
pub fn detect(zip: &mut Zip) -> Option<Kind> {
    // ODF: a `mimetype` entry whose content is an OpenDocument media type.
    if let Some(mt) = read_named(zip, "mimetype") {
        if contains_ci(&mt, b"opendocument") {
            return Some(Kind::Odf);
        }
    }
    // OOXML: the Open Packaging Conventions content-types part.
    if zip.by_name("[Content_Types].xml").is_ok() {
        return Some(Kind::Ooxml);
    }
    None
}

/// Extract the embedded preview, or None for an Office doc that has none.
pub fn extract(zip: &mut Zip, kind: Kind) -> Option<Vec<u8>> {
    match kind {
        Kind::Odf => decodable_image(read_named(zip, "Thumbnails/thumbnail.png")?),
        Kind::Ooxml => ooxml_thumbnail(zip),
    }
}

fn ooxml_thumbnail(zip: &mut Zip) -> Option<Vec<u8>> {
    // Resolve the thumbnail relationship's target from the package-root rels.
    if let Some(rels) = read_named(zip, "_rels/.rels") {
        if let Some(target) = thumbnail_target(&rels) {
            let name = target.trim_start_matches('/');
            if let Some(data) = read_named(zip, name) {
                return decodable_image(data);
            }
        }
    }
    // Fallback to the conventional path desktop PowerPoint uses.
    for name in ["docProps/thumbnail.jpeg", "docProps/thumbnail.jpg", "docProps/thumbnail.png"] {
        if let Some(data) = read_named(zip, name) {
            return decodable_image(data);
        }
    }
    None
}

/// Pull the `Target` of the `<Relationship>` whose `Type` ends in
/// `…/metadata/thumbnail`. A light string parse — the rels file is tiny.
fn thumbnail_target(rels_xml: &[u8]) -> Option<String> {
    let xml = std::str::from_utf8(rels_xml).ok()?;
    for rel in xml.split("<Relationship") {
        // The thumbnail relationship's Type ends in "/metadata/thumbnail".
        if rel.contains("/thumbnail\"") || rel.contains("/thumbnail'") {
            if let Some(t) = attr(rel, "Target") {
                return Some(t);
            }
        }
    }
    None
}

fn attr(s: &str, key: &str) -> Option<String> {
    let pat = format!("{key}=\"");
    let start = s.find(&pat)? + pat.len();
    let end = s[start..].find('"')? + start;
    Some(s[start..end].to_string())
}

fn contains_ci(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w.eq_ignore_ascii_case(needle))
}

/// Office previews are sometimes EMF/WMF, which our image tiers can't decode.
/// Only pass bytes that look like a raster format we can actually render.
fn decodable_image(data: Vec<u8>) -> Option<Vec<u8>> {
    let ok = data.starts_with(&[0xFF, 0xD8, 0xFF]) // JPEG
        || data.starts_with(&[0x89, b'P', b'N', b'G']) // PNG
        || data.starts_with(b"GIF8") // GIF
        || data.starts_with(b"BM") // BMP
        || (data.len() >= 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WEBP"); // WebP
    ok.then_some(data)
}
