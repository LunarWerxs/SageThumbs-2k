//! EPUB cover extraction — the cover cascade ported from DarkThumbs' epub.cpp:
//!   META-INF/container.xml -> OPF rootfile, then in the OPF:
//!     1. <meta name="cover" content="ID"> -> <item id="ID" href="...">
//!     2. content="ID" that is itself an image path
//!     3. EPUB3 <item properties="cover-image"|id="cover-image" href="...">
//!     4. brute-force: first archive image whose name contains "cover"
//! SVG covers (e.g. Standard Ebooks) flow straight to resvg via the decoder.

use super::zipfmt::{read_index, read_named, Zip};

pub fn extract(zip: &mut Zip) -> Option<Vec<u8>> {
    let container = read_named(zip, "META-INF/container.xml")?;
    let container = String::from_utf8_lossy(&container);
    let opf_path = tag_attr_anywhere(&container, "full-path")?;

    let opf = read_named_ci(zip, &opf_path)?;
    let opf = String::from_utf8_lossy(&opf);

    let rootdir = match opf_path.rfind('/') {
        Some(p) => opf_path[..=p].to_string(),
        None => String::new(),
    };

    let cover_path = cover_from_opf(&opf, &rootdir).or_else(|| brute_force_cover(zip))?;
    read_named_ci(zip, &cover_path)
}

fn cover_from_opf(opf: &str, rootdir: &str) -> Option<String> {
    // 1. <meta name="cover" content="ID">
    if let Some(cover_id) = meta_cover(opf) {
        if let Some(href) = item_href_by_id(opf, &cover_id) {
            return Some(pct(&join(rootdir, &href)));
        }
        // 2. some books put the image path directly in content=
        if super::is_image_name(&cover_id) {
            return Some(pct(&join(rootdir, &cover_id)));
        }
    }
    // 3. EPUB3 cover-image item (by `properties` or literal id)
    if let Some(href) = item_href_by_marker(opf, "cover-image") {
        return Some(pct(&join(rootdir, &href)));
    }
    None
}

fn brute_force_cover(zip: &mut Zip) -> Option<String> {
    for i in 0..zip.len() {
        // Skip an unreadable member instead of bailing on the whole archive.
        let Ok(f) = zip.by_index(i) else { continue };
        if f.is_dir() {
            continue;
        }
        let name = f.name().to_string();
        if super::is_image_name(&name) && name.to_ascii_lowercase().contains("cover") {
            return Some(name);
        }
    }
    None
}

// ---- tiny XML/string helpers (mirroring DarkThumbs' substring search) ----

/// The `<...>` tag containing byte position `pos`.
fn tag_around(s: &str, pos: usize) -> Option<&str> {
    let start = s.get(..pos)?.rfind('<')?;
    let rel_end = s.get(pos..)?.find('>')?;
    s.get(start..pos + rel_end + 1)
}

/// Value of `attr="..."` within `tag`.
fn tag_attr(tag: &str, attr: &str) -> Option<String> {
    let key = format!("{attr}=\"");
    let start = tag.find(&key)? + key.len();
    let end = tag.get(start..)?.find('"')? + start;
    Some(tag.get(start..end)?.to_string())
}

/// First `attr="..."` value anywhere in the document.
fn tag_attr_anywhere(s: &str, attr: &str) -> Option<String> {
    let key = format!("{attr}=\"");
    let start = s.find(&key)? + key.len();
    let end = s.get(start..)?.find('"')? + start;
    Some(s.get(start..end)?.to_string())
}

fn meta_cover(opf: &str) -> Option<String> {
    let pos = opf.find("name=\"cover\"")?;
    tag_attr(tag_around(opf, pos)?, "content")
}

fn item_href_by_id(opf: &str, id: &str) -> Option<String> {
    let pos = opf.find(&format!("id=\"{id}\""))?;
    tag_attr(tag_around(opf, pos)?, "href")
}

fn item_href_by_marker(opf: &str, marker: &str) -> Option<String> {
    let pos = opf.find(marker)?;
    let tag = tag_around(opf, pos)?;
    if !tag.contains("<item") {
        return None;
    }
    tag_attr(tag, "href")
}

fn join(rootdir: &str, href: &str) -> String {
    format!("{rootdir}{href}")
}

fn pct(s: &str) -> String {
    percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned()
}

/// Read an entry by exact name, falling back to a case-insensitive match (EPUB
/// hrefs occasionally differ in case from the stored entry).
fn read_named_ci(zip: &mut Zip, name: &str) -> Option<Vec<u8>> {
    if let Some(b) = read_named(zip, name) {
        return Some(b);
    }
    let target = name.to_ascii_lowercase();
    for i in 0..zip.len() {
        let Ok(f) = zip.by_index(i) else { continue };
        if f.name().to_ascii_lowercase() == target {
            drop(f);
            return read_index(zip, i);
        }
    }
    None
}
