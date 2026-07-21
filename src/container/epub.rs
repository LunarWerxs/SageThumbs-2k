//! EPUB cover extraction — the cover cascade ported from DarkThumbs' epub.cpp:
//!   META-INF/container.xml -> OPF rootfile, then in the OPF:
//!     1. <meta name="cover" content="ID"> -> <item id="ID" href="...">
//!     2. content="ID" that is itself an image path
//!     3. EPUB3 <item properties="cover-image"|id="cover-image" href="...">
//!     4. deprecated EPUB2 <guide><reference type="cover" href="..."> (often xhtml)
//!     5. brute-force: first archive image whose name contains "cover"
//! When the resolved cover is itself an (x)html WRAPPER page (cover.xhtml that just
//! <img>/<svg>-references the real image — Standard Ebooks, many EPUB2 books), we
//! follow it to the embedded image instead of handing the shell undecodable XHTML
//! (DarkThumbs issues #9 / #20 / #34). SVG cover *files* flow straight to resvg.

use std::io::{Read, Seek};

use zip::ZipArchive;

use super::zipfmt::{read_index, read_named};

/// Generic over the archive's reader so the SEEKABLE path (an oversized EPUB read
/// straight off the shell's IStream) runs this same cascade as the in-memory one —
/// they used to diverge, and a big book got a worse cover than a small one.
pub fn extract<R: Read + Seek>(zip: &mut ZipArchive<R>) -> Option<Vec<u8>> {
    let container = read_named(zip, "META-INF/container.xml")?;
    let container = String::from_utf8_lossy(&container);
    let opf_path = tag_attr_anywhere(&container, "full-path")?;

    let opf = read_named_ci(zip, &opf_path)?;
    let opf = String::from_utf8_lossy(&opf);

    let rootdir = match opf_path.rfind('/') {
        Some(p) => opf_path[..=p].to_string(),
        None => String::new(),
    };

    let cover_path = cover_from_opf(&opf, &rootdir)
        .or_else(|| guide_cover(&opf, &rootdir))
        .or_else(|| brute_force_cover(zip))?;

    // The cover may be an XHTML wrapper that only references the real image. Follow
    // it; if the wrapper holds no usable image, fall back to brute-force search.
    if is_html_path(&cover_path) {
        if let Some(img) = resolve_html_cover(zip, &cover_path) {
            return Some(img);
        }
        let fallback = brute_force_cover(zip)?;
        return read_named_ci(zip, &fallback);
    }
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

fn brute_force_cover<R: Read + Seek>(zip: &mut ZipArchive<R>) -> Option<String> {
    // Bounded like every other listing path (`MAX_LIST_ENTRIES`): this allocates a
    // String per entry, and since the cascade now also runs on the SEEKABLE path,
    // it meets archives large enough to be streamed. A crafted directory declaring
    // millions of entries must not drive millions of allocations in the shell host.
    for i in 0..zip.len().min(super::MAX_LIST_ENTRIES) {
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

// ---- xhtml-wrapper cover following (#9 / #20 / #34) ----

/// Does this archive path point at an (x)html page rather than a real image?
fn is_html_path(p: &str) -> bool {
    let lower = p.to_ascii_lowercase();
    lower.ends_with(".xhtml") || lower.ends_with(".html") || lower.ends_with(".htm")
}

/// Deprecated EPUB2 cover declaration: `<guide><reference type="cover" href="...">`.
/// The href is OPF-relative (usually a cover.xhtml page, occasionally an image).
fn guide_cover(opf: &str, rootdir: &str) -> Option<String> {
    let pos = opf.find("type=\"cover\"")?;
    let tag = tag_around(opf, pos)?;
    if !tag.contains("reference") {
        return None;
    }
    let href = tag_attr(tag, "href")?;
    Some(pct(&join(rootdir, &href)))
}

/// Read an (x)html cover wrapper and return the image it references: the first
/// `<img src>` or SVG `<image (xlink:)href>`, resolved relative to the page's own
/// directory (so `../Images/cover.jpg` works), then fetched from the archive.
fn resolve_html_cover<R: Read + Seek>(zip: &mut ZipArchive<R>, html_path: &str) -> Option<Vec<u8>> {
    let html = read_named_ci(zip, html_path)?;
    let html = String::from_utf8_lossy(&html);
    let src = first_html_image(&html)?;
    let base = match html_path.rfind('/') {
        Some(p) => &html_path[..=p],
        None => "",
    };
    let resolved = resolve_relative(base, &pct(&src));
    if !super::is_image_name(&resolved) {
        return None;
    }
    read_named_ci(zip, &resolved)
}

/// First image reference in an (x)html/SVG page: the earliest `<img` (HTML) or
/// `<image` (SVG) tag — note `<img` is NOT a prefix of `<image` (img vs im*a*ge),
/// so both must be searched. We then read `src`, else `xlink:href`, else `href`.
fn first_html_image(html: &str) -> Option<String> {
    let mut search = 0usize;
    loop {
        let rest = html.get(search..)?;
        let next = ["<img", "<image"].iter().filter_map(|pat| rest.find(pat)).min()?;
        let pos = search + next;
        if let Some(tag) = tag_from(html, pos) {
            let href = tag_attr(tag, "src")
                .or_else(|| tag_attr(tag, "xlink:href"))
                .or_else(|| tag_attr(tag, "href"));
            if let Some(v) = href {
                if !v.is_empty() {
                    return Some(v);
                }
            }
        }
        search = pos + 1; // past this '<' so we don't re-match the same tag
    }
}

/// The `<...>` tag that STARTS at byte position `start` (the '<').
fn tag_from(s: &str, start: usize) -> Option<&str> {
    let rel_end = s.get(start..)?.find('>')?;
    s.get(start..start + rel_end + 1)
}

/// Resolve `href` (may contain `./`, `../`, a leading `/`, or a `#fragment`)
/// against `base` (a dir ending in `/`, or empty), normalizing to an archive path.
fn resolve_relative(base: &str, href: &str) -> String {
    let href = href.split('#').next().unwrap_or(href);
    let combined = if let Some(abs) = href.strip_prefix('/') {
        abs.to_string()
    } else {
        format!("{base}{href}")
    };
    let mut out: Vec<&str> = Vec::new();
    for part in combined.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            p => out.push(p),
        }
    }
    out.join("/")
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
fn read_named_ci<R: Read + Seek>(zip: &mut ZipArchive<R>, name: &str) -> Option<Vec<u8>> {
    if let Some(b) = read_named(zip, name) {
        return Some(b);
    }
    let target = name.to_ascii_lowercase();
    // Same `MAX_LIST_ENTRIES` bound as the other listing scans (see
    // `brute_force_cover`) — this fallback lowercases every entry name.
    for i in 0..zip.len().min(super::MAX_LIST_ENTRIES) {
        let Ok(f) = zip.by_index(i) else { continue };
        if f.name().to_ascii_lowercase() == target {
            drop(f);
            return read_index(zip, i);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_path_detection() {
        assert!(is_html_path("OEBPS/Text/cover.xhtml"));
        assert!(is_html_path("cover.HTML"));
        assert!(is_html_path("a/b.htm"));
        assert!(!is_html_path("Images/cover.jpg"));
        assert!(!is_html_path("cover.svg")); // an SVG file is a real image (resvg)
    }

    #[test]
    fn relative_resolution() {
        assert_eq!(resolve_relative("OEBPS/Text/", "../Images/cover.jpg"), "OEBPS/Images/cover.jpg");
        assert_eq!(resolve_relative("OEBPS/Text/", "./pic.png"), "OEBPS/Text/pic.png");
        assert_eq!(resolve_relative("OEBPS/Text/", "cover.jpg#x"), "OEBPS/Text/cover.jpg");
        assert_eq!(resolve_relative("OEBPS/Text/", "/Images/c.jpg"), "Images/c.jpg");
        assert_eq!(resolve_relative("", "cover.png"), "cover.png");
    }

    #[test]
    fn first_image_in_img_and_svg() {
        // plain <img>
        assert_eq!(
            first_html_image(r#"<html><body><img alt="c" src="../Images/cover.jpg"/></body></html>"#).as_deref(),
            Some("../Images/cover.jpg")
        );
        // SVG <image xlink:href> (Standard Ebooks style)
        assert_eq!(
            first_html_image(r#"<svg><image width="1" xlink:href="images/cover.svg"/></svg>"#).as_deref(),
            Some("images/cover.svg")
        );
        // SVG <image href> (no xlink namespace)
        assert_eq!(
            first_html_image(r#"<svg><image href="cover.png"/></svg>"#).as_deref(),
            Some("cover.png")
        );
        assert_eq!(first_html_image("<html><body>no images here</body></html>"), None);
    }

    #[test]
    fn guide_reference_cover() {
        let opf = r#"<package><guide><reference type="cover" title="Cover" href="Text/cover.xhtml"/></guide></package>"#;
        assert_eq!(guide_cover(opf, "OEBPS/").as_deref(), Some("OEBPS/Text/cover.xhtml"));
        // a <meta name="cover"> alone (no <reference>) must not be mistaken for a guide
        assert_eq!(guide_cover(r#"<meta name="cover" content="id"/>"#, ""), None);
    }
}
