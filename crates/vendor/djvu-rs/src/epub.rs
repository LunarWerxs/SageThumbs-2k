//! DjVu to EPUB 3 converter — preserves document structure.
//!
//! Converts DjVu documents to EPUB 3 while preserving:
//! - Page images as PNG (one per page)
//! - Invisible text overlay for search and copy
//! - NAVM bookmarks as EPUB navigation (`nav.xhtml`)
//! - ANTz/ANTa hyperlinks as `<a href>` overlays on each page
//!
//! # Example
//!
//! ```no_run
//! use djvu_rs::djvu_document::DjVuDocument;
//! use djvu_rs::epub::{djvu_to_epub, EpubOptions};
//!
//! let data = std::fs::read("book.djvu").unwrap();
//! let doc = DjVuDocument::parse(&data).unwrap();
//! let epub_bytes = djvu_to_epub(&doc, &EpubOptions::default()).unwrap();
//! std::fs::write("book.epub", epub_bytes).unwrap();
//! ```

use std::io::Write;

use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

use crate::{
    annotation::MapArea,
    djvu_document::{DjVuBookmark, DjVuDocument, DjVuPage, DocError},
    djvu_render::{RenderError, RenderOptions},
};

// ── Errors ────────────────────────────────────────────────────────────────────

/// Errors from EPUB conversion.
#[derive(Debug, thiserror::Error)]
pub enum EpubError {
    /// Document model error.
    #[error("document error: {0}")]
    Doc(#[from] DocError),
    /// Render error.
    #[error("render error: {0}")]
    Render(#[from] RenderError),
    /// ZIP I/O error.
    #[error("zip error: {0}")]
    Zip(#[from] zip::result::ZipError),
    /// I/O error.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

// ── Options ───────────────────────────────────────────────────────────────────

/// Options for EPUB conversion.
#[derive(Debug, Clone)]
pub struct EpubOptions {
    /// Title embedded in the OPF metadata. Defaults to `"DjVu Document"`.
    pub title: String,
    /// Author embedded in the OPF metadata. Defaults to empty.
    pub author: String,
    /// DPI for page rendering. Defaults to 150.
    pub dpi: u32,
    /// BCP-47 language tag for `<dc:language>`. Defaults to `"en"`.
    pub language: String,
    /// ISO 8601 timestamp for `dcterms:modified` (e.g. `"2026-04-14T00:00:00Z"`).
    /// When `None`, the current UTC time is used (computed from `std::time::SystemTime`).
    pub modified: Option<String>,
    /// Append a reflowable-text section after the page image on each page
    /// XHTML, populated from [`TextLayer::reflowable_text`]. Defaults to
    /// `false` (existing fixed-layout behaviour, page image + invisible
    /// overlay text).
    ///
    /// When `true`, each page body gets an extra `<section class="djvu-reflowable">`
    /// containing one `<p>` per extracted paragraph. Reading apps that prefer
    /// flowable text (most e-readers) can render it; image-first viewers
    /// still get the bitmap above.
    pub reflowable_text: bool,
    /// JPEG quality (1–100) for page images. `None` (default) keeps the
    /// PNG-only behaviour. JPEG is core EPUB — every reader renders it — and
    /// wins heavily on photo/continuous-tone pages (#580).
    pub jpeg_quality: Option<u8>,
    /// With `jpeg_quality: Some(_)`, encode each page image *both* ways and
    /// keep the smaller (the PDF_ADAPTIVE_RASTER pattern; only one page's
    /// pair is ever live at once). Without it, `Some(q)` means JPEG always.
    pub adaptive: bool,
}

impl Default for EpubOptions {
    fn default() -> Self {
        Self {
            title: "DjVu Document".to_owned(),
            author: String::new(),
            dpi: 150,
            language: "en".to_owned(),
            modified: None,
            reflowable_text: false,
            jpeg_quality: None,
            adaptive: false,
        }
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Convert a DjVu document to EPUB 3.
///
/// Returns the raw bytes of a valid EPUB file (ZIP archive).
///
/// # Errors
///
/// Returns [`EpubError`] if page rendering or ZIP writing fails.
pub fn djvu_to_epub(doc: &DjVuDocument, opts: &EpubOptions) -> Result<Vec<u8>, EpubError> {
    let buf = Vec::new();
    let cursor = std::io::Cursor::new(buf);
    let mut zip = ZipWriter::new(cursor);

    // 1. mimetype — MUST be first and STORED (no compression), per EPUB spec
    zip.start_file(
        "mimetype",
        SimpleFileOptions::default().compression_method(CompressionMethod::Stored),
    )?;
    zip.write_all(b"application/epub+zip")?;

    // 2. META-INF/container.xml
    zip.start_file(
        "META-INF/container.xml",
        SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
    )?;
    zip.write_all(CONTAINER_XML.as_bytes())?;

    // 3. Per-page content. Building a page's artifacts (render → PNG encode →
    //    text overlay → XHTML) is independent per page and CPU-heavy; only the
    //    ZIP writing must be serial (a single `ZipWriter`, not `Send`). With the
    //    `parallel` feature, build every page's artifacts concurrently via rayon,
    //    then write them in index order — mirrors the PDF parallel exporter
    //    (#298). Output bytes are identical to the sequential path.
    let page_count = doc.page_count();
    let indices: Vec<usize> = crate::export_common::page_indices(doc, None).collect();

    let mut image_names: Vec<String> = Vec::with_capacity(indices.len());
    #[cfg(feature = "parallel")]
    {
        use rayon::prelude::*;
        let artifacts: Vec<PageArtifacts> = indices
            .par_iter()
            .map(|&i| {
                // #629: cold clone — decode caches drop with the page.
                let page = doc.page(i)?.clone();
                build_page_artifacts(&page, i, opts)
            })
            .collect::<Result<Vec<_>, EpubError>>()?;
        for art in &artifacts {
            write_page_artifacts(&mut zip, art)?;
            image_names.push(art.img_name.clone());
        }
    }

    #[cfg(not(feature = "parallel"))]
    for &i in &indices {
        // #629: cold clone — decode caches drop with the page.
        let page = doc.page(i)?.clone();
        let art = build_page_artifacts(&page, i, opts)?;
        write_page_artifacts(&mut zip, &art)?;
        image_names.push(art.img_name.clone());
    }

    // 4. Navigation document
    let nav_xhtml = build_nav(doc.bookmarks(), page_count);
    zip.start_file(
        "OEBPS/nav.xhtml",
        SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
    )?;
    zip.write_all(nav_xhtml.as_bytes())?;

    // 5. OPF package document
    let opf = build_opf(opts, page_count, &image_names);
    zip.start_file(
        "OEBPS/content.opf",
        SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
    )?;
    zip.write_all(opf.as_bytes())?;

    let cursor = zip.finish()?;
    Ok(cursor.into_inner())
}

// ── Per-page writer ───────────────────────────────────────────────────────────

/// CPU-built, ZIP-ready artifacts for one page: the two entries a page
/// contributes (the PNG image and the XHTML document), each with its archive
/// path. Producing these is the parallelisable, `Send`-safe work; writing them
/// into the single `ZipWriter` is the serial tail.
struct PageArtifacts {
    img_path: String,
    /// Image file name (with extension) as referenced by the XHTML/OPF.
    img_name: String,
    png_bytes: Vec<u8>,
    xhtml_path: String,
    xhtml_bytes: Vec<u8>,
}

/// Write one page's pre-built artifacts into the ZIP, in the same order and with
/// the same compression methods the old inline writer used.
fn write_page_artifacts(
    zip: &mut ZipWriter<std::io::Cursor<Vec<u8>>>,
    art: &PageArtifacts,
) -> Result<(), EpubError> {
    zip.start_file(
        &art.img_path,
        SimpleFileOptions::default().compression_method(CompressionMethod::Stored),
    )?;
    zip.write_all(&art.png_bytes)?;

    zip.start_file(
        &art.xhtml_path,
        SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
    )?;
    zip.write_all(&art.xhtml_bytes)?;
    Ok(())
}

fn build_page_artifacts(
    page: &DjVuPage,
    index: usize,
    opts: &EpubOptions,
) -> Result<PageArtifacts, EpubError> {
    // Native page dimensions in DjVu pixels
    let pw = page.width() as u32;
    let ph = page.height() as u32;

    // Scale to the requested output DPI. The pipeline derives the decode scale
    // from `width`, so we set only the size.
    let (w, h) = crate::export_common::size_at_dpi(page, opts.dpi as f32);

    let render_opts = RenderOptions {
        width: w,
        height: h,
        ..RenderOptions::default()
    };
    // Stream the RGBA scanlines when the page allows it, else fall back to a
    // full pixmap — the shared raster seam every exporter routes through.
    let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);
    crate::export_common::render_rows_or_pixmap(page, &render_opts, |row| {
        rgba.extend_from_slice(row);
    })?;

    // Encode the page image (#580): PNG (Gray8 when the render is pure
    // grayscale — 3× less raw data into deflate, pixel-identical), optionally
    // JPEG, or both with keep-smaller (`adaptive`, the PDF_ADAPTIVE_RASTER
    // pattern — only one page's pair of encodings is ever live at once).
    let gray = rgba
        .chunks_exact(4)
        .all(|px| px[0] == px[1] && px[1] == px[2]);
    let make_png = || encode_rgba_to_png(&rgba, w, h, gray);
    let make_jpeg = |q: u8| encode_rgba_to_jpeg(&rgba, w, h, q, gray);
    let (img_bytes, is_jpeg) = match (opts.jpeg_quality, opts.adaptive) {
        (None, _) => (make_png(), false),
        (Some(q), false) => {
            let j = make_jpeg(q);
            if j.is_empty() {
                (make_png(), false)
            } else {
                (j, true)
            }
        }
        (Some(q), true) => {
            let p = make_png();
            let j = make_jpeg(q);
            if !j.is_empty() && j.len() < p.len() {
                (j, true)
            } else {
                (p, false)
            }
        }
    };
    let png_bytes = img_bytes;

    let page_num = index + 1;
    let ext = if is_jpeg { "jpg" } else { "png" };
    let img_name = format!("page_{page_num:04}.{ext}");
    let img_path = format!("OEBPS/images/{img_name}");

    // Text overlay (invisible selectable text)
    let text_overlay = build_text_overlay(page, pw, ph);

    // Hyperlink overlays from ANTz/ANTa annotations
    let hyperlinks = page.hyperlinks().unwrap_or_default();

    // Optional reflowable paragraphs (#228) — extracted from the same text
    // layer that feeds the overlay, but joined with reading-order rules.
    let reflowable: Vec<String> = if opts.reflowable_text {
        page.text_layer()
            .ok()
            .flatten()
            .map(|tl| {
                tl.reflowable_text()
                    .into_iter()
                    .map(|p| p.text)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    // Build XHTML page
    let xhtml = build_page_xhtml(
        &img_name,
        w,
        h,
        pw,
        ph,
        &text_overlay,
        &hyperlinks,
        &reflowable,
    );
    let xhtml_path = format!("OEBPS/pages/page_{page_num:04}.xhtml");

    Ok(PageArtifacts {
        img_path,
        img_name,
        png_bytes,
        xhtml_path,
        xhtml_bytes: xhtml.into_bytes(),
    })
}

// ── PNG encoder ───────────────────────────────────────────────────────────────

fn encode_rgba_to_png(rgba: &[u8], width: u32, height: u32, gray: bool) -> Vec<u8> {
    // Pages are always opaque (the compositor writes alpha=255 inline —
    // ALPHA_INL), so encode RGB — 25% less raw data into deflate for zero
    // information loss (#599) — or Gray8 when the render is pure grayscale
    // (a further 3×, pixel-identical; #580).
    let data: Vec<u8> = if gray {
        rgba.chunks_exact(4).map(|px| px[0]).collect()
    } else {
        rgba_to_rgb(rgba)
    };
    let mut buf = Vec::new();
    {
        let mut enc = png::Encoder::new(std::io::Cursor::new(&mut buf), width, height);
        enc.set_color(if gray {
            png::ColorType::Grayscale
        } else {
            png::ColorType::Rgb
        });
        enc.set_depth(png::BitDepth::Eight);
        if let Ok(mut writer) = enc.write_header() {
            let _ = writer.write_image_data(&data);
        }
    }
    buf
}

/// Encode the page as JPEG (`quality` 1–100); empty `Vec` on encoder failure
/// (the caller falls back to PNG).
fn encode_rgba_to_jpeg(rgba: &[u8], width: u32, height: u32, quality: u8, gray: bool) -> Vec<u8> {
    use jpeg_encoder::{ColorType, Encoder};
    let mut out = Vec::new();
    let (data, ct): (Vec<u8>, ColorType) = if gray {
        (
            rgba.chunks_exact(4).map(|px| px[0]).collect(),
            ColorType::Luma,
        )
    } else {
        (rgba_to_rgb(rgba), ColorType::Rgb)
    };
    let enc = Encoder::new(&mut out, quality);
    if enc.encode(&data, width as u16, height as u16, ct).is_err() {
        return Vec::new();
    }
    out
}

/// Strip the constant alpha channel from packed RGBA rows.
fn rgba_to_rgb(rgba: &[u8]) -> Vec<u8> {
    let mut rgb = Vec::with_capacity(rgba.len() / 4 * 3);
    for px in rgba.chunks_exact(4) {
        rgb.extend_from_slice(&px[..3]);
    }
    rgb
}

// ── Text overlay ─────────────────────────────────────────────────────────────

/// Returns `(x_pct, y_pct, w_pct, h_pct, text)` for word/char zones.
///
/// Coordinates are CSS percentages of the rendered image dimensions.
/// DjVu text zones use bottom-left origin; the y-axis is inverted for CSS.
fn build_text_overlay(page: &DjVuPage, pw: u32, ph: u32) -> Vec<(f32, f32, f32, f32, String)> {
    let text_layer = match page.text_layer() {
        Ok(Some(tl)) => tl,
        _ => return Vec::new(),
    };

    let mut spans = Vec::new();

    // Map each leaf word/character zone (shared zone-walk) to a CSS-percentage
    // overlay rect. DjVu rects are top-left origin; the overlay is anchored
    // from the bottom, so the vertical position is flipped.
    for span in crate::export_common::word_spans(&text_layer) {
        let r = span.rect;
        let x = r.x as f32 / pw as f32 * 100.0;
        let y = crate::export_common::flip_y_bottom(ph, r.y, r.height) as f32 / ph as f32 * 100.0;
        let w = r.width as f32 / pw as f32 * 100.0;
        let h = r.height as f32 / ph as f32 * 100.0;
        if w > 0.0 && h > 0.0 {
            spans.push((x, y, w, h, xml_escape(span.text)));
        }
    }

    spans
}

// ── XHTML page ────────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn build_page_xhtml(
    img_name: &str,
    w: u32,
    h: u32,
    pw: u32,
    ph: u32,
    text_overlay: &[(f32, f32, f32, f32, String)],
    hyperlinks: &[MapArea],
    reflowable: &[String],
) -> String {
    let mut html = String::new();
    html.push_str(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
<head>
<meta charset="UTF-8"/>
<title>Page</title>
<style>
body { margin: 0; padding: 0; }
.djvu-page { position: relative; display: block; }
.djvu-page img { display: block; width: 100%; height: auto; }
.djvu-text {
  position: absolute;
  color: transparent;
  background: transparent;
  white-space: pre;
  overflow: hidden;
  pointer-events: none;
}
.djvu-link {
  position: absolute;
  display: block;
}
</style>
</head>
<body>
"#,
    );

    html.push_str(&format!(
        r#"<div class="djvu-page" style="width:{w}px; height:{h}px;">"#
    ));
    html.push_str(&format!(
        r#"<img src="../images/{img_name}" alt="page" width="{w}" height="{h}"/>"#
    ));

    for (x, y, ww, hh, text) in text_overlay {
        html.push_str(&format!(
            r#"<span class="djvu-text" aria-hidden="true" style="left:{x:.3}%;top:{y:.3}%;width:{ww:.3}%;height:{hh:.3}%;">{text}</span>"#
        ));
    }

    for ma in hyperlinks {
        if let Some((x, y, ww, hh)) = map_area_to_css(ma, pw, ph) {
            let href = resolve_link_href(&ma.url);
            let title = xml_escape(&ma.description);
            html.push_str(&format!(
                r#"<a class="djvu-link" href="{href}" title="{title}" style="left:{x:.3}%;top:{y:.3}%;width:{ww:.3}%;height:{hh:.3}%;"></a>"#
            ));
        }
    }

    html.push_str("</div>\n");

    if !reflowable.is_empty() {
        html.push_str(r#"<section class="djvu-reflowable">"#);
        html.push('\n');
        for para in reflowable {
            html.push_str("  <p>");
            html.push_str(&xml_escape(para));
            html.push_str("</p>\n");
        }
        html.push_str("</section>\n");
    }

    html.push_str("</body>\n</html>\n");
    html
}

/// Convert a `MapArea` shape to CSS percentage coordinates `(left, top, width, height)`.
///
/// Returns `None` for empty or degenerate (zero-area) shapes. The bounding box
/// and that skip are the shared [`crate::export_common::shape_bbox`]; here we
/// only scale into page percentages and flip the bottom-left-origin box to a
/// CSS top offset via [`crate::export_common::flip_y_bottom`] — the same flip
/// the text overlay uses.
fn map_area_to_css(ma: &MapArea, pw: u32, ph: u32) -> Option<(f32, f32, f32, f32)> {
    if pw == 0 || ph == 0 {
        return None;
    }
    let rect = crate::export_common::shape_bbox(&ma.shape)?;
    let x = (rect.x as f32 / pw as f32) * 100.0;
    let y =
        (crate::export_common::flip_y_bottom(ph, rect.y, rect.height) as f32 / ph as f32) * 100.0;
    let ww = (rect.width as f32 / pw as f32) * 100.0;
    let hh = (rect.height as f32 / ph as f32) * 100.0;
    Some((x, y, ww, hh))
}

/// Resolve a DjVu annotation URL to an EPUB-relative href.
fn resolve_link_href(url: &str) -> String {
    bookmark_href(url)
}

// ── OPF package ──────────────────────────────────────────────────────────────

fn build_opf(opts: &EpubOptions, page_count: usize, image_names: &[String]) -> String {
    let title = xml_escape(&opts.title);
    let author = xml_escape(&opts.author);
    let language = xml_escape(&opts.language);
    let modified = opts
        .modified
        .as_deref()
        .map(str::to_owned)
        .unwrap_or_else(current_timestamp);

    let mut manifest_items = String::new();
    let mut spine_items = String::new();

    // nav document
    manifest_items.push_str(
        r#"    <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
"#,
    );

    let media_type = |name: &str| {
        if name.ends_with(".jpg") {
            "image/jpeg"
        } else {
            "image/png"
        }
    };
    let img = |i: usize| -> String {
        image_names
            .get(i - 1)
            .cloned()
            .unwrap_or_else(|| format!("page_{i:04}.png"))
    };

    // cover image (first page)
    if page_count > 0 {
        let name = img(1);
        manifest_items.push_str(&format!(
            "    <item id=\"cover-image\" href=\"images/{name}\" media-type=\"{}\" properties=\"cover-image\"/>\n",
            media_type(&name)
        ));
    }

    for i in 1..=page_count {
        let pid = format!("page_{i:04}");
        // skip the cover-image item (already added above) but still add the page entry
        if i > 1 {
            let name = img(i);
            manifest_items.push_str(&format!(
                "    <item id=\"img_{pid}\" href=\"images/{name}\" media-type=\"{}\"/>\n",
                media_type(&name)
            ));
        }
        manifest_items.push_str(&format!(
            "    <item id=\"{pid}\" href=\"pages/page_{i:04}.xhtml\" media-type=\"application/xhtml+xml\"/>\n"
        ));
        spine_items.push_str(&format!("    <itemref idref=\"{pid}\"/>\n"));
    }

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" epub:type="book"
         xmlns:epub="http://www.idpf.org/2007/ops" unique-identifier="uid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>{title}</dc:title>
    <dc:creator>{author}</dc:creator>
    <dc:language>{language}</dc:language>
    <dc:identifier id="uid">djvu-rs-export</dc:identifier>
    <meta property="dcterms:modified">{modified}</meta>
  </metadata>
  <manifest>
{manifest_items}  </manifest>
  <spine>
{spine_items}  </spine>
</package>
"#
    )
}

/// Return an ISO 8601 UTC timestamp for the current time.
///
/// Uses only `std::time::SystemTime` — no external crate dependency.
fn current_timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Compute Y/M/D H:M:S from Unix timestamp (no leap seconds, Gregorian)
    let (y, mo, d, hh, mm, ss) = unix_secs_to_parts(secs);
    format!("{y:04}-{mo:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// Decompose a Unix timestamp (seconds since 1970-01-01 00:00:00 UTC) into
/// `(year, month, day, hour, min, sec)`.
fn unix_secs_to_parts(secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    let ss = (secs % 60) as u32;
    let mins = secs / 60;
    let mm = (mins % 60) as u32;
    let hours = mins / 60;
    let hh = (hours % 24) as u32;
    let days = (hours / 24) as u32;

    // Days since 1970-01-01 → Gregorian date (algorithm by Henry Fliegel)
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    (y, mo, d, hh, mm, ss)
}

// ── Navigation document ───────────────────────────────────────────────────────

fn build_nav(bookmarks: &[DjVuBookmark], page_count: usize) -> String {
    let toc_items = if bookmarks.is_empty() {
        build_default_nav_items(page_count)
    } else {
        build_bookmark_nav_items(bookmarks)
    };

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
<head><meta charset="UTF-8"/><title>Navigation</title></head>
<body>
<nav epub:type="toc" id="toc">
  <h1>Contents</h1>
  <ol>
{toc_items}  </ol>
</nav>
</body>
</html>
"#
    )
}

fn build_default_nav_items(page_count: usize) -> String {
    let mut s = String::new();
    for i in 1..=page_count {
        s.push_str(&format!(
            "    <li><a href=\"pages/page_{i:04}.xhtml\">Page {i}</a></li>\n"
        ));
    }
    s
}

fn build_bookmark_nav_items(bookmarks: &[DjVuBookmark]) -> String {
    let mut s = String::new();
    for bm in bookmarks {
        let title = xml_escape(&bm.title);
        let href = bookmark_href(&bm.url);
        s.push_str(&format!("    <li><a href=\"{href}\">{title}</a>"));
        if !bm.children.is_empty() {
            s.push_str("\n    <ol>\n");
            s.push_str(&build_bookmark_nav_items_inner(&bm.children, 2));
            s.push_str("    </ol>");
        }
        s.push_str("</li>\n");
    }
    s
}

fn build_bookmark_nav_items_inner(bookmarks: &[DjVuBookmark], depth: usize) -> String {
    let indent = "  ".repeat(depth + 1);
    let mut s = String::new();
    for bm in bookmarks {
        let title = xml_escape(&bm.title);
        let href = bookmark_href(&bm.url);
        s.push_str(&format!("{indent}<li><a href=\"{href}\">{title}</a>"));
        if !bm.children.is_empty() {
            s.push_str(&format!("\n{indent}<ol>\n"));
            s.push_str(&build_bookmark_nav_items_inner(&bm.children, depth + 1));
            s.push_str(&format!("{indent}</ol>"));
        }
        s.push_str("</li>\n");
    }
    s
}

/// Convert a DjVu bookmark URL to an EPUB relative href.
/// DjVu bookmarks use `#page=N` (1-based) or bare `#anchor` format.
fn bookmark_href(url: &str) -> String {
    // Resolve internal `#page=N` / `#page_N` / `#N` references through the
    // shared bookmark parser the PDF exporter also uses (#364).
    if let Some(idx) = crate::export_common::bookmark_page_index(url) {
        let page_num = idx + 1;
        return format!("pages/page_{page_num:04}.xhtml");
    }
    if url.starts_with('#') {
        // plain anchor — link to page 1 with anchor
        return format!("pages/page_0001.xhtml{}", xml_escape(url));
    }
    // External URL — keep as-is
    xml_escape(url)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

const CONTAINER_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="OEBPS/content.opf"
              media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_escape_basic() {
        assert_eq!(
            xml_escape("a&b<c>d\"e'f"),
            "a&amp;b&lt;c&gt;d&quot;e&apos;f"
        );
    }

    #[test]
    fn bookmark_href_page_number() {
        assert_eq!(bookmark_href("#page=3"), "pages/page_0003.xhtml");
        assert_eq!(bookmark_href("#page=1"), "pages/page_0001.xhtml");
    }

    #[test]
    fn bookmark_href_external() {
        assert_eq!(bookmark_href("https://example.com"), "https://example.com");
    }

    #[test]
    fn nav_has_toc_for_empty_bookmarks() {
        let nav = build_nav(&[], 2);
        assert!(nav.contains("epub:type=\"toc\""));
        assert!(nav.contains("page_0001.xhtml"));
        assert!(nav.contains("page_0002.xhtml"));
    }

    #[test]
    fn current_timestamp_looks_like_iso8601() {
        let ts = current_timestamp();
        // e.g. "2026-04-14T12:34:56Z"
        assert_eq!(ts.len(), 20);
        assert!(ts.ends_with('Z'));
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[7..8], "-");
        assert_eq!(&ts[10..11], "T");
    }

    #[test]
    fn unix_secs_epoch() {
        let (y, mo, d, hh, mm, ss) = unix_secs_to_parts(0);
        assert_eq!((y, mo, d, hh, mm, ss), (1970, 1, 1, 0, 0, 0));
    }

    #[test]
    fn unix_secs_known_date() {
        // 2026-04-14T00:00:00Z = 1776124800
        let (y, mo, d, hh, mm, ss) = unix_secs_to_parts(1_776_124_800);
        assert_eq!((y, mo, d, hh, mm, ss), (2026, 4, 14, 0, 0, 0));
    }

    #[test]
    fn epub_options_default_language_is_en() {
        assert_eq!(EpubOptions::default().language, "en");
    }

    #[test]
    fn epub_options_default_modified_is_none() {
        assert!(EpubOptions::default().modified.is_none());
    }

    #[test]
    fn epub_options_default_reflowable_text_is_off() {
        assert!(!EpubOptions::default().reflowable_text);
    }

    #[test]
    fn build_page_xhtml_omits_reflowable_when_empty() {
        let html = build_page_xhtml("p_0001.png", 800, 1000, 800, 1000, &[], &[], &[]);
        assert!(!html.contains("djvu-reflowable"));
    }

    #[test]
    fn build_page_xhtml_emits_reflowable_paragraphs() {
        let paras = vec!["First paragraph.".to_string(), "Second & last.".to_string()];
        let html = build_page_xhtml("p_0001.png", 800, 1000, 800, 1000, &[], &[], &paras);
        assert!(html.contains(r#"<section class="djvu-reflowable">"#));
        assert!(html.contains("<p>First paragraph.</p>"));
        // XML-escapes ampersand
        assert!(html.contains("<p>Second &amp; last.</p>"));
    }

    #[test]
    fn opf_contains_cover_image_for_nonempty_doc() {
        let opf = build_opf(&EpubOptions::default(), 3, &[]);
        assert!(opf.contains("cover-image"));
        assert!(opf.contains("properties=\"cover-image\""));
    }

    #[test]
    fn opf_no_cover_image_for_empty_doc() {
        let opf = build_opf(&EpubOptions::default(), 0, &[]);
        assert!(!opf.contains("cover-image"));
    }

    #[test]
    fn opf_uses_custom_language() {
        let opts = EpubOptions {
            language: "ru".to_owned(),
            ..Default::default()
        };
        let opf = build_opf(&opts, 1, &[]);
        assert!(opf.contains("<dc:language>ru</dc:language>"));
    }

    #[test]
    fn opf_uses_custom_modified() {
        let opts = EpubOptions {
            modified: Some("2025-01-01T00:00:00Z".to_owned()),
            ..Default::default()
        };
        let opf = build_opf(&opts, 1, &[]);
        assert!(opf.contains("2025-01-01T00:00:00Z"));
    }
}
