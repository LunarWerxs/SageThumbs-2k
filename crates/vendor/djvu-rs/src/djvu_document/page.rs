//! The lazy page handle and its raw chunk store.

use super::*;

// ---- Page -------------------------------------------------------------------

/// A raw chunk extracted from a page FORM:DJVU.
#[derive(Debug, Clone)]
pub(super) struct RawChunk {
    pub(super) id: [u8; 4],
    pub(super) data: Vec<u8>,
}

/// Shared, owned backing store for a document's bytes (an owned `Vec<u8>` from
/// [`crate::Document::from_bytes`], or a `memmap2::Mmap`). Lazily-constructed
/// pages (`ChunkStore::Lazy`) hold an `Arc` clone of this so their chunk bytes
/// can be materialised on first access without copying them at open time.
#[cfg(feature = "std")]
pub(crate) type Backing = Arc<dyn AsRef<[u8]> + Send + Sync>;

/// The bytes behind a [`Backing`].
#[cfg(feature = "std")]
pub(super) fn backing_bytes(b: &Backing) -> &[u8] {
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
pub(super) enum ChunkStore {
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
    pub(super) fn get(&self) -> &[RawChunk] {
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
pub(super) fn decode_paired_payload(
    z: Option<&[u8]>,
    a: Option<&[u8]>,
) -> Result<Option<Vec<u8>>, BzzError> {
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
    pub(super) raw: Vec<u8>,
    pub(super) decoded: std::sync::OnceLock<Option<Jb2Dict>>,
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
    pub(super) fn get(&self) -> Option<&Jb2Dict> {
        self.decoded
            .get_or_init(|| crate::jb2::decode_dict(&self.raw, None).ok())
            .as_ref()
    }

    /// Length of the raw `Djbz` bytes (for `Debug`).
    pub(super) fn raw_len(&self) -> usize {
        self.raw.len()
    }
}

pub struct DjVuPage {
    /// Page info parsed from the INFO chunk.
    pub(super) info: PageInfo,
    /// All raw chunks from this page's FORM:DJVU, in order. In `std` builds this
    /// may be lazily materialised from the document backing (see [`ChunkStore`]);
    /// `no_std` always holds the eagerly-copied chunks.
    #[cfg(feature = "std")]
    pub(super) chunks: ChunkStore,
    #[cfg(not(feature = "std"))]
    pub(super) chunks: Vec<RawChunk>,
    /// Page index within the document (0-based).
    pub(super) index: usize,
    /// Raw Djbz data from the DJVI shared dictionary component referenced via
    /// the page's INCL chunk, if present.  Stored here so that `extract_mask`
    /// can decode it without access to the parent document.
    ///
    /// Wrapped in `Arc` so that multi-page documents share one allocation —
    /// and, via [`SharedDict`], one *decode* — instead of cloning the bytes and
    /// re-decoding per page.
    #[cfg(feature = "std")]
    pub(super) shared_djbz: Option<Arc<SharedDict>>,
    #[cfg(not(feature = "std"))]
    pub(super) shared_djbz: Option<Vec<u8>>,
    /// Render-tier cache of this page's decoded layers (background, mask,
    /// quarter-resolution mask, foreground).  The decode logic and the
    /// compositor-subsampling concern live in
    /// [`crate::djvu_render::PageLayers`]; the page only holds the handle so
    /// repeated renders reuse the decode.  Populated on first render.
    /// Only available when the `std` feature is enabled (`OnceLock` requires std).
    #[cfg(feature = "std")]
    pub(super) render_layers: std::sync::OnceLock<Arc<crate::djvu_render::PageLayers>>,
    /// Resource limits inherited from the parent document at parse time.
    pub(super) resource_limits: Option<crate::resource_limits::ResourceLimits>,
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
            resource_limits: self.resource_limits,
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

    /// Resource limits inherited from the parent document at parse time.
    pub fn resource_limits(&self) -> Option<crate::resource_limits::ResourceLimits> {
        self.resource_limits
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
    /// `PageLayers` cache that lives as long as the owning
    /// document. Rendering many pages of a large document therefore accumulates
    /// one such cache per page — the peak RSS grows linearly with pages rendered
    /// (measured ≈ 11 MB/page on `colorbook.djvu`), which can exhaust memory in a
    /// long-lived viewer over a big book.
    ///
    /// This resets the cache so the memory is reclaimed; it rebuilds lazily on
    /// the next render of this page. A viewer can call it on pages scrolled
    /// off-screen (or use [`DjVuDocument::retain_render_caches`]) to bound memory.
    ///
    /// Takes `&self` since 0.33 (READ_CACHE_BOUNDED), so it can run from inside
    /// a render. A render already holding a layer keeps it until it finishes.
    #[cfg(feature = "std")]
    pub fn evict_render_cache(&self) {
        if let Some(layers) = self.render_layers.get() {
            layers.evict_shared();
        }
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
    ///
    /// Takes `&self` since 0.33, for the same reason as
    /// [`evict_render_cache`](Self::evict_render_cache).
    #[cfg(feature = "std")]
    pub fn downgrade_render_cache(&self) {
        if let Some(layers) = self.render_layers.get() {
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
    pub(super) fn chunk_slice(&self) -> &[RawChunk] {
        self.chunks.get()
    }
    #[cfg(not(feature = "std"))]
    pub(super) fn chunk_slice(&self) -> &[RawChunk] {
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
    ///
    /// Legacy standalone `FORM:BM44` / `FORM:PM44` documents store the same
    /// IW44 bitstream under `BM44` / `PM44` chunk ids; those are returned here
    /// so the existing render pipeline can decode them without a separate path.
    pub fn bg44_chunks(&self) -> Vec<&[u8]> {
        let bg44 = self.find_chunks(b"BG44");
        if !bg44.is_empty() {
            return bg44;
        }
        let bm44 = self.find_chunks(b"BM44");
        if !bm44.is_empty() {
            return bm44;
        }
        self.find_chunks(b"PM44")
    }

    /// The render-tier layer cache for this page (decoded on first render).
    ///
    /// The page holds the handle; the decode logic, the cached forms, and the
    /// compositor-subsampling concern all live in
    /// [`crate::djvu_render::PageLayers`].
    #[cfg(feature = "std")]
    pub(crate) fn render_layers(&self) -> &crate::djvu_render::PageLayers {
        let layers = self.render_layers.get_or_init(|| {
            // Held behind an `Arc` so `crate::render_cache` can keep a weak
            // reference and sweep this cache when the process goes over its
            // ceiling. Two threads racing here both build one; the loser is
            // dropped and its weak entry is pruned by the next sweep.
            let layers = Arc::new(crate::djvu_render::PageLayers::new());
            crate::render_cache::register(&layers);
            layers
        });
        // Stamp the page's LRU access tick so `enforce_cache_budget` can evict
        // the least-recently-rendered pages first. The process-wide governor
        // ranks individual layers by their own ticks instead (#813).
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
    /// full-resolution case) via `decoded_bg_rgb_s1`;
    /// other subsample levels recompute the conversion each call.
    #[cfg(feature = "std")]
    pub fn decoded_bg44(&self) -> Option<Arc<Iw44Image>> {
        self.render_layers().bg44(self)
    }

    #[cfg(not(feature = "std"))]
    pub fn decoded_bg44(&self) -> Option<Arc<Iw44Image>> {
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
    pub fn decoded_bg44_partial(&self) -> Option<Arc<Iw44Image>> {
        self.render_layers().bg44_partial(self)
    }

    #[cfg(not(feature = "std"))]
    pub fn decoded_bg44_partial(&self) -> Option<Arc<Iw44Image>> {
        None
    }

    /// This page's cached RGB conversion for a `subsample > 4` render, when
    /// the slot holds exactly that subsample. Never decodes.
    #[cfg(feature = "std")]
    pub(crate) fn cached_bg_rgb_subhi(&self, subsample: u32) -> Option<Arc<Pixmap>> {
        self.render_layers.get()?.bg_rgb_subhi(subsample)
    }

    /// Memoise the RGB conversion for a `subsample > 4` render. The first
    /// subsample a page is rendered at wins; later ones reconvert.
    #[cfg(feature = "std")]
    pub(crate) fn store_bg_rgb_subhi(&self, subsample: u32, px: Arc<Pixmap>) {
        self.render_layers().store_bg_rgb_subhi(subsample, px);
    }

    /// This page's first-chunk BG44 image **only if it is already cached** —
    /// never decodes. See `PageLayers::bg44_partial_cached` for why the
    /// subsample > 4 render path peeks instead of memoising.
    #[cfg(feature = "std")]
    pub(crate) fn cached_bg44_partial(&self) -> Option<Arc<Iw44Image>> {
        self.render_layers.get()?.bg44_partial_cached()
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

    /// Decode the foreground mask directly at 1/4 resolution (2 bits shifted
    /// off each axis), OR-reducing (max-pooling) instead of allocating a
    /// full-resolution [`Bitmap`] and downsampling it afterward.
    ///
    /// Bit-for-bit identical to `downsample_mask_4x(extract_mask()?)` (see the
    /// `djvu-jb2` crate's `decode_downsampled` equivalence tests and
    /// `mask_sub4_matches_extract_mask_then_downsample` below) — it exists to
    /// skip the full-resolution JB2 canvas allocation for callers that only
    /// ever need the coarse mask (the thumbnail / heavy-downscale compositor
    /// path, [`crate::djvu_render::PageLayers::mask_sub4`]).
    ///
    /// Returns `Ok(None)` if the page has neither an Sjbz nor an Smmr chunk.
    ///
    /// `std`-only: its only caller, [`crate::djvu_render::PageLayers`], is
    /// itself `std`-only (it caches decoded layers behind `std::sync::OnceLock`),
    /// and the Smmr fallback below reuses the `std`-only
    /// [`crate::djvu_render::downsample_mask_4x`].
    #[cfg(feature = "std")]
    pub(crate) fn extract_mask_sub4(&self) -> Result<Option<crate::bitmap::Bitmap>, DocError> {
        if let Some(sjbz) = self.find_chunk(b"Sjbz") {
            let inline_dict;
            let dict_ref = if let Some(djbz) = self.find_chunk(b"Djbz") {
                inline_dict = crate::jb2::decode_dict(djbz, None)?;
                Some(&inline_dict)
            } else {
                self.decoded_shared_dict()
            };
            let bm = crate::jb2::decode_downsampled(sjbz, dict_ref, 2)?;
            return Ok(Some(bm));
        }
        if let Some(smmr) = self.find_chunk(b"Smmr") {
            // No reduced-scale G4/MMR decoder exists; fall back to a full
            // decode + the same max-pool reduction `mask_sub4` would apply.
            // Smmr masks are rare in practice (Sjbz is the common case).
            let bm = crate::smmr::decode_smmr(smmr).map_err(|e| DocError::Smmr(e.to_string()))?;
            return Ok(Some(crate::djvu_render::downsample_mask_4x(&bm)));
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
    /// page's `PageLayers` cache) so that repeated renders of
    /// the same page — e.g. at different DPI levels — do not re-run the ZP
    /// arithmetic + symbol decode.
    ///
    /// Returns `None` if the page has no Sjbz chunk or if decoding fails.
    #[cfg(feature = "std")]
    pub fn decoded_mask(&self) -> Option<Arc<crate::bitmap::Bitmap>> {
        self.render_layers().mask(self)
    }

    #[cfg(not(feature = "std"))]
    pub fn decoded_mask(&self) -> Option<Arc<crate::bitmap::Bitmap>> {
        None
    }

    /// Return the decoded FG44 foreground color layer, decoding and caching on
    /// first call.  Subsequent renders reuse the cached `Pixmap`.
    ///
    /// Returns `None` if the page has no FG44 chunks or if decoding fails.
    #[cfg(feature = "std")]
    pub fn decoded_fg44(&self) -> Option<Arc<Pixmap>> {
        self.render_layers().fg44(self)
    }

    #[cfg(not(feature = "std"))]
    pub fn decoded_fg44(&self) -> Option<Arc<Pixmap>> {
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
    pub(crate) fn decoded_bg_rgb_s1(&self) -> Option<Arc<Pixmap>> {
        self.render_layers().bg_rgb_s1(self)
    }

    #[cfg(not(feature = "std"))]
    pub(crate) fn decoded_bg_rgb_s1(&self) -> Option<Arc<Pixmap>> {
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
    pub(crate) fn decoded_bg_rgb_s2(&self) -> Option<Arc<Pixmap>> {
        self.render_layers().bg_rgb_s2(self)
    }

    #[cfg(not(feature = "std"))]
    pub(crate) fn decoded_bg_rgb_s2(&self) -> Option<Arc<Pixmap>> {
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
    pub(crate) fn decoded_bg_rgb_s4(&self) -> Option<Arc<Pixmap>> {
        self.render_layers().bg_rgb_s4(self)
    }

    #[cfg(not(feature = "std"))]
    pub(crate) fn decoded_bg_rgb_s4(&self) -> Option<Arc<Pixmap>> {
        None
    }

    /// Return the decoded JB2 mask + per-pixel blit-index map for FGbz-palette
    /// pages, decoding and caching on first call.
    ///
    /// Caches both the JB2 ZP arithmetic decode and the page-sized blit map so
    /// that repeated palette renders pay neither cost after the first call.
    /// Returns `None` if the page has no Sjbz/Smmr chunk or decoding fails.
    #[cfg(feature = "std")]
    pub(crate) fn decoded_mask_indexed(&self) -> Option<Arc<crate::djvu_render::IndexedMask>> {
        self.render_layers().mask_indexed(self)
    }

    #[cfg(not(feature = "std"))]
    pub(crate) fn decoded_mask_indexed(&self) -> Option<Arc<crate::djvu_render::IndexedMask>> {
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
    /// - [`crate::djvu_render::RenderError::UnsupportedOption`] if the options
    ///   need a whole pixmap (see [`RenderRequest::write_rgba`](crate::djvu_render::RenderRequest::write_rgba))
    /// - Propagates IW44 / JB2 decode errors
    pub fn render_into(
        &self,
        opts: &crate::djvu_render::RenderOptions,
        buf: &mut [u8],
    ) -> Result<(), crate::djvu_render::RenderError> {
        crate::djvu_render::RenderRequest::new(opts.clone())
            .operation("render_into")
            .write_rgba(self, buf)
    }
}
