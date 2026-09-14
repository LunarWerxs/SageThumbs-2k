//! Pure-Rust DjVu decoder written from the DjVu v3 public specification.
//!
//! This crate implements the full DjVu v3 document format in safe Rust,
//! including IFF container parsing, JB2 bilevel decoding, IW44 wavelet
//! decoding, BZZ decompression, text layer extraction, and annotation parsing.
//! All algorithms are written from the public DjVu spec with no GPL code.
//!
//! # Key public types
//!
//! - [`DjVuError`] — top-level error enum (wraps [`IffError`], etc.)
//! - [`IffError`] — errors from the IFF container parser
//! - [`PageInfo`] — page metadata parsed from the INFO chunk
//! - [`Rotation`] — page rotation enum (None, Ccw90, Rot180, Cw90)
//! - [`DjVuDocument`] — high-level document model (IFF/BZZ/IW44 based)
//! - [`DjVuPage`] — lazy page handle
//! - [`DjVuBookmark`] — NAVM bookmark (table of contents)
//! - [`DocError`] — error type for the document model
//! - [`djvu_render::RenderOptions`] — render parameters
//! - [`djvu_render::RenderError`] — render pipeline error type
//! - [`text::TextLayer`] — text layer from TXTz/TXTa chunks
//! - [`text::TextZone`] — a zone node in the text layer hierarchy
//! - [`annotation::Annotation`] — page-level annotation
//! - [`annotation::MapArea`] — clickable area with URL and shape
//! - [`Pixmap`] — RGBA pixel buffer returned by render methods
//! - [`Bitmap`] — 1-bit bitmap for JB2 mask layers
//! - [`Document`] — owned DjVu document (high-level std API, requires std feature)
//! - [`Page`] — a page within a [`Document`]
//!
//! # Quick start
//!
//! ```no_run
//! use djvu_rs::Document;
//!
//! let doc = Document::open("file.djvu").unwrap();
//! println!("{} pages", doc.page_count());
//!
//! let page = doc.page(0).unwrap();
//! println!("{}x{} @ {} dpi", page.width(), page.height(), page.dpi());
//!
//! let pixmap = page.render().unwrap();
//! // pixmap.data: RGBA bytes
//! ```
//!
//! # IFF parser
//!
//! ```no_run
//! use djvu_rs::iff::parse_form;
//!
//! let data = std::fs::read("file.djvu").unwrap();
//! let form = parse_form(&data).unwrap();
//! println!("form type: {:?}", std::str::from_utf8(&form.form_type));
//! ```

#![cfg_attr(not(feature = "std"), no_std)]
#![deny(unsafe_code)]
#[cfg(not(feature = "std"))]
extern crate alloc;

// Compile every ```rust block in README.md as a doctest, so the README cannot
// drift from the public API (the `from_mmap` class of staleness). Gated on the
// feature union the README examples need; exercised by
// `cargo test --doc --features cli,tiff,async,serde,image,epub`
// in scripts/check.sh and CI (nextest does not run doctests).
#[cfg(all(
    doctest,
    feature = "pdf",
    feature = "tiff",
    feature = "async",
    feature = "serde",
    feature = "image",
    feature = "epub"
))]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;

// ---- Phase-1 modules -------------------------------------------------------
//
// Clean-room implementations written from the DjVu spec.

/// IFF container parser (phase 1, written from spec).
pub mod iff;

/// Typed error hierarchy for the new implementation (phase 1).
///
/// Key types: `DjVuError`, `IffError`, `BzzError`, `Jb2Error`, `Iw44Error`,
/// `LegacyError`. See also `text::TextError` and `annotation::AnnotationError`.
pub mod error;

/// INFO chunk parser (phase 1).
pub(crate) mod info;

/// ZP arithmetic coder — clean-room implementation.
///
/// Lives in the standalone [`djvu_zp`] sub-crate (workspace member) since
/// PR1 of #229.  Re-exported here as `zp_impl` for backwards compatibility
/// with the historical internal path used by BZZ, JB2, and IW44.
#[allow(unused_imports)]
pub(crate) use djvu_zp as zp_impl;

/// BZZ decompressor — clean-room implementation.
///
/// Provides `bzz::bzz_decode` for decompressing DjVu BZZ streams
/// (DIRM, NAVM, ANTz chunks).
#[allow(dead_code)]
pub mod bzz;

/// Compatibility alias for the former `bzz_new` module name. The `_new` suffix
/// fossilised the codec-extraction migration (there was a pre-extraction
/// `bzz.rs`); the module is now simply [`bzz`]. Prefer `bzz` in new code.
pub use bzz as bzz_new;

/// BZZ compressor — encoding counterpart to `bzz`.
#[cfg(feature = "std")]
pub mod bzz_encode;

/// DJVM document merge and split operations.
#[cfg(feature = "std")]
pub mod djvm;

/// `DIRM` directory-chunk model — the single owner of the DIRM byte layout,
/// shared by the read model, the byte-preserving mutator, and DJVM merge/split.
pub(crate) mod dirm;

/// JB2 bilevel image decoder — clean-room implementation.
///
/// Decodes JB2-encoded bitonal images from DjVu Sjbz and Djbz chunks using
/// ZP adaptive arithmetic coding with a symbol dictionary.
///
/// Key public types: `jb2::Jb2Dict`, `jb2::decode`, `jb2::decode_dict`.
pub mod jb2;

/// IW44 wavelet image decoder — clean-room implementation (phase 2c).
///
/// Provides `iw44::Iw44Image` for decoding BG44/FG44/TH44 chunks.
/// Uses planar YCbCr storage and a ZP arithmetic coder.
/// RGB conversion happens only in `iw44::Iw44Image::to_rgb`.
pub mod iw44;

/// Compatibility alias for the former `iw44_new` module name. The `_new` suffix
/// fossilised the codec-extraction migration; the module is now simply
/// [`iw44`]. Prefer `iw44` in new code.
pub use iw44 as iw44_new;

/// IW44 wavelet encoder — produces BG44/FG44/TH44 chunk payloads.
///
/// Provides [`iw44_encode::encode_iw44_color`] and [`iw44_encode::encode_iw44_gray`].
#[cfg(feature = "std")]
pub mod iw44_encode;

/// JB2 bilevel image encoder — produces Sjbz chunk payloads.
///
/// Provides [`jb2_encode::encode_jb2`] (single record-type-3 direct encoding) and
/// [`jb2_encode::encode_jb2_dict`] (connected-component symbol-dictionary encoding).
#[cfg(feature = "std")]
pub mod jb2_encode;

/// FGbz foreground-palette encoder — produces FGbz chunk payloads.
///
/// Provides [`fgbz_encode::encode_fgbz`] (palette + optional per-blit
/// index table) and [`fgbz_encode::decode_fgbz`] (inverse), plus the
/// [`fgbz_encode::FgbzColor`] palette-entry type.
#[cfg(feature = "std")]
pub mod fgbz_encode;

/// High-level page encoder — composes the codec primitives into a
/// complete `FORM:DJVU` page.
///
/// Provides [`djvu_encode::PageEncoder`] (builder-style entry point),
/// [`djvu_encode::EncodeQuality`] (Lossless / Quality / Archival / Photo
/// profiles), [`djvu_encode::BilevelCodec`] (JB2 or explicit Smmr), and
/// [`djvu_encode::EncodeError`].
#[cfg(feature = "std")]
pub mod djvu_encode;

/// TH44 thumbnail generation for multi-page bundles.
///
/// Provides [`thumbnail::encode_th44_color`] and
/// [`thumbnail::encode_th44_gray_from_bitmap`] for embedding small IW44
/// thumbnails as `TH44` chunks inside a page's `FORM:DJVU` component.
#[cfg(feature = "std")]
pub mod thumbnail;

/// Photometric foreground/background segmentation — splits an RGBA
/// page into a bilevel ink mask and a sub-sampled background pixmap.
///
/// Provides [`segment::segment_page`], [`segment::SegmentOptions`],
/// [`segment::Binarization`], and [`segment::SegmentedPage`]. The default is
/// fixed-threshold + block-averaged BG; optional Sauvola binarisation and
/// fully-masked BG-block inpainting are available through `SegmentOptions`.
#[cfg(feature = "std")]
pub mod segment;

/// Encoder ingest policy — alpha, bit-depth down-conversion, and related knobs
/// (#694). See `docs/encoder-ingestion.md` for the supported input matrix.
#[cfg(feature = "std")]
pub mod ingest;

/// PNG file → [`Pixmap`] decoder.
///
/// Provides [`png_io::decode_png_to_pixmap`] for decoding PNG inputs into the
/// RGBA [`Pixmap`] format used throughout djvu-rs.
#[cfg(feature = "std")]
pub mod png_io;

/// Shared configurable resource limits for parse, render, and validate.
pub mod resource_limits;

/// New document model — phase 3.
///
/// Provides [`DjVuDocument`] (high-level document API built on the new IFF/BZZ/IW44
/// clean-room implementations), [`DjVuPage`] (lazy page handle), and
/// [`DjVuBookmark`] (NAVM table-of-contents entry).
pub mod djvu_document;

/// In-place document mutation — byte-preserving rewrite primitive (PR1 of #222).
///
/// Provides [`djvu_mut::DjVuDocumentMut`] for editing a DjVu document while
/// preserving the bytes of every chunk that wasn't touched.
#[cfg(feature = "std")]
pub mod djvu_mut;

/// High-level document optimization planning and safe structural cleanup.
#[cfg(feature = "std")]
pub mod optimizer;

/// Versioned, typed document editing operations with validation and atomic
/// output handoff.
#[cfg(feature = "std")]
pub mod editor;

/// Validated dependency graph for bundled `FORM:DJVM` components.
#[cfg(feature = "std")]
pub mod component_graph;

/// Layered, non-rendering DjVu document validation.
///
/// Structural, dependency, and codec checks are available now. Semantic and
/// resource layers are reserved for later validator slices so callers can rely
/// on a stable finding schema from the first release.
#[cfg(feature = "std")]
pub mod validate;

/// Semantic comparison of two documents (#696): page properties, text,
/// annotations, metadata, bookmarks, and the component graph.
#[cfg(feature = "std")]
pub mod semantic_diff;

/// Rendering pipeline for [`DjVuPage`] — phase 5.
///
/// Provides `djvu_render::RenderOptions`, `djvu_render::RenderRect`,
/// `djvu_render::render_into`, `djvu_render::render_pixmap`,
/// `djvu_render::render_region`, `djvu_render::render_coarse`, and
/// `djvu_render::render_progressive`.
pub mod djvu_render;

/// Tile-first rendering API for viewer engines (#691).
///
/// Provides `djvu_tile::TileLayout`, `djvu_tile::TileRect`,
/// `djvu_tile::render_tile`, and `djvu_tile::render_tile_cached` — a
/// display-space tile grid over the region renderer, with byte-identical
/// assembly and order-independent tile pixels — plus tile-granular cache
/// control (`djvu_tile::tile_cache_usage`, `djvu_tile::set_tile_cache_budget`,
/// `djvu_tile::clear_tile_cache`, `djvu_tile::invalidate_tile_region`),
/// progressive quality steps and cooperative cancellation
/// (`djvu_tile::render_tile_with`, `djvu_tile::TileRenderControls`,
/// `djvu_tile::TileCancelToken`) and, with the `parallel` feature, bounded
/// background `djvu_tile::prefetch_tiles` /
/// `djvu_tile::prefetch_tiles_cancellable`. Contract:
/// `docs/tile-rendering.md`.
pub mod djvu_tile;

/// Perceptual image-quality metrics (PSNR, SSIM) for render experiments.
///
/// Judges whether a render change is perceptually better/worse against a
/// reference, which the arithmetic pixel-diff tooling cannot. See
/// [`quality::compare`]. Std-only (render-side).
#[cfg(feature = "std")]
pub mod quality;

/// FGbz foreground-palette parser — decodes the `FGbz` chunk into a color
/// palette and per-blit index table so [`djvu_render`] receives already-decoded
/// data and never calls `bzz_decode` directly.
pub(crate) mod fgbz;

/// DjVu text layer — data model and parser.
///
/// Defines [`text::TextLayer`], [`text::TextZone`], [`text::TextZoneKind`],
/// [`text::Rect`], [`text::Paragraph`], and [`text::TextError`], and provides
/// the pure [`text::parse_text_layer`] parser for TXTa/TXTz chunks.
/// BZZ-compressed `TXTz` payloads are decompressed upstream by
/// [`DjVuPage::chunk_payload`].
pub mod text;

/// Annotation parser for DjVu ANTz/ANTa chunks — phase 4.
///
/// Provides the pure [`annotation::parse_annotations`] parser plus typed
/// structs [`annotation::Annotation`], [`annotation::MapArea`],
/// [`annotation::Shape`], and [`annotation::Color`].  BZZ-compressed `ANTz`
/// payloads are decompressed upstream by [`DjVuPage::chunk_payload`].
pub mod annotation;

/// Document metadata parser for METa/METz chunks — phase 4 extension.
///
/// Provides the pure [`metadata::parse_metadata`] parser plus
/// [`metadata::DjVuMetadata`] and [`metadata::MetadataError`].  BZZ-compressed
/// `METz` payloads are decompressed upstream by [`DjVuDocument::chunk_payload`].
pub mod metadata;

/// Shared, depth-guarded S-expression reader used by the annotation and
/// metadata parsers (#368). Owns the tokenizer and recursive-descent reader so
/// the recursion-depth guard protects both consumers.
mod sexp;

/// Lenient (UTF-8 with CP1252 fallback) decoding for non-structural DjVu
/// strings (#524): NAVM bookmark titles/URLs, TXTz/TXTa text layers, METa
/// values. One bad legacy byte in a bookmark must not abort `Document::open`.
mod lenient_text;

/// Shared document-to-export traversal primitives (#345) used by the PDF,
/// EPUB, TIFF, and OCR exporters: the per-page loop, the scale→size kernel,
/// the leaf word/character zone-walk, and the vertical coordinate flip.
#[cfg(feature = "std")]
mod export_common;

/// Shared progress reporting and cooperative cancellation for export writers.
#[cfg(feature = "std")]
pub mod export_control;

/// Test-only sinks for exercising streaming exporter failure contracts.
#[cfg(all(test, feature = "std"))]
pub(crate) mod export_test_support;

/// DjVu to PDF converter — phase 6.
///
/// Converts DjVu documents to PDF preserving structure: rasterized page images,
/// invisible text layer (searchable), bookmarks (PDF outline), and hyperlinks
/// (PDF link annotations).
///
/// Key function: [`pdf::djvu_to_pdf`].
#[cfg(feature = "pdf")]
pub mod pdf;

/// DjVu to EPUB 3 exporter.
///
/// Converts DjVu documents to EPUB 3 while preserving page images,
/// invisible text overlay for search/copy, and NAVM bookmarks as navigation.
///
/// Key function: [`epub::djvu_to_epub`].
#[cfg(feature = "epub")]
pub mod epub;

/// DjVu to CBZ (comic book archive) exporter.
///
/// A ZIP of per-page PNGs; page rendering/encoding parallelises under the
/// `parallel` feature (render-parallel, write-serial — #598).
///
/// Key function: [`cbz::djvu_to_cbz`].
#[cfg(feature = "cbz")]
pub mod cbz;

/// DjVu to TIFF exporter — phase 4 format extension.
///
/// Converts DjVu documents to multi-page TIFF files in color (RGB8) or
/// bilevel (Gray8) modes.
///
/// Key function: [`tiff_export::djvu_to_tiff`].
#[cfg(feature = "tiff")]
pub mod tiff_export;

/// Async render surface for [`DjVuPage`] — phase 5 extension.
///
/// CPU-bound IW44/JB2 work belongs on the blocking thread pool: call the
/// synchronous render entry points inside [`tokio::task::spawn_blocking`]
/// rather than on the async runtime thread (see the module docs for the
/// one-line pattern).
///
/// Key abstractions: [`djvu_async::LazyDocument`],
/// [`djvu_async::render_progressive_stream`],
/// [`djvu_async::render_tile_async`],
/// [`djvu_async::render_tile_progressive_stream`],
/// [`djvu_async::load_document_async_streaming`].
#[cfg(feature = "async")]
pub mod djvu_async;

/// Async adapters for the synchronous PDF and DJVM streaming writers.
#[cfg(feature = "async")]
pub mod export_async;

/// `image::ImageDecoder` integration — allows DjVu pages to be used as
/// first-class image sources in the `image` crate ecosystem.
///
/// Key types: [`image_compat::DjVuDecoder`], [`image_compat::ImageCompatError`].
#[cfg(feature = "image")]
pub mod image_compat;

/// hOCR and ALTO XML serialization for the text layer.
///
/// This serializes an existing text layer; it does **not** run OCR (see the
/// [`ocr`] cluster for recognition).
///
/// Key functions: [`text_serialize::to_hocr`], [`text_serialize::to_alto`].
/// Key types: [`text_serialize::HocrOptions`], [`text_serialize::AltoOptions`],
/// [`text_serialize::TextSerializeError`].
#[cfg(feature = "std")]
pub mod text_serialize;

/// Pluggable OCR backend trait and error types.
///
/// Provides [`ocr::OcrBackend`] — recognize text in rendered page images.
/// Backend implementations are gated behind feature flags.
#[cfg(feature = "std")]
pub mod ocr;

/// Tesseract OCR backend (requires `ocr-tesseract` feature).
#[cfg(feature = "ocr-tesseract")]
pub mod ocr_tesseract;

/// Experimental ONNX OCR helper via tract (requires `ocr-onnx` feature).
#[cfg(feature = "ocr-onnx")]
pub mod ocr_onnx;

/// Experimental neural OCR placeholder (requires `ocr-neural` feature).
#[cfg(feature = "ocr-neural")]
pub mod ocr_neural;

/// TXTa/TXTz text layer encoder — writes [`text::TextLayer`] back to DjVu binary format.
#[cfg(feature = "std")]
pub mod text_encode;

/// NAVM bookmark encoder — serializes [`djvu_document::DjVuBookmark`] trees to BZZ-compressed binary.
#[cfg(feature = "std")]
pub mod navm_encode;

/// Smmr chunk codec — ITU-T G4 (MMR) bilevel image compression.
///
/// Provides [`decode_smmr`](crate::smmr::decode_smmr) (chunk → [`Bitmap`]) and
/// [`encode_smmr`](crate::smmr::encode_smmr) ([`Bitmap`] → chunk). Useful as an alternative
/// to JB2 for fax-style scans without recurring glyph structure.
pub mod smmr;

/// Chunk-encoder seam — one `(id, payload)` interface ([`chunk_encode::ChunkEncoder`])
/// over the per-chunk encoders, with a single [`chunk_encode::EncodeError`]
/// discipline (no panic, no silent truncation).
#[cfg(feature = "std")]
pub mod chunk_encode;

#[cfg(feature = "wasm")]
pub mod wasm;

/// Shared core for the foreign-language bindings (`ffi` and `wasm`).
///
/// Owns the open→size→render→text flow and the `DjVuError`→`(code, message)`
/// taxonomy; `ffi` and `wasm` are thin target-specific caps over it.
#[cfg(feature = "std")]
mod foreign;

/// C FFI bindings for foreign language integration.
///
/// Provides `extern "C"` functions with no-panic guarantees.
/// Key functions: `djvu_doc_open`, `djvu_doc_free`, `djvu_page_render`,
/// `djvu_pixmap_free`, `djvu_page_text`.
#[cfg(feature = "std")]
#[allow(unsafe_code)]
pub mod ffi;

// Re-export new phase-1 error types
pub use error::{BzzError, DjVuError, IffError, Iw44Error, Jb2Error};

// Re-export new phase-3 document model
pub use djvu_document::{
    ComponentDirectoryEntry, ComponentId, ComponentKind, ComponentResolveError, ComponentResolver,
    DjVuBookmark, DjVuDocument, DjVuPage, DocError,
};

// Re-export the validated editor entry points for callers that do not need to
// distinguish the module from the rest of the high-level API.
#[cfg(feature = "std")]
pub use editor::{
    DocumentEditor, EDIT_SCHEMA_VERSION, EditError, EditOperation, EditOperationKind, EditPlan,
    EditRequest, EditTarget, PlannedEdit,
};

// Re-export the bundled component-graph API for callers that do not need to
// distinguish the module from the rest of the high-level document API.
#[cfg(feature = "std")]
pub use component_graph::{ComponentGraph, ComponentNode, ComponentNodeKind, GraphError};

/// Re-export the semantic diff API for callers that prefer the crate root.
#[cfg(feature = "std")]
pub use semantic_diff::{PlaneDiff, PlaneStatus, SemanticDiff, semantic_diff};

pub use resource_limits::{
    DEFAULT_MAX_RENDER_PIXELS, ParseOptions, ResourceLimitAxis, ResourceLimitExceeded,
    ResourceLimits,
};
/// Re-export the layered validation API for callers that prefer the crate root.
#[cfg(feature = "std")]
pub use validate::{
    Finding, Layer as ValidationLayer, ResourceEstimate, Severity, ValidateOptions,
    ValidationReport, ValidationSummary,
};

/// Shared export progress and cancellation types.
#[cfg(feature = "std")]
pub use export_control::{ExportObserver, NoOpObserver};

// Re-export new phase-1 page info types
pub use info::{PageInfo, Rotation};

// ---- Rendering / document modules ------------------------------------------
//
// These modules implement the rendering pipeline. They depend on bitmap,
// pixmap, iw44, jb2, bzz. They require std (std::io, std::path, Vec, etc.)
// so they are gated behind #[cfg(feature = "std")].

#[doc(hidden)]
pub(crate) mod bitmap;

#[doc(hidden)]
pub(crate) mod pixmap;

pub use bitmap::Bitmap;
pub use pixmap::{GrayPixmap, Pixmap};

// Re-export text types from the new pipeline
#[cfg(feature = "std")]
pub use text::{TextLayer, TextZone, TextZoneKind};

// Bookmark type alias — same shape as DjVuBookmark
#[cfg(feature = "std")]
pub type Bookmark = DjVuBookmark;

// Legacy error type (re-exported from legacy_error module included via error.rs)
#[cfg(feature = "std")]
pub use error::LegacyError as Error;

/// A parsed DjVu document. Owns the parsed structure.
///
/// Parsing happens once at construction time. All subsequent `page()` and
/// `render()` calls reuse the parsed chunk tree with zero re-parsing overhead.
#[cfg(feature = "std")]
pub struct Document {
    doc: DjVuDocument,
}

#[cfg(feature = "std")]
impl Document {
    /// Open a DjVu file from disk.
    ///
    /// For **indirect** multi-page DJVM files (where component pages live in
    /// separate files next to the index), use [`Document::open_dir`] instead.
    /// This method uses `DjVuDocument::parse` which only handles bundled
    /// (self-contained) files; it will return an error for indirect documents.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self, Error> {
        Self::open_with_options(path, &crate::resource_limits::ParseOptions::default())
    }

    /// Open a DjVu file with configurable resource limits.
    pub fn open_with_options(
        path: impl AsRef<std::path::Path>,
        opts: &crate::resource_limits::ParseOptions,
    ) -> Result<Self, Error> {
        let data = std::fs::read(path.as_ref())
            .map_err(|e| Error::FormatError(format!("failed to read file: {}", e)))?;
        Self::from_bytes_with_options(data, opts)
    }

    /// Open an indirect DJVM document from disk, resolving component pages
    /// from the same directory as the index file.
    ///
    /// Use this when the DjVu file is an *indirect* multi-page document where
    /// individual page files (e.g. `page001.djvu`) live alongside the index.
    /// For self-contained (bundled) files, [`Document::open`] is sufficient.
    pub fn open_dir(path: impl AsRef<std::path::Path>) -> Result<Self, Error> {
        Self::open_dir_with_options(path, &crate::resource_limits::ParseOptions::default())
    }

    /// Open an indirect DJVM document with configurable resource limits.
    pub fn open_dir_with_options(
        path: impl AsRef<std::path::Path>,
        opts: &crate::resource_limits::ParseOptions,
    ) -> Result<Self, Error> {
        let path = path.as_ref();
        let data = std::fs::read(path)
            .map_err(|e| Error::FormatError(format!("failed to read file: {}", e)))?;
        let base_dir = path.parent().unwrap_or(std::path::Path::new("."));
        let doc = DjVuDocument::parse_from_dir_with_options(&data, base_dir, opts)
            .map_err(|e| Error::FormatError(e.to_string()))?;
        Ok(Document { doc })
    }

    /// Parse a DjVu document from a reader (reads all bytes into memory).
    pub fn from_reader(reader: impl std::io::Read) -> Result<Self, Error> {
        let mut reader = reader;
        let mut data = Vec::new();
        reader
            .read_to_end(&mut data)
            .map_err(|e| Error::FormatError(format!("failed to read: {}", e)))?;
        Self::from_bytes(data)
    }

    /// Parse a DjVu document from owned bytes.
    ///
    /// The bytes are moved into a shared backing so bundled multi-page documents
    /// can construct pages lazily (chunk bytes are materialised on first access
    /// rather than copied at open time; see `DjVuDocument::parse_backed`).
    pub fn from_bytes(data: Vec<u8>) -> Result<Self, Error> {
        Self::from_bytes_with_options(data, &crate::resource_limits::ParseOptions::default())
    }

    /// Parse a DjVu document from owned bytes with configurable resource limits.
    pub fn from_bytes_with_options(
        data: Vec<u8>,
        opts: &crate::resource_limits::ParseOptions,
    ) -> Result<Self, Error> {
        let backing: std::sync::Arc<dyn AsRef<[u8]> + Send + Sync> = std::sync::Arc::new(data);
        let doc = DjVuDocument::parse_backed_with_options(backing, opts)
            .map_err(|e| Error::FormatError(e.to_string()))?;
        Ok(Document { doc })
    }

    /// Configurable resource limits supplied at parse/open time, if any.
    pub fn resource_limits(&self) -> Option<crate::resource_limits::ResourceLimits> {
        self.doc.resource_limits()
    }

    /// Parse the NAVM bookmarks (table of contents).
    pub fn bookmarks(&self) -> Result<Vec<Bookmark>, Error> {
        Ok(self.doc.bookmarks().to_vec())
    }

    /// Number of pages.
    pub fn page_count(&self) -> usize {
        self.doc.page_count()
    }

    /// Access a page by 0-based index.
    pub fn page(&self, index: usize) -> Result<Page<'_>, Error> {
        let page = self
            .doc
            .page(index)
            .map_err(|e| Error::FormatError(e.to_string()))?;
        Ok(Page { page, index })
    }

    /// Build a thumbnail for every page, each scaled to fit within a
    /// `max_w × max_h` box (aspect-preserving). Uses [`ThumbnailStrategy::Auto`]:
    /// the page's embedded `TH44` preview when present (20–30× faster than a
    /// real render, per the `D5_TH44_PREVIEW` experiment), otherwise a
    /// downscaled real render via [`djvu_render::RenderOptions::fit_to_box`].
    ///
    /// # Quality note
    ///
    /// A `TH44` thumbnail is the **encoder's own separately-lossy preview**,
    /// not a faithful downscale of the real page — it is a small (long side ≤
    /// 128 px) IW44 encode baked in at *encode* time, so its content can
    /// diverge from what a real render of the same page produces (different
    /// resampling, different quantisation, sometimes a stale image if the
    /// page was re-encoded without regenerating the thumbnail). Measured SSIM
    /// vs. a real render is **0.50–0.68** (`D5_TH44_PREVIEW`); this is a real
    /// quality trade, not a free 20–30× win — acceptable for a dense
    /// thumbnail *grid* where throughput dominates and the user can open the
    /// real page to look closely, but not a substitute for
    /// [`Page::render`]/[`Page::render_to_size`] as a faithful preview.
    ///
    /// With the `parallel` feature, pages are built concurrently on rayon
    /// (mirrors the PDF/EPUB parallel exporters); each page's `Result` is
    /// independent so one broken page doesn't fail the whole grid.
    ///
    /// Returns `Vec<Result<Pixmap, Error>>` rather than `Vec<Option<Pixmap>>`:
    /// with the render fallback in place there is no longer a legitimate "no
    /// thumbnail" case for a valid page, only success or a real decode error,
    /// and the latter must not be silently swallowed.
    pub fn thumbnails(&self, max_w: u32, max_h: u32) -> Vec<Result<Pixmap, Error>> {
        self.thumbnails_with_strategy(max_w, max_h, ThumbnailStrategy::Auto)
    }

    /// Like [`Document::thumbnails`] but with explicit control over the
    /// TH44-vs-render policy — see [`ThumbnailStrategy`].
    pub fn thumbnails_with_strategy(
        &self,
        max_w: u32,
        max_h: u32,
        strategy: ThumbnailStrategy,
    ) -> Vec<Result<Pixmap, Error>> {
        let indices: Vec<usize> = (0..self.page_count()).collect();

        let build = |&i: &usize| -> Result<Pixmap, Error> {
            let page = self.page(i)?;
            page.thumbnail_with_strategy(max_w, max_h, strategy)
        };

        #[cfg(feature = "parallel")]
        {
            use rayon::prelude::*;
            indices.par_iter().map(build).collect()
        }
        #[cfg(not(feature = "parallel"))]
        {
            indices.iter().map(build).collect()
        }
    }
}

/// Policy for [`Document::thumbnails_with_strategy`]: how to source each
/// page's thumbnail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThumbnailStrategy {
    /// Use the page's embedded `TH44` preview when present; otherwise fall
    /// back to a downscaled real render. Fastest and the default — see the
    /// quality note on [`Document::thumbnails`].
    #[default]
    Auto,
    /// Always render the full page and downscale, ignoring any `TH44` chunk.
    /// Slower but faithful; useful for quality comparisons against the
    /// `TH44` path and for documents where a stale/absent thumbnail must
    /// never be shown.
    RenderOnly,
    /// Require an embedded `TH44` chunk; returns an error for pages that
    /// lack one instead of falling back to render. Never touches the page's
    /// BG44/JB2 background decode. Useful to prove a corpus's thumbnail grid
    /// stays on the fast path, or to fail loudly rather than silently render.
    Th44Only,
}

/// A page within a DjVu document.
#[cfg(feature = "std")]
pub struct Page<'a> {
    page: &'a DjVuPage,
    index: usize,
}

#[cfg(feature = "std")]
impl<'a> Page<'a> {
    /// Page width in pixels (before rotation).
    pub fn width(&self) -> u32 {
        self.page.width() as u32
    }

    /// Page height in pixels (before rotation).
    pub fn height(&self) -> u32 {
        self.page.height() as u32
    }

    /// Effective page width after rotation.
    pub fn display_width(&self) -> u32 {
        self.display_dims().0
    }

    /// Effective page height after rotation.
    pub fn display_height(&self) -> u32 {
        self.display_dims().1
    }

    fn display_dims(&self) -> (u32, u32) {
        djvu_render::display_dimensions(self.page)
    }

    /// Build options that render the (rotation-aware) display size scaled by
    /// `scale`. The single home for the `display × scale` size idiom the
    /// scale-based render methods share.
    fn opts_for_scale(&self, scale: f32) -> djvu_render::RenderOptions {
        let (dw, dh) = self.display_dims();
        let w = ((dw as f32 * scale).round() as u32).max(1);
        let h = ((dh as f32 * scale).round() as u32).max(1);
        // The pipeline re-derives the decode scale from `w` and the page's
        // display width, so we set only the size; `scale` is no longer an input.
        djvu_render::RenderOptions {
            width: w,
            height: h,
            ..Default::default()
        }
    }

    /// Build options for an explicit target `width × height`, deriving the
    /// rotation-aware `scale` from [`RenderOptions::fit_to_width`] and then
    /// honoring the caller's exact `height` (which need not preserve aspect).
    /// The single home for the size-based scale idiom.
    fn opts_for_size(&self, width: u32, height: u32) -> djvu_render::RenderOptions {
        let mut opts = djvu_render::RenderOptions::fit_to_width(self.page, width);
        opts.height = height;
        opts
    }

    /// Render with caller-supplied [`RenderOptions`] — the single entry point
    /// the convenience methods funnel through.
    ///
    /// Build `opts` with the rotation-aware
    /// [`RenderOptions::fit_to_width`](djvu_render::RenderOptions::fit_to_width)
    /// / `fit_to_height` / `fit_to_box` constructors, then set
    /// `bold` / `aa` / `resampling` as needed. This supersedes the bespoke
    /// `render_bold` / `render_aa` / `render_scaled*` methods.
    pub fn render_with(&self, opts: &djvu_render::RenderOptions) -> Result<Pixmap, Error> {
        djvu_render::render_pixmap_with_limits(self.page, opts, self.page.resource_limits())
            .map_err(Self::render_err)
    }

    /// Render with caller-supplied [`RenderOptions`] and an optional limit override.
    pub fn render_with_limits(
        &self,
        opts: &djvu_render::RenderOptions,
        limits: Option<crate::resource_limits::ResourceLimits>,
    ) -> Result<Pixmap, Error> {
        djvu_render::render_pixmap_with_limits(self.page, opts, limits).map_err(Self::render_err)
    }

    /// Page resolution in dots per inch.
    pub fn dpi(&self) -> u16 {
        self.page.dpi()
    }

    /// Pixel `(width, height)` this page renders to at `target_dpi`.
    ///
    /// The rounding/clamping policy lives in one place (the crate-internal
    /// `export_common` sizing helper); a `0`-DPI page is treated as 1 DPI so the
    /// scale stays finite.
    pub fn size_at_dpi(&self, target_dpi: f32) -> (u32, u32) {
        export_common::size_at_dpi(self.page, target_dpi)
    }

    /// The 0-based index of this page within the document.
    pub fn index(&self) -> usize {
        self.index
    }

    /// Page rotation from the INFO chunk.
    pub fn rotation(&self) -> info::Rotation {
        self.page.rotation()
    }

    fn render_err(e: djvu_render::RenderError) -> Error {
        Error::FormatError(e.to_string())
    }

    /// Decode the JB2/G4 mask layer only (no compositing).
    ///
    /// Returns `None` when the page has no mask chunk (pure IW44 background page).
    pub fn decode_mask(&self) -> Result<Option<Bitmap>, Error> {
        self.page
            .extract_mask()
            .map_err(|e| Error::FormatError(e.to_string()))
    }

    /// Render the page to an RGBA pixmap at native resolution.
    pub fn render(&self) -> Result<Pixmap, Error> {
        self.render_with(&self.opts_for_scale(1.0))
    }

    /// Render the page to an RGBA pixmap at a target size.
    pub fn render_to_size(&self, width: u32, height: u32) -> Result<Pixmap, Error> {
        self.render_with(&self.opts_for_size(width, height))
    }

    /// Render a rectangular region of the page.
    ///
    /// `full_w × full_h` set the full-render output size the region is cut
    /// from (the zoom level); `(x, y, w, h)` select the viewport within that
    /// space. Routed through the composited-tile cache
    /// ([`djvu_render::render_region_tiled`]) so viewer-style pans and
    /// revisits reuse tiles (C4_TILE_CACHE / TILE_LRU) — O(viewport) work
    /// instead of O(page).
    pub fn render_region(
        &self,
        full_w: u32,
        full_h: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Pixmap, Error> {
        let opts = self.opts_for_size(full_w, full_h);
        djvu_render::render_region_tiled(
            self.page,
            djvu_render::RenderRect {
                x,
                y,
                width: w,
                height: h,
            },
            &opts,
        )
        .map_err(Self::render_err)
    }

    /// Fast coarse render — decodes only the first BG44 chunk (a blurry but
    /// near-instant preview). Returns `Ok(None)` for bilevel-only pages.
    pub fn render_coarse(&self, width: u32, height: u32) -> Result<Option<Pixmap>, Error> {
        let opts = self.opts_for_size(width, height);
        djvu_render::render_coarse(self.page, &opts).map_err(Self::render_err)
    }

    /// Progressive render: decode BG44 chunks `0..=chunk_n` plus all
    /// foreground layers. `chunk_n = bg44_chunk_count() - 1` equals the full
    /// render; each lower value is a coarser refinement stage.
    pub fn render_progressive(
        &self,
        width: u32,
        height: u32,
        chunk_n: usize,
    ) -> Result<Pixmap, Error> {
        let opts = self.opts_for_size(width, height);
        djvu_render::render_progressive(self.page, &opts, chunk_n).map_err(Self::render_err)
    }

    /// Number of BG44 refinement chunks on this page (0 for bilevel pages).
    pub fn bg44_chunk_count(&self) -> usize {
        self.page.bg44_chunks().len()
    }

    /// Decode the page thumbnail, if available.
    pub fn thumbnail(&self) -> Result<Option<Pixmap>, Error> {
        self.page
            .thumbnail()
            .map_err(|e| Error::FormatError(e.to_string()))
    }

    /// Build this page's thumbnail scaled to fit within `max_w × max_h`
    /// (aspect-preserving), sourced per `strategy`. The single-page building
    /// block behind [`Document::thumbnails`] / `thumbnails_with_strategy`;
    /// see those for the TH44-vs-render quality/speed trade.
    pub fn thumbnail_with_strategy(
        &self,
        max_w: u32,
        max_h: u32,
        strategy: ThumbnailStrategy,
    ) -> Result<Pixmap, Error> {
        match strategy {
            ThumbnailStrategy::RenderOnly => self.render_fit_to_box(max_w, max_h),
            ThumbnailStrategy::Th44Only => {
                let pm = self.thumbnail()?.ok_or_else(|| {
                    Error::FormatError(format!(
                        "page {} has no embedded TH44 thumbnail (Th44Only strategy)",
                        self.index
                    ))
                })?;
                Ok(Self::fit_within(pm, max_w, max_h))
            }
            ThumbnailStrategy::Auto => match self.thumbnail()? {
                Some(pm) => Ok(Self::fit_within(pm, max_w, max_h)),
                None => self.render_fit_to_box(max_w, max_h),
            },
        }
    }

    /// Render-fallback path: full decode + downscale to fit `max_w × max_h`.
    /// Never reads a `TH44` chunk.
    fn render_fit_to_box(&self, max_w: u32, max_h: u32) -> Result<Pixmap, Error> {
        let opts = djvu_render::RenderOptions::fit_to_box(self.page, max_w, max_h);
        self.render_with(&opts)
    }

    /// If `pm` exceeds `max_w × max_h`, Lanczos-3 downscale it to fit
    /// (preserving aspect); otherwise return it unchanged. A `TH44`
    /// thumbnail is capped at `THUMBNAIL_MAX_SIDE` (128 px) by the encoder,
    /// so this is a no-op whenever the caller's box is at least that big —
    /// the common case for a thumbnail grid. It is never *upscaled* to fill
    /// a larger box: doing so would add cost without adding real detail.
    fn fit_within(pm: Pixmap, max_w: u32, max_h: u32) -> Pixmap {
        let max_w = max_w.max(1);
        let max_h = max_h.max(1);
        if pm.width <= max_w && pm.height <= max_h {
            return pm;
        }
        let scale_w = max_w as f64 / pm.width.max(1) as f64;
        let scale_h = max_h as f64 / pm.height.max(1) as f64;
        let scale = scale_w.min(scale_h);
        let tw = ((pm.width as f64 * scale).round() as u32).max(1);
        let th = ((pm.height as f64 * scale).round() as u32).max(1);
        crate::pixmap::scale_lanczos3(&pm, tw, th)
    }

    /// Extract the text layer (TXTz/TXTa) with zone hierarchy.
    pub fn text_layer(&self) -> Result<Option<TextLayer>, Error> {
        self.page
            .text_layer()
            .map_err(|e| Error::FormatError(e.to_string()))
    }

    /// Extract the plain text content of the page.
    pub fn text(&self) -> Result<Option<String>, Error> {
        Ok(self.text_layer()?.map(|tl| tl.text))
    }
}

// Compile-time assertions: Document is Send + Sync.
#[cfg(feature = "std")]
#[allow(dead_code)]
const _: () = {
    fn assert_send<T: Send>() {}
    fn assert_sync<T: Sync>() {}
    fn assertions() {
        assert_send::<Document>();
        assert_sync::<Document>();
    }
};

#[cfg(test)]
mod tests {
    use super::*;

    fn chicken() -> &'static std::path::Path {
        std::path::Path::new("references/djvujs/library/assets/chicken.djvu")
    }

    fn chicken_bytes() -> Vec<u8> {
        std::fs::read(chicken()).unwrap()
    }

    #[test]
    fn document_open_succeeds() {
        let doc = Document::open(chicken()).unwrap();
        assert!(doc.page_count() > 0);
    }

    #[test]
    fn document_from_reader_succeeds() {
        let f = std::fs::File::open(chicken()).unwrap();
        let doc = Document::from_reader(f).unwrap();
        assert!(doc.page_count() > 0);
    }

    #[test]
    fn document_from_bytes_succeeds() {
        let doc = Document::from_bytes(chicken_bytes()).unwrap();
        assert!(doc.page_count() > 0);
    }

    #[test]
    fn document_bookmarks_returns_vec() {
        let doc = Document::open(chicken()).unwrap();
        let bm = doc.bookmarks().unwrap();
        // chicken.djvu has no bookmarks
        assert!(bm.is_empty() || !bm.is_empty());
    }

    #[test]
    fn document_page_out_of_bounds_returns_error() {
        let doc = Document::open(chicken()).unwrap();
        assert!(doc.page(999).is_err());
    }

    #[test]
    fn page_dimensions_and_dpi() {
        let doc = Document::open(chicken()).unwrap();
        let page = doc.page(0).unwrap();
        assert!(page.width() > 0);
        assert!(page.height() > 0);
        assert_eq!(page.display_width(), page.width());
        assert_eq!(page.display_height(), page.height());
        assert!(page.dpi() > 0);
        assert_eq!(page.index(), 0);
        let _ = page.rotation();
    }

    #[test]
    fn page_size_at_dpi() {
        let doc = Document::open(chicken()).unwrap();
        let page = doc.page(0).unwrap();
        let (w, h) = page.size_at_dpi(72.0);
        assert!(w > 0 && h > 0);
    }

    #[test]
    fn page_render_and_render_to_size() {
        let doc = Document::open(chicken()).unwrap();
        let page = doc.page(0).unwrap();
        let pm = page.render().unwrap();
        assert!(pm.width > 0 && pm.height > 0);
        let pm2 = page.render_to_size(50, 60).unwrap();
        assert_eq!(pm2.width, 50);
        assert_eq!(pm2.height, 60);
    }

    #[test]
    fn page_render_with_opts() {
        use crate::djvu_render::RenderOptions;
        let doc = Document::open(chicken()).unwrap();
        let page = doc.page(0).unwrap();
        let opts = RenderOptions {
            width: 32,
            height: 32,
            ..Default::default()
        };
        let pm = page.render_with(&opts).unwrap();
        assert!(pm.width > 0);
    }

    #[test]
    fn page_decode_mask_does_not_panic() {
        let doc = Document::open(chicken()).unwrap();
        let page = doc.page(0).unwrap();
        let _ = page.decode_mask().unwrap();
    }

    #[test]
    fn page_thumbnail_does_not_panic() {
        let doc = Document::open(chicken()).unwrap();
        let page = doc.page(0).unwrap();
        let _ = page.thumbnail().unwrap();
    }

    #[test]
    fn page_text_layer_and_text() {
        let doc = Document::open(chicken()).unwrap();
        let page = doc.page(0).unwrap();
        let _ = page.text_layer().unwrap();
        let _ = page.text().unwrap();
    }

    #[test]
    fn page_render_with_invalid_dims_returns_error() {
        use crate::djvu_render::RenderOptions;
        let doc = Document::open(chicken()).unwrap();
        let page = doc.page(0).unwrap();
        let opts = RenderOptions {
            width: 0,
            height: 0,
            ..Default::default()
        };
        let err = page.render_with(&opts).unwrap_err();
        assert!(
            matches!(err, Error::FormatError(_)),
            "invalid dims must return FormatError, got {err:?}"
        );
    }

    #[test]
    fn document_open_dir_bundled_succeeds() {
        // chicken.djvu is bundled, so open_dir also works
        let doc = Document::open_dir(chicken()).unwrap();
        assert!(doc.page_count() > 0);
    }

    // ---- TH44 thumbnail-grid API (Document::thumbnails) -----------------

    /// Build a tiny 2-page bilevel bundle with TH44 thumbnails embedded via
    /// the #476 encoder (`encode_djvm_bundle_jb2_with_thumbnails`).  A high
    /// `shared_dict_page_threshold` keeps the encoding path simple (no
    /// DJVI/INCL shared dict) so there is exactly one `Sjbz` chunk per page.
    fn th44_bundle_bytes() -> Vec<u8> {
        let mut p1 = Bitmap::new(64, 48);
        for y in 4..44 {
            p1.set_black(8, y);
            p1.set_black(y % 60, 8);
        }
        let mut p2 = Bitmap::new(64, 48);
        for x in 4..60 {
            p2.set_black(x, 20);
        }
        crate::jb2_encode::encode_djvm_bundle_jb2_with_thumbnails(
            &[p1, p2],
            100, // threshold higher than page count: no symbol sharing
            crate::jb2_encode::BUNDLE_DEFAULT_DPI,
            true,
        )
    }

    /// Corrupt every `Sjbz` chunk's payload bytes in place (length field and
    /// framing untouched, so the IFF container still parses) so JB2 decode
    /// of the background/mask fails while the container and its `TH44`
    /// chunks remain intact.
    fn corrupt_all_sjbz_payloads(bundle: &mut [u8]) {
        let mut search_from = 0usize;
        let mut corrupted = 0u32;
        while let Some(rel) = bundle[search_from..].windows(4).position(|w| w == b"Sjbz") {
            let pos = search_from + rel;
            let len_start = pos + 4;
            let len = u32::from_be_bytes([
                bundle[len_start],
                bundle[len_start + 1],
                bundle[len_start + 2],
                bundle[len_start + 3],
            ]) as usize;
            let payload_start = len_start + 4;
            for b in &mut bundle[payload_start..payload_start + len] {
                *b = 0xFF;
            }
            corrupted += 1;
            search_from = payload_start + len;
        }
        assert!(corrupted > 0, "test bundle must contain Sjbz chunks");
    }

    #[test]
    fn thumbnails_auto_uses_th44_and_matches_th44_only() {
        let bundle = th44_bundle_bytes();
        let doc = Document::from_bytes(bundle).unwrap();
        assert_eq!(doc.page_count(), 2);

        let auto = doc.thumbnails(128, 128);
        let th44_only = doc.thumbnails_with_strategy(128, 128, ThumbnailStrategy::Th44Only);
        assert_eq!(auto.len(), 2);
        assert_eq!(th44_only.len(), 2);
        for i in 0..2 {
            let a = auto[i].as_ref().expect("Auto must succeed");
            let t = th44_only[i].as_ref().expect("Th44Only must succeed");
            // Auto must have taken the TH44 branch (not render-fallback):
            // its pixmap is byte-identical to the explicit Th44Only result.
            assert_eq!(a.width, t.width);
            assert_eq!(a.height, t.height);
            assert_eq!(
                a.data, t.data,
                "Auto must match Th44Only when TH44 is present"
            );
        }
    }

    /// Structural proof that the TH44 path never touches BG44/JB2 decode:
    /// corrupt every page's `Sjbz` payload (the JB2-encoded mask/background)
    /// so any render attempt fails, while leaving the `TH44` chunks intact.
    /// `RenderOnly` must then fail for every page (proving render really
    /// depends on the corrupted chunk), while `Th44Only` and `Auto` must
    /// still succeed — the only way that's possible is if they never
    /// decoded the corrupted Sjbz chunk at all.
    #[test]
    fn thumbnails_th44_path_never_touches_corrupted_jb2_background() {
        let mut bundle = th44_bundle_bytes();
        corrupt_all_sjbz_payloads(&mut bundle);
        let doc = Document::from_bytes(bundle).unwrap();
        assert_eq!(doc.page_count(), 2);

        let render_only = doc.thumbnails_with_strategy(128, 128, ThumbnailStrategy::RenderOnly);
        for (i, r) in render_only.iter().enumerate() {
            assert!(
                r.is_err(),
                "page {i}: RenderOnly must fail against a corrupted Sjbz chunk"
            );
        }

        let th44_only = doc.thumbnails_with_strategy(128, 128, ThumbnailStrategy::Th44Only);
        for (i, r) in th44_only.iter().enumerate() {
            assert!(
                r.is_ok(),
                "page {i}: Th44Only must succeed even with a corrupted Sjbz chunk, got {r:?}"
            );
        }

        let auto = doc.thumbnails(128, 128);
        for (i, r) in auto.iter().enumerate() {
            assert!(
                r.is_ok(),
                "page {i}: Auto must take the TH44 fast path and never touch the corrupted background, got {r:?}"
            );
        }
    }

    #[test]
    fn thumbnails_th44_only_errors_without_th44() {
        // chicken.djvu has no embedded TH44 chunks.
        let doc = Document::open(chicken()).unwrap();
        let results = doc.thumbnails_with_strategy(128, 128, ThumbnailStrategy::Th44Only);
        assert_eq!(results.len(), doc.page_count());
        for r in &results {
            assert!(r.is_err(), "Th44Only must error when no TH44 chunk exists");
        }
    }

    #[test]
    fn thumbnails_auto_falls_back_to_render_without_th44() {
        // chicken.djvu has no embedded TH44 chunks, so Auto must render.
        let doc = Document::open(chicken()).unwrap();
        let auto = doc.thumbnails(64, 64);
        let render_only = doc.thumbnails_with_strategy(64, 64, ThumbnailStrategy::RenderOnly);
        assert_eq!(auto.len(), render_only.len());
        for (a, r) in auto.iter().zip(render_only.iter()) {
            let a = a.as_ref().expect("Auto must succeed via render fallback");
            let r = r.as_ref().expect("RenderOnly must succeed");
            assert_eq!(a.width, r.width);
            assert_eq!(a.height, r.height);
            assert_eq!(
                a.data, r.data,
                "Auto fallback must match RenderOnly exactly"
            );
        }
    }

    #[test]
    fn thumbnails_never_upscale_a_small_th44_thumbnail() {
        // Request a box bigger than the embedded 128px-cap TH44 thumbnail;
        // the result must stay at the TH44's native (small) size, not be
        // upscaled to fill the requested box.
        let bundle = th44_bundle_bytes();
        let doc = Document::from_bytes(bundle).unwrap();
        let page = doc.page(0).unwrap();
        let native = page.thumbnail().unwrap().expect("page has TH44");
        let grid = page
            .thumbnail_with_strategy(4096, 4096, ThumbnailStrategy::Th44Only)
            .unwrap();
        assert_eq!(grid.width, native.width);
        assert_eq!(grid.height, native.height);
    }

    #[test]
    fn thumbnails_downscale_an_oversized_th44_thumbnail() {
        // Request a box smaller than the embedded thumbnail: it must be
        // downscaled to fit, preserving aspect ratio.
        let bundle = th44_bundle_bytes();
        let doc = Document::from_bytes(bundle).unwrap();
        let page = doc.page(0).unwrap();
        let native = page.thumbnail().unwrap().expect("page has TH44");
        assert!(native.width > 16 || native.height > 16);
        let grid = page
            .thumbnail_with_strategy(16, 16, ThumbnailStrategy::Th44Only)
            .unwrap();
        assert!(grid.width <= 16 && grid.height <= 16);
        assert!(grid.width >= 1 && grid.height >= 1);
    }
}
