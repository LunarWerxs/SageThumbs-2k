//! Integration tests for DjVu → PDF conversion.
//!
//! These tests verify that the PDF output is structurally valid and contains
//! the expected features (images, text, bookmarks, hyperlinks).

#![cfg(feature = "pdf")]

use djvu_rs::djvu_document::DjVuDocument;
use djvu_rs::pdf::djvu_to_pdf;
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn load_doc(name: &str) -> DjVuDocument {
    let data = std::fs::read(fixture(name)).unwrap();
    DjVuDocument::parse(&data).unwrap()
}

/// Helper: check that PDF bytes start with the correct header and end with %%EOF.
fn assert_valid_pdf_structure(pdf: &[u8]) {
    assert!(pdf.starts_with(b"%PDF-1.4"), "missing PDF header");
    let tail = &pdf[pdf.len().saturating_sub(20)..];
    assert!(
        tail.windows(5).any(|w| w == b"%%EOF"),
        "missing %%EOF trailer"
    );
    // Check for xref table
    assert!(pdf.windows(4).any(|w| w == b"xref"), "missing xref table");
    // Check for trailer
    assert!(pdf.windows(7).any(|w| w == b"trailer"), "missing trailer");
}

/// Helper: check that the PDF contains a specific byte pattern.
fn pdf_contains(pdf: &[u8], pattern: &[u8]) -> bool {
    pdf.windows(pattern.len()).any(|w| w == pattern)
}

// ── Basic structure tests ────────────────────────────────────────────────────

#[test]
fn test_single_page_iw44_background() {
    let doc = load_doc("boy.djvu");
    let pdf = djvu_to_pdf(&doc).unwrap();
    assert_valid_pdf_structure(&pdf);
    // Should have exactly 1 page
    assert!(pdf_contains(&pdf, b"/Count 1"));
    // Should have an image XObject
    assert!(pdf_contains(&pdf, b"/Subtype /Image"));
    // Should have DeviceRGB color space
    assert!(pdf_contains(&pdf, b"/ColorSpace /DeviceRGB"));
}

#[test]
fn test_bilevel_page_with_mask() {
    let doc = load_doc("boy_jb2.djvu");
    let pdf = djvu_to_pdf(&doc).unwrap();
    assert_valid_pdf_structure(&pdf);
    assert!(pdf_contains(&pdf, b"/Count 1"));
    // Should have an ImageMask for the JB2 foreground
    assert!(pdf_contains(&pdf, b"/ImageMask true"));
}

#[test]
fn test_multi_page_bundled() {
    let doc = load_doc("DjVu3Spec_bundled.djvu");
    let pdf = djvu_to_pdf(&doc).unwrap();
    assert_valid_pdf_structure(&pdf);
    // DjVu3Spec has multiple pages
    assert!(pdf_contains(&pdf, b"/Count "), "should have page count");
    // Pages that fail to render should still produce a page object
    let count_str = String::from_utf8_lossy(&pdf);
    assert!(
        count_str.contains("/Type /Page"),
        "should have at least one page"
    );
}

#[test]
fn test_3layer_composite() {
    // chicken.djvu is a 3-layer document (BG44 + FG44 + Sjbz)
    let doc = load_doc("chicken.djvu");
    let pdf = djvu_to_pdf(&doc).unwrap();
    assert_valid_pdf_structure(&pdf);
    // Should have RGB background image
    assert!(pdf_contains(&pdf, b"/ColorSpace /DeviceRGB"));
}

// ── Text layer tests (#4) ───────────────────────────────────────────────────

#[test]
fn test_text_layer_invisible() {
    // DjVu3Spec has text layers on some pages
    let doc = load_doc("DjVu3Spec_bundled.djvu");
    let pdf = djvu_to_pdf(&doc).unwrap();

    // Content streams are FlateDecode-compressed, so we check for:
    // 1. Font resource references in page objects (not compressed)
    assert!(
        pdf_contains(&pdf, b"/Font <<"),
        "should have font resources for text"
    );
    // 2. Helvetica font definition (separate object, not compressed)
    assert!(
        pdf_contains(&pdf, b"/BaseFont /Helvetica"),
        "should use Helvetica font"
    );
}

// ── Bookmark tests (#5) ─────────────────────────────────────────────────────

#[test]
fn test_bookmarks_outline() {
    let doc = load_doc("navm_fgbz.djvu");
    let bookmarks = doc.bookmarks();

    if !bookmarks.is_empty() {
        let pdf = djvu_to_pdf(&doc).unwrap();
        assert_valid_pdf_structure(&pdf);
        // Should have outline objects
        assert!(
            pdf_contains(&pdf, b"/Type /Outlines"),
            "should have PDF outline (bookmarks)"
        );
        // Should open with outlines visible
        assert!(
            pdf_contains(&pdf, b"/PageMode /UseOutlines"),
            "should set PageMode to show outlines"
        );
    }
}

// ── Hyperlink tests (#6) ────────────────────────────────────────────────────

#[test]
fn test_hyperlinks_annotations() {
    let doc = load_doc("links.djvu");
    let pdf = djvu_to_pdf(&doc).unwrap();
    assert_valid_pdf_structure(&pdf);

    // Check page has hyperlinks
    let page = doc.page(0).unwrap();
    let links = page.hyperlinks().unwrap();
    if !links.is_empty() {
        // Should have link annotations
        assert!(
            pdf_contains(&pdf, b"/Subtype /Link"),
            "should have link annotations"
        );
        // Should have URI action
        assert!(
            pdf_contains(&pdf, b"/S /URI"),
            "should have URI actions for hyperlinks"
        );
    }
}

// ── Edge cases ──────────────────────────────────────────────────────────────

#[test]
fn test_colorbook_multicolor_foreground() {
    let doc = load_doc("colorbook.djvu");
    let pdf = djvu_to_pdf(&doc).unwrap();
    assert_valid_pdf_structure(&pdf);
    // Should still produce valid PDF with color foreground
    assert!(pdf_contains(&pdf, b"/Type /Page"));
}

#[test]
fn test_rotated_page() {
    let doc = load_doc("boy_jb2_rotate90.djvu");
    let pdf = djvu_to_pdf(&doc).unwrap();
    assert_valid_pdf_structure(&pdf);
    assert!(pdf_contains(&pdf, b"/Type /Page"));
}

#[test]
fn test_pdf_output_nonzero_size() {
    // Ensure all fixture files produce non-trivial PDFs
    let fixtures = [
        "boy.djvu",
        "boy_jb2.djvu",
        "chicken.djvu",
        "colorbook.djvu",
        "links.djvu",
        "irish.djvu",
    ];
    for name in &fixtures {
        let doc = load_doc(name);
        let pdf = djvu_to_pdf(&doc).unwrap();
        assert!(
            pdf.len() > 100,
            "{name} produced suspiciously small PDF ({} bytes)",
            pdf.len()
        );
        assert_valid_pdf_structure(&pdf);
    }
}

// ── FGbz coloured foreground stencils (#559) ────────────────────────────────

/// Colored-FGbz pages must emit one stencil per palette colour, painted in
/// non-black fill colours — not a single black /Mask0 that flattens coloured
/// text (#559).
#[test]
fn test_fgbz_colored_foreground_multi_stencil() {
    let doc = load_doc("navm_fgbz.djvu");
    let pdf = djvu_to_pdf(&doc).unwrap();
    assert_valid_pdf_structure(&pdf);
    // More than one stencil layer on at least one page.
    assert!(
        pdf_contains(&pdf, b"/Mask1"),
        "coloured-palette page must emit more than one stencil layer"
    );
    // At least one stencil painted in a non-black, non-white colour: the
    // content streams are deflated, so check via the raw content ops instead.
    // navm_fgbz page 0 has a pure-yellow (255,255,0) highlight → `1 1 0 rg`.
    let decompressed = decompress_all_streams(&pdf);
    assert!(
        decompressed
            .windows(b"1 1 0 rg".len())
            .any(|w| w == b"1 1 0 rg"),
        "expected a yellow `1 1 0 rg` stencil fill operator"
    );
}

/// Shared-dictionary (DJVI Djbz) documents must still get a foreground mask in
/// the PDF: the pre-#559 path decoded the mask with the inline Djbz only, so
/// shared-dict pages silently lost their entire text overlay.
#[test]
fn test_fgbz_shared_dict_mask_present() {
    let doc = load_doc("navm_fgbz.djvu");
    let pdf = djvu_to_pdf(&doc).unwrap();
    assert!(
        pdf_contains(&pdf, b"/ImageMask true"),
        "shared-dict pages must embed foreground ImageMask stencils"
    );
}

/// Inflate every FlateDecode stream in the PDF and concatenate the results
/// (raw streams are appended as-is).
fn decompress_all_streams(pdf: &[u8]) -> Vec<u8> {
    // `>>\nstream\n` can't be confused with the `stream` inside `endstream`.
    const START: &[u8] = b">>\nstream\n";
    const END: &[u8] = b"\nendstream";
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(off) = pdf[pos..]
        .windows(START.len())
        .position(|w| w == START)
        .map(|o| pos + o + START.len())
    {
        let end = match pdf[off..].windows(END.len()).position(|w| w == END) {
            Some(e) => off + e,
            None => break,
        };
        match miniz_oxide::inflate::decompress_to_vec_zlib(&pdf[off..end]) {
            Ok(mut d) => out.append(&mut d),
            Err(_) => out.extend_from_slice(&pdf[off..end]),
        }
        pos = end + END.len();
    }
    out
}
