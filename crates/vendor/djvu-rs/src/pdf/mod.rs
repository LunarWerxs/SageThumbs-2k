//! DjVu to PDF converter — preserves document structure.
//!
//! Converts DjVu documents to PDF while preserving:
//! - IW44 background as compressed RGB image (#2)
//! - JB2 foreground mask as 1-bit image (#3)
//! - Text layer as invisible selectable text (#4)
//! - NAVM bookmarks as PDF outline / table of contents (#5)
//! - ANTz hyperlinks as PDF link annotations (#6)
//!
//! # Example
//!
//! ```no_run
//! use djvu_rs::djvu_document::DjVuDocument;
//! use djvu_rs::pdf::djvu_to_pdf;
//!
//! let data = std::fs::read("input.djvu").unwrap();
//! let doc = DjVuDocument::parse(&data).unwrap();
//! let pdf_bytes = djvu_to_pdf(&doc).unwrap();
//! std::fs::write("output.pdf", pdf_bytes).unwrap();
//! ```

#[cfg(not(feature = "std"))]
use alloc::{format, string::String, sync::Arc, vec, vec::Vec};
#[cfg(feature = "std")]
use std::sync::Arc;

use crate::{
    annotation::Shape,
    djvu_document::{DjVuBookmark, DjVuDocument, DjVuPage, DocError},
    djvu_render::{self, RenderOptions},
    export_common::{PageRun, export_pages},
    export_control::{ExportObserver, NoOpObserver},
    info::Rotation,
    render_size::RenderSize,
    text::Rect,
};

// One file per concern (#911). Every item a sibling needs is `pub(super)`:
// the same reach a private item had when this module was one file.

mod mask;
mod outline;
mod page;
mod text;
mod writer;

use mask::*;
use outline::*;
use page::*;
use text::*;
use writer::*;

/// Errors from PDF conversion.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PdfError {
    /// Document model error.
    #[error("document error: {0}")]
    Doc(#[from] DocError),
    /// Render error.
    #[error("render error: {0}")]
    Render(#[from] djvu_render::RenderError),
    /// I/O error writing to the output sink (`djvu_to_pdf_to_writer`).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// Export was cancelled by its observer.
    #[error("export cancelled")]
    Cancelled,
}

/// Convert a DjVu document to PDF bytes.
///
/// Options for DjVu → PDF conversion.
///
/// Use `PdfOptions::default()` for sensible defaults:
/// - 150 DPI output (screen-quality, ~16× fewer pixels than native 600 DPI)
/// - DCTDecode (JPEG quality 80) for color backgrounds
/// - 1-bit FlateDecode for bilevel masks
/// - Bilevel-only pages skip RGB render entirely (direct 1-bit embed)
#[derive(Debug, Clone)]
pub struct PdfOptions {
    /// JPEG quality for background image encoding (1–100).
    ///
    /// Higher values produce better quality at larger file sizes.
    /// Set to `None` to use lossless FlateDecode (PNG-like, larger output).
    pub jpeg_quality: Option<u8>,

    /// Output resolution in DPI.
    ///
    /// Controls the pixel dimensions of embedded images. Lower values produce
    /// smaller files and faster exports; higher values preserve more detail.
    ///
    /// - `150` — screen quality (default); ~16× fewer pixels than native 600 DPI
    /// - `300` — print quality
    /// - `0` — use native page DPI (maximum quality, slowest)
    pub output_dpi: u32,

    /// Opt-in per-page adaptive raster encoding (default `false`).
    ///
    /// When `jpeg_quality` is `Some`, the default behaviour always emits
    /// DCTDecode (JPEG). On near-flat/text-dominated colour pages this can be
    /// *larger* than plain FlateDecode at no quality gain — JPEG's DCT
    /// overhead doesn't pay for itself when there's little photographic
    /// detail to amortize it against (see `PDF_DCT_PROBE` in
    /// `PERF_EXPERIMENTS.md`).
    ///
    /// When `true`, each page's rendered RGB is encoded *both* ways and
    /// whichever stream is smaller is embedded — losslessly (FlateDecode) when
    /// Deflate wins, lossy (DCTDecode) when JPEG wins. Only one page's pair of
    /// encodings is ever held in memory at a time (the loser is dropped
    /// immediately), so this doesn't change the O(1)-per-page memory profile.
    /// Has no effect when `jpeg_quality` is `None` (already all-Deflate).
    pub adaptive_raster: bool,

    /// Opt-in CCITT Group 4 (T.6) encoding for JB2 bilevel masks (default `false`).
    ///
    /// Every page's mask (the bilevel-only `/Im0` fast path *and* the `/Mask0`
    /// overlay on mixed pages) is currently always emitted as Deflate of the
    /// raw 1-bit raster. For scanned/text-dominated bilevel content, Group 4
    /// (fax) run-length coding exploits row-to-row redundancy that Deflate's
    /// generic LZ77 window doesn't reliably catch, often 1.5-2x+ smaller.
    ///
    /// When `true`, each mask is encoded *both* ways (Deflate and G4 via
    /// [`crate::smmr::encode_g4`]) and whichever stream is smaller is
    /// embedded — this can never regress a page's mask size, the same
    /// "encode both, keep smaller" pattern as `adaptive_raster`. On
    /// halftone/dithered bilevel content (rare for JB2 masks, which are
    /// normally already-segmented text/line-art) G4's run-length model can
    /// lose to Deflate; the per-mask min guards against that.
    pub ccitt_g4: bool,

    /// Opt-in true-MRC layering (default `false`).
    ///
    /// The default mixed-page path embeds `/Im0` as the **composited** render
    /// (background WITH text) at `output_dpi`, then repaints the text via the
    /// stencils anyway — the raster layer wastes bits on high-frequency glyph
    /// edges (JPEG ringing halos) and is stored upsampled relative to the
    /// background's native BG44 resolution. With `mrc: true`, pages whose
    /// foreground is fully covered by stencils embed the **background layer
    /// only** (no composited text) at its native subsampled resolution; the
    /// stencils carry the text (coloured per the FGbz/uniform-FG44 policy).
    /// Pages where the stencil is skipped (multi-colour FG44), photo-only
    /// pages, and bilevel pages fall back to the default path unchanged.
    pub mrc: bool,
}

impl Default for PdfOptions {
    fn default() -> Self {
        PdfOptions {
            jpeg_quality: Some(80),
            output_dpi: 150,
            adaptive_raster: false,
            ccitt_g4: false,
            mrc: false,
        }
    }
}

impl PdfOptions {
    /// High-quality archival preset: native DPI, JPEG quality 90.
    pub fn archival() -> Self {
        PdfOptions {
            jpeg_quality: Some(90),
            output_dpi: 0,
            adaptive_raster: false,
            ccitt_g4: false,
            mrc: false,
        }
    }
}

/// Convert a DjVu document to PDF bytes using custom options.
///
/// See [`PdfOptions`] for available settings.
pub fn djvu_to_pdf_with_options(
    doc: &DjVuDocument,
    opts: &PdfOptions,
) -> Result<Vec<u8>, PdfError> {
    let mut buf = Vec::new();
    djvu_to_pdf_to_writer(doc, opts, &mut buf)?;
    Ok(buf)
}

/// Convert a DjVu document to PDF, streaming the output to `sink` (#606).
///
/// Object bodies are written as they are produced and dropped immediately, so
/// peak memory stays O(1 page) plus the xref bookkeeping instead of holding
/// every object body *and* a second full serialization buffer. Output bytes
/// are identical to [`djvu_to_pdf_with_options`] (which now wraps this with a
/// `Vec` sink). Wrap `sink` in a [`std::io::BufWriter`] for file output.
///
/// # Errors
///
/// Returns `PdfError` if page rendering, text layer parsing, or writing to
/// `sink` fails. On error the sink may contain a partial PDF; the library does
/// not clean it up or provide atomic replacement (that policy belongs to the
/// CLI/application layer).
pub fn djvu_to_pdf_to_writer<W: std::io::Write>(
    doc: &DjVuDocument,
    opts: &PdfOptions,
    sink: W,
) -> Result<(), PdfError> {
    let mut observer = NoOpObserver;
    djvu_to_pdf_to_writer_with_observer(doc, opts, sink, &mut observer)
}

/// Convert a DjVu document to PDF while reporting progress through `observer`.
///
/// With the `parallel` feature, cancellation is polled before each bounded
/// render batch. Work already scheduled in the current batch may complete
/// before the cancellation is observed.
///
/// On error, `sink` may contain a partial PDF; the library does not clean it
/// up or provide atomic replacement (that policy belongs to the CLI/application
/// layer).
pub fn djvu_to_pdf_to_writer_with_observer<W: std::io::Write>(
    doc: &DjVuDocument,
    opts: &PdfOptions,
    sink: W,
    observer: &mut dyn ExportObserver,
) -> Result<(), PdfError> {
    djvu_to_pdf_impl(doc, opts, sink, observer)
}

/// This produces a PDF 1.4 file with:
/// - Rasterized page images (IW44 background + JB2 mask composite)
/// - Invisible text layer for search and selection
/// - Bookmarks (PDF outline) from NAVM
/// - Hyperlink annotations from ANTz
///
/// Background images are encoded as DCTDecode (JPEG at quality 80) by default,
/// producing significantly smaller files than the legacy FlateDecode path.
/// Use [`djvu_to_pdf_with_options`] with `jpeg_quality: None` for lossless output.
///
/// # Errors
///
/// Returns `PdfError` if page rendering or text layer parsing fails.
pub fn djvu_to_pdf(doc: &DjVuDocument) -> Result<Vec<u8>, PdfError> {
    djvu_to_pdf_with_options(doc, &PdfOptions::default())
}

fn djvu_to_pdf_impl<W: std::io::Write>(
    doc: &DjVuDocument,
    opts: &PdfOptions,
    sink: W,
    observer: &mut dyn ExportObserver,
) -> Result<(), PdfError> {
    let mut w = PdfWriter::new(sink)?;

    // Reserve IDs for catalog and pages
    let catalog_id = w.alloc_id(); // 1
    let pages_id = w.alloc_id(); // 2

    // Reserve a font object ID
    let font_id = w.alloc_id(); // 3
    w.add_obj(font_id, font_dict())?;

    let page_count = doc.page_count();

    // #629: each page renders on a cold clone so its decode caches die with
    // it — the export never revisits a page, and caching on the document made
    // peak RSS grow O(pages). PdfWriter is not Send, so only rendering runs in
    // parallel; objects are emitted in page order.
    let pages: Vec<usize> = (0..page_count).collect();
    let mut page_obj_ids = Vec::with_capacity(page_count);
    let run = export_pages(
        &pages,
        observer,
        |i| render_page_data(&doc.page(i)?.clone(), opts),
        |_, rendered| {
            page_obj_ids.push(emit_page_objects(&mut w, rendered, pages_id, font_id)?);
            Ok(())
        },
    )?;
    if run == PageRun::Cancelled {
        return Err(PdfError::Cancelled);
    }

    // Build outline from bookmarks
    let outline_id = build_outline(&mut w, doc.bookmarks(), &page_obj_ids)?;

    // Pages object
    let kids = page_obj_ids
        .iter()
        .map(|id| format!("{id} 0 R"))
        .collect::<Vec<_>>()
        .join(" ");
    let n = page_obj_ids.len();
    w.add_obj(
        pages_id,
        format!("<< /Type /Pages /Kids [{kids}] /Count {n} >>").into_bytes(),
    )?;

    // Catalog
    let outline_ref = match outline_id {
        Some(oid) => format!(" /Outlines {oid} 0 R /PageMode /UseOutlines"),
        None => String::new(),
    };
    w.add_obj(
        catalog_id,
        format!("<< /Type /Catalog /Pages {pages_id} 0 R{outline_ref} >>").into_bytes(),
    )?;

    w.finish()
}

#[cfg(test)]
mod tests;
