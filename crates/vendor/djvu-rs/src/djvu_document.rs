//! New document model for DjVu files — phase 3.
//!
//! This module provides the high-level `DjVuDocument` API built on top of the
//! clean-room IFF parser (phase 1), BZZ decompressor (phase 2a), and IW44 decoder
//! (phase 2c).
//!
//! ## Key public types
//!
//! - [`DjVuDocument`] — opened DjVu document (single-page or multi-page)
//! - [`DjVuPage`] — lazy page handle (raw chunks stored until `thumbnail()` is called)
//! - [`DjVuBookmark`] — table-of-contents entry from the NAVM chunk
//! - [`DocError`] — typed errors for this module
//!
//! ## Document kinds
//!
//! - **FORM:DJVU** — single-page document
//! - **FORM:DJVM + DIRM** — bundled multi-page document with an in-file page index
//! - **FORM:DJVM + DIRM (indirect)** — pages live in separate files; a resolver
//!   callback `fn(name: &str) -> Result<Vec<u8>, DocError>` is required
//!
//! ## Lazy decoding contract
//!
//! `DjVuPage` stores only the raw chunk bytes. No image decoding happens until
//! the caller explicitly calls `thumbnail()` (which invokes the IW44 decoder).

#[cfg(not(feature = "std"))]
use alloc::{
    string::{String, ToString},
    vec,
    vec::Vec,
};

use crate::{
    annotation::{Annotation, AnnotationError, MapArea},
    bzz::bzz_decode,
    dirm::{DirmComponentKind, DirmPayload},
    error::{BzzError, IffError, Iw44Error, Jb2Error},
    iff::{IffChunk, parse_form, parse_form_body},
    info::PageInfo,
    iw44::Iw44Image,
    jb2::Jb2Dict,
    metadata::{DjVuMetadata, MetadataError},
    pixmap::Pixmap,
    text::{TextError, TextLayer},
};

#[cfg(feature = "std")]
use std::sync::Arc;

// ---- Error type -------------------------------------------------------------

/// Errors that can occur when working with the DjVuDocument API.
#[derive(Debug, thiserror::Error)]
pub enum DocError {
    /// IFF container parse error.
    #[error("IFF error: {0}")]
    Iff(#[from] IffError),

    /// BZZ decompression error.
    #[error("BZZ error: {0}")]
    Bzz(#[from] BzzError),

    /// IW44 wavelet decoding error.
    #[error("IW44 error: {0}")]
    Iw44(#[from] Iw44Error),

    /// JB2 bilevel image decoding error.
    #[error("JB2 error: {0}")]
    Jb2(#[from] Jb2Error),

    /// The file is not a supported DjVu format.
    #[error("not a DjVu file: found form type {0:?}")]
    NotDjVu([u8; 4]),

    /// A required chunk is missing.
    #[error("missing required chunk: {0}")]
    MissingChunk(&'static str),

    /// The document is malformed (description included).
    #[error("malformed DjVu document: {0}")]
    Malformed(&'static str),

    /// An indirect page reference could not be resolved.
    #[error("failed to resolve indirect page '{0}'")]
    IndirectResolve(String),

    /// Page index is out of range.
    #[error("page index {index} is out of range (document has {count} pages)")]
    PageOutOfRange { index: usize, count: usize },

    /// Invalid UTF-8 in a string field.
    ///
    /// No longer produced since #524: NAVM bookmark strings are decoded
    /// leniently (CP1252 fallback). Kept so matching code keeps compiling.
    #[error("invalid UTF-8 in DjVu metadata")]
    InvalidUtf8,

    /// The resolver callback is required for indirect documents but was not provided.
    #[error("indirect DjVu document requires a resolver callback")]
    NoResolver,

    /// I/O error when reading file data (only with `std` feature).
    #[cfg(feature = "std")]
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// G4/MMR mask decoding error.
    #[error("Smmr decode error: {0}")]
    Smmr(String),

    /// Text layer parse error.
    #[error("text layer error: {0}")]
    Text(#[from] TextError),

    /// Annotation parse error.
    #[error("annotation error: {0}")]
    Annotation(#[from] AnnotationError),

    /// Metadata parse error.
    #[error("metadata error: {0}")]
    Metadata(#[from] MetadataError),
}

// ---- Bookmark ---------------------------------------------------------------

/// A table-of-contents entry from the NAVM chunk.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct DjVuBookmark {
    /// Display title.
    pub title: String,
    /// Target URL (DjVu internal URL format).
    pub url: String,
    /// Nested child entries.
    pub children: Vec<DjVuBookmark>,
}

// ---- Page -------------------------------------------------------------------

/// A raw chunk extracted from a page FORM:DJVU.
#[derive(Debug, Clone)]
struct RawChunk {
    id: [u8; 4],
    data: Vec<u8>,
}

/// Shared, owned backing store for a document's bytes (an owned `Vec<u8>` from
/// [`crate::Document::from_bytes`], or a `memmap2::Mmap`). Lazily-constructed
/// pages ([`ChunkStore::Lazy`]) hold an `Arc` clone of this so their chunk bytes
/// can be materialised on first access without copying them at open time.
#[cfg(feature = "std")]
pub(crate) type Backing = Arc<dyn AsRef<[u8]> + Send + Sync>;

/// The bytes behind a [`Backing`].
#[cfg(feature = "std")]
fn backing_bytes(b: &Backing) -> &[u8] {
    (**b).as_ref()
}

/// Where a page's chunk list comes from.
///
/// `Eager` holds the copied chunks (the historical behaviour, used for
/// single-page, indirect, and `no_std` documents). `Lazy` defers the per-chunk
/// `to_vec` copy until first access: it keeps the shared document backing and
/// this page's `FORM` byte range, and materialises the chunks once on demand.
/// This is what makes opening a large bundled document O(1) in copies instead of
/// O(total bytes) when only some pages are ever rendered (LAZY_PAGE_CONSTRUCT).
#[cfg(feature = "std")]
enum ChunkStore {
    Eager(Vec<RawChunk>),
    Lazy {
        backing: Backing,
        range: core::ops::Range<usize>,
        cache: std::sync::OnceLock<Vec<RawChunk>>,
    },
}

#[cfg(feature = "std")]
impl ChunkStore {
    /// The page's chunks, materialising them from the backing on first call for
    /// the `Lazy` variant. A corrupt/out-of-range slice yields an empty list
    /// (permissive, matching the render path's error handling).
    fn get(&self) -> &[RawChunk] {
        match self {
            ChunkStore::Eager(v) => v,
            ChunkStore::Lazy {
                backing,
                range,
                cache,
            } => cache.get_or_init(|| {
                let Some(sub) = backing_bytes(backing).get(range.clone()) else {
                    return Vec::new();
                };
                match parse_sub_form(sub) {
                    Ok(chunks) => chunks
                        .iter()
                        .map(|c| RawChunk {
                            id: c.id,
                            data: c.data.to_vec(),
                        })
                        .collect(),
                    Err(_) => Vec::new(),
                }
            }),
        }
    }
}

#[cfg(feature = "std")]
impl Clone for ChunkStore {
    fn clone(&self) -> Self {
        match self {
            ChunkStore::Eager(v) => ChunkStore::Eager(v.clone()),
            // A cloned page re-defers: same backing + range, fresh cache.
            ChunkStore::Lazy { backing, range, .. } => ChunkStore::Lazy {
                backing: backing.clone(),
                range: range.clone(),
                cache: std::sync::OnceLock::new(),
            },
        }
    }
}

/// Decode the payload of a paired `*z` (BZZ-compressed) / `*a` (raw) chunk.
///
/// DjVu stores most variable-length payloads as a pair of chunk ids: a
/// BZZ-compressed `*z` variant (`TXTz`, `ANTz`, `METz`, …) and a raw `*a`
/// variant (`TXTa`, `ANTa`, `METa`, …).  This is the single place that owns
/// the "is it compressed?" decision: it prefers the compressed chunk, falls
/// back to the raw chunk, and treats a present-but-empty chunk as "no payload"
/// (DjVu uses a zero-length chunk as a placeholder).  Callers receive already
/// decoded bytes, so the format parsers stay pure `&[u8]` functions that never
/// touch compression.
fn decode_paired_payload(z: Option<&[u8]>, a: Option<&[u8]>) -> Result<Option<Vec<u8>>, BzzError> {
    if let Some(z) = z {
        return if z.is_empty() {
            Ok(None)
        } else {
            Ok(Some(bzz_decode(z)?))
        };
    }
    if let Some(a) = a {
        return Ok(if a.is_empty() { None } else { Some(a.to_vec()) });
    }
    Ok(None)
}

/// A lazy DjVu page handle.
///
/// Raw chunk data is stored on construction. No image decoding is performed
/// until the caller invokes `thumbnail()` or a render function.
///
/// The fully decoded BG44 wavelet image is cached after the first render so
/// that subsequent renders skip the expensive ZP arithmetic decode and only
/// run the wavelet inverse-transform and compositor.
///
/// ## Caching
///
/// [`decoded_bg44`](Self::decoded_bg44), [`decoded_mask`](Self::decoded_mask),
/// and [`decoded_fg44`](Self::decoded_fg44) cache their results in a
/// `std::sync::OnceLock` after the first call. Prefer these over the
/// `extract_*` methods in performance-sensitive loops.
///
/// **`Clone` resets the cache.** A cloned `DjVuPage` starts with empty caches;
/// the first render on the clone re-runs the full decode.
/// A shared JB2 symbol dictionary referenced by one or more pages via their
/// INCL chunk.
///
/// The raw `Djbz` bytes and the lazily-decoded [`Jb2Dict`] are wrapped in the
/// **same** `Arc`, which every page referencing this DJVI component clones. So
/// when many pages share one dictionary (the common case for bundled scans —
/// e.g. 85 pages over 2 dictionaries), the ZP arithmetic decode runs **once per
/// document** rather than once per page. Previously the raw bytes were shared
/// (via `Arc`) but each page decoded them into its own per-page cache.
#[cfg(feature = "std")]
pub(crate) struct SharedDict {
    raw: Vec<u8>,
    decoded: std::sync::OnceLock<Option<Jb2Dict>>,
}

#[cfg(feature = "std")]
impl SharedDict {
    /// Wrap raw `Djbz` bytes; the dictionary is decoded lazily on first use.
    pub(crate) fn new(raw: Vec<u8>) -> Self {
        Self {
            raw,
            decoded: std::sync::OnceLock::new(),
        }
    }

    /// Decode the dictionary on first call and return it, caching the result so
    /// every page sharing this `Arc` reuses the single decode.
    fn get(&self) -> Option<&Jb2Dict> {
        self.decoded
            .get_or_init(|| crate::jb2::decode_dict(&self.raw, None).ok())
            .as_ref()
    }

    /// Length of the raw `Djbz` bytes (for `Debug`).
    fn raw_len(&self) -> usize {
        self.raw.len()
    }
}

pub struct DjVuPage {
    /// Page info parsed from the INFO chunk.
    info: PageInfo,
    /// All raw chunks from this page's FORM:DJVU, in order. In `std` builds this
    /// may be lazily materialised from the document backing (see [`ChunkStore`]);
    /// `no_std` always holds the eagerly-copied chunks.
    #[cfg(feature = "std")]
    chunks: ChunkStore,
    #[cfg(not(feature = "std"))]
    chunks: Vec<RawChunk>,
    /// Page index within the document (0-based).
    index: usize,
    /// Raw Djbz data from the DJVI shared dictionary component referenced via
    /// the page's INCL chunk, if present.  Stored here so that `extract_mask`
    /// can decode it without access to the parent document.
    ///
    /// Wrapped in `Arc` so that multi-page documents share one allocation —
    /// and, via [`SharedDict`], one *decode* — instead of cloning the bytes and
    /// re-decoding per page.
    #[cfg(feature = "std")]
    shared_djbz: Option<Arc<SharedDict>>,
    #[cfg(not(feature = "std"))]
    shared_djbz: Option<Vec<u8>>,
    /// Render-tier cache of this page's decoded layers (background, mask,
    /// quarter-resolution mask, foreground).  The decode logic and the
    /// compositor-subsampling concern live in
    /// [`crate::djvu_render::PageLayers`]; the page only holds the handle so
    /// repeated renders reuse the decode.  Populated on first render.
    /// Only available when the `std` feature is enabled (`OnceLock` requires std).
    #[cfg(feature = "std")]
    render_layers: std::sync::OnceLock<crate::djvu_render::PageLayers>,
}

impl Clone for DjVuPage {
    fn clone(&self) -> Self {
        DjVuPage {
            info: self.info.clone(),
            chunks: self.chunks.clone(),
            index: self.index,
            shared_djbz: self.shared_djbz.clone(),
            // The render cache is not cloned — it is lazily recomputed. The
            // shared-dict decode lives inside the `shared_djbz` Arc, so a cloned
            // page keeps sharing the single decode (the dict is immutable).
            #[cfg(feature = "std")]
            render_layers: std::sync::OnceLock::new(),
        }
    }
}

impl core::fmt::Debug for DjVuPage {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        #[cfg(feature = "std")]
        let dbz_len = self.shared_djbz.as_ref().map(|v| v.raw_len());
        #[cfg(not(feature = "std"))]
        let dbz_len = self.shared_djbz.as_ref().map(|v| v.len());
        f.debug_struct("DjVuPage")
            .field("info", &self.info)
            .field("chunks", &self.chunk_slice())
            .field("index", &self.index)
            .field("shared_djbz", &dbz_len)
            .finish_non_exhaustive()
    }
}

impl DjVuPage {
    /// Page width in pixels.
    pub fn width(&self) -> u16 {
        self.info.width
    }

    /// Page height in pixels.
    pub fn height(&self) -> u16 {
        self.info.height
    }

    /// Page resolution in dots per inch.
    pub fn dpi(&self) -> u16 {
        self.info.dpi
    }

    /// Display gamma from the INFO chunk.
    pub fn gamma(&self) -> f32 {
        self.info.gamma
    }

    /// Page rotation from the INFO chunk.
    pub fn rotation(&self) -> crate::info::Rotation {
        self.info.rotation
    }

    /// 0-based page index within the document.
    pub fn index(&self) -> usize {
        self.index
    }

    /// Dimensions as `(width, height)`.
    pub fn dimensions(&self) -> (u16, u16) {
        (self.info.width, self.info.height)
    }

    /// Decode the thumbnail for this page from TH44 chunks, if present.
    ///
    /// No image data is decoded until this method is called (lazy contract).
    ///
    /// Returns `Ok(None)` if the page has no TH44 thumbnail.
    pub fn thumbnail(&self) -> Result<Option<Pixmap>, DocError> {
        let th44_chunks: Vec<&[u8]> = self
            .chunk_slice()
            .iter()
            .filter(|c| &c.id == b"TH44")
            .map(|c| c.data.as_slice())
            .collect();

        if th44_chunks.is_empty() {
            return Ok(None);
        }

        let mut img = Iw44Image::new();
        for chunk_data in &th44_chunks {
            img.decode_chunk(chunk_data)?;
        }
        let pixmap = img.to_rgb()?;
        Ok(Some(pixmap))
    }

    /// Drop this page's render-tier decode cache, reclaiming its per-page memory.
    ///
    /// A rendered page memoises its decoded background (including the full-res
    /// RGB pixmap — up to `width × height × 4` bytes), mask, and foreground in a
    /// [`crate::djvu_render::PageLayers`] that lives as long as the owning
    /// document. Rendering many pages of a large document therefore accumulates
    /// one such cache per page — the peak RSS grows linearly with pages rendered
    /// (measured ≈ 11 MB/page on `colorbook.djvu`), which can exhaust memory in a
    /// long-lived viewer over a big book.
    ///
    /// This resets the cache so the memory is reclaimed; it rebuilds lazily on
    /// the next render of this page. A viewer can call it on pages scrolled
    /// off-screen (or use [`DjVuDocument::retain_render_caches`]) to bound memory.
    /// Requires `&mut` since the cache uses interior mutability for shared reads.
    #[cfg(feature = "std")]
    pub fn evict_render_cache(&mut self) {
        self.render_layers = std::sync::OnceLock::new();
    }

    /// C5_COMPRESS: cheaper alternative to [`evict_render_cache`](Self::evict_render_cache)
    /// — instead of dropping this page's entire render cache, drop only the
    /// expensive full-resolution derivations (the BG44 coefficient image,
    /// the full-res RGB pixmap, mask, foreground, and composited tiles) while
    /// keeping any already-cached downscaled RGB pixmap (`bg_rgb_s2` /
    /// `bg_rgb_s4`, populated by a prior 150-DPI-class or thumbnail render).
    ///
    /// A later downscaled render of this page (subsample ≥ 2) then stays
    /// warm instead of re-paying the full BG44 ZP decode; a full-resolution
    /// render still cold-decodes, same as after a full
    /// [`evict_render_cache`](Self::evict_render_cache) — see
    /// PERF_EXPERIMENTS.md C5_COMPRESS for why a cheap sub=2→sub=1 "upgrade"
    /// is not possible. No-op if the page was never rendered.
    #[cfg(feature = "std")]
    pub fn downgrade_render_cache(&mut self) {
        if let Some(layers) = self.render_layers.get_mut() {
            layers.downgrade();
        }
    }

    /// Return the raw bytes of the first chunk with the given 4-byte ID.
    ///
    /// Returns `None` if no chunk with that ID exists.  The returned slice
    /// points into the owned chunk storage — zero copy.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let sjbz = page.raw_chunk(b"Sjbz").expect("page must have a JB2 chunk");
    /// ```
    /// This page's raw chunk list, materialising lazily-stored chunks on first
    /// access (`std`) or returning the eagerly-copied list (`no_std`).
    #[cfg(feature = "std")]
    fn chunk_slice(&self) -> &[RawChunk] {
        self.chunks.get()
    }
    #[cfg(not(feature = "std"))]
    fn chunk_slice(&self) -> &[RawChunk] {
        &self.chunks
    }

    pub fn raw_chunk(&self, id: &[u8; 4]) -> Option<&[u8]> {
        self.chunk_slice()
            .iter()
            .find(|c| &c.id == id)
            .map(|c| c.data.as_slice())
    }

    /// Return the raw bytes of all chunks with the given 4-byte ID, in order.
    ///
    /// Returns an empty `Vec` if no such chunk exists.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let bg44_chunks = page.all_chunks(b"BG44");
    /// assert!(!bg44_chunks.is_empty(), "colour page must have BG44 data");
    /// ```
    pub fn all_chunks(&self, id: &[u8; 4]) -> Vec<&[u8]> {
        self.chunk_slice()
            .iter()
            .filter(|c| &c.id == id)
            .map(|c| c.data.as_slice())
            .collect()
    }

    /// Return the IDs of all chunks present on this page, in order.
    ///
    /// Duplicate IDs appear multiple times (once per chunk).
    pub fn chunk_ids(&self) -> Vec<[u8; 4]> {
        self.chunk_slice().iter().map(|c| c.id).collect()
    }

    /// Deprecated alias for [`Self::raw_chunk`]; kept for internal callers.
    #[doc(hidden)]
    pub fn find_chunk(&self, id: &[u8; 4]) -> Option<&[u8]> {
        self.raw_chunk(id)
    }

    /// Deprecated alias for [`Self::all_chunks`]; kept for internal callers.
    #[doc(hidden)]
    pub fn find_chunks(&self, id: &[u8; 4]) -> Vec<&[u8]> {
        self.all_chunks(id)
    }

    /// Decode the payload of a paired `*z` (BZZ-compressed) / `*a` (raw) chunk,
    /// e.g. `chunk_payload(b"TXTz", b"TXTa")` for the text layer.
    ///
    /// This is the single seam that owns the BZZ-or-raw decision for every
    /// paired chunk on a page; the per-format parsers receive the returned
    /// already-decoded bytes.  Returns `Ok(None)` when neither chunk is present
    /// (or the present chunk is empty), `Err` only if BZZ decompression fails.
    pub fn chunk_payload(
        &self,
        id_z: &[u8; 4],
        id_a: &[u8; 4],
    ) -> Result<Option<Vec<u8>>, DocError> {
        Ok(decode_paired_payload(
            self.raw_chunk(id_z),
            self.raw_chunk(id_a),
        )?)
    }

    /// Return all BG44 background chunk data slices, in order.
    pub fn bg44_chunks(&self) -> Vec<&[u8]> {
        self.find_chunks(b"BG44")
    }

    /// The render-tier layer cache for this page (decoded on first render).
    ///
    /// The page holds the handle; the decode logic, the cached forms, and the
    /// compositor-subsampling concern all live in
    /// [`crate::djvu_render::PageLayers`].
    #[cfg(feature = "std")]
    pub(crate) fn render_layers(&self) -> &crate::djvu_render::PageLayers {
        let layers = self
            .render_layers
            .get_or_init(crate::djvu_render::PageLayers::new);
        // Stamp the LRU access tick so `enforce_cache_budget` can evict the
        // least-recently-rendered pages first.
        layers.bump_access();
        layers
    }

    /// Approximate resident bytes held by this page's render cache (0 if never
    /// rendered). See [`evict_render_cache`](Self::evict_render_cache).
    #[cfg(feature = "std")]
    pub fn render_cache_bytes(&self) -> usize {
        self.render_layers.get().map_or(0, |l| l.cached_bytes())
    }

    /// The page's LRU access tick (higher = more recently rendered), read
    /// without touching the cache. Used by
    /// [`DjVuDocument::enforce_cache_budget`].
    #[cfg(feature = "std")]
    pub(crate) fn render_cache_access_tick(&self) -> u64 {
        self.render_layers.get().map_or(0, |l| l.access_tick())
    }

    /// Return the fully decoded BG44 wavelet image, decoding and caching on first call.
    ///
    /// Returns `None` if the page has no BG44 chunks or if strict decoding fails.
    /// This method is infallible; callers that need tolerant recovery should use
    /// a permissive render path instead.
    ///
    /// The result is computed once (all ZP arithmetic decode + block assembly) and
    /// then cached in the page's render-tier layer cache.  Subsequent
    /// calls return the cached value immediately.  The wavelet inverse-transform
    /// and YCbCr→RGB conversion are also cached for subsample=1 (the common
    /// full-resolution case) via [`decoded_bg_rgb_s1`](Self::decoded_bg_rgb_s1);
    /// other subsample levels recompute the conversion each call.
    #[cfg(feature = "std")]
    pub fn decoded_bg44(&self) -> Option<&Iw44Image> {
        self.render_layers().bg44(self)
    }

    #[cfg(not(feature = "std"))]
    pub fn decoded_bg44(&self) -> Option<&Iw44Image> {
        None
    }

    /// Return a partially-decoded BG44 background image, decoding and caching
    /// on first call.  Only the first BG44 chunk is decoded — subsequent
    /// refinement chunks are skipped.  This gives roughly 4× lower ZP decode
    /// cost at the expense of coarser quantization, which is imperceptible at
    /// sub=4 (quarter-resolution) or sub=8 output.
    ///
    /// Use this instead of [`Self::decoded_bg44`] when `subsample >= 4`.
    #[cfg(feature = "std")]
    pub fn decoded_bg44_partial(&self) -> Option<&Iw44Image> {
        self.render_layers().bg44_partial(self)
    }

    #[cfg(not(feature = "std"))]
    pub fn decoded_bg44_partial(&self) -> Option<&Iw44Image> {
        None
    }

    /// Return the decoded JB2 shared dictionary, decoding and caching on first call.
    ///
    /// Returns `None` if the page has no shared dictionary (no INCL reference).
    /// The result is computed once and then cached so that repeated renders
    /// do not re-decode the dictionary each time.
    #[cfg(feature = "std")]
    pub(crate) fn decoded_shared_dict(&self) -> Option<&Jb2Dict> {
        // The decode is memoized inside the shared `Arc<SharedDict>`, so all
        // pages that INCL the same DJVI component share one decode per document.
        self.shared_djbz.as_ref()?.get()
    }

    #[cfg(not(feature = "std"))]
    pub(crate) fn decoded_shared_dict(&self) -> Option<&Jb2Dict> {
        None
    }

    /// Return all FG44 foreground chunk data slices, in order.
    pub fn fg44_chunks(&self) -> Vec<&[u8]> {
        self.find_chunks(b"FG44")
    }

    /// Extract the text layer from TXTz (BZZ-compressed) or TXTa (plain) chunks.
    ///
    /// Returns `Ok(None)` if the page has no text layer.
    pub fn text_layer(&self) -> Result<Option<TextLayer>, DocError> {
        Ok(self.text_layer_shared()?.map(|arc| (*arc).clone()))
    }

    /// Shared-handle variant of [`text_layer`](Self::text_layer): the decoded
    /// layer is cached per page (#605), so warm accesses skip the BZZ decode
    /// and zone-tree rebuild and only bump an `Arc`. Prefer this in loops
    /// (search, selection overlays); `text_layer` clones out of the same
    /// cache for callers that need owned data.
    #[cfg(feature = "std")]
    pub fn text_layer_shared(&self) -> Result<Option<std::sync::Arc<TextLayer>>, DocError> {
        self.render_layers().text_layer_cached(|| {
            let page_height = self.info.height as u32;
            match self.chunk_payload(b"TXTz", b"TXTa")? {
                Some(bytes) => Ok(Some(std::sync::Arc::new(crate::text::parse_text_layer(
                    &bytes,
                    page_height,
                )?))),
                None => Ok(None),
            }
        })
    }

    #[cfg(not(feature = "std"))]
    pub fn text_layer_shared(&self) -> Result<Option<alloc::sync::Arc<TextLayer>>, DocError> {
        let page_height = self.info.height as u32;
        match self.chunk_payload(b"TXTz", b"TXTa")? {
            Some(bytes) => Ok(Some(alloc::sync::Arc::new(crate::text::parse_text_layer(
                &bytes,
                page_height,
            )?))),
            None => Ok(None),
        }
    }

    /// Parse the text layer and transform all zone rectangles to match a
    /// rendered page of size `render_w × render_h`.
    ///
    /// This is a convenience wrapper around [`Self::text_layer`] followed by
    /// [`TextLayer::transform`].  It applies the page's own rotation (from the
    /// INFO chunk) and scales coordinates proportionally to the requested
    /// render size, so callers can use the returned rects directly for text
    /// selection / copy-paste overlays without any additional maths.
    ///
    /// Returns `Ok(None)` if the page has no text layer.
    pub fn text_layer_at_size(
        &self,
        render_w: u32,
        render_h: u32,
    ) -> Result<Option<TextLayer>, DocError> {
        let page_w = self.info.width as u32;
        let page_h = self.info.height as u32;
        let rotation = self.info.rotation;
        Ok(self
            .text_layer()?
            .map(|tl| tl.transform(page_w, page_h, rotation, render_w, render_h)))
    }

    /// Extract the plain text content of the page (convenience wrapper).
    ///
    /// Returns `Ok(None)` if the page has no text layer.
    pub fn text(&self) -> Result<Option<String>, DocError> {
        Ok(self.text_layer()?.map(|tl| tl.text))
    }

    /// Parse the annotation layer from ANTz (BZZ-compressed) or ANTa (plain) chunks.
    ///
    /// Returns `Ok(None)` if the page has no annotation chunk.
    pub fn annotations(&self) -> Result<Option<(Annotation, Vec<MapArea>)>, DocError> {
        Ok(self.annotations_shared()?.map(|arc| (*arc).clone()))
    }

    /// Shared-handle variant of [`annotations`](Self::annotations), cached per
    /// page (#605) — warm accesses skip the BZZ decode and parse.
    #[cfg(feature = "std")]
    pub fn annotations_shared(
        &self,
    ) -> Result<Option<crate::djvu_render::SharedAnnotations>, DocError> {
        self.render_layers()
            .annotations_cached(|| match self.chunk_payload(b"ANTz", b"ANTa")? {
                Some(bytes) => Ok(Some(std::sync::Arc::new(
                    crate::annotation::parse_annotations(&bytes)?,
                ))),
                None => Ok(None),
            })
    }

    #[cfg(not(feature = "std"))]
    pub fn annotations_shared(
        &self,
    ) -> Result<Option<alloc::sync::Arc<(Annotation, Vec<MapArea>)>>, DocError> {
        match self.chunk_payload(b"ANTz", b"ANTa")? {
            Some(bytes) => Ok(Some(alloc::sync::Arc::new(
                crate::annotation::parse_annotations(&bytes)?,
            ))),
            None => Ok(None),
        }
    }

    /// Return all hyperlinks (MapAreas with a non-empty URL) on this page.
    pub fn hyperlinks(&self) -> Result<Vec<MapArea>, DocError> {
        match self.annotations()? {
            None => Ok(Vec::new()),
            Some((_, mapareas)) => Ok(mapareas.into_iter().filter(|m| !m.url.is_empty()).collect()),
        }
    }

    /// Decode the JB2 foreground mask as a 1-bit [`Bitmap`](crate::bitmap::Bitmap).
    ///
    /// Returns `Ok(None)` if the page has no Sjbz (JB2 mask) chunk.
    /// Decode the foreground mask layer.
    ///
    /// Handles both JB2 (`Sjbz`) and G4/MMR (`Smmr`) encoded masks.
    /// Returns `Ok(None)` if the page has neither chunk.
    ///
    /// **Performance note:** this method decodes fresh on every call. Prefer
    /// [`decoded_mask`](Self::decoded_mask) in hot paths — it caches the result
    /// after the first call. `extract_mask` remains useful when you need a
    /// uniquely owned `Bitmap` or call it only once.
    pub fn extract_mask(&self) -> Result<Option<crate::bitmap::Bitmap>, DocError> {
        if let Some(sjbz) = self.find_chunk(b"Sjbz") {
            // Prefer an inline Djbz chunk (decoded fresh — rare, usually small).
            // Otherwise use the cached shared dictionary to avoid repeated multi-MB
            // allocations on every render.
            let inline_dict;
            let dict_ref = if let Some(djbz) = self.find_chunk(b"Djbz") {
                inline_dict = crate::jb2::decode_dict(djbz, None)?;
                Some(&inline_dict)
            } else {
                self.decoded_shared_dict()
            };
            let bm = crate::jb2::decode(sjbz, dict_ref)?;
            return Ok(Some(bm));
        }
        if let Some(smmr) = self.find_chunk(b"Smmr") {
            let bm = crate::smmr::decode_smmr(smmr).map_err(|e| DocError::Smmr(e.to_string()))?;
            return Ok(Some(bm));
        }
        Ok(None)
    }

    /// Decode the foreground mask with per-pixel blit index tracking.
    ///
    /// Falls back to a plain `Smmr` mask (without blit indices) when only an
    /// `Smmr` chunk is present; in that case all blit indices are set to `0`.
    /// Returns `Ok(None)` if the page has neither chunk.
    pub fn extract_mask_indexed(
        &self,
    ) -> Result<Option<(crate::bitmap::Bitmap, Vec<i32>)>, DocError> {
        if let Some(sjbz) = self.find_chunk(b"Sjbz") {
            let inline_dict;
            let dict_ref = if let Some(djbz) = self.find_chunk(b"Djbz") {
                inline_dict = crate::jb2::decode_dict(djbz, None)?;
                Some(&inline_dict)
            } else {
                self.decoded_shared_dict()
            };
            let (bm, blit_map) = crate::jb2::decode_indexed(sjbz, dict_ref)?;
            return Ok(Some((bm, blit_map)));
        }
        if let Some(smmr) = self.find_chunk(b"Smmr") {
            let bm = crate::smmr::decode_smmr(smmr).map_err(|e| DocError::Smmr(e.to_string()))?;
            let len = (bm.width * bm.height) as usize;
            return Ok(Some((bm, vec![0i32; len])));
        }
        Ok(None)
    }

    /// Decode the IW44 foreground layer (FG44 chunks) if present.
    ///
    /// Returns `Ok(None)` if the page has no FG44 chunks.
    ///
    /// **Performance note:** this method allocates a fresh `Pixmap` on every call.
    /// Prefer [`decoded_fg44`](Self::decoded_fg44) in hot paths — it returns a
    /// cached reference after the first call.
    pub fn extract_foreground(&self) -> Result<Option<Pixmap>, DocError> {
        let chunks = self.fg44_chunks();
        if chunks.is_empty() {
            return Ok(None);
        }

        let mut img = Iw44Image::new();
        for chunk_data in &chunks {
            img.decode_chunk(chunk_data)?;
        }
        let pixmap = img.to_rgb()?;
        Ok(Some(pixmap))
    }

    /// Return the decoded JB2 mask (Sjbz), decoding and caching on first call.
    ///
    /// Unlike [`Self::extract_mask`] this method caches the result (in the
    /// page's [`crate::djvu_render::PageLayers`]) so that repeated renders of
    /// the same page — e.g. at different DPI levels — do not re-run the ZP
    /// arithmetic + symbol decode.
    ///
    /// Returns `None` if the page has no Sjbz chunk or if decoding fails.
    #[cfg(feature = "std")]
    pub fn decoded_mask(&self) -> Option<&crate::bitmap::Bitmap> {
        self.render_layers().mask(self)
    }

    #[cfg(not(feature = "std"))]
    pub fn decoded_mask(&self) -> Option<&crate::bitmap::Bitmap> {
        None
    }

    /// Return the decoded FG44 foreground color layer, decoding and caching on
    /// first call.  Subsequent renders reuse the cached `Pixmap`.
    ///
    /// Returns `None` if the page has no FG44 chunks or if decoding fails.
    #[cfg(feature = "std")]
    pub fn decoded_fg44(&self) -> Option<&Pixmap> {
        self.render_layers().fg44(self)
    }

    #[cfg(not(feature = "std"))]
    pub fn decoded_fg44(&self) -> Option<&Pixmap> {
        None
    }

    /// Return the full-resolution (subsample=1) RGB `Pixmap` derived from the
    /// BG44 wavelet background, decoding and caching on first call.
    ///
    /// This caches both the ZP arithmetic decode (via [`decoded_bg44`](Self::decoded_bg44))
    /// and the IW44 inverse-transform + YCbCr→RGB conversion, so repeated
    /// renders at native resolution pay neither cost after the first call.
    ///
    /// Returns `None` if the page has no BG44 layer or if decoding fails.
    #[cfg(feature = "std")]
    pub(crate) fn decoded_bg_rgb_s1(&self) -> Option<&Pixmap> {
        self.render_layers().bg_rgb_s1(self)
    }

    #[cfg(not(feature = "std"))]
    pub(crate) fn decoded_bg_rgb_s1(&self) -> Option<&Pixmap> {
        None
    }

    /// Return the half-resolution (subsample=2) RGB `Pixmap` derived from the
    /// BG44 wavelet background, decoding and caching on first call.
    ///
    /// Mirrors [`decoded_bg_rgb_s1`](Self::decoded_bg_rgb_s1) for the common
    /// 150-from-300-DPI render: caches both the ZP arithmetic decode and the
    /// IW44 inverse-transform + YCbCr→RGB conversion at subsample 2.
    ///
    /// Returns `None` if the page has no BG44 layer or if decoding fails.
    #[cfg(feature = "std")]
    pub(crate) fn decoded_bg_rgb_s2(&self) -> Option<&Pixmap> {
        self.render_layers().bg_rgb_s2(self)
    }

    #[cfg(not(feature = "std"))]
    pub(crate) fn decoded_bg_rgb_s2(&self) -> Option<&Pixmap> {
        None
    }

    /// Return the quarter-resolution (subsample=4) RGB `Pixmap` derived from the
    /// partial BG44 wavelet background, decoding and caching on first call.
    ///
    /// Mirrors [`decoded_bg_rgb_s2`](Self::decoded_bg_rgb_s2) for the common
    /// heavy-downscale / thumbnail render (e.g. 150-from-400-DPI): caches both
    /// the ZP arithmetic decode (first chunk only) and the IW44 inverse-transform
    /// + YCbCr→RGB conversion at subsample 4.
    ///
    /// Returns `None` if the page has no BG44 layer or if decoding fails.
    #[cfg(feature = "std")]
    pub(crate) fn decoded_bg_rgb_s4(&self) -> Option<&Pixmap> {
        self.render_layers().bg_rgb_s4(self)
    }

    #[cfg(not(feature = "std"))]
    pub(crate) fn decoded_bg_rgb_s4(&self) -> Option<&Pixmap> {
        None
    }

    /// Return the decoded JB2 mask + per-pixel blit-index map for FGbz-palette
    /// pages, decoding and caching on first call.
    ///
    /// Caches both the JB2 ZP arithmetic decode and the page-sized blit map so
    /// that repeated palette renders pay neither cost after the first call.
    /// Returns `None` if the page has no Sjbz/Smmr chunk or decoding fails.
    #[cfg(feature = "std")]
    pub(crate) fn decoded_mask_indexed(&self) -> Option<&(crate::bitmap::Bitmap, Vec<i32>)> {
        self.render_layers().mask_indexed(self)
    }

    #[cfg(not(feature = "std"))]
    pub(crate) fn decoded_mask_indexed(&self) -> Option<&(crate::bitmap::Bitmap, Vec<i32>)> {
        None
    }

    /// Decode the IW44 background layer (BG44 chunks) if present.
    ///
    /// Returns `Ok(None)` if the page has no BG44 chunks.
    ///
    /// **Performance note:** this method allocates a fresh `Pixmap` on every call.
    /// Prefer [`decoded_bg44`](Self::decoded_bg44) in hot paths — it returns a
    /// cached reference after the first call.
    pub fn extract_background(&self) -> Result<Option<Pixmap>, DocError> {
        let chunks = self.bg44_chunks();
        if chunks.is_empty() {
            return Ok(None);
        }

        let mut img = Iw44Image::new();
        for chunk_data in &chunks {
            img.decode_chunk(chunk_data)?;
        }
        let pixmap = img.to_rgb()?;
        Ok(Some(pixmap))
    }

    /// Render this page into a pre-allocated RGBA buffer using the given options.
    ///
    /// This is the zero-allocation render path: no heap allocation occurs when
    /// `buf` is already sized to `opts.width * opts.height * 4` bytes.
    ///
    /// # Errors
    ///
    /// - [`crate::djvu_render::RenderError::BufTooSmall`] if buffer is too small
    /// - [`crate::djvu_render::RenderError::InvalidDimensions`] if width/height is 0
    /// - Propagates IW44 / JB2 decode errors
    pub fn render_into(
        &self,
        opts: &crate::djvu_render::RenderOptions,
        buf: &mut [u8],
    ) -> Result<(), crate::djvu_render::RenderError> {
        crate::djvu_render::render_into(self, opts, buf)
    }
}

// ---- Document ---------------------------------------------------------------

/// Options for [`DjVuDocument::enforce_cache_budget_with`].
///
/// Default (`downgrade_before_drop: false`) is byte-identical to
/// [`DjVuDocument::enforce_cache_budget`]'s all-or-nothing eviction.
#[cfg(feature = "std")]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheBudgetOptions {
    /// C5_COMPRESS's cheaper middle tier: when true, a least-recently-used
    /// page over budget is first [`DjVuPage::downgrade_render_cache`]d —
    /// keeping its already-cached downscaled RGB pixmap
    /// (`bg_rgb_s2`/`bg_rgb_s4`) alive while dropping the expensive
    /// full-resolution derivations — rather than fully dropped. If the
    /// document is still over budget after downgrading every eligible page
    /// (or a page has nothing left to downgrade), the sweep falls back to a
    /// full drop, LRU-first, exactly like `enforce_cache_budget`. So the byte
    /// ceiling is honoured identically either way; only the *shape* of what
    /// stays cached changes.
    pub downgrade_before_drop: bool,
}

/// An opened DjVu document.
///
/// Supports single-page FORM:DJVU, bundled multi-page FORM:DJVM, and indirect
/// multi-page FORM:DJVM (via resolver callback).
#[derive(Debug)]
pub struct DjVuDocument {
    /// All pages, indexed by 0-based page number.
    pages: Vec<DjVuPage>,
    /// Parsed NAVM bookmarks, or empty if none.
    bookmarks: Vec<DjVuBookmark>,
    /// Raw document-level chunks (NAVM, DIRM, etc.) from the DJVM container,
    /// or from the top-level DJVU form for single-page documents.
    global_chunks: Vec<RawChunk>,
    /// Byte ranges of each page's outer FORM chunk inside the original
    /// document buffer, in page order. Populated only for bundled DJVM
    /// documents parsed from a contiguous slice; empty otherwise (single-page
    /// DJVU, indirect DJVM, or when offsets were unavailable).
    ///
    /// Used by [`DjVuDocument::page_byte_range`] (#196 Phase 2). Lets a
    /// future HTTP-Range fetcher (#196 Phase 3) request exactly the bytes
    /// for a given page.
    page_byte_ranges: Vec<core::ops::Range<u64>>,
}

impl DjVuDocument {
    /// Parse a DjVu document from a byte slice.
    ///
    /// For indirect documents (INCL references to external files), a resolver
    /// must be supplied via [`DjVuDocument::parse_with_resolver`].
    ///
    /// # Errors
    ///
    /// Returns `DocError::NoResolver` if the document is indirect and no resolver
    /// was provided.
    pub fn parse(data: &[u8]) -> Result<Self, DocError> {
        Self::parse_with_resolver(data, None::<fn(&str) -> Result<Vec<u8>, DocError>>)
    }

    /// Parse from an owned, shared backing store (an owned `Vec<u8>` or an
    /// `Mmap`), constructing **lazy** pages for bundled DJVM documents.
    ///
    /// For a bundled document only the cheap per-page `INFO` header is parsed up
    /// front; each page's chunk bytes are materialised from `backing` on first
    /// access instead of being copied at open time (LAZY_PAGE_CONSTRUCT). This
    /// makes "open a 500-page book, render page 1" O(1) in copies rather than
    /// O(total document bytes). For `mmap` backings the copy is avoided entirely
    /// until a page is touched.
    ///
    /// Single-page, non-DJVM, and indirect documents fall back to the eager
    /// [`parse`](Self::parse) path (they are small or need a resolver), so this
    /// is safe to call for any input. Keep the bundled loop below in sync with
    /// the eager one in [`parse_with_resolver`](Self::parse_with_resolver).
    #[cfg(feature = "std")]
    pub(crate) fn parse_backed(backing: Backing) -> Result<Self, DocError> {
        let data = backing_bytes(&backing);
        let form = parse_form(data)?;
        if &form.form_type != b"DJVM" {
            return Self::parse(data);
        }
        let Some(dirm_chunk) = form.chunks.iter().find(|c| &c.id == b"DIRM") else {
            return Self::parse(data);
        };
        let payload = DirmPayload::decode(dirm_chunk.data).map_err(DocError::Malformed)?;
        if !payload.is_bundled() {
            // Indirect: needs a resolver — defer to the eager path (which errors
            // consistently with the previous behaviour).
            return Self::parse(data);
        }

        let entries = payload.components();
        let comp_offsets = &payload.offsets;
        let bookmarks = parse_navm_bookmarks(&form.chunks)?;
        let global_chunks: Vec<RawChunk> = form
            .chunks
            .iter()
            .filter(|c| &c.id != b"FORM")
            .map(|c| RawChunk {
                id: c.id,
                data: c.data.to_vec(),
            })
            .collect();

        let sub_forms: Vec<&IffChunk<'_>> =
            form.chunks.iter().filter(|c| &c.id == b"FORM").collect();

        use std::collections::BTreeMap;
        let djvi_djbz: BTreeMap<String, Arc<SharedDict>> = entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.kind == DirmComponentKind::Shared)
            .filter_map(|(comp_idx, entry)| {
                let sf = sub_forms.get(comp_idx)?;
                let chunks = parse_sub_form(sf.data).ok()?;
                let djbz = chunks.iter().find(|c| &c.id == b"Djbz")?;
                Some((
                    entry.id.clone(),
                    Arc::new(SharedDict::new(djbz.data.to_vec())),
                ))
            })
            .collect();

        let base = data.as_ptr() as usize;
        let mut pages = Vec::new();
        let mut page_byte_ranges = Vec::new();
        let mut page_idx = 0usize;
        for (comp_idx, entry) in entries.iter().enumerate() {
            if entry.kind != DirmComponentKind::Page {
                continue;
            }
            let sub_form = sub_forms.get(comp_idx).ok_or(DocError::Malformed(
                "DIRM entry count exceeds FORM children",
            ))?;
            let sub_chunks = parse_sub_form(sub_form.data)?;
            let shared_djbz = sub_chunks
                .iter()
                .find(|c| &c.id == b"INCL")
                .and_then(|incl| core::str::from_utf8(incl.data.trim_ascii_end()).ok())
                .and_then(|name| djvi_djbz.get(name))
                .cloned();

            // The page's FORM sub-form is a slice of `data`, which is `backing`'s
            // bytes — so its offset within `backing` lets the lazy store re-slice
            // and parse it on demand.
            let off = sub_form.data.as_ptr() as usize - base;
            let range = off..off + sub_form.data.len();
            let page = parse_page_lazy(&sub_chunks, page_idx, shared_djbz, backing.clone(), range)?;
            pages.push(page);

            if let Some(off2) = comp_offsets.get(comp_idx) {
                let start = *off2 as usize;
                if let Some(size_bytes) = data.get(start + 4..start + 8) {
                    let size_be = [size_bytes[0], size_bytes[1], size_bytes[2], size_bytes[3]];
                    page_byte_ranges.push(crate::dirm::form_byte_range(*off2, size_be));
                }
            }
            page_idx += 1;
        }
        if page_byte_ranges.len() != pages.len() {
            page_byte_ranges.clear();
        }

        Ok(DjVuDocument {
            pages,
            bookmarks,
            global_chunks,
            page_byte_ranges,
        })
    }

    /// Parse a DjVu document with an optional resolver for indirect pages.
    ///
    /// The resolver receives the `name` field from each INCL chunk and must
    /// return the raw bytes of that external component file.
    pub fn parse_with_resolver<R>(data: &[u8], resolver: Option<R>) -> Result<Self, DocError>
    where
        R: Fn(&str) -> Result<Vec<u8>, DocError>,
    {
        let form = parse_form(data)?;

        match &form.form_type {
            b"DJVU" => {
                // Single-page document — expose all top-level chunks as global
                let global_chunks: Vec<RawChunk> = form
                    .chunks
                    .iter()
                    .map(|c| RawChunk {
                        id: c.id,
                        data: c.data.to_vec(),
                    })
                    .collect();
                let page = parse_page_from_chunks(&form.chunks, 0, None)?;
                // Single-page document spans the entire buffer.
                #[allow(clippy::single_range_in_vec_init)]
                let page_byte_ranges = vec![0u64..(data.len() as u64)];
                Ok(DjVuDocument {
                    pages: vec![page],
                    bookmarks: vec![],
                    global_chunks,
                    page_byte_ranges,
                })
            }
            b"DJVM" => {
                // Multi-page document — parse DIRM first
                let dirm_chunk = form
                    .chunks
                    .iter()
                    .find(|c| &c.id == b"DIRM")
                    .ok_or(DocError::MissingChunk("DIRM"))?;

                let payload = DirmPayload::decode(dirm_chunk.data).map_err(DocError::Malformed)?;
                let entries = payload.components();
                let is_bundled = payload.is_bundled();
                let comp_offsets = payload.offsets;

                // Collect NAVM bookmarks (BZZ-compressed)
                let bookmarks = parse_navm_bookmarks(&form.chunks)?;

                // Store non-FORM global chunks (DIRM, NAVM, etc.)
                let global_chunks: Vec<RawChunk> = form
                    .chunks
                    .iter()
                    .filter(|c| &c.id != b"FORM")
                    .map(|c| RawChunk {
                        id: c.id,
                        data: c.data.to_vec(),
                    })
                    .collect();

                if is_bundled {
                    // Bundled: FORM:DJVU / FORM:DJVI sub-forms follow DIRM in sequence.
                    let sub_forms: Vec<&IffChunk<'_>> =
                        form.chunks.iter().filter(|c| &c.id == b"FORM").collect();

                    // Build a map of DJVI component ID → raw Djbz bytes for
                    // shared symbol dictionaries (referenced via INCL chunks).
                    // Use BTreeMap so this compiles in no_std (alloc::collections::BTreeMap
                    // is available; std::collections::HashMap is not).
                    #[cfg(not(feature = "std"))]
                    use alloc::collections::BTreeMap;
                    #[cfg(feature = "std")]
                    use std::collections::BTreeMap;
                    // Wrap shared dict bytes in Arc (std) so all pages that
                    // reference the same DJVI component share one allocation.
                    #[cfg(feature = "std")]
                    let djvi_djbz: BTreeMap<String, Arc<SharedDict>> = entries
                        .iter()
                        .enumerate()
                        .filter(|(_, e)| e.kind == DirmComponentKind::Shared)
                        .filter_map(|(comp_idx, entry)| {
                            let sf = sub_forms.get(comp_idx)?;
                            let chunks = parse_sub_form(sf.data).ok()?;
                            let djbz = chunks.iter().find(|c| &c.id == b"Djbz")?;
                            Some((
                                entry.id.clone(),
                                Arc::new(SharedDict::new(djbz.data.to_vec())),
                            ))
                        })
                        .collect();
                    #[cfg(not(feature = "std"))]
                    let djvi_djbz: BTreeMap<String, Vec<u8>> = entries
                        .iter()
                        .enumerate()
                        .filter(|(_, e)| e.kind == DirmComponentKind::Shared)
                        .filter_map(|(comp_idx, entry)| {
                            let sf = sub_forms.get(comp_idx)?;
                            let chunks = parse_sub_form(sf.data).ok()?;
                            let djbz = chunks.iter().find(|c| &c.id == b"Djbz")?;
                            Some((entry.id.clone(), djbz.data.to_vec()))
                        })
                        .collect();

                    let mut pages = Vec::new();
                    let mut page_byte_ranges = Vec::new();
                    let mut page_idx = 0usize;
                    for (comp_idx, entry) in entries.iter().enumerate() {
                        if entry.kind != DirmComponentKind::Page {
                            continue;
                        }
                        let sub_form = sub_forms.get(comp_idx).ok_or(DocError::Malformed(
                            "DIRM entry count exceeds FORM children",
                        ))?;
                        let sub_chunks = parse_sub_form(sub_form.data)?;

                        // Resolve the page's INCL references to a shared DJVI
                        // dictionary. A page may include several components
                        // (e.g. a shared-annotation DJVI *and* the symbol
                        // dictionary — czech.djvu carries three INCLs, #624),
                        // so scan them all and take the first whose target
                        // actually holds a Djbz.
                        let shared_djbz = sub_chunks
                            .iter()
                            .filter(|c| &c.id == b"INCL")
                            .filter_map(|incl| {
                                core::str::from_utf8(incl.data.trim_ascii_end()).ok()
                            })
                            .find_map(|name| djvi_djbz.get(name))
                            .cloned();

                        let page = parse_page_from_chunks(&sub_chunks, page_idx, shared_djbz)?;
                        pages.push(page);

                        // Record the byte range of this page's outer FORM. The
                        // offset→range arithmetic lives in `dirm::form_byte_range`;
                        // here we just supply the four size bytes from the in-memory
                        // FORM header.
                        if let Some(off) = comp_offsets.get(comp_idx) {
                            let start = *off as usize;
                            if let Some(size_bytes) = data.get(start + 4..start + 8) {
                                let size_be =
                                    [size_bytes[0], size_bytes[1], size_bytes[2], size_bytes[3]];
                                page_byte_ranges.push(crate::dirm::form_byte_range(*off, size_be));
                            }
                        }
                        page_idx += 1;
                    }

                    // Only expose offsets if we got one for every page; partial
                    // tables would surprise callers iterating by page index.
                    if page_byte_ranges.len() != pages.len() {
                        page_byte_ranges.clear();
                    }

                    Ok(DjVuDocument {
                        pages,
                        bookmarks,
                        global_chunks,
                        page_byte_ranges,
                    })
                } else {
                    // Indirect: pages must be resolved by name
                    let resolver = resolver.ok_or(DocError::NoResolver)?;

                    let mut pages = Vec::new();
                    let mut page_idx = 0usize;
                    for entry in &entries {
                        if entry.kind != DirmComponentKind::Page {
                            continue;
                        }
                        let resolved_data = resolver(&entry.id)
                            .map_err(|_| DocError::IndirectResolve(entry.id.clone()))?;
                        let sub_form = parse_form(&resolved_data)?;
                        let page = parse_page_from_chunks(&sub_form.chunks, page_idx, None)?;
                        pages.push(page);
                        page_idx += 1;
                    }

                    Ok(DjVuDocument {
                        pages,
                        bookmarks,
                        global_chunks,
                        // Indirect: per-page bytes live in external files, not the
                        // index buffer — no meaningful range to expose here.
                        page_byte_ranges: Vec::new(),
                    })
                }
            }
            other => Err(DocError::NotDjVu(*other)),
        }
    }

    #[cfg(all(feature = "std", feature = "async"))]
    pub(crate) fn parse_single_page_with_shared(
        data: &[u8],
        index: usize,
        shared_djbz: Option<Arc<SharedDict>>,
    ) -> Result<DjVuPage, DocError> {
        let form = parse_form(data)?;
        if form.form_type != *b"DJVU" {
            return Err(DocError::NotDjVu(form.form_type));
        }
        parse_page_from_chunks(&form.chunks, index, shared_djbz)
    }

    /// Number of pages.
    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// Byte range of `page`'s outer FORM chunk inside the original document
    /// buffer (#196 Phase 2).
    ///
    /// Returns `Some(start..end)` where `start` is the absolute offset of the
    /// 4-byte `FORM` magic and `end` is one past the last byte of the chunk
    /// payload. The range is suitable for an HTTP `Range:` request that
    /// fetches exactly the bytes needed to decode that page (assuming any
    /// referenced shared `DJVI` dictionaries are already in hand — those
    /// have their own ranges too, but `page_byte_range` only covers pages).
    ///
    /// Returns `None` for:
    /// - `index >= page_count()`
    /// - Indirect DJVM documents (per-page bytes live in external files)
    /// - Bundled DJVM documents whose DIRM offset table couldn't be matched
    ///   to every page
    ///
    /// Single-page DJVU documents always return the full buffer range.
    pub fn page_byte_range(&self, index: usize) -> Option<core::ops::Range<u64>> {
        self.page_byte_ranges.get(index).cloned()
    }

    /// Speculatively decode page `index`'s render layers (background, mask,
    /// foreground) on a background thread pool, so that a **later**,
    /// synchronous [`crate::djvu_render::render_pixmap`] call at native
    /// resolution finds the caches already warm (the B7 next-page prefetch
    /// lever — e.g. call `doc.prefetch_page(k + 1)` right after rendering
    /// page `k`, while the reader is still looking at it).
    ///
    /// Requires an `Arc<DjVuDocument>` so the spawned task can outlive this
    /// call — the background closure holds its own clone of the `Arc` and
    /// writes into the *same* page's existing `OnceLock`-backed
    /// [`crate::djvu_render::PageLayers`] cache, so there is no separate
    /// "prefetch buffer" to race against: whichever caller (the background
    /// task or a later foreground render) reaches `get_or_init` first does
    /// the decode, the other observes the cached result. Out-of-range
    /// `index` is a no-op. This is a hint, not a guarantee — if the
    /// background task hasn't finished by the time the page is rendered, the
    /// caller still gets correct output, just without the latency win.
    ///
    /// Requires the `parallel` feature (spawns onto the shared rayon pool).
    #[cfg(all(feature = "std", feature = "parallel"))]
    pub fn prefetch_page(self: &Arc<Self>, index: usize) {
        if index >= self.page_count() {
            return;
        }
        let doc = Arc::clone(self);
        rayon::spawn(move || {
            let Ok(page) = doc.page(index) else {
                return;
            };
            // Mirrors the common full-resolution render path's dependency
            // chain: mask/fg44 are independent of bg; bg_rgb_s1 subsumes the
            // bg44 ZP arithmetic decode (see `PageLayers::bg_rgb_s1`), so this
            // warms every cache slot a native-resolution `render_pixmap` call
            // reads from.
            let _ = page.decoded_mask();
            let _ = page.decoded_fg44();
            let _ = page.decoded_bg_rgb_s1();
        });
    }

    /// Access a page by 0-based index.
    ///
    /// # Errors
    ///
    /// Returns `DocError::PageOutOfRange` if `index >= page_count()`.
    pub fn page(&self, index: usize) -> Result<&DjVuPage, DocError> {
        self.pages.get(index).ok_or(DocError::PageOutOfRange {
            index,
            count: self.pages.len(),
        })
    }

    /// Drop every page's render-tier decode cache, reclaiming all per-page
    /// render memory in one call.
    ///
    /// See [`DjVuPage::evict_render_cache`]: rendered pages memoise their decoded
    /// layers for the document's lifetime, so peak RSS grows linearly with pages
    /// rendered. This frees all of it (each page rebuilds lazily on next render).
    #[cfg(feature = "std")]
    pub fn evict_render_caches(&mut self) {
        for p in &mut self.pages {
            p.evict_render_cache();
        }
    }

    /// Drop the render cache of every page **except** those whose index is in
    /// `keep`, bounding memory to a working set (e.g. the visible pages plus a
    /// small prefetch window) in a long-lived viewer.
    #[cfg(feature = "std")]
    pub fn retain_render_caches(&mut self, keep: &[usize]) {
        for (i, p) in self.pages.iter_mut().enumerate() {
            if !keep.contains(&i) {
                p.evict_render_cache();
            }
        }
    }

    /// Approximate total resident bytes held by all pages' render caches.
    ///
    /// Sum of [`DjVuPage::render_cache_bytes`]; use it to decide when to call
    /// [`enforce_cache_budget`](Self::enforce_cache_budget).
    #[cfg(feature = "std")]
    pub fn render_cache_bytes(&self) -> usize {
        self.pages.iter().map(|p| p.render_cache_bytes()).sum()
    }

    /// Evict least-recently-rendered pages' caches until the total render-cache
    /// memory is at most `max_bytes`, never evicting a page whose index is in
    /// `protect`. Returns the bytes freed.
    ///
    /// This is the automatic form of [`retain_render_caches`](Self::retain_render_caches):
    /// instead of naming exactly which pages to keep, the caller sets a memory
    /// ceiling and a small protected working set (e.g. the visible pages), and
    /// the least-recently-used cached pages are dropped first (via the per-page
    /// LRU access tick stamped on every render). A viewer can call it after each
    /// page render to hold memory near a fixed budget. No-op (returns 0) when
    /// already under budget. Evicted caches rebuild lazily and identically.
    #[cfg(feature = "std")]
    pub fn enforce_cache_budget(&mut self, max_bytes: usize, protect: &[usize]) -> usize {
        let mut total = self.render_cache_bytes();
        if total <= max_bytes {
            return 0;
        }
        // Evictable pages (cached, not protected), least-recently-used first.
        let mut cands: Vec<(usize, u64, usize)> = self
            .pages
            .iter()
            .enumerate()
            .filter(|(i, p)| !protect.contains(i) && p.render_cache_bytes() > 0)
            .map(|(i, p)| (i, p.render_cache_access_tick(), p.render_cache_bytes()))
            .collect();
        cands.sort_by_key(|&(_, tick, _)| tick);

        let mut freed = 0usize;
        for (i, _, bytes) in cands {
            if total <= max_bytes {
                break;
            }
            self.pages[i].evict_render_cache();
            freed += bytes;
            total = total.saturating_sub(bytes);
        }
        freed
    }

    /// C5_COMPRESS: like [`downgrade_render_caches`](Self::downgrade_render_caches)
    /// applied to every page — downgrade instead of drop.
    #[cfg(feature = "std")]
    pub fn downgrade_render_caches(&mut self) {
        for p in &mut self.pages {
            p.downgrade_render_cache();
        }
    }

    /// Like [`enforce_cache_budget`](Self::enforce_cache_budget), but taking
    /// [`CacheBudgetOptions`] to opt into the C5_COMPRESS downgrade-before-drop
    /// tier. Returns the bytes freed (net of any bytes still held by
    /// downgraded — not fully dropped — pages).
    #[cfg(feature = "std")]
    pub fn enforce_cache_budget_with(
        &mut self,
        max_bytes: usize,
        protect: &[usize],
        opts: CacheBudgetOptions,
    ) -> usize {
        if !opts.downgrade_before_drop {
            return self.enforce_cache_budget(max_bytes, protect);
        }
        let mut total = self.render_cache_bytes();
        if total <= max_bytes {
            return 0;
        }
        let starting_total = total;

        // Pass 1: downgrade LRU-first (cheap tier) until under budget or no
        // eligible candidates remain.
        let mut cands: Vec<(usize, u64, usize)> = self
            .pages
            .iter()
            .enumerate()
            .filter(|(i, p)| !protect.contains(i) && p.render_cache_bytes() > 0)
            .map(|(i, p)| (i, p.render_cache_access_tick(), p.render_cache_bytes()))
            .collect();
        cands.sort_by_key(|&(_, tick, _)| tick);

        for &(i, _, before) in &cands {
            if total <= max_bytes {
                break;
            }
            self.pages[i].downgrade_render_cache();
            let after = self.pages[i].render_cache_bytes();
            total = total.saturating_sub(before.saturating_sub(after));
        }

        // Pass 2: still over budget (downgrading wasn't enough, e.g. many
        // small pages or nothing left to shrink) — fall back to full drops,
        // LRU-first, same as `enforce_cache_budget`.
        if total > max_bytes {
            let mut cands2: Vec<(usize, u64, usize)> = self
                .pages
                .iter()
                .enumerate()
                .filter(|(i, p)| !protect.contains(i) && p.render_cache_bytes() > 0)
                .map(|(i, p)| (i, p.render_cache_access_tick(), p.render_cache_bytes()))
                .collect();
            cands2.sort_by_key(|&(_, tick, _)| tick);

            for (i, _, bytes) in cands2 {
                if total <= max_bytes {
                    break;
                }
                self.pages[i].evict_render_cache();
                total = total.saturating_sub(bytes);
            }
        }

        starting_total.saturating_sub(total)
    }

    /// The NAVM table of contents, or an empty slice if not present.
    pub fn bookmarks(&self) -> &[DjVuBookmark] {
        &self.bookmarks
    }

    /// Parse document-level metadata from a METz (BZZ-compressed) or METa
    /// (plain text) chunk.
    ///
    /// Returns `Ok(None)` if no METa/METz chunk is present.
    pub fn metadata(&self) -> Result<Option<DjVuMetadata>, DocError> {
        match self.chunk_payload(b"METz", b"METa")? {
            Some(bytes) => Ok(Some(crate::metadata::parse_metadata(&bytes)?)),
            None => Ok(None),
        }
    }

    /// Return the raw bytes of the first document-level chunk with the given
    /// 4-byte ID.
    ///
    /// For single-page DJVU files this covers all top-level chunks (INFO,
    /// Sjbz, BG44, …).  For multi-page DJVM files this covers non-page chunks
    /// such as DIRM and NAVM.  Per-page chunks are accessed via
    /// [`DjVuPage::raw_chunk`].
    ///
    /// Returns `None` if no such chunk exists.
    pub fn raw_chunk(&self, id: &[u8; 4]) -> Option<&[u8]> {
        self.global_chunks
            .iter()
            .find(|c| &c.id == id)
            .map(|c| c.data.as_slice())
    }

    /// Return the raw bytes of all document-level chunks with the given ID.
    ///
    /// Returns an empty `Vec` if no such chunk exists.
    pub fn all_chunks(&self, id: &[u8; 4]) -> Vec<&[u8]> {
        self.global_chunks
            .iter()
            .filter(|c| &c.id == id)
            .map(|c| c.data.as_slice())
            .collect()
    }

    /// Return the IDs of all document-level chunks, in order.
    ///
    /// For multi-page DJVM files this is the sequence of non-page chunks
    /// (DIRM, NAVM, …).  Duplicate IDs appear once per chunk.
    pub fn chunk_ids(&self) -> Vec<[u8; 4]> {
        self.global_chunks.iter().map(|c| c.id).collect()
    }

    /// Decode the payload of a paired `*z` (BZZ-compressed) / `*a` (raw)
    /// document-level chunk, e.g. `chunk_payload(b"METz", b"METa")` for
    /// document metadata.
    ///
    /// The document-level counterpart of [`DjVuPage::chunk_payload`]; it owns
    /// the BZZ-or-raw decision once so the format parsers stay pure.
    pub fn chunk_payload(
        &self,
        id_z: &[u8; 4],
        id_a: &[u8; 4],
    ) -> Result<Option<Vec<u8>>, DocError> {
        Ok(decode_paired_payload(
            self.raw_chunk(id_z),
            self.raw_chunk(id_a),
        )?)
    }

    /// Parse an indirect DjVu document from bytes, resolving component files
    /// relative to `base_dir`.
    ///
    /// For bundled documents this is equivalent to [`DjVuDocument::parse`].
    /// For indirect documents, component names from the DIRM are resolved as
    /// paths under `base_dir`, and each referenced file is read from disk.
    ///
    /// # Errors
    ///
    /// Returns `DocError::Io` if a component file cannot be read, or any parse
    /// error from the component data.
    #[cfg(feature = "std")]
    pub fn parse_from_dir(
        data: &[u8],
        base_dir: impl AsRef<std::path::Path>,
    ) -> Result<Self, DocError> {
        let base = base_dir.as_ref().to_path_buf();
        let resolver = move |name: &str| -> Result<Vec<u8>, DocError> {
            // Strip any "file://" prefix
            let name = name.strip_prefix("file://").unwrap_or(name);
            let path = if std::path::Path::new(name).is_absolute() {
                std::path::PathBuf::from(name)
            } else {
                base.join(name)
            };
            std::fs::read(&path).map_err(|_| DocError::IndirectResolve(name.to_string()))
        };
        Self::parse_with_resolver(data, Some(resolver))
    }
}

// ---- Memory-mapped document -------------------------------------------------

/// A DjVu document backed by a memory-mapped file.
///
/// Instead of copying the entire file into a `Vec<u8>`, this type maps the file
/// into the process address space using the OS virtual-memory subsystem.  The
/// kernel pages data from disk on demand, which can significantly reduce peak
/// memory usage for large multi-volume scans (100+ MB).
///
/// # Safety contract
///
/// **The underlying file must not be modified or truncated while the mapping is
/// alive.**  Mutating a memory-mapped file is undefined behaviour on most
/// platforms (SIGBUS on Linux/macOS, access violation on Windows).  The caller
/// is responsible for ensuring file immutability for the lifetime of this
/// struct.
///
/// Requires the `mmap` feature flag.
#[cfg(feature = "mmap")]
pub struct MmapDocument {
    /// The memory mapping, wrapped in the shared [`Backing`] type and kept alive
    /// for the document's lifetime. For bundled documents the parsed pages are
    /// **lazy** and read their chunk bytes directly from this mapping on demand
    /// (zero-copy open); for single-page / indirect documents the pages own copies
    /// and this simply outlives the parse. Held via the same `Arc` the pages
    /// clone, so the mapping cannot be dropped while a lazy page still needs it.
    _backing: Backing,
    /// The same mapping, kept as its concrete type (an extra `Arc` clone of
    /// the identical allocation `_backing` erases) so
    /// [`MmapDocument::advise_page_willneed`] can call `memmap2::Mmap::advise_range`
    /// directly — the type-erased `Backing` alias can't expose that method.
    mmap: Arc<memmap2::Mmap>,
    doc: DjVuDocument,
}

#[cfg(feature = "mmap")]
impl MmapDocument {
    /// Open a DjVu file via memory-mapped I/O.
    ///
    /// # Safety contract
    ///
    /// The file at `path` **must not be modified or truncated** while the
    /// returned `MmapDocument` is alive.  See the struct-level documentation
    /// for details.
    ///
    /// # Errors
    ///
    /// Returns `DocError::Io` if the file cannot be opened or mapped, or any
    /// parse error from [`DjVuDocument::parse`].
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self, DocError> {
        let file = std::fs::File::open(path.as_ref())?;

        // SAFETY: The caller guarantees the file is not modified while mapped.
        // memmap2::Mmap provides a &[u8] view of the file contents.
        #[allow(unsafe_code)]
        let mmap = unsafe { memmap2::Mmap::map(&file) }?;

        // Move the mapping into the shared backing; bundled pages read from it
        // lazily (zero-copy open), and the `Arc` keeps it alive for them. Keep a
        // second, concretely-typed clone (same allocation, just another strong
        // ref) for `advise_page_willneed`.
        let mmap = Arc::new(mmap);
        let backing: Backing = mmap.clone();
        let doc = DjVuDocument::parse_backed(backing.clone())?;
        Ok(MmapDocument {
            _backing: backing,
            mmap,
            doc,
        })
    }

    /// Open a DjVu file with automatic filesystem resolution for indirect pages.
    ///
    /// For bundled documents this is identical to [`MmapDocument::open`].
    /// For indirect DJVM documents, component files named in the DIRM are
    /// resolved relative to the directory containing `path`.
    ///
    /// # Safety contract
    ///
    /// The file at `path` **must not be modified or truncated** while the
    /// returned `MmapDocument` is alive.
    pub fn open_indirect(path: impl AsRef<std::path::Path>) -> Result<Self, DocError> {
        let path = path.as_ref();
        let file = std::fs::File::open(path)?;
        #[allow(unsafe_code)]
        let mmap = unsafe { memmap2::Mmap::map(&file) }?;

        let base_dir = path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        // Indirect documents resolve external component files, so pages are eager
        // here; the mapping is still held via the shared backing for uniformity.
        let doc = DjVuDocument::parse_from_dir(&mmap, &base_dir)?;
        let mmap = Arc::new(mmap);
        let backing: Backing = mmap.clone();
        Ok(MmapDocument {
            _backing: backing,
            mmap,
            doc,
        })
    }

    /// Access the parsed [`DjVuDocument`].
    pub fn document(&self) -> &DjVuDocument {
        &self.doc
    }

    /// Number of pages in the document.
    pub fn page_count(&self) -> usize {
        self.doc.page_count()
    }

    /// Access a page by 0-based index.
    pub fn page(&self, index: usize) -> Result<&DjVuPage, DocError> {
        self.doc.page(index)
    }

    /// Hint to the OS that page `index`'s own bytes will be needed soon
    /// (`MADV_WILLNEED`). The cold-open (B6) lever: call this right after
    /// [`MmapDocument::open`], before the first render, so the kernel can
    /// start readahead I/O while the caller does anything else (parse
    /// metadata, build UI, etc.) instead of only starting on the page fault
    /// the render's first chunk read triggers.
    ///
    /// # Measured (COLD_OPEN, round 36)
    ///
    /// On a local NVMe SSD (M1) with `pathogenic_bacteria_1896.djvu` (517
    /// pages, 26 MB — small per-page FORM ranges, tens of KB), this hint is a
    /// wash: ±0–2%, inside measurement noise, whether issued for page 0 or a
    /// page deep in the file. Two things account for it: (1) most of a
    /// bundled document's structural cost (walking every page's IFF chunk
    /// headers to build `page_byte_range`) is already paid synchronously
    /// inside [`MmapDocument::open`], before this hint can be issued — there
    /// isn't much cold-read work left to schedule ahead by the time the
    /// caller gets a `MmapDocument` back; (2) a single page's FORM range is
    /// small enough that on fast local storage the demand-fault path is
    /// already close to the readahead path's latency. **An earlier version
    /// of this method advised the whole `0..range.end` prefix (covering the
    /// header/DIRM region too) and *reproducibly regressed cold open by
    /// ~12%*** (low dispersion, not noise) — advising far more than what's
    /// about to be read is actively harmful, not just wasted effort. Scoped
    /// to just `range` (this page's own bytes) it's harmless but unproven on
    /// this host; likely worth revisiting on higher-latency storage (network
    /// mounts, spinning disks) where a real win is more plausible. See
    /// `examples/cold_open_bench.rs --mode madvise`.
    ///
    /// Best-effort — a `madvise` failure (unsupported platform, unmapped
    /// range) is surfaced as `Err` but changes no state; correctness never
    /// depends on the hint landing. A `None` from
    /// [`DjVuDocument::page_byte_range`] (out-of-range index, indirect
    /// document, or an unmatched DIRM offset table) is treated as a no-op
    /// `Ok(())` rather than an error, since there is nothing wrong to report
    /// — there's just no known byte range to advise on.
    ///
    /// Only supported on Unix (the underlying `memmap2::Mmap::advise_range`
    /// is `#[cfg(unix)]`); a no-op stub is not provided for other platforms —
    /// gate calls with `#[cfg(unix)]` if you need to build for Windows too.
    #[cfg(unix)]
    pub fn advise_page_willneed(&self, index: usize) -> std::io::Result<()> {
        let Some(range) = self.doc.page_byte_range(index) else {
            return Ok(());
        };
        let start = (range.start as usize).min(self.mmap.len());
        let end = (range.end as usize).min(self.mmap.len());
        if end <= start {
            return Ok(());
        }
        self.mmap
            .advise_range(memmap2::Advice::WillNeed, start, end - start)
    }

    /// Consume this `MmapDocument`, returning the owned [`DjVuDocument`].
    ///
    /// Bundled documents' lazily-constructed pages ([`ChunkStore::Lazy`])
    /// hold their own `Arc` clone of the memory mapping, so it stays mapped
    /// for as long as any page needs it — dropping this wrapper's own
    /// reference here is safe (indirect documents' pages are eager and don't
    /// reference the mapping at all after parsing). Useful to obtain an owned
    /// value to wrap in `Arc<DjVuDocument>`, which [`DjVuDocument::prefetch_page`]
    /// requires so a background task can share ownership of the same page
    /// caches the foreground render uses.
    pub fn into_document(self) -> DjVuDocument {
        self.doc
    }
}

#[cfg(feature = "mmap")]
impl core::ops::Deref for MmapDocument {
    type Target = DjVuDocument;
    fn deref(&self) -> &DjVuDocument {
        &self.doc
    }
}

// ---- Internal parsing helpers -----------------------------------------------

/// Parse a `DjVuPage` from the chunks of a FORM:DJVU.
///
/// `shared_djbz` is the raw `Djbz` data from a referenced DJVI component
/// (resolved from the page's INCL chunk by the caller); pass `None` if no
/// shared dictionary is available.
/// Build a page whose chunk bytes are materialised lazily from `backing`.
///
/// Only the cheap fixed-size `INFO` header is parsed now; the per-chunk copy is
/// deferred to first [`DjVuPage::chunk_slice`] access. `range` is the page's
/// `FORM` sub-form byte range within `backing`.
#[cfg(feature = "std")]
fn parse_page_lazy(
    chunks: &[IffChunk<'_>],
    index: usize,
    shared_djbz: Option<Arc<SharedDict>>,
    backing: Backing,
    range: core::ops::Range<usize>,
) -> Result<DjVuPage, DocError> {
    let info_chunk = chunks
        .iter()
        .find(|c| &c.id == b"INFO")
        .ok_or(DocError::MissingChunk("INFO"))?;
    let info = PageInfo::parse(info_chunk.data)?;
    Ok(DjVuPage {
        info,
        chunks: ChunkStore::Lazy {
            backing,
            range,
            cache: std::sync::OnceLock::new(),
        },
        index,
        shared_djbz,
        render_layers: std::sync::OnceLock::new(),
    })
}

#[cfg(feature = "std")]
fn parse_page_from_chunks(
    chunks: &[IffChunk<'_>],
    index: usize,
    shared_djbz: Option<Arc<SharedDict>>,
) -> Result<DjVuPage, DocError> {
    let info_chunk = chunks
        .iter()
        .find(|c| &c.id == b"INFO")
        .ok_or(DocError::MissingChunk("INFO"))?;

    let info = PageInfo::parse(info_chunk.data)?;

    // Copy all chunks to owned storage for lazy decode later.
    let raw_chunks: Vec<RawChunk> = chunks
        .iter()
        .map(|c| RawChunk {
            id: c.id,
            data: c.data.to_vec(),
        })
        .collect();

    Ok(DjVuPage {
        info,
        chunks: ChunkStore::Eager(raw_chunks),
        index,
        shared_djbz,
        render_layers: std::sync::OnceLock::new(),
    })
}

#[cfg(not(feature = "std"))]
fn parse_page_from_chunks(
    chunks: &[IffChunk<'_>],
    index: usize,
    shared_djbz: Option<Vec<u8>>,
) -> Result<DjVuPage, DocError> {
    let info_chunk = chunks
        .iter()
        .find(|c| &c.id == b"INFO")
        .ok_or(DocError::MissingChunk("INFO"))?;

    let info = PageInfo::parse(info_chunk.data)?;

    let raw_chunks: Vec<RawChunk> = chunks
        .iter()
        .map(|c| RawChunk {
            id: c.id,
            data: c.data.to_vec(),
        })
        .collect();

    Ok(DjVuPage {
        info,
        chunks: raw_chunks,
        index,
        shared_djbz,
    })
}

/// Parse sub-form chunks from the data portion of a FORM chunk.
///
/// The `data` bytes start with a 4-byte form type (e.g. `DJVU`), followed by
/// sequential IFF chunks.
fn parse_sub_form(data: &[u8]) -> Result<Vec<IffChunk<'_>>, DocError> {
    if data.len() < 4 {
        return Err(DocError::Malformed("sub-form data too short"));
    }
    // data[0..4] = form type (DJVU / DJVI / THUM …)
    // data[4..] = sequential chunks
    let body = data
        .get(4..)
        .ok_or(DocError::Malformed("sub-form body missing"))?;
    let chunks = parse_form_body(body).map_err(DocError::Iff)?;
    Ok(chunks)
}

/// Maximum NAVM bookmark nesting depth (real outlines are a few levels deep).
/// Bounds `parse_bookmark_entry` recursion so a crafted deep chain can't overflow
/// the stack.
const MAX_NAVM_DEPTH: u32 = 256;

/// Parse NAVM bookmarks from the chunk list of a FORM:DJVM.
///
/// Returns an empty Vec if there is no NAVM chunk.
fn parse_navm_bookmarks(chunks: &[IffChunk<'_>]) -> Result<Vec<DjVuBookmark>, DocError> {
    let navm_data = match chunks.iter().find(|c| &c.id == b"NAVM") {
        Some(c) => c.data,
        None => return Ok(vec![]),
    };

    let decoded = bzz_decode(navm_data)?;

    if decoded.len() < 2 {
        return Ok(vec![]);
    }

    let b0 = *decoded
        .first()
        .ok_or(DocError::Malformed("NAVM total count byte 0"))?;
    let b1 = *decoded
        .get(1)
        .ok_or(DocError::Malformed("NAVM total count byte 1"))?;
    let total_count = u16::from_be_bytes([b0, b1]) as usize;

    let mut pos = 2usize;
    let mut bookmarks = Vec::new();
    let mut decoded_count = 0usize;

    while decoded_count < total_count {
        let bm = parse_bookmark_entry(&decoded, &mut pos, &mut decoded_count, 0)?;
        bookmarks.push(bm);
    }

    Ok(bookmarks)
}

/// Recursively parse one bookmark entry and its children.
///
/// `total_counter` is a shared counter for ALL bookmark nodes across all recursion
/// levels, matching the DjVu NAVM format's flat total-count field.
fn parse_bookmark_entry(
    data: &[u8],
    pos: &mut usize,
    total_counter: &mut usize,
    depth: u32,
) -> Result<DjVuBookmark, DocError> {
    // `total_counter` bounds the *number* of nodes but not the *depth*: a crafted
    // chain of single-child entries recurses as deep as the node count (up to
    // ~65 535), overflowing the stack. Real bookmark trees are a few levels deep.
    if depth > MAX_NAVM_DEPTH {
        return Err(DocError::Malformed("NAVM bookmark nesting too deep"));
    }
    if *pos >= data.len() {
        return Err(DocError::Malformed("NAVM bookmark entry truncated"));
    }

    // n_children is a single byte in the NAVM format
    let n_children = *data
        .get(*pos)
        .ok_or(DocError::Malformed("NAVM children count"))? as usize;
    *pos += 1;

    let title = read_navm_str(data, pos)?;
    let url = read_navm_str(data, pos)?;
    *total_counter += 1;

    // Children: fixed count, recurse with the same global total_counter
    let mut children = Vec::with_capacity(n_children);
    for _ in 0..n_children {
        let child = parse_bookmark_entry(data, pos, total_counter, depth + 1)?;
        children.push(child);
    }

    Ok(DjVuBookmark {
        title,
        url,
        children,
    })
}

/// Read a length-prefixed string from NAVM data.
///
/// Format: `[be_u24 length][text bytes]`. Nominally UTF-8, but legacy files
/// (DjVuLibre on Windows) carry CP1252 bytes in bookmark titles; decoded
/// leniently so one bad byte cannot abort `Document::open` (#524).
fn read_navm_str(data: &[u8], pos: &mut usize) -> Result<String, DocError> {
    if *pos + 3 > data.len() {
        return Err(DocError::Malformed("NAVM string length truncated"));
    }
    let len = ((*data.get(*pos).ok_or(DocError::Malformed("NAVM str"))? as usize) << 16)
        | ((*data.get(*pos + 1).ok_or(DocError::Malformed("NAVM str"))? as usize) << 8)
        | (*data.get(*pos + 2).ok_or(DocError::Malformed("NAVM str"))? as usize);
    *pos += 3;

    let bytes = data
        .get(*pos..*pos + len)
        .ok_or(DocError::Malformed("NAVM string bytes truncated"))?;
    *pos += len;

    Ok(crate::lenient_text::decode_lossy_string(bytes))
}

// ---- Tests ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// #624: a page may carry several `INCL` chunks (czech.djvu: shared
    /// annotations + two symbol-dictionary includes). Resolution must scan
    /// them all and pick the include that actually holds a `Djbz` — taking
    /// only the first INCL left every czech mask undecodable
    /// (`MissingSharedDict`). The expected mask is byte-identical to
    /// DjVuLibre's `ddjvu -mode=mask` output.
    #[test]
    fn multi_incl_page_resolves_shared_dict() {
        let path =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/czech.djvu");
        let data = std::fs::read(path).unwrap();
        let doc = DjVuDocument::parse(&data).unwrap();
        let mask = doc
            .page(1)
            .unwrap()
            .extract_mask()
            .expect("mask decode must succeed")
            .expect("page 1 has an Sjbz mask");
        assert_eq!((mask.width, mask.height), (1095, 1750));
        let black: u64 = (0..mask.height)
            .map(|y| (0..mask.width).filter(|&x| mask.get(x, y)).count() as u64)
            .sum();
        assert_eq!(
            black, 308_624,
            "mask content must match the ddjvu reference"
        );
    }

    /// A NAVM bookmark chain nested far deeper than `MAX_NAVM_DEPTH` must error,
    /// not recurse until the stack overflows (security finding). Drives the
    /// internal entry parser directly with a crafted decoded buffer.
    #[test]
    fn deeply_nested_bookmarks_are_rejected_not_overflow() {
        // [total_count u16 = 1] then one entry that is a 400-deep single-child
        // chain: each node = [n_children=1][title len3=0][url len3=0]; deepest =
        // [n_children=0][..][..].
        let mut decoded = vec![0x00, 0x01];
        for _ in 0..400 {
            decoded.push(1); // n_children
            decoded.extend_from_slice(&[0, 0, 0]); // empty title (3-byte len)
            decoded.extend_from_slice(&[0, 0, 0]); // empty url
        }
        decoded.push(0); // deepest: no children
        decoded.extend_from_slice(&[0, 0, 0]);
        decoded.extend_from_slice(&[0, 0, 0]);

        let mut pos = 2usize;
        let mut counter = 0usize;
        let r = parse_bookmark_entry(&decoded, &mut pos, &mut counter, 0);
        assert!(r.is_err(), "deep bookmark chain must error, not overflow");
    }

    fn assets_path() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("references/djvujs/library/assets")
    }

    // ---- TDD: failing tests written first (Red phase) -----------------------

    /// Single-page FORM:DJVU — basic parse, page count, dimensions, DPI.
    #[test]
    fn single_page_parse_and_metadata() {
        let data =
            std::fs::read(assets_path().join("chicken.djvu")).expect("chicken.djvu must exist");
        let doc = DjVuDocument::parse(&data).expect("parse should succeed");

        assert_eq!(doc.page_count(), 1);
        let page = doc.page(0).expect("page 0 must exist");
        assert_eq!(page.width(), 181);
        assert_eq!(page.height(), 240);
        assert_eq!(page.dpi(), 100);
        assert!((page.gamma() - 2.2).abs() < 0.01, "gamma should be ~2.2");
    }

    /// Single-page document: page index out of range.
    #[test]
    fn single_page_out_of_range() {
        let data =
            std::fs::read(assets_path().join("chicken.djvu")).expect("chicken.djvu must exist");
        let doc = DjVuDocument::parse(&data).expect("parse should succeed");
        let err = doc.page(1).expect_err("page 1 should be out of range");
        assert!(
            matches!(err, DocError::PageOutOfRange { index: 1, count: 1 }),
            "unexpected error: {err:?}"
        );
    }

    // ---- #342: chunk-payload dispatch (compressed / raw / missing) ----------
    //
    // These exercise the single BZZ-or-raw seam directly, decoupled from any
    // format parser: `decode_paired_payload` (the free function) and the
    // `DjVuPage::chunk_payload` accessor built on it.

    #[test]
    fn paired_payload_prefers_compressed_z_chunk() {
        let raw = b"the quick brown fox".as_slice();
        let z = crate::bzz_encode::bzz_encode(raw);
        // Both present: the compressed `*z` chunk wins.
        let out = decode_paired_payload(Some(&z), Some(b"ignored raw"))
            .expect("bzz decode should succeed");
        assert_eq!(out.as_deref(), Some(raw));
    }

    #[test]
    fn paired_payload_falls_back_to_raw_a_chunk() {
        let raw = b"plain uncompressed payload".as_slice();
        let out = decode_paired_payload(None, Some(raw)).expect("raw passthrough");
        assert_eq!(out.as_deref(), Some(raw));
    }

    #[test]
    fn paired_payload_missing_both_is_none() {
        assert_eq!(decode_paired_payload(None, None).expect("none"), None);
    }

    #[test]
    fn paired_payload_empty_chunk_is_placeholder_none() {
        // DjVu uses a zero-length chunk as a "no payload" placeholder for both
        // the compressed and raw variants.
        assert_eq!(
            decode_paired_payload(Some(&[]), None).expect("empty z"),
            None
        );
        assert_eq!(
            decode_paired_payload(None, Some(&[])).expect("empty a"),
            None
        );
    }

    #[test]
    fn paired_payload_invalid_bzz_errors() {
        // A non-empty `*z` chunk that is not valid BZZ must surface the error,
        // not be silently treated as missing.
        let result = decode_paired_payload(Some(&[0xff, 0x00, 0x13, 0x37]), None);
        assert!(result.is_err(), "invalid BZZ must error, got {result:?}");
    }

    /// Build a minimal valid INFO chunk payload (10 bytes) for the given size.
    fn make_info(width: u16, height: u16) -> Vec<u8> {
        let mut v = Vec::with_capacity(10);
        v.extend_from_slice(&width.to_be_bytes());
        v.extend_from_slice(&height.to_be_bytes());
        v.extend_from_slice(&[0, 0]); // version bytes (unused here)
        v.extend_from_slice(&100u16.to_le_bytes()); // dpi (little-endian)
        v.push(22); // gamma byte → 2.2
        v.push(0); // flags → no rotation
        v
    }

    /// Build a `DjVuPage` directly from hand-made chunks (INFO + extras), so the
    /// accessor can be tested without a full file round-trip through a parser.
    /// #605: repeated metadata access returns identical results through the
    /// cache, and the shared handles point at one allocation.
    #[test]
    fn metadata_cache_repeated_access_is_consistent() {
        let data = std::fs::read(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/links.djvu"),
        )
        .unwrap();
        let doc = DjVuDocument::parse(&data).unwrap();
        let page = doc.page(0).unwrap();

        let a1 = page.annotations().unwrap();
        let a2 = page.annotations().unwrap();
        assert_eq!(
            a1.as_ref().map(|(_, m)| m.len()),
            a2.as_ref().map(|(_, m)| m.len())
        );
        let s1 = page.annotations_shared().unwrap();
        let s2 = page.annotations_shared().unwrap();
        if let (Some(s1), Some(s2)) = (s1, s2) {
            assert!(
                std::sync::Arc::ptr_eq(&s1, &s2),
                "warm hits must share one decode"
            );
        }
        let h1 = page.hyperlinks().unwrap();
        let h2 = page.hyperlinks().unwrap();
        assert_eq!(h1.len(), h2.len());
    }

    /// #605: malformed TXTz keeps erroring on every call (errors are not
    /// cached), matching the pre-cache behaviour.
    #[test]
    fn metadata_cache_does_not_cache_errors() {
        // TXTz payload that BZZ-decodes but fails structured parse — or fails
        // BZZ outright; either way both calls must return Err.
        let bogus = [0xFFu8, 0x00, 0x12, 0x34, 0x56];
        let page = page_with_chunks(&[(b"TXTz", &bogus)]);
        assert!(page.text_layer().is_err());
        assert!(
            page.text_layer().is_err(),
            "second call must error identically"
        );
    }

    fn page_with_chunks(extra: &[(&[u8; 4], &[u8])]) -> DjVuPage {
        let info = make_info(64, 48);
        let mut chunks = Vec::new();
        chunks.push(IffChunk {
            id: *b"INFO",
            data: &info,
        });
        for (id, data) in extra {
            chunks.push(IffChunk { id: **id, data });
        }
        parse_page_from_chunks(&chunks, 0, None).expect("page should build")
    }

    #[test]
    fn chunk_payload_decodes_compressed_txtz() {
        let raw = b"decoded text-layer payload".as_slice();
        let z = crate::bzz_encode::bzz_encode(raw);
        let page = page_with_chunks(&[(b"TXTz", &z)]);
        let out = page
            .chunk_payload(b"TXTz", b"TXTa")
            .expect("chunk_payload should succeed");
        assert_eq!(out.as_deref(), Some(raw));
    }

    #[test]
    fn chunk_payload_passes_through_raw_txta() {
        let raw = b"raw text-layer payload".as_slice();
        let page = page_with_chunks(&[(b"TXTa", raw)]);
        let out = page
            .chunk_payload(b"TXTz", b"TXTa")
            .expect("chunk_payload should succeed");
        assert_eq!(out.as_deref(), Some(raw));
    }

    #[test]
    fn chunk_payload_missing_chunk_is_none() {
        let page = page_with_chunks(&[]); // INFO only, no TXT* chunks
        let out = page
            .chunk_payload(b"TXTz", b"TXTa")
            .expect("chunk_payload should succeed");
        assert_eq!(out, None);
    }

    /// Single-page document: no thumbnails expected.
    #[test]
    fn single_page_no_thumbnail() {
        let data =
            std::fs::read(assets_path().join("chicken.djvu")).expect("chicken.djvu must exist");
        let doc = DjVuDocument::parse(&data).expect("parse should succeed");
        let page = doc.page(0).expect("page 0 must exist");
        // Data is not decoded until thumbnail() is called — verify lazy contract
        let thumb = page.thumbnail().expect("thumbnail() should not error");
        assert!(
            thumb.is_none(),
            "single-page chicken.djvu has no TH44 chunks"
        );
    }

    /// Single-page: dimensions helper.
    #[test]
    fn single_page_dimensions() {
        let data =
            std::fs::read(assets_path().join("chicken.djvu")).expect("chicken.djvu must exist");
        let doc = DjVuDocument::parse(&data).expect("parse should succeed");
        let page = doc.page(0).unwrap();
        assert_eq!(page.dimensions(), (181, 240));
    }

    /// Bundled multi-page FORM:DJVM — page count and DIRM parsing.
    #[test]
    fn multipage_bundled_page_count() {
        let data = std::fs::read(assets_path().join("DjVu3Spec_bundled.djvu"))
            .expect("DjVu3Spec_bundled.djvu must exist");
        let doc = DjVuDocument::parse(&data).expect("bundled parse should succeed");
        // The bundled spec PDF has many pages — just check > 1
        assert!(
            doc.page_count() > 1,
            "bundled document should have more than 1 page, got {}",
            doc.page_count()
        );
    }

    /// Bundled multi-page: each page should have valid metadata.
    #[test]
    fn multipage_bundled_page_metadata() {
        let data = std::fs::read(assets_path().join("DjVu3Spec_bundled.djvu"))
            .expect("DjVu3Spec_bundled.djvu must exist");
        let doc = DjVuDocument::parse(&data).expect("bundled parse should succeed");

        let page0 = doc.page(0).expect("page 0 must exist");
        assert!(page0.width() > 0, "page width must be non-zero");
        assert!(page0.height() > 0, "page height must be non-zero");
        assert!(page0.dpi() > 0, "page dpi must be non-zero");
    }

    /// NAVM bookmarks from a document that contains them.
    #[test]
    fn navm_bookmarks_present() {
        let data =
            std::fs::read(assets_path().join("navm_fgbz.djvu")).expect("navm_fgbz.djvu must exist");
        let doc = DjVuDocument::parse(&data).expect("parse should succeed");
        // navm_fgbz.djvu has NAVM chunk — should return at least one bookmark
        let bm = doc.bookmarks();
        assert!(
            !bm.is_empty(),
            "navm_fgbz.djvu should have at least one bookmark"
        );
    }

    /// Documents without NAVM should return empty bookmark list.
    #[test]
    fn no_navm_returns_empty_bookmarks() {
        let data =
            std::fs::read(assets_path().join("chicken.djvu")).expect("chicken.djvu must exist");
        let doc = DjVuDocument::parse(&data).expect("parse should succeed");
        assert!(
            doc.bookmarks().is_empty(),
            "chicken.djvu has no NAVM — bookmarks should be empty"
        );
    }

    /// Indirect document: parse with resolver callback.
    ///
    /// We simulate an indirect document by constructing a DJVM DIRM that marks
    /// entries as non-bundled and supplying a resolver that returns the bytes of
    /// the real chicken.djvu page.
    #[test]
    fn indirect_document_with_resolver() {
        // Load chicken.djvu — we'll use it as the "resolved" page.
        let chicken_data =
            std::fs::read(assets_path().join("chicken.djvu")).expect("chicken.djvu must exist");
        // Build a minimal indirect DJVM document referencing "chicken.djvu"
        let djvm_data = build_indirect_djvm_bytes("chicken.djvu");

        let resolver = |name: &str| -> Result<Vec<u8>, DocError> {
            if name == "chicken.djvu" {
                Ok(chicken_data.clone())
            } else {
                Err(DocError::IndirectResolve(name.to_string()))
            }
        };

        let doc = DjVuDocument::parse_with_resolver(&djvm_data, Some(resolver))
            .expect("indirect parse should succeed");

        assert_eq!(doc.page_count(), 1);
        let page = doc.page(0).unwrap();
        assert_eq!(page.width(), 181);
        assert_eq!(page.height(), 240);
    }

    /// Indirect document without resolver must return NoResolver error.
    #[test]
    fn indirect_document_no_resolver_returns_error() {
        let djvm_data = build_indirect_djvm_bytes("chicken.djvu");
        let err = DjVuDocument::parse(&djvm_data).expect_err("should fail without resolver");
        assert!(
            matches!(err, DocError::NoResolver),
            "expected NoResolver, got {err:?}"
        );
    }

    /// Page must not decode image data before thumbnail() is called.
    ///
    /// We verify laziness by confirming that constructing the document and
    /// accessing `page()` without calling `thumbnail()` does not involve
    /// any IW44 decoder side-effects.  We test this by calling thumbnail()
    /// on a page with no TH44 chunks and verifying we get Ok(None).
    #[test]
    fn page_is_lazy_no_decode_before_thumbnail() {
        let data =
            std::fs::read(assets_path().join("boy_jb2.djvu")).expect("boy_jb2.djvu must exist");
        let doc = DjVuDocument::parse(&data).expect("parse should succeed");
        let page = doc.page(0).expect("page 0 must exist");

        // Chunks are available (materialised on access for lazy pages) but no
        // IW44 decoding has happened yet.
        assert!(!page.chunk_slice().is_empty(), "chunks must be available");

        // thumbnail() triggers decode — but there's no TH44 chunk in boy_jb2.djvu
        let thumb = page.thumbnail().expect("thumbnail() should not error");
        assert!(thumb.is_none());
    }

    /// Non-DjVu file returns NotDjVu error.
    #[test]
    fn not_djvu_returns_error() {
        // Construct a valid IFF with a non-DjVu form type ("XXXX" + 4 dummy
        // bytes), routed through the emission seam.
        let data = crate::iff::partial_emit(*b"XXXX", &[crate::iff::EmitPart::Verbatim(b"XXXX")])
            .expect("fits within u32");
        let err = DjVuDocument::parse(&data).expect_err("should fail");
        assert!(
            matches!(err, DocError::NotDjVu(_) | DocError::Iff(_)),
            "expected NotDjVu or Iff error, got {err:?}"
        );
    }

    // ---- Helpers: build minimal DJVM documents for indirect tests -----------

    /// Build a minimal indirect FORM:DJVM with 1 page component named "chicken.djvu".
    ///
    /// DIRM format: flags=0x00 (not bundled), nfiles=1, followed by BZZ-compressed
    /// metadata. The BZZ bytes below were pre-computed using the reference `bzz -e`
    /// tool encoding the metadata:
    ///   `\x00\x00\x00` (size, 3 bytes) + `\x01` (Page flag) + `chicken.djvu\x00`
    fn build_indirect_djvm_bytes(_page_name: &str) -> Vec<u8> {
        // BZZ-encoded DIRM metadata for 1 Page component named "chicken.djvu".
        // Generated with: printf '\x00\x00\x00\x01chicken.djvu\x00' | bzz -e - file.bzz
        // Verified to decode back to the original 17-byte meta block.
        let bzz_meta: &[u8] = &[
            0xff, 0xff, 0xed, 0xbf, 0x8a, 0x1f, 0xbe, 0xad, 0x14, 0x57, 0x10, 0xc9, 0x63, 0x19,
            0x11, 0xf0, 0x85, 0x28, 0x12, 0x8a, 0xbf,
        ];

        let mut dirm_data = Vec::new();
        dirm_data.push(0x00); // flags: not bundled (is_bundled bit = 0)
        dirm_data.push(0x00); // nfiles high byte
        dirm_data.push(0x01); // nfiles low byte = 1
        dirm_data.extend_from_slice(bzz_meta);

        build_djvm_with_dirm(&dirm_data)
    }

    fn build_djvm_with_dirm(dirm_data: &[u8]) -> Vec<u8> {
        // A FORM:DJVM carrying a single DIRM chunk, built through the seam.
        let dirm = crate::iff::Chunk::Leaf {
            id: *b"DIRM",
            data: dirm_data.to_vec(),
        };
        crate::iff::partial_emit(*b"DJVM", &[crate::iff::EmitPart::Chunk(&dirm)])
            .expect("fits within u32")
    }

    /// Sub-FORM with < 4 bytes of data: parse_sub_form returns Malformed (line 1225).
    #[test]
    fn parse_bundled_djvm_with_short_sub_form_returns_malformed() {
        use crate::dirm::DirmPayload;
        // Bundled DIRM with 1 Page entry (flags=0x80 = bundled, flag=0x01=Page)
        let dirm_payload = DirmPayload::build_bundled(1, &[0x01], &["p0001.djvu".to_string()], &[]);
        let dirm = crate::iff::Chunk::Leaf {
            id: *b"DIRM",
            data: dirm_payload.encode(),
        };
        // Short sub-FORM: FORM ID (4 bytes) + length=2 (4 bytes) + 2 data bytes
        // When the IFF parser reads this, data.len() = 2 < 4 → parse_sub_form Err
        let short_form_bytes: &[u8] = b"FORM\x00\x00\x00\x02AB";
        let djvm = crate::iff::partial_emit(
            *b"DJVM",
            &[
                crate::iff::EmitPart::Chunk(&dirm),
                crate::iff::EmitPart::Verbatim(short_form_bytes),
            ],
        )
        .expect("fits within u32");

        let err = DjVuDocument::parse(&djvm).expect_err("short sub-form must error");
        assert!(
            matches!(err, DocError::Malformed(_)),
            "expected Malformed, got {err:?}"
        );
    }

    // ── raw chunk API (Issue #43) ────────────────────────────────────────────

    /// `DjVuPage::raw_chunk` returns bytes for known chunk types.
    #[test]
    fn page_raw_chunk_info_present() {
        let data =
            std::fs::read(assets_path().join("chicken.djvu")).expect("chicken.djvu must exist");
        let doc = DjVuDocument::parse(&data).expect("parse must succeed");
        let page = doc.page(0).expect("page 0 must exist");

        // INFO chunk must be present
        let info = page.raw_chunk(b"INFO").expect("INFO chunk must be present");
        assert_eq!(info.len(), 10, "INFO chunk is always 10 bytes");
    }

    /// `DjVuPage::raw_chunk` returns None for absent chunk types.
    #[test]
    fn page_raw_chunk_absent() {
        let data =
            std::fs::read(assets_path().join("chicken.djvu")).expect("chicken.djvu must exist");
        let doc = DjVuDocument::parse(&data).expect("parse must succeed");
        let page = doc.page(0).expect("page 0 must exist");

        assert!(
            page.raw_chunk(b"XXXX").is_none(),
            "unknown chunk type must return None"
        );
    }

    /// `DjVuPage::all_chunks` returns multiple BG44 chunks in order.
    #[test]
    fn page_all_chunks_bg44_multiple() {
        // big-scanned-page.djvu has 4 progressive BG44 chunks
        let data = std::fs::read(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/big-scanned-page.djvu"),
        )
        .expect("big-scanned-page.djvu must exist");
        let doc = DjVuDocument::parse(&data).expect("parse must succeed");
        let page = doc.page(0).expect("page 0 must exist");

        let bg44 = page.all_chunks(b"BG44");
        assert!(
            bg44.len() >= 2,
            "colour page must have ≥2 BG44 chunks, got {}",
            bg44.len()
        );

        // Chunks must be non-empty
        for (i, chunk) in bg44.iter().enumerate() {
            assert!(!chunk.is_empty(), "BG44 chunk {i} must not be empty");
        }
    }

    /// `DjVuPage::chunk_ids` lists all chunk IDs in order.
    #[test]
    fn page_chunk_ids_includes_info() {
        let data =
            std::fs::read(assets_path().join("chicken.djvu")).expect("chicken.djvu must exist");
        let doc = DjVuDocument::parse(&data).expect("parse must succeed");
        let page = doc.page(0).expect("page 0 must exist");

        let ids = page.chunk_ids();
        assert!(!ids.is_empty(), "chunk_ids must not be empty");
        assert!(
            ids.contains(b"INFO"),
            "chunk_ids must include INFO, got: {:?}",
            ids.iter()
                .map(|id| std::str::from_utf8(id).unwrap_or("????"))
                .collect::<Vec<_>>()
        );
    }

    /// `DjVuDocument::raw_chunk` works for single-page DJVU files.
    #[test]
    fn document_raw_chunk_single_page() {
        let data =
            std::fs::read(assets_path().join("chicken.djvu")).expect("chicken.djvu must exist");
        let doc = DjVuDocument::parse(&data).expect("parse must succeed");

        // Single-page DJVU exposes all top-level chunks at document level too
        let info = doc
            .raw_chunk(b"INFO")
            .expect("document must expose INFO chunk");
        assert_eq!(info.len(), 10);
    }

    // ── DJVI shared dictionary / INCL chunks (Issue #45) ────────────────────

    /// DjVu3Spec_bundled.djvu has shared DJVI symbol dictionaries.
    /// Parsing must succeed and pages with INCL references must carry the dict.
    #[test]
    fn djvi_shared_dict_parsed_from_bundled_djvm() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/DjVu3Spec_bundled.djvu");
        let data = std::fs::read(&path).expect("DjVu3Spec_bundled.djvu must exist");
        let doc = DjVuDocument::parse(&data).expect("parse must succeed");

        assert!(doc.page_count() > 0, "document must have pages");

        // At least one page should have a shared dict loaded (shared_djbz Some)
        let pages_with_dict = doc.pages.iter().filter(|p| p.shared_djbz.is_some()).count();
        assert!(
            pages_with_dict > 0,
            "at least one page must have a resolved shared DJVI dict"
        );
    }

    /// Pages with INCL references must render their mask without error.
    #[test]
    fn djvi_incl_page_mask_renders_ok() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/DjVu3Spec_bundled.djvu");
        let data = std::fs::read(&path).expect("DjVu3Spec_bundled.djvu must exist");
        let doc = DjVuDocument::parse(&data).expect("parse must succeed");

        // Find first page with a shared dict and render its mask
        let page = doc
            .pages
            .iter()
            .find(|p| p.shared_djbz.is_some())
            .expect("at least one page must have a shared dict");

        let mask = page
            .extract_mask()
            .expect("extract_mask must succeed for INCL page");
        assert!(mask.is_some(), "INCL page must have a JB2 mask");
        let bm = mask.unwrap();
        assert!(
            bm.width > 0 && bm.height > 0,
            "mask must have non-zero dimensions"
        );
    }

    /// Pages without INCL still render correctly (no regression).
    #[test]
    fn no_regression_non_incl_pages() {
        // boy_jb2.djvu has a Sjbz mask and no INCL reference
        let data = std::fs::read(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/boy_jb2.djvu"),
        )
        .expect("boy_jb2.djvu must exist");
        let doc = DjVuDocument::parse(&data).expect("parse must succeed");
        let page = doc.page(0).expect("page 0 must exist");
        assert!(
            page.shared_djbz.is_none(),
            "single-page DJVU has no shared dict"
        );
        let mask = page.extract_mask().expect("extract_mask must succeed");
        assert!(mask.is_some(), "boy_jb2.djvu page must have a JB2 mask");
    }

    /// `carte.djvu` has a 5-byte INFO chunk (width, height, version byte —
    /// no dpi/gamma/flags) instead of the canonical 10-byte layout. The file
    /// itself is intact (byte-exact IFF framing; `djvudump`/`ddjvu` from
    /// DjVuLibre parse and render it without complaint), so `DjVuDocument::parse`
    /// rejecting it as `Iff(Truncated)` was a parser-strictness bug, not a
    /// corrupt fixture. Regression test for that bug (see `info.rs`'s
    /// `carte_style_five_byte_info_parses_with_defaults` for the unit-level
    /// check).
    #[test]
    fn parse_carte_with_short_info_chunk() {
        let data = std::fs::read(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/carte.djvu"),
        )
        .expect("carte.djvu must exist");
        let doc = DjVuDocument::parse(&data).expect("carte.djvu must parse despite short INFO");
        assert_eq!(doc.page_count(), 1);
        let page = doc.page(0).expect("page 0 must exist");
        assert_eq!(page.width(), 4200);
        assert_eq!(page.height(), 2556);
    }

    /// Round-trip: bytes from `raw_chunk` re-parse to the same metadata.
    #[test]
    fn page_raw_chunk_info_roundtrip() {
        let data =
            std::fs::read(assets_path().join("chicken.djvu")).expect("chicken.djvu must exist");
        let doc = DjVuDocument::parse(&data).expect("parse must succeed");
        let page = doc.page(0).expect("page 0 must exist");

        let raw_info = page.raw_chunk(b"INFO").expect("INFO chunk must be present");
        let reparsed = crate::info::PageInfo::parse(raw_info).expect("re-parse must succeed");
        assert_eq!(reparsed.width, page.width() as u16);
        assert_eq!(reparsed.height, page.height() as u16);
        assert_eq!(reparsed.dpi, page.dpi());
    }

    // ── #196 Phase 2: page_byte_range ────────────────────────────────────────

    /// Single-page DJVU: byte range covers the entire input buffer.
    #[test]
    fn page_byte_range_single_page_covers_full_buffer() {
        let data =
            std::fs::read(assets_path().join("chicken.djvu")).expect("chicken.djvu must exist");
        let doc = DjVuDocument::parse(&data).expect("parse must succeed");

        let r = doc.page_byte_range(0).expect("page 0 must have a range");
        assert_eq!(r.start, 0);
        assert_eq!(r.end, data.len() as u64);

        assert!(
            doc.page_byte_range(1).is_none(),
            "out-of-range index returns None"
        );
    }

    /// Bundled DJVM: every page's byte range is non-empty, in-bounds,
    /// non-overlapping with neighbours, and re-parseable as a FORM.
    #[test]
    fn page_byte_range_bundled_djvm_round_trips() {
        let path = assets_path().join("DjVu3Spec_bundled.djvu");
        let Ok(data) = std::fs::read(&path) else {
            eprintln!("skip: {} missing", path.display());
            return;
        };
        let doc = DjVuDocument::parse(&data).expect("bundled DJVM parse must succeed");

        let mut prev_end = 0u64;
        for i in 0..doc.page_count() {
            let r = doc
                .page_byte_range(i)
                .unwrap_or_else(|| panic!("page {i} must have a range"));
            assert!(r.end <= data.len() as u64, "page {i} range OOB");
            assert!(r.start < r.end, "page {i} range empty");
            assert!(r.start >= prev_end, "page {i} overlaps previous");
            prev_end = r.end;

            // The range must start with `b"FORM"` magic.
            let slice = &data[r.start as usize..r.end as usize];
            assert_eq!(&slice[..4], b"FORM", "page {i} range must start with FORM");
        }
    }

    #[test]
    fn page_thumbnail_with_th44_data() {
        // Extract real TH44 chunk bytes from carte.djvu (which contains TH44 data)
        // and embed them in a synthetic page to cover the thumbnail decode path.
        let carte = std::fs::read(assets_path().join("carte.djvu")).unwrap();
        // Find TH44 in the raw bytes and extract chunk payload
        let th44_pos = carte.windows(4).position(|w| w == b"TH44");
        if let Some(pos) = th44_pos
            && pos + 8 <= carte.len()
        {
            let chunk_len = u32::from_be_bytes([
                carte[pos + 4],
                carte[pos + 5],
                carte[pos + 6],
                carte[pos + 7],
            ]) as usize;
            let chunk_data = carte.get(pos + 8..pos + 8 + chunk_len).unwrap_or(&[]);
            if !chunk_data.is_empty() {
                let page = page_with_chunks(&[(b"TH44", chunk_data)]);
                // This should decode successfully (covers lines 298-303)
                let thumb = page.thumbnail();
                assert!(thumb.is_ok(), "thumbnail decode should not error");
                // The thumbnail may or may not be Some depending on IW44 data validity
            }
        }
    }

    #[test]
    fn extract_mask_from_smmr_chunk() {
        // Build a page with an Smmr chunk (G4/MMR-encoded mask). This covers the
        // Smmr decode path in extract_mask() (lines 545-546).
        use crate::chunk_encode::{ChunkEncoder, SmmrChunk};
        let mut bm = crate::bitmap::Bitmap::new(8, 8);
        bm.set_black(2, 2);
        let smmr_chunk = SmmrChunk(&bm).encode_chunk().unwrap();
        let page = page_with_chunks(&[(b"Smmr", &smmr_chunk.payload)]);
        let result = page.extract_mask().unwrap();
        assert!(result.is_some(), "Smmr page should have a mask");
        assert_eq!(result.unwrap().width, 8);
    }

    #[test]
    fn extract_background_returns_none_for_jb2_only_page() {
        // A page with only Sjbz (no BG44) → extract_background returns Ok(None)
        // This covers lines 638-641 in djvu_document.rs.
        let jb2_data = std::fs::read(assets_path().join("boy_jb2.djvu")).unwrap();
        let doc = DjVuDocument::parse(&jb2_data).unwrap();
        let page = doc.page(0).unwrap();
        let bg = page.extract_background().unwrap();
        assert!(bg.is_none(), "JB2-only page should have no background");
    }

    #[test]
    fn extract_mask_indexed_smmr_path() {
        // Page with Smmr chunk: extract_mask_indexed takes the Smmr path (lines 570-575).
        use crate::chunk_encode::{ChunkEncoder, SmmrChunk};
        let mut bm = crate::bitmap::Bitmap::new(4, 4);
        bm.set_black(1, 1);
        let smmr_chunk = SmmrChunk(&bm).encode_chunk().unwrap();
        let page = page_with_chunks(&[(b"Smmr", &smmr_chunk.payload)]);
        let result = page.extract_mask_indexed().unwrap();
        assert!(result.is_some());
        let (mask, indices) = result.unwrap();
        assert_eq!(mask.width, 4);
        assert_eq!(indices.len(), 4 * 4);
    }

    #[test]
    fn extract_mask_indexed_no_chunks_returns_none() {
        // Page with no Sjbz or Smmr → Ok(None) (line 588).
        let page = page_with_chunks(&[]);
        let result = page.extract_mask_indexed().unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn extract_background_decodes_iw44_from_color_page() {
        // chicken.djvu has BG44 → extract_background decodes IW44 (lines 644-649).
        let data = std::fs::read(assets_path().join("chicken.djvu")).unwrap();
        let doc = DjVuDocument::parse(&data).unwrap();
        let page = doc.page(0).unwrap();
        let bg = page.extract_background().unwrap();
        assert!(bg.is_some(), "chicken.djvu page should have a background");
        let pm = bg.unwrap();
        assert!(pm.width > 0 && pm.height > 0);
    }

    #[test]
    fn djvu_page_debug_impl_does_not_panic() {
        let data = std::fs::read(assets_path().join("chicken.djvu")).unwrap();
        let doc = DjVuDocument::parse(&data).unwrap();
        let page = doc.page(0).unwrap();
        let s = format!("{page:?}");
        assert!(s.contains("DjVuPage"));
    }

    #[test]
    fn page_index_returns_zero_for_first_page() {
        let data = std::fs::read(assets_path().join("chicken.djvu")).unwrap();
        let doc = DjVuDocument::parse(&data).unwrap();
        let page = doc.page(0).unwrap();
        assert_eq!(page.index(), 0);
    }

    #[test]
    fn page_text_returns_some_for_text_page() {
        let data = std::fs::read(assets_path().join("colorbook.djvu")).unwrap();
        let doc = DjVuDocument::parse(&data).unwrap();
        let page = doc.page(0).unwrap();
        let t = page.text().unwrap();
        assert!(t.is_some(), "colorbook page 0 should have text");
    }

    /// Out-of-range page index returns None.
    #[test]
    fn page_byte_range_out_of_range() {
        let data =
            std::fs::read(assets_path().join("chicken.djvu")).expect("chicken.djvu must exist");
        let doc = DjVuDocument::parse(&data).expect("parse must succeed");
        assert!(doc.page_byte_range(99).is_none());
    }

    /// MmapDocument opens a file and parses identically to in-memory parse.
    #[test]
    #[cfg(feature = "mmap")]
    fn mmap_document_matches_parse() {
        let path = assets_path().join("chicken.djvu");
        let mmap_doc = MmapDocument::open(&path).expect("mmap open should succeed");
        let data = std::fs::read(&path).expect("read should succeed");
        let mem_doc = DjVuDocument::parse(&data).expect("parse should succeed");

        assert_eq!(mmap_doc.page_count(), mem_doc.page_count());
        for i in 0..mmap_doc.page_count() {
            let mp = mmap_doc.page(i).unwrap();
            let pp = mem_doc.page(i).unwrap();
            assert_eq!(mp.width(), pp.width());
            assert_eq!(mp.height(), pp.height());
            assert_eq!(mp.dpi(), pp.dpi());
        }
    }

    #[test]
    fn extract_foreground_returns_none_when_no_fg44() {
        // JB2-only page has no FG44 chunks — extract_foreground returns Ok(None).
        let data = std::fs::read(assets_path().join("boy_jb2.djvu")).unwrap();
        let doc = DjVuDocument::parse(&data).unwrap();
        let fg = doc.page(0).unwrap().extract_foreground().unwrap();
        assert!(fg.is_none());
    }

    #[test]
    fn metadata_returns_none_for_doc_without_meta_chunk() {
        let data = std::fs::read(assets_path().join("chicken.djvu")).unwrap();
        let doc = DjVuDocument::parse(&data).unwrap();
        let meta = doc.metadata().unwrap();
        // chicken.djvu has no METa/METz chunk
        assert!(meta.is_none());
    }

    #[test]
    fn all_chunks_returns_matching_chunks() {
        let data = std::fs::read(assets_path().join("chicken.djvu")).unwrap();
        let doc = DjVuDocument::parse(&data).unwrap();
        // INFO is a global chunk for single-page DJVU
        let info = doc.all_chunks(b"INFO");
        assert!(!info.is_empty());
        // Non-existent chunk returns empty
        let none = doc.all_chunks(b"XXXX");
        assert!(none.is_empty());
    }

    #[test]
    fn chunk_ids_returns_nonempty_for_djvu() {
        let data = std::fs::read(assets_path().join("chicken.djvu")).unwrap();
        let doc = DjVuDocument::parse(&data).unwrap();
        let ids = doc.chunk_ids();
        assert!(!ids.is_empty());
    }

    #[test]
    #[cfg(feature = "mmap")]
    fn mmap_open_indirect_on_bundled_doc_succeeds() {
        let path = assets_path().join("chicken.djvu");
        let doc = MmapDocument::open_indirect(&path).expect("open_indirect should work on bundled");
        assert!(doc.page_count() > 0);
    }

    #[test]
    #[cfg(feature = "mmap")]
    fn mmap_document_method_and_deref_are_reachable() {
        let path = assets_path().join("chicken.djvu");
        let mmap_doc = MmapDocument::open(&path).expect("mmap open should succeed");
        // document() accessor (line 1128-1129)
        assert!(mmap_doc.document().page_count() > 0);
        // Deref to &DjVuDocument (lines 1146-1147)
        let inner: &DjVuDocument = &mmap_doc;
        assert!(inner.page_count() > 0);
    }

    /// `advise_page_willneed` is a best-effort hint: it must not error on a
    /// real bundled document and must be a harmless no-op for an
    /// out-of-range index (COLD_OPEN B6).
    #[test]
    #[cfg(all(feature = "mmap", unix))]
    fn mmap_advise_page_willneed_in_range_and_out_of_range() {
        let path = assets_path().join("chicken.djvu");
        let mmap_doc = MmapDocument::open(&path).expect("mmap open should succeed");
        mmap_doc
            .advise_page_willneed(0)
            .expect("advise on page 0 should not error");
        // Out of range: page_byte_range returns None, so this must be a no-op
        // Ok(()), not an error.
        mmap_doc
            .advise_page_willneed(9_999)
            .expect("advise on an out-of-range page must be a harmless no-op");
    }

    /// `into_document` must yield a document that still renders correctly —
    /// the lazily-constructed pages hold their own `Arc` clone of the
    /// mapping, so dropping `MmapDocument`'s own reference must not unmap the
    /// file out from under them (COLD_OPEN B6/B7 prerequisite).
    #[test]
    #[cfg(feature = "mmap")]
    fn mmap_into_document_pages_still_render_after_wrapper_dropped() {
        let path = assets_path().join("chicken.djvu");
        let mmap_doc = MmapDocument::open(&path).expect("mmap open should succeed");
        let doc = mmap_doc.into_document();
        let page = doc.page(0).expect("page 0 should exist");
        let pm = crate::djvu_render::render_pixmap(
            page,
            &crate::djvu_render::RenderOptions {
                width: page.width() as u32,
                height: page.height() as u32,
                ..crate::djvu_render::RenderOptions::default()
            },
        )
        .expect("render after into_document should succeed");
        assert!(pm.width > 0 && pm.height > 0);
    }

    /// `prefetch_page` must actually warm the page's render caches before the
    /// caller does a synchronous render (COLD_OPEN B7). Not a timing
    /// assertion (that's what the `cold_open_bench` example measures) — just
    /// correctness: the background decode must land in the same cache a
    /// subsequent `render_pixmap` reads, and out-of-range indices must be a
    /// no-op rather than a panic.
    #[test]
    #[cfg(all(feature = "mmap", feature = "parallel"))]
    fn prefetch_page_warms_cache_and_ignores_out_of_range() {
        let path = assets_path().join("chicken.djvu");
        let mmap_doc = MmapDocument::open(&path).expect("mmap open should succeed");
        let doc = Arc::new(mmap_doc.into_document());

        doc.prefetch_page(9_999); // out of range: must not panic
        doc.prefetch_page(0);

        // Give the background task a moment to finish (this test only checks
        // correctness, not latency — a generous sleep avoids flakiness).
        std::thread::sleep(std::time::Duration::from_millis(200));

        let page = doc.page(0).unwrap();
        // Cache should already be warm: render_layers() bytes > 0 without us
        // having called any decoded_* accessor on this thread ourselves.
        assert!(
            page.render_cache_bytes() > 0,
            "prefetch_page should have populated the render cache"
        );

        // A subsequent render must still succeed and be unaffected.
        let pm = crate::djvu_render::render_pixmap(
            page,
            &crate::djvu_render::RenderOptions {
                width: page.width() as u32,
                height: page.height() as u32,
                ..crate::djvu_render::RenderOptions::default()
            },
        )
        .expect("render after prefetch should succeed");
        assert!(pm.width > 0 && pm.height > 0);
    }

    #[test]
    fn metadata_returns_some_for_doc_with_meta_chunk() {
        // Build a synthetic FORM:DJVU containing an INFO chunk and a METa chunk.
        use crate::iff::{Chunk, DjvuFile, emit};
        use crate::metadata::{DjVuMetadata, encode_metadata};

        let info = make_info(100, 100);
        let meta = DjVuMetadata {
            author: Some("TestAuthor".into()),
            ..DjVuMetadata::default()
        };
        let meta_bytes = encode_metadata(&meta);
        if meta_bytes.is_empty() {
            return; // encode returned empty — nothing to test
        }

        let file = DjvuFile {
            root: Chunk::Form {
                secondary_id: *b"DJVU",
                length: 0, // emit recalculates
                children: vec![
                    Chunk::Leaf {
                        id: *b"INFO",
                        data: info,
                    },
                    Chunk::Leaf {
                        id: *b"METa",
                        data: meta_bytes,
                    },
                ],
            },
        };
        let bytes = emit(&file);
        let doc = DjVuDocument::parse(&bytes).expect("parse should succeed");
        let m = doc.metadata().expect("metadata() should not error");
        assert!(
            m.is_some(),
            "metadata should be Some for a doc with METa chunk"
        );
        assert_eq!(m.unwrap().author.as_deref(), Some("TestAuthor"));
    }

    #[test]
    fn extract_mask_uses_inline_djbz_when_present() {
        // Build a page with both Sjbz (using shared shapes) and an inline Djbz.
        // This hits the `find_chunk(b"Djbz")` branch in extract_mask (lines 535-537).
        use crate::jb2_encode::{
            cluster_shared_symbols, encode_jb2_dict_with_shared, encode_jb2_djbz,
        };

        let mut shape = crate::bitmap::Bitmap::new(8, 8);
        shape.set_black(2, 2);
        shape.set_black(3, 3);
        let shapes = cluster_shared_symbols(&[shape.clone(), shape.clone()], 2);
        if shapes.is_empty() {
            return; // no shared shapes; skip
        }
        let djbz_data = encode_jb2_djbz(&shapes);
        let sjbz_data = encode_jb2_dict_with_shared(&shape, &shapes);

        let page = page_with_chunks(&[(b"Djbz", &djbz_data), (b"Sjbz", &sjbz_data)]);
        let result = page.extract_mask();
        assert!(
            result.is_ok(),
            "extract_mask with inline Djbz should succeed"
        );
    }

    #[test]
    fn extract_mask_indexed_uses_inline_djbz_when_present() {
        // Same as above but for extract_mask_indexed (lines 561-563).
        use crate::jb2_encode::{
            cluster_shared_symbols, encode_jb2_dict_with_shared, encode_jb2_djbz,
        };

        let mut shape = crate::bitmap::Bitmap::new(8, 8);
        shape.set_black(2, 2);
        shape.set_black(3, 3);
        let shapes = cluster_shared_symbols(&[shape.clone(), shape.clone()], 2);
        if shapes.is_empty() {
            return;
        }
        let djbz_data = encode_jb2_djbz(&shapes);
        let sjbz_data = encode_jb2_dict_with_shared(&shape, &shapes);

        let page = page_with_chunks(&[(b"Djbz", &djbz_data), (b"Sjbz", &sjbz_data)]);
        let result = page.extract_mask_indexed();
        assert!(
            result.is_ok(),
            "extract_mask_indexed with inline Djbz should succeed"
        );
    }

    /// NAVM with BZZ-decoded payload shorter than 2 bytes returns Ok([]).
    #[test]
    fn parse_navm_bookmarks_short_decoded_returns_empty() {
        use crate::bzz_encode::bzz_encode;
        // Encode a single byte — decoded is 1 byte < 2 → line 1248
        let bzz = bzz_encode(b"x");
        let chunk = crate::iff::IffChunk {
            id: *b"NAVM",
            data: &bzz,
        };
        let result = parse_navm_bookmarks(&[chunk]).unwrap();
        assert!(
            result.is_empty(),
            "NAVM with decoded < 2 bytes must yield empty bookmarks"
        );
    }

    /// NAVM with total_count > 0 but no actual entries → truncated entry error.
    #[test]
    fn parse_navm_bookmarks_truncated_entry_returns_error() {
        use crate::bzz_encode::bzz_encode;
        // Declare total_count = 1 (2 bytes) but no bookmark data follows → line 1281
        let payload = vec![0x00, 0x01]; // total_count = 1
        let bzz = bzz_encode(&payload);
        let chunk = crate::iff::IffChunk {
            id: *b"NAVM",
            data: &bzz,
        };
        let result = parse_navm_bookmarks(&[chunk]);
        assert!(
            result.is_err(),
            "NAVM with declared count > 0 but no entry data must error"
        );
    }

    /// NAVM bookmark title with CP1252 bytes (0x96 en dash — DjVuLibre on
    /// Windows) must decode leniently instead of aborting the open (#524).
    #[test]
    fn parse_navm_bookmarks_cp1252_title_is_lenient() {
        use crate::bzz_encode::bzz_encode;
        // [total_count u16 = 1][n_children u8 = 0]
        // [title: u24 len + bytes][url: u24 len + bytes]
        let title = b"Chapter 1 \x96 Intro";
        let mut payload = vec![0x00, 0x01, 0x00];
        payload.extend_from_slice(&[0x00, 0x00, title.len() as u8]);
        payload.extend_from_slice(title);
        payload.extend_from_slice(&[0x00, 0x00, 0x02]);
        payload.extend_from_slice(b"#1");
        let bzz = bzz_encode(&payload);
        let chunk = crate::iff::IffChunk {
            id: *b"NAVM",
            data: &bzz,
        };
        let bookmarks = parse_navm_bookmarks(&[chunk]).expect("CP1252 title must not abort");
        assert_eq!(bookmarks.len(), 1);
        assert_eq!(bookmarks[0].title, "Chapter 1 \u{2013} Intro");
        assert_eq!(bookmarks[0].url, "#1");
    }

    /// NAVM entry whose n_children byte is present but the title string's 3-byte
    /// length prefix is cut off → read_navm_str returns Malformed (line 1313).
    #[test]
    fn parse_navm_bookmarks_string_length_truncated_returns_error() {
        use crate::bzz_encode::bzz_encode;
        // Decoded layout: [total_count u16 = 1][n_children u8 = 0]
        // After reading n_children (pos=3), read_navm_str needs 3 more bytes
        // for the length prefix but data.len()=3 → 3+3>3 → Malformed (line 1313).
        let payload = vec![0x00, 0x01, 0x00]; // total_count=1, n_children=0
        let bzz = bzz_encode(&payload);
        let chunk = crate::iff::IffChunk {
            id: *b"NAVM",
            data: &bzz,
        };
        let result = parse_navm_bookmarks(&[chunk]);
        assert!(
            result.is_err(),
            "NAVM with truncated string length must error"
        );
    }

    /// Indirect DJVM with a shared DJVI component entry: the shared entry must
    /// be skipped (line 876 `continue`) and the page resolved via the resolver.
    #[test]
    fn indirect_djvm_with_shared_djvi_entry_skips_to_page() {
        use crate::dirm::DirmPayload;
        let chicken_data =
            std::fs::read(assets_path().join("chicken.djvu")).expect("chicken.djvu must exist");

        // Build DIRM: entry 0 = Shared (flag=0x00), entry 1 = Page (flag=0x01)
        let dirm_payload = DirmPayload::build_indirect(
            2,
            &[0x00, 0x01],
            &["shared.djvi".to_string(), "page.djvu".to_string()],
        );
        let dirm_data = dirm_payload.encode();
        let djvm_data = build_djvm_with_dirm(&dirm_data);

        let resolver = |name: &str| -> Result<Vec<u8>, DocError> {
            if name == "page.djvu" {
                Ok(chicken_data.clone())
            } else {
                Err(DocError::IndirectResolve(name.to_string()))
            }
        };

        let doc = DjVuDocument::parse_with_resolver(&djvm_data, Some(resolver))
            .expect("indirect DJVM with shared entry must parse");
        assert_eq!(doc.page_count(), 1);
        let page = doc.page(0).unwrap();
        assert_eq!(page.width(), 181);
    }

    /// parse_from_dir with a DIRM component named as an absolute path (line 1040).
    #[test]
    fn parse_from_dir_resolves_absolute_component_path() {
        use crate::dirm::DirmPayload;
        use crate::iff::{self as iff_mod, Chunk, EmitPart};

        // Write a single-page DJVU to a temp file at an absolute path.
        let chicken =
            std::fs::read(assets_path().join("chicken.djvu")).expect("chicken.djvu must exist");
        let tmp_dir = std::env::temp_dir();
        let abs_name = tmp_dir.join("djvu_rs_test_abs_component.djvu");
        std::fs::write(&abs_name, &chicken).expect("write tmp component");
        let abs_name_str = abs_name.to_str().unwrap().to_string();

        let dirm_payload =
            DirmPayload::build_indirect(1, &[0x01], std::slice::from_ref(&abs_name_str));
        let dirm = Chunk::Leaf {
            id: *b"DIRM",
            data: dirm_payload.encode(),
        };
        let djvm =
            iff_mod::partial_emit(*b"DJVM", &[EmitPart::Chunk(&dirm)]).expect("fits within u32");

        let doc = DjVuDocument::parse_from_dir(&djvm, &tmp_dir)
            .expect("absolute-path component must resolve");
        assert_eq!(doc.page_count(), 1);
        let _ = std::fs::remove_file(&abs_name);
    }

    /// parse_single_page_with_shared: form type is not DJVU → NotDjVu error (line 908).
    #[cfg(all(feature = "std", feature = "async"))]
    #[test]
    fn parse_single_page_with_shared_wrong_form_type_returns_not_djvu() {
        use crate::iff::{self as iff_mod, Chunk, DjvuFile};

        let bytes = iff_mod::emit(&DjvuFile {
            root: Chunk::Form {
                secondary_id: *b"DJVI",
                length: 0,
                children: vec![],
            },
        });
        let err = DjVuDocument::parse_single_page_with_shared(&bytes, 0, None)
            .expect_err("FORM:DJVI must not be accepted as a page");
        assert!(
            matches!(err, DocError::NotDjVu(_)),
            "expected NotDjVu, got {err:?}"
        );
    }

    /// DIRM offset points outside the file bytes, so the byte-range lookup
    /// for the page fails and `page_byte_ranges.clear()` (line 859) fires.
    /// The document still parses successfully (the IFF tree is intact); the
    /// page is accessible but `page_byte_range` returns None.
    #[test]
    fn bundled_djvm_out_of_bounds_dirm_offset_clears_page_byte_ranges() {
        use crate::dirm::DirmPayload;
        use crate::iff::{self as iff_mod, Chunk, EmitPart};

        let chicken =
            std::fs::read(assets_path().join("chicken.djvu")).expect("chicken.djvu must exist");

        // Build a bundled DIRM with one Page entry but set its offset to a value
        // far beyond the end of the file so the byte-range lookup fails.
        let mut dirm_payload = DirmPayload::build_bundled(1, &[0x01], &["p.djvu".to_string()], &[]);
        dirm_payload.offsets[0] = 0xFFFF_FFFF; // points well outside the file
        let dirm_data = dirm_payload.encode();

        let dirm = Chunk::Leaf {
            id: *b"DIRM",
            data: dirm_data,
        };
        // Strip the 4-byte AT&T magic from chicken.djvu to get the bare FORM bytes.
        let form_bytes = chicken
            .strip_prefix(b"AT&T")
            .expect("chicken.djvu must start with AT&T");

        let djvm = iff_mod::partial_emit(
            *b"DJVM",
            &[EmitPart::Chunk(&dirm), EmitPart::Verbatim(form_bytes)],
        )
        .expect("fits within u32");

        let doc = DjVuDocument::parse(&djvm).expect("DJVM with bad offset must still parse");
        assert_eq!(doc.page_count(), 1, "page must still be accessible");
        // page_byte_range is cleared because the offset was out of bounds.
        assert!(
            doc.page_byte_range(0).is_none(),
            "page_byte_range must be None when DIRM offset is out of bounds"
        );
    }
}
