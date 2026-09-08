//! Shared core for the foreign-language bindings (#379).
//!
//! The C ABI (`crate::ffi`) and the wasm-bindgen surface (`crate::wasm`) are
//! two adapters over the same flow — open a document, query page size, render
//! a page to an RGBA buffer, extract text — and each used to re-translate the
//! typed [`DocError`]/[`RenderError`] into a string at every call site, from
//! two different parse entry points.
//!
//! This module is the single home for that flow:
//!
//!  * [`open`] — the one parse entry point both bindings use
//!    ([`DjVuDocument::parse`]); the C binding no longer routes through the
//!    legacy `Document` facade and its second error wrapping.
//!  * [`page_width`] / [`page_height`] / [`page_dpi`] / [`text`] /
//!    [`render_at_dpi`] — the per-page operations, each mapping the model
//!    error into the shared [`ForeignError`] taxonomy.
//!  * [`render_opts_for_dpi`] — the canonical [`RenderOptions`] for a target
//!    DPI (AA off so the RGBA length stays `w * h * 4`; permissive so a
//!    partially-corrupt page yields a best-effort image rather than failing).
//!
//! Each binding then adds only its target-specific cap: `CString` lifecycle
//! and a `catch_unwind` panic boundary for C, `JsError` conversion for wasm.
//! A new foreign function is one core function plus a thin cap.

use crate::djvu_document::{DjVuDocument, DjVuPage, DocError};
use crate::djvu_render::{self, RenderError, RenderOptions};
use crate::pixmap::Pixmap;

/// Error taxonomy shared by every foreign binding.
///
/// The variant determines the stable integer [`code`](ForeignError::code) the
/// C ABI exposes; the wasm binding uses the [`Display`] message. Centralizing
/// it means an "out of range" failure carries the same code regardless of
/// which entry point surfaced it — previously the C codes were assigned
/// per-call-site and disagreed (a bad page index was `2` from render/text but
/// `3` from the size queries).
#[derive(Debug)]
pub(crate) enum ForeignError {
    /// The document could not be parsed. Code `1`.
    Parse(String),
    /// A page's content could not be decoded or rendered. Code `2`.
    Decode(String),
    /// A page index was out of range (or a handle was null). Code `3`.
    OutOfRange(String),
}

impl ForeignError {
    /// The stable C ABI error code: `1` parse, `2` decode/render, `3` range.
    pub(crate) fn code(&self) -> i32 {
        match self {
            ForeignError::Parse(_) => 1,
            ForeignError::Decode(_) => 2,
            ForeignError::OutOfRange(_) => 3,
        }
    }

    /// Classify an error from page lookup: a genuine out-of-range index is
    /// [`OutOfRange`](ForeignError::OutOfRange); anything else that surfaces
    /// while resolving a page is a [`Decode`](ForeignError::Decode) failure.
    fn from_page_lookup(e: DocError) -> Self {
        match e {
            DocError::PageOutOfRange { .. } => ForeignError::OutOfRange(e.to_string()),
            other => ForeignError::Decode(other.to_string()),
        }
    }
}

impl core::fmt::Display for ForeignError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ForeignError::Parse(m) | ForeignError::Decode(m) | ForeignError::OutOfRange(m) => {
                f.write_str(m)
            }
        }
    }
}

/// Parse a DjVu document from bytes — the single entry point both bindings
/// share.
pub(crate) fn open(data: &[u8]) -> Result<DjVuDocument, ForeignError> {
    DjVuDocument::parse(data).map_err(|e| ForeignError::Parse(e.to_string()))
}

/// Resolve page `index`, mapping a lookup failure into the shared taxonomy.
pub(crate) fn page(doc: &DjVuDocument, index: usize) -> Result<&DjVuPage, ForeignError> {
    doc.page(index).map_err(ForeignError::from_page_lookup)
}

/// Width in pixels of page `index`.
pub(crate) fn page_width(doc: &DjVuDocument, index: usize) -> Result<u32, ForeignError> {
    Ok(page(doc, index)?.width() as u32)
}

/// Height in pixels of page `index`.
pub(crate) fn page_height(doc: &DjVuDocument, index: usize) -> Result<u32, ForeignError> {
    Ok(page(doc, index)?.height() as u32)
}

/// Native DPI of page `index`.
pub(crate) fn page_dpi(doc: &DjVuDocument, index: usize) -> Result<u32, ForeignError> {
    Ok(page(doc, index)?.dpi() as u32)
}

/// Plain text of page `index`, or `None` when the page has no text layer.
pub(crate) fn text(doc: &DjVuDocument, index: usize) -> Result<Option<String>, ForeignError> {
    page(doc, index)?
        .text()
        .map_err(|e| ForeignError::Decode(e.to_string()))
}

/// Canonical [`RenderOptions`] for rendering `page` at `target_dpi`.
///
/// Anti-aliasing is disabled so the output is always exactly
/// `width * height * 4` RGBA bytes, and rendering is permissive so a page with
/// a few corrupt chunks still yields a best-effort image instead of an error —
/// the right default for a viewer-facing binding.
pub(crate) fn render_opts_for_dpi(page: &DjVuPage, target_dpi: f32) -> RenderOptions {
    let (width, height) = crate::export_common::size_at_dpi(page, target_dpi);
    // Only the size is set; the pipeline derives the decode scale from `width`
    // (see `RenderOptions::decode_scale`). `permissive` is the viewer-facing
    // default; the rest come from `RenderOptions::default()`.
    RenderOptions {
        width,
        height,
        permissive: true,
        ..RenderOptions::default()
    }
}

/// Render page `index` at `target_dpi` to an RGBA [`Pixmap`].
pub(crate) fn render_at_dpi(
    doc: &DjVuDocument,
    index: usize,
    target_dpi: f32,
) -> Result<Pixmap, ForeignError> {
    let page = page(doc, index)?;
    let opts = render_opts_for_dpi(page, target_dpi);
    djvu_render::render_pixmap(page, &opts).map_err(map_render_err)
}

/// Map a [`RenderError`] into the shared taxonomy (always a decode failure).
pub(crate) fn map_render_err(e: RenderError) -> ForeignError {
    ForeignError::Decode(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chicken_bytes() -> Vec<u8> {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("references/djvujs/library/assets/chicken.djvu");
        std::fs::read(&path).expect("chicken.djvu must exist")
    }

    #[test]
    fn foreign_error_codes() {
        assert_eq!(ForeignError::Parse("p".into()).code(), 1);
        assert_eq!(ForeignError::Decode("d".into()).code(), 2);
        assert_eq!(ForeignError::OutOfRange("o".into()).code(), 3);
    }

    #[test]
    fn foreign_error_display() {
        assert_eq!(ForeignError::Parse("msg".into()).to_string(), "msg");
        assert_eq!(ForeignError::Decode("msg".into()).to_string(), "msg");
        assert_eq!(ForeignError::OutOfRange("msg".into()).to_string(), "msg");
    }

    #[test]
    fn open_parses_valid_doc() {
        let data = chicken_bytes();
        let doc = open(&data).expect("chicken.djvu must parse");
        assert_eq!(doc.page_count(), 1);
    }

    #[test]
    fn open_invalid_bytes_returns_parse_error() {
        let err = open(b"not a djvu file").unwrap_err();
        assert!(matches!(err, ForeignError::Parse(_)));
    }

    #[test]
    fn page_width_height_dpi_are_nonzero() {
        let data = chicken_bytes();
        let doc = open(&data).unwrap();
        assert!(page_width(&doc, 0).unwrap() > 0);
        assert!(page_height(&doc, 0).unwrap() > 0);
        assert!(page_dpi(&doc, 0).unwrap() > 0);
    }

    #[test]
    fn page_out_of_range_returns_out_of_range_error() {
        let data = chicken_bytes();
        let doc = open(&data).unwrap();
        let err = page(&doc, 999).unwrap_err();
        assert!(matches!(err, ForeignError::OutOfRange(_)), "{err:?}");
    }

    #[test]
    fn text_returns_option() {
        let data = chicken_bytes();
        let doc = open(&data).unwrap();
        // chicken.djvu may or may not have text; just check it doesn't error
        let _ = text(&doc, 0).expect("text() must not error on valid page");
    }

    #[test]
    fn map_render_err_returns_decode_error() {
        let render_err = RenderError::BufTooSmall { need: 100, got: 10 };
        let err = map_render_err(render_err);
        assert!(matches!(err, ForeignError::Decode(_)), "{err:?}");
    }

    #[test]
    fn render_at_dpi_returns_pixmap() {
        let data = chicken_bytes();
        let doc = open(&data).unwrap();
        let pm = render_at_dpi(&doc, 0, 72.0).expect("render must succeed");
        assert!(pm.width > 0 && pm.height > 0);
    }
}
