//! The tile cache and the per-page decoded-layer cache.

use super::*;

/// Tile edge length (pixels) used by [`render_region_tiled`]'s composited-
/// output cache. 256 keeps a full RGBA tile at 256 KiB — small enough that a
/// pan/zoom viewer's working set (a screenful of tiles) stays a few MB, large
/// enough that the per-tile `HashMap`/`Mutex` overhead is negligible next to
/// the compositor work it avoids repeating.
pub(super) const TILE_SIZE: u32 = 256;

/// Default per-page byte budget for the composited-tile cache — about 32
/// full tiles (256×256×4 B = 256 KiB each). Independent of, but counted
/// towards, [`PageLayers::cached_bytes`] / `DjVuDocument::enforce_cache_budget`:
/// a document-wide budget sweep evicts whole pages (tiles included), while
/// this bound keeps one page's own pan history from growing unboundedly
/// between sweeps. Overridable per page via
/// [`crate::djvu_tile::set_tile_cache_budget`] (#691 slice 2).
pub(crate) const TILE_CACHE_MAX_BYTES: usize = 8 * 1024 * 1024;

/// Composited-output tile cache key: the [`RenderOptions`] fields a tile's
/// pixels depend on, plus the tile's top-left corner in the unrotated output
/// page ([`unrotated_size`]).
///
/// Rotation runs after the tiles are assembled, and permissive decode matches
/// strict decode on every page a strict request can reach the cache for, so
/// neither is in the key. Any option that can change a tile's bytes must be
/// part of this key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct TileKey {
    /// The composited canvas (`opts.width × opts.height`, min 1). Under
    /// anti-aliasing the output page is half of it, so two canvas sizes can
    /// share an output size; the canvas tells them apart.
    pub(super) canvas: (u32, u32),
    /// The tile's top-left corner in the output page.
    pub(super) x: u32,
    pub(super) y: u32,
    pub(super) bold: u8,
    pub(super) mask_aa: bool,
    /// [`aa_halves`]: the output page is the canvas averaged 2×2.
    pub(super) aa: bool,
    /// [`lanczos_rescales`]: the output page is the native page rescaled.
    pub(super) lanczos: bool,
}

impl TileKey {
    /// The output page this tile belongs to.
    pub(super) fn page_size(&self) -> (u32, u32) {
        if self.aa {
            ((self.canvas.0 / 2).max(1), (self.canvas.1 / 2).max(1))
        } else {
            self.canvas
        }
    }
}

/// One cached composited tile: `w × h` (≤ [`TILE_SIZE`], smaller at the
/// page's right/bottom edge) RGBA bytes, row-major, stride `w * 4`.
pub(super) struct TileEntry {
    pub(super) w: u32,
    pub(super) h: u32,
    pub(super) data: Vec<u8>,
}

/// [`PageLayers`]'s composited-tile store: the tile map, FIFO insertion order
/// for eviction, and the running byte total (avoids re-summing on every
/// insert/evict).
#[derive(Default)]
pub(super) struct TileCacheState {
    pub(super) map: std::collections::HashMap<TileKey, std::sync::Arc<TileEntry>>,
    pub(super) order: std::collections::VecDeque<TileKey>,
    pub(super) bytes: usize,
    /// Per-page budget override (#691 slice 2); `None` means
    /// [`TILE_CACHE_MAX_BYTES`]. Kept as an `Option` so `derive(Default)`
    /// stays valid and "still on the default" remains observable.
    pub(super) budget: Option<usize>,
    /// Last-used tick from [`ACCESS_TICK`], stamped on every hit and insert,
    /// so the governor can rank the tile store against the decoded layers.
    pub(super) tick: u64,
    /// Hit/miss/eviction telemetry (#576). Test-only so the release lock
    /// section stays exactly as cheap as before.
    #[cfg(test)]
    pub(super) hits: usize,
    #[cfg(test)]
    pub(super) misses: usize,
    #[cfg(test)]
    pub(super) evictions: usize,
}

impl TileCacheState {
    /// The budget this cache currently enforces (override or default).
    pub(super) fn effective_budget(&self) -> usize {
        self.budget.unwrap_or(TILE_CACHE_MAX_BYTES)
    }

    /// Evict oldest-first until `bytes` is back under the effective budget.
    pub(super) fn evict_to_budget(&mut self) {
        while self.bytes > self.effective_budget() {
            match self.order.pop_front() {
                Some(old_key) => {
                    if let Some(old) = self.map.remove(&old_key) {
                        self.bytes = self.bytes.saturating_sub(old.data.len());
                        #[cfg(test)]
                        {
                            self.evictions += 1;
                        }
                    }
                }
                None => break,
            }
        }
    }
}

/// Render-tier cache of a page's decoded layers.
///
/// These are the decoded wavelet / bitmap forms the compositor consumes —
/// background (`bg44`, plus the first-chunk-only `bg44_partial` used at
/// subsample ≥ 4), the JB2 mask (`mask`), its quarter-resolution max-pool
/// downsample (`mask_sub4`, a pure compositor concern), and the FG44 colour
/// layer (`fg44`). They live here in the render tier rather than on
/// [`DjVuPage`] so the page model stays close to its raw bytes and every
/// render concern — including compositor subsampling — concentrates in one
/// place.
///
/// Each layer is decoded lazily and cached, so repeated renders of the same
/// page (e.g. thumbnails then full resolution) reuse the expensive ZP
/// arithmetic decode. A `DjVuPage` holds one of these behind a `OnceLock`;
/// the accessors borrow the page only to decode on a cache miss, so the
/// returned reference is tied to the cache, not the call.
/// Cached decoded ANTz payload (#605): the parsed annotation record plus its
/// map areas, shared behind an `Arc` between the per-page cache and callers.
pub(crate) type SharedAnnotations = std::sync::Arc<(
    crate::annotation::Annotation,
    Vec<crate::annotation::MapArea>,
)>;

/// A memoisation slot that can also be **cleared through a shared borrow**.
///
/// `std::sync::OnceLock` can be filled through `&self` but only emptied through
/// `&mut self`, and every render entry point holds `&DjVuPage`. So a render
/// could grow the page cache and nothing could shrink it: the whole
/// cache-budget API (`enforce_cache_budget`, `retain_render_caches`,
/// `evict_render_cache`) needed `&mut self`, could not run from inside a
/// render, and was therefore opt-in — leaving the read path unbounded by
/// default at ~6.1 MB per rendered page. See PERF_EXPERIMENTS.md
/// READ_CACHE_BOUNDED.
///
/// This slot keeps the value behind an `RwLock` and hands out `Arc` clones. An
/// eviction drops the cache's handle under `&self`; a render still holding a
/// clone keeps its copy alive until it finishes, so eviction is never visible
/// as a dangling or half-freed layer.
///
/// The outer `Option` is "has this been computed?", the inner one is "did the
/// computation produce a value?" — a decode that legitimately yields `None`
/// (no such chunk on this page) is memoised as a miss, exactly as the
/// `OnceLock<Option<T>>` it replaces did.
///
/// Each slot is one evictable layer to the process-wide governor (#813,
/// PERF_EXPERIMENTS.md RENDER_CACHE_LAYER_EVICT): it records its own resident
/// size the moment it fills and stamps itself with the global tick on every
/// use, so a sweep can rank layers across pages and drop the stalest one
/// without touching the others on the same page.
pub(crate) struct CacheSlot<T> {
    pub(super) inner: std::sync::RwLock<Option<Option<std::sync::Arc<T>>>>,
    /// How to measure a stored value. Fixed at construction so the slot can
    /// record its size when it fills rather than re-measure on every sweep.
    pub(super) size: fn(&T) -> usize,
    /// Resident bytes of the stored value; 0 when empty or a memoised miss.
    /// Kept beside the lock, not behind it, so the byte accounting and the
    /// governor read it without contending with a decode in progress.
    pub(super) bytes: std::sync::atomic::AtomicUsize,
    /// Last-used tick from [`ACCESS_TICK`]: stamped on every hit, fill and
    /// store. Higher is more recent. A `peek` that finds nothing leaves it.
    pub(super) tick: std::sync::atomic::AtomicU64,
}

impl<T> CacheSlot<T> {
    /// An empty slot whose values are measured by `size`.
    pub(crate) fn new(size: fn(&T) -> usize) -> Self {
        Self {
            inner: std::sync::RwLock::new(None),
            size,
            bytes: std::sync::atomic::AtomicUsize::new(0),
            tick: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Stamp the slot as just used.
    pub(super) fn touch(&self) {
        let t = ACCESS_TICK.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.tick.store(t, std::sync::atomic::Ordering::Relaxed);
    }

    /// Store `value` under an already-held write lock, recording its size.
    pub(super) fn store(
        &self,
        w: &mut Option<Option<std::sync::Arc<T>>>,
        value: Option<std::sync::Arc<T>>,
    ) {
        let bytes = value.as_deref().map_or(0, self.size);
        self.bytes
            .store(bytes, std::sync::atomic::Ordering::Relaxed);
        *w = Some(value);
        self.touch();
    }

    pub(super) fn read(&self) -> std::sync::RwLockReadGuard<'_, Option<Option<std::sync::Arc<T>>>> {
        self.inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(super) fn write(
        &self,
    ) -> std::sync::RwLockWriteGuard<'_, Option<Option<std::sync::Arc<T>>>> {
        self.inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The cached value, without ever running the initialiser.
    pub(crate) fn peek(&self) -> Option<std::sync::Arc<T>> {
        let v = self.read().as_ref()?.clone();
        if v.is_some() {
            self.touch();
        }
        v
    }

    /// Whether the slot has been computed (even to a cached `None`).
    pub(crate) fn is_computed(&self) -> bool {
        self.read().is_some()
    }

    /// The cached value, computing it with `init` on the first call.
    ///
    /// `init` runs **outside** the lock, so a slow decode never blocks a
    /// concurrent reader and an initialiser may read other slots without
    /// risking a deadlock. The cost is that two threads racing on a cold slot
    /// can both decode; the first to store wins and both callers get that one
    /// value, so the result is still a single shared copy. Decoding is pure, so
    /// the duplicate work is wasted time, never a different answer.
    pub(crate) fn get_or_init(
        &self,
        init: impl FnOnce() -> Option<T>,
    ) -> Option<std::sync::Arc<T>> {
        if let Some(v) = self.read().as_ref() {
            self.touch();
            return v.clone();
        }
        let computed = init().map(std::sync::Arc::new);
        let mut w = self.write();
        if let Some(v) = w.as_ref() {
            self.touch();
            return v.clone();
        }
        self.store(&mut w, computed.clone());
        computed
    }

    /// Store `value` if the slot is still empty; keep the existing entry
    /// otherwise. Mirrors `OnceLock::set` — the first writer wins.
    pub(crate) fn set_if_empty(&self, value: Option<T>) {
        let mut w = self.write();
        if w.is_none() {
            self.store(&mut w, value.map(std::sync::Arc::new));
        }
    }

    /// Like [`set_if_empty`](Self::set_if_empty) for a value the caller already
    /// holds behind an `Arc` (the text and annotation trees are shared with
    /// their parsers).
    pub(crate) fn set_if_empty_arc(&self, value: Option<std::sync::Arc<T>>) {
        let mut w = self.write();
        if w.is_none() {
            self.store(&mut w, value);
        }
    }

    /// Drop the cached value, reclaiming its memory. The slot goes back to
    /// "not computed", so the next access decodes again.
    pub(crate) fn clear(&self) {
        let mut w = self.write();
        *w = None;
        self.bytes.store(0, std::sync::atomic::Ordering::Relaxed);
    }

    /// Resident bytes held by the cached value, as recorded when it was
    /// stored. Never computes and never locks.
    pub(crate) fn bytes(&self) -> usize {
        self.bytes.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// One evictable unit of a page cache, as the process-wide governor sees it
/// (#813). Every [`CacheSlot`] is one, and so is the composited-tile store,
/// which the governor treats as a single layer with the tick of its last hit.
pub(crate) trait CacheLayer {
    /// Last-used tick from [`ACCESS_TICK`]; higher is more recent.
    fn last_used(&self) -> u64;
    /// Resident bytes, 0 when empty.
    fn resident_bytes(&self) -> usize;
    /// Drop the cached data through a shared borrow. The next access rebuilds
    /// it; a reader that already holds a handle is unaffected.
    fn drop_cached(&self);
}

impl<T> CacheLayer for CacheSlot<T> {
    fn last_used(&self) -> u64 {
        self.tick.load(std::sync::atomic::Ordering::Relaxed)
    }
    fn resident_bytes(&self) -> usize {
        self.bytes()
    }
    fn drop_cached(&self) {
        self.clear();
    }
}

impl CacheLayer for std::sync::Mutex<TileCacheState> {
    fn last_used(&self) -> u64 {
        self.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .tick
    }
    fn resident_bytes(&self) -> usize {
        self.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .bytes
    }
    /// Tiles go, but a per-page budget override survives — it is
    /// configuration, not cached data (same rule as `downgrade`).
    fn drop_cached(&self) {
        let mut tiles = self
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *tiles = TileCacheState {
            budget: tiles.budget,
            ..TileCacheState::default()
        };
    }
}

pub(crate) struct PageLayers {
    pub(super) bg44: CacheSlot<Iw44Image>,
    pub(super) bg44_partial: CacheSlot<Iw44Image>,
    pub(super) mask: CacheSlot<crate::bitmap::Bitmap>,
    pub(super) mask_sub4: CacheSlot<crate::bitmap::Bitmap>,
    pub(super) fg44: CacheSlot<Pixmap>,
    // Full-resolution (subsample=1) RGB Pixmap derived from bg44. Cached so
    // repeated renders of the same page skip the 2–3 ms IW44 IDWT + YCbCr→RGB
    // conversion. Populated on first full-resolution render; left empty on
    // pages that are never rendered at sub=1 (e.g. thumbnails only).
    pub(super) bg_rgb_s1: CacheSlot<Pixmap>,
    // Half-resolution (subsample=2) RGB Pixmap derived from bg44, for the common
    // 150-from-300-DPI render. Same memoization as `bg_rgb_s1` but ~4× smaller;
    // left empty on pages never rendered at sub=2.
    pub(super) bg_rgb_s2: CacheSlot<Pixmap>,
    // Quarter-resolution (subsample=4) RGB Pixmap derived from the *partial*
    // bg44 (first chunk only, matching the sub>=4 decode path). Caches the
    // IDWT + YCbCr->RGB conversion for the common thumbnail / heavy-downscale
    // render (e.g. 150-from-400-DPI, contact-sheet zoom); ~16x smaller than the
    // sub=1 cache. Left empty on pages never rendered at sub=4.
    pub(super) bg_rgb_s4: CacheSlot<Pixmap>,
    // Decoded JB2 mask + per-pixel blit-index map for FGbz-palette pages. The
    // plain `mask` cache does not cover the indexed variant, so without this
    // every warm render of a palette page re-runs the full JB2 ZP decode and
    // re-allocates the page-sized blit map. Only populated for palette pages.
    // THUMB_PARTIAL_MEMO: the converted RGB for the first subsample > 4 this
    // page was rendered at, stored as `(subsample, pixmap)`. Thumbnail grids
    // and zoomed-out pans land here, and they land on the *same* subsample for
    // a given page, so one slot serves them. Tiny — ~90 KB for a 128 px
    // thumbnail of a colorbook.djvu page — and it lets the sub > 4 path stop
    // memoising the full-size `bg44_partial` coefficient image (5.75 MB) it
    // derives from. A render at a different sub > 4 misses and reconverts.
    pub(super) bg_rgb_subhi: CacheSlot<(u32, Arc<Pixmap>)>,
    pub(super) mask_indexed: CacheSlot<IndexedMask>,
    // Decoded page metadata (#605): the TXTz/ANTz payloads are BZZ-compressed
    // and rebuilt into full zone/annotation trees on every access, yet viewers
    // ask for the same metadata repeatedly (search, selection, link overlays).
    // Cached behind `Arc` so warm accesses share one decode. Only populated
    // for pages whose metadata is actually touched; parse *errors* are not
    // cached (malformed chunks keep erroring per call, unchanged behaviour).
    pub(super) text_layer: CacheSlot<crate::text::TextLayer>,
    pub(super) annotations: CacheSlot<(
        crate::annotation::Annotation,
        Vec<crate::annotation::MapArea>,
    )>,
    /// Resident bytes this cache last reported to the process-wide total
    /// (see [`crate::render_cache`]). Re-measuring every registered page on
    /// every cache fill would be O(pages); instead each fill re-measures only
    /// its own page and folds the change into one global counter, so the
    /// common path stays O(1) and the governor sweeps only when the total is
    /// actually over budget.
    pub(super) reported: std::sync::atomic::AtomicUsize,
    /// Monotonic last-access tick for LRU cache-budget eviction. Bumped from a
    /// process-global counter every time this page's layers are touched; read
    /// (without touching) by `DjVuDocument::enforce_cache_budget` to evict the
    /// least-recently-used pages first. Not part of the decoded data.
    pub(super) access: std::sync::atomic::AtomicU64,
    /// C4_TILE_CACHE: composited-output tiles for [`render_region_tiled`],
    /// keyed by `(full_w, full_h, tile_x, tile_y, bold, mask_aa)`. Unlike the
    /// layers above (which cache *decoded* data), these cache the *compositor's
    /// output* — the per-pixel work `composite_into` repeats on every call is
    /// not memoized anywhere else. Bounded to `TILE_CACHE_MAX_BYTES` per page
    /// with FIFO eviction; counted in [`cached_bytes`](Self::cached_bytes) so
    /// it shares the page's C5 byte-budget accounting, and dropped whenever
    /// this whole `PageLayers` is (`evict_render_cache`).
    pub(super) tile_cache: std::sync::Mutex<TileCacheState>,
}

impl Drop for PageLayers {
    /// Take this cache's bytes out of the process-wide total (see
    /// [`crate::render_cache`]). Dropping the page is the one path that frees
    /// cached layers without going through `evict_shared`.
    fn drop(&mut self) {
        let reported = *self.reported.get_mut();
        if reported > 0 {
            crate::render_cache::adjust_resident(reported, 0);
        }
    }
}

/// Process-global monotonic source for the LRU access ticks: one per page
/// (`PageLayers::access`, read by the per-document sweep) and one per layer
/// (`CacheSlot::tick`, read by the process-wide governor, #813).
pub(super) static ACCESS_TICK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Decode the first BG44 chunk of `page` into a fresh `Iw44Image`.
///
/// The body of [`PageLayers::bg44_partial`]'s initialiser, factored out so the
/// subsample > 4 path can produce the same image without memoising it (see
/// [`PageLayers::bg44_partial_cached`]).
pub(super) fn decode_bg44_partial(page: &DjVuPage) -> Option<Iw44Image> {
    let chunks = page.bg44_chunks();
    if chunks.is_empty() {
        return None;
    }
    let mut img = Iw44Image::new();
    if img.decode_chunk(chunks[0]).is_err() {
        return None;
    }
    if img.width == 0 {
        return None;
    }
    // Same dimension cross-check as `PageLayers::bg44`.
    if !iw44_reduction_is_legal(
        page.width() as u32,
        page.height() as u32,
        img.width,
        img.height,
    ) {
        return None;
    }
    Some(img)
}

impl PageLayers {
    /// An empty cache. Layers are decoded on first access.
    ///
    /// Every pixel layer is measured from the `Vec` it owns, not estimated:
    /// [`DjVuDocument::enforce_cache_budget`](crate::djvu_document::DjVuDocument::enforce_cache_budget)
    /// and the process-wide governor turn these numbers into a memory ceiling,
    /// so an estimate here becomes a wrong ceiling for the caller. The BG44
    /// coefficient images used to be sized as `w·h·2`, which counts the luma
    /// plane and drops the two chroma planes a colour page also keeps — the
    /// whole cache reported at ~38 % of the truth, and a 16 MiB budget held
    /// ~52 MB (PERF_EXPERIMENTS.md DECODE_CACHE_ACCOUNTING; guarded by
    /// `tests/decode_cache_accounting.rs`). The text/annotation trees stay
    /// approximate — they are node counts, not buffers, and are small next to
    /// the pixel caches.
    pub(crate) fn new() -> Self {
        Self {
            bg44: CacheSlot::new(Iw44Image::heap_bytes),
            bg44_partial: CacheSlot::new(Iw44Image::heap_bytes),
            mask: CacheSlot::new(|b| b.data.len()),
            mask_sub4: CacheSlot::new(|b| b.data.len()),
            fg44: CacheSlot::new(|p| p.data.len()),
            bg_rgb_s1: CacheSlot::new(|p| p.data.len()),
            bg_rgb_s2: CacheSlot::new(|p| p.data.len()),
            bg_rgb_s4: CacheSlot::new(|p| p.data.len()),
            bg_rgb_subhi: CacheSlot::new(|(_, p)| p.data.len()),
            mask_indexed: CacheSlot::new(|(b, v)| b.data.len() + v.len() * 4),
            // Metadata caches (#605): approximate — text bytes + a fixed cost
            // per zone/map-area node.
            text_layer: CacheSlot::new(|t| t.text.len() + count_zones(&t.zones) * 64),
            annotations: CacheSlot::new(|a| a.1.len() * 96 + 64),
            reported: std::sync::atomic::AtomicUsize::new(0),
            access: std::sync::atomic::AtomicU64::new(0),
            tile_cache: std::sync::Mutex::new(TileCacheState::default()),
        }
    }

    /// The number of layers [`layers`](Self::layers) returns.
    pub(crate) const LAYER_COUNT: usize = 13;

    /// Every layer of this cache as the governor sees it (#813): the twelve
    /// decoded/derived slots and the composited-tile store as the thirteenth.
    /// Order is fixed but carries no meaning; the sweep ranks by tick.
    pub(crate) fn layers(&self) -> [&dyn CacheLayer; Self::LAYER_COUNT] {
        [
            &self.bg44,
            &self.bg44_partial,
            &self.mask,
            &self.mask_sub4,
            &self.fg44,
            &self.bg_rgb_s1,
            &self.bg_rgb_s2,
            &self.bg_rgb_s4,
            &self.bg_rgb_subhi,
            &self.mask_indexed,
            &self.text_layer,
            &self.annotations,
            &self.tile_cache,
        ]
    }

    /// Record an access, stamping this cache with the next global tick (LRU).
    pub(crate) fn bump_access(&self) {
        let t = ACCESS_TICK.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.access.store(t, std::sync::atomic::Ordering::Relaxed);
    }

    /// The last-access tick (higher = more recently used).
    pub(crate) fn access_tick(&self) -> u64 {
        self.access.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Resident bytes held by this page's decoded caches.
    ///
    /// The sum of what every layer recorded when it filled (see
    /// [`new`](Self::new) for how each is measured). Reads no lock but the
    /// tile store's, and never initialises anything.
    pub(crate) fn cached_bytes(&self) -> usize {
        self.layers().iter().map(|l| l.resident_bytes()).sum()
    }

    /// C5_COMPRESS: drop the expensive full-resolution derivations —
    /// `bg44`/`bg44_partial` coefficient images, `bg_rgb_s1`, mask +
    /// `mask_sub4`, `fg44`, `mask_indexed`, and the composited-tile cache —
    /// while **preserving** any already-cached `bg_rgb_s2` / `bg_rgb_s4`
    /// downscaled RGB pixmap.
    ///
    /// This is the cheaper middle tier between "keep everything" and
    /// [`evict_render_cache`](DjVuPage::evict_render_cache)'s full drop: a
    /// later downscaled render (subsample ≥ 2 — e.g. a thumbnail, contact
    /// sheet, or zoomed-out pan) stays warm, while a full-resolution render
    /// still pays a cold decode. Measured (see PERF_EXPERIMENTS.md
    /// C5_COMPRESS): the coefficient `Iw44Image` retained by `bg44` is *not*
    /// cheaper to keep than the derived RGB pixmap (same size class once
    /// colour planes are counted), so there is no cheap "upgrade sub=2→sub=1"
    /// path — only the already-decoded downscaled pixmaps are worth keeping.
    /// No-op on fields that were never populated.
    pub(crate) fn downgrade(&self) {
        self.bg44.clear();
        self.bg44_partial.clear();
        self.mask.clear();
        // mask_sub4 intentionally preserved (#607): ~1/16 of the packed mask
        // bytes keeps sub>=4 re-renders (thumbnails, zoomed-out pans) warm
        // without re-running the JB2 arithmetic decode. `decode_layers`
        // consults it before forcing a full mask decode.
        self.fg44.clear();
        self.mask_indexed.clear();
        self.bg_rgb_s1.clear();
        // Tiles are dropped, but a per-page budget override (#691 slice 2)
        // survives the downgrade — it is configuration, not cached data.
        self.tile_cache.drop_cached();
        // bg_rgb_s2 / bg_rgb_s4 / bg_rgb_subhi / access tick intentionally
        // preserved — all three are the cheap downscaled tiers a later
        // zoomed-out render reuses.
        self.report_bytes();
    }

    /// Drop every cached layer through a shared borrow.
    ///
    /// This is [`DjVuPage::evict_render_cache`]'s whole body. Dropping the
    /// `PageLayers` itself needs `&mut DjVuPage`, which no render path has;
    /// emptying each slot needs only `&self` (see [`CacheSlot`]) and reclaims
    /// the same memory — the struct that stays behind is a few hundred bytes
    /// of empty locks.
    ///
    /// A render that already holds a layer keeps it until it finishes; the
    /// next access decodes again.
    pub(crate) fn evict_shared(&self) {
        for layer in self.layers() {
            layer.drop_cached();
        }
        self.report_bytes();
    }

    /// Re-measure this cache and fold the change into the process-wide total.
    ///
    /// Call it after anything that grows or shrinks the cache. Returns the new
    /// process-wide total.
    pub(crate) fn report_bytes(&self) -> usize {
        use std::sync::atomic::Ordering;
        let now = self.cached_bytes();
        let prev = self.reported.swap(now, Ordering::AcqRel);
        crate::render_cache::adjust_resident(prev, now)
    }

    /// Fill `slot` through `init`, then keep the byte accounting current.
    ///
    /// Every layer accessor goes through here, so one place both memoises the
    /// decode and tells the governor the cache grew (READ_CACHE_BOUNDED). The
    /// governor may then drop any layer but the one just filled — including
    /// another layer of this page, if it is the stalest in the process (#813).
    pub(super) fn fill<T>(
        &self,
        slot: &CacheSlot<T>,
        init: impl FnOnce() -> Option<T>,
    ) -> Option<Arc<T>> {
        let was_computed = slot.is_computed();
        let value = slot.get_or_init(init);
        if !was_computed {
            let total = self.report_bytes();
            crate::render_cache::sweep_if_over(total, Self::layer_id(slot));
        }
        value
    }

    /// The address the governor uses to recognise the layer being filled.
    pub(super) fn layer_id<L: CacheLayer>(layer: &L) -> *const () {
        (layer as *const L).cast()
    }

    /// Bytes currently held by the composited-tile cache (see `tile_cache`).
    pub(crate) fn tile_cache_bytes(&self) -> usize {
        self.tile_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .bytes
    }

    /// Look up a cached composited tile, cloning the `Arc` handle on a hit.
    ///
    /// LRU (#576): a hit moves the key to the back of the eviction order.
    /// Under a back-and-forth pan — the classic reading pattern — FIFO evicts
    /// exactly the tiles about to be reused; move-to-back keeps them. The
    /// order deque holds ≤ `TILE_CACHE_MAX_BYTES / tile_bytes` ≈ 32 keys, so
    /// the linear reposition is a few dozen comparisons per hit.
    pub(super) fn get_tile(&self, key: TileKey) -> Option<std::sync::Arc<TileEntry>> {
        let mut state = self
            .tile_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let hit = state.map.get(&key).cloned();
        #[cfg(test)]
        {
            if hit.is_some() {
                state.hits += 1;
            } else {
                state.misses += 1;
            }
        }
        if hit.is_some()
            && let Some(pos) = state.order.iter().position(|k| *k == key)
        {
            state.order.remove(pos);
            state.order.push_back(key);
            state.tick = ACCESS_TICK.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        hit
    }

    /// Insert a freshly composited tile, evicting the oldest tiles (FIFO)
    /// until back under the page's effective tile-cache budget.
    pub(super) fn insert_tile(&self, key: TileKey, entry: std::sync::Arc<TileEntry>) {
        let mut state = self
            .tile_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.map.contains_key(&key) {
            return;
        }
        state.bytes += entry.data.len();
        state.map.insert(key, entry);
        state.order.push_back(key);
        state.tick = ACCESS_TICK.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        state.evict_to_budget();
        drop(state);
        // Tiles are bounded per page already, but they still count against the
        // process-wide ceiling (READ_CACHE_BOUNDED).
        let total = self.report_bytes();
        crate::render_cache::sweep_if_over(total, Self::layer_id(&self.tile_cache));
    }

    /// The tile-cache budget this page currently enforces (#691 slice 2).
    pub(crate) fn tile_cache_budget(&self) -> usize {
        self.tile_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .effective_budget()
    }

    /// Number of composited tiles currently cached (#691 slice 2).
    pub(crate) fn tile_cache_len(&self) -> usize {
        self.tile_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .map
            .len()
    }

    /// Override this page's tile-cache byte budget, evicting oldest-first
    /// down to the new bound immediately (#691 slice 2). A budget of `0`
    /// effectively disables composited-tile caching for the page.
    pub(crate) fn set_tile_cache_budget(&self, max_bytes: usize) {
        let mut state = self
            .tile_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.budget = Some(max_bytes);
        state.evict_to_budget();
    }

    /// Drop every cached composited tile, returning the bytes freed
    /// (#691 slice 2). The budget override, if any, is kept.
    pub(crate) fn clear_tile_cache(&self) -> usize {
        let mut state = self
            .tile_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let freed = state.bytes;
        state.map.clear();
        state.order.clear();
        state.bytes = 0;
        freed
    }

    /// Drop every cached composited tile that intersects `rect`, where `rect`
    /// is given in the pre-rotation pixel space of a `canvas_w × canvas_h`
    /// output page (#691 slice 2). Cached tiles belonging to *other* output
    /// sizes are matched by scaling the rect proportionally (outward, so a
    /// boundary-straddling tile is always dropped rather than kept). Returns
    /// the bytes freed.
    ///
    /// A Lanczos-3 pixel reads the native page up to 3 pixels of the smaller
    /// of the two scales away, so for Lanczos-3 tiles the scaled rect grows by
    /// that reach on every side. `native` is the page's native size.
    pub(crate) fn remove_tiles_intersecting(
        &self,
        rect: RenderRect,
        canvas_w: u32,
        canvas_h: u32,
        native: (u32, u32),
    ) -> usize {
        if canvas_w == 0 || canvas_h == 0 || rect.width == 0 || rect.height == 0 {
            return 0;
        }
        let mut state = self
            .tile_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut freed = 0usize;
        let doomed: Vec<TileKey> = state
            .map
            .iter()
            .filter(|(key, entry)| {
                // Scale the rect into this entry's (fw × fh) output space,
                // rounding outward (floor start, ceil end).
                let (fw, fh) = key.page_size();
                let x0 = u64::from(rect.x) * u64::from(fw) / u64::from(canvas_w);
                let x1 = ((u64::from(rect.x) + u64::from(rect.width)) * u64::from(fw))
                    .div_ceil(u64::from(canvas_w));
                let y0 = u64::from(rect.y) * u64::from(fh) / u64::from(canvas_h);
                let y1 = ((u64::from(rect.y) + u64::from(rect.height)) * u64::from(fh))
                    .div_ceil(u64::from(canvas_h));
                let (mx, my) = if key.lanczos {
                    (lanczos_reach(fw, native.0), lanczos_reach(fh, native.1))
                } else {
                    (0, 0)
                };
                let (x0, x1) = (x0.saturating_sub(mx), x1 + mx);
                let (y0, y1) = (y0.saturating_sub(my), y1 + my);
                let (tx0, ty0) = (u64::from(key.x), u64::from(key.y));
                let (tx1, ty1) = (tx0 + u64::from(entry.w), ty0 + u64::from(entry.h));
                tx0 < x1 && tx1 > x0 && ty0 < y1 && ty1 > y0
            })
            .map(|(k, _)| *k)
            .collect();
        for key in doomed {
            if let Some(old) = state.map.remove(&key) {
                freed += old.data.len();
            }
            if let Some(pos) = state.order.iter().position(|k| *k == key) {
                state.order.remove(pos);
            }
        }
        state.bytes = state.bytes.saturating_sub(freed);
        freed
    }

    /// Tile-cache telemetry snapshot `(hits, misses, evictions)` (#576).
    #[cfg(test)]
    pub(super) fn tile_cache_stats(&self) -> (usize, usize, usize) {
        let s = self
            .tile_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (s.hits, s.misses, s.evictions)
    }

    /// The fully decoded BG44 wavelet image (all chunks), decoding on first
    /// call. `None` when the page has no BG44 chunks or when any chunk fails
    /// strict decoding. The wavelet inverse-transform / YCbCr→RGB conversion is
    /// *not* cached here — it runs per render at the requested subsample.
    pub(crate) fn bg44(&self, page: &DjVuPage) -> Option<std::sync::Arc<Iw44Image>> {
        self.fill(&self.bg44, || {
            let chunks = page.bg44_chunks();
            if chunks.is_empty() {
                return None;
            }
            // IW44_CHECKPOINT (#608): resume from the cached first-chunk
            // decode when a sub>=4 render already paid for it (the common
            // thumbnail -> full-view flow). Chunk 0 is the most expensive
            // chunk (~16-19% of a 4-chunk full decode on the corpus), the
            // clone is ~0.04-0.5 ms, and progressive decode is defined to
            // produce byte-identical output to a fresh 0..n decode. Peek
            // only (`get`) -- a cold full decode must not populate the
            // partial tier as a side effect.
            let mut img;
            let mut start = 0;
            if let Some(partial) = self.bg44_partial.peek() {
                img = (*partial).clone();
                start = 1;
            } else {
                img = Iw44Image::new();
            }
            for chunk_data in &chunks[start.min(chunks.len())..] {
                #[cfg(test)]
                count_bg44_chunk_decode();
                if img.decode_chunk(chunk_data).is_err() {
                    return None;
                }
            }
            if img.width == 0 {
                return None;
            }
            // Dimension cross-check (see `iw44_reduction_is_legal`): a
            // BG44 plane whose header declares a size that isn't a legal
            // 1:1..1:12 reduction of the page's INFO dimensions is
            // corrupted/desynced — DjVuLibre rejects the whole page for
            // this, so treat it the same as any other BG44 decode
            // failure rather than stretching it onto the page.
            if !iw44_reduction_is_legal(
                page.width() as u32,
                page.height() as u32,
                img.width,
                img.height,
            ) {
                return None;
            }
            Some(img)
        })
    }

    /// A partially-decoded BG44 image — first chunk only — decoding on first
    /// call. Roughly 4× cheaper to decode; used at subsample ≥ 4 where the
    /// high-frequency refinement chunks are imperceptible.
    pub(crate) fn bg44_partial(&self, page: &DjVuPage) -> Option<std::sync::Arc<Iw44Image>> {
        self.fill(&self.bg44_partial, || decode_bg44_partial(page))
    }

    /// The first-chunk BG44 image **only if it is already cached** — never
    /// decodes, never populates the slot.
    ///
    /// THUMB_PARTIAL_MEMO: the subsample > 4 render path uses this instead of
    /// [`bg44_partial`](Self::bg44_partial). A "partial" `Iw44Image` decodes ~4x
    /// faster than a full one but is exactly as large: `PlaneDecoder` allocates
    /// every 32x32 coefficient block up front, whichever chunks are decoded
    /// into them. Memoising it costs a full-size image per page (5.75 MB on
    /// `colorbook.djvu`) and buys nothing at sub > 4, where the derived RGB is
    /// not cached either — so a thumbnail sweep retained the whole book's
    /// backgrounds and re-ran the conversion anyway. See PERF_EXPERIMENTS.md
    /// THUMB_PARTIAL_MEMO.
    pub(crate) fn bg44_partial_cached(&self) -> Option<std::sync::Arc<Iw44Image>> {
        self.bg44_partial.peek()
    }

    /// The cached RGB conversion for `subsample`, when this page's single
    /// `sub > 4` slot holds exactly that subsample. Never decodes.
    pub(crate) fn bg_rgb_subhi(&self, subsample: u32) -> Option<Arc<Pixmap>> {
        let slot = self.bg_rgb_subhi.peek()?;
        (slot.0 == subsample).then(|| slot.1.clone())
    }

    /// Fill the `sub > 4` slot if it is still empty. No-op once set, so the
    /// first subsample a page is rendered at wins. Takes a shared handle, so
    /// storing the conversion costs a refcount bump rather than a pixmap copy.
    pub(crate) fn store_bg_rgb_subhi(&self, subsample: u32, px: Arc<Pixmap>) {
        self.bg_rgb_subhi.set_if_empty(Some((subsample, px)));
        let total = self.report_bytes();
        crate::render_cache::sweep_if_over(total, Self::layer_id(&self.bg_rgb_subhi));
    }

    /// The decoded JB2 / G4 foreground mask, decoding on first call. `None`
    /// when the page has no mask chunk or decoding fails.
    pub(crate) fn mask(&self, page: &DjVuPage) -> Option<std::sync::Arc<crate::bitmap::Bitmap>> {
        self.fill(&self.mask, || {
            #[cfg(test)]
            count_jb2_mask_decode();
            page.extract_mask().ok().flatten()
        })
    }

    /// A 1/4-resolution max-pool downsample of the mask. Each bit is 1 if any
    /// bit in the corresponding 4×4 block is set, letting the compositor do
    /// one lookup per output pixel at subsample ≥ 4 instead of 4–9. Purely a
    /// compositor optimisation, which is why it lives in the render tier
    /// rather than on the page.
    ///
    /// If the full-resolution mask is already cached (e.g. a prior
    /// native-resolution render of this page), downsamples that instead of
    /// decoding again. Otherwise decodes straight to 1/4 resolution via
    /// [`DjVuPage::extract_mask_sub4`] — the thumbnail / heavy-downscale
    /// path's common case — skipping the full-resolution JB2 canvas
    /// allocation and the full-canvas downsample scan (round 89 follow-up:
    /// `extract_mask` was 12.6 MB of the 47 MB thumbnail-sweep allocation
    /// total, decoded only to be immediately downsampled and discarded).
    pub(crate) fn mask_sub4(
        &self,
        page: &DjVuPage,
    ) -> Option<std::sync::Arc<crate::bitmap::Bitmap>> {
        self.fill(&self.mask_sub4, || {
            if let Some(full) = self.mask.peek() {
                return Some(downsample_mask_4x(&full));
            }
            page.extract_mask_sub4().ok().flatten()
        })
    }

    /// Peek at an already-built 1/4-resolution mask without triggering any
    /// decode. `Some` only when a previous sub>=4 render populated the slot
    /// (possibly retained across [`downgrade`](Self::downgrade), #607).
    ///
    /// Test-only: `decode_layers` used to gate its #607 fast path on this
    /// (only firing when already warm); it now calls `mask_sub4` directly so
    /// a *cold* sub>=4 render benefits too (round 89 follow-up). Kept as a
    /// non-triggering cache-warmth probe for the structural regression tests.
    #[cfg(test)]
    pub(crate) fn mask_sub4_cached(&self) -> Option<std::sync::Arc<crate::bitmap::Bitmap>> {
        self.mask_sub4.peek()
    }

    /// The decoded FG44 foreground colour layer, decoding on first call.
    /// `None` when the page has no FG44 chunks or decoding fails.
    pub(crate) fn fg44(&self, page: &DjVuPage) -> Option<std::sync::Arc<Pixmap>> {
        self.fill(&self.fg44, || page.extract_foreground().ok().flatten())
    }

    /// Full-resolution (sub=1) RGB Pixmap from BG44, cached after first call.
    ///
    /// Builds on the already-cached [`bg44`](Self::bg44) wavelet image so the
    /// ZP arithmetic decode is paid at most once per page. The IDWT + YCbCr→RGB
    /// conversion (≈2–3 ms for a typical A4 scan) is cached here so that
    /// repeated renders at native resolution skip it entirely.
    ///
    /// `None` when the page has no BG44 layer or the conversion fails — and
    /// for a page so large that the renderer composites it from bands of the
    /// wavelet image instead (#811, [`Iw44Image::rgb_band_rows`]): such a
    /// pixmap would cost hundreds of megabytes and no render would read it.
    pub(crate) fn bg_rgb_s1(&self, page: &DjVuPage) -> Option<std::sync::Arc<Pixmap>> {
        self.fill(&self.bg_rgb_s1, || {
            let img = self.bg44(page)?;
            if img.rgb_band_rows().is_some() {
                return None;
            }
            img.to_rgb_subsample(1).ok()
        })
    }

    /// Half-resolution (sub=2) RGB Pixmap from BG44, cached after first call.
    ///
    /// Mirrors [`bg_rgb_s1`](Self::bg_rgb_s1) for the common 150-from-300-DPI
    /// render: builds on the already-cached [`bg44`](Self::bg44) wavelet image so
    /// the ZP decode is paid once, then caches the IDWT + YCbCr→RGB conversion at
    /// subsample 2 (a ~8 MB Pixmap, 4× smaller than the sub=1 cache).
    ///
    /// `None` when the page has no BG44 layer or the conversion fails.
    pub(crate) fn bg_rgb_s2(&self, page: &DjVuPage) -> Option<std::sync::Arc<Pixmap>> {
        self.fill(&self.bg_rgb_s2, || {
            let img = self.bg44(page)?;
            img.to_rgb_subsample(2).ok()
        })
    }

    /// Quarter-resolution (sub=4) RGB Pixmap from the partial BG44, cached after
    /// first call.
    ///
    /// Mirrors [`bg_rgb_s2`](Self::bg_rgb_s2) for the common heavy-downscale /
    /// thumbnail render (e.g. 150-from-400-DPI). Builds on the already-cached
    /// [`bg44_partial`](Self::bg44_partial) wavelet image — matching the sub>=4
    /// decode path, which uses the first chunk only — so the ZP decode is paid
    /// once, then caches the IDWT + YCbCr->RGB conversion at subsample 4.
    ///
    /// `None` when the page has no BG44 layer or the conversion fails.
    pub(crate) fn bg_rgb_s4(&self, page: &DjVuPage) -> Option<std::sync::Arc<Pixmap>> {
        self.fill(&self.bg_rgb_s4, || {
            let img = self.bg44_partial(page)?;
            img.to_rgb_subsample(4).ok()
        })
    }

    /// The decoded JB2 mask + per-pixel blit-index map, decoding on first call.
    ///
    /// Used for FGbz-palette pages, where the compositor needs the blit index of
    /// each foreground pixel to look up its palette colour. Caches the full JB2
    /// ZP decode and the page-sized `Vec<i32>` blit map so repeated renders of the
    /// same page skip both. `None` when the page has no Sjbz/Smmr chunk or decode
    /// fails. The blit map is ~`width*height*4` bytes — only pages actually
    /// rendered with a palette ever populate this slot.
    /// Cached decoded text layer (#605). `try_init` runs at most once
    /// successfully; a parse error is returned without caching.
    pub(crate) fn text_layer_cached(
        &self,
        parse: impl FnOnce() -> Result<
            Option<std::sync::Arc<crate::text::TextLayer>>,
            crate::djvu_document::DocError,
        >,
    ) -> Result<Option<std::sync::Arc<crate::text::TextLayer>>, crate::djvu_document::DocError>
    {
        if self.text_layer.is_computed() {
            return Ok(self.text_layer.peek());
        }
        let v = parse()?;
        self.text_layer.set_if_empty_arc(v.clone());
        self.report_bytes();
        Ok(v)
    }

    /// Cached decoded annotations (#605); same error semantics as
    /// [`text_layer_cached`](Self::text_layer_cached).
    pub(crate) fn annotations_cached(
        &self,
        parse: impl FnOnce() -> Result<Option<SharedAnnotations>, crate::djvu_document::DocError>,
    ) -> Result<Option<SharedAnnotations>, crate::djvu_document::DocError> {
        if self.annotations.is_computed() {
            return Ok(self.annotations.peek());
        }
        let v = parse()?;
        self.annotations.set_if_empty_arc(v.clone());
        self.report_bytes();
        Ok(v)
    }

    pub(crate) fn mask_indexed(&self, page: &DjVuPage) -> Option<Arc<IndexedMask>> {
        self.fill(&self.mask_indexed, || {
            #[cfg(test)]
            count_jb2_mask_decode();
            page.extract_mask_indexed()
                .ok()
                .flatten()
                .map(|(bm, blits)| (Arc::new(bm), Arc::new(blits)))
        })
    }
}

/// Recursive zone count for the metadata-cache byte estimate (#605).
pub(super) fn count_zones(zones: &[crate::text::TextZone]) -> usize {
    zones
        .iter()
        .map(|z| 1 + count_zones(&z.children))
        .sum::<usize>()
}
