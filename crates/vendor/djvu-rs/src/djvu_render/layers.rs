//! Layer decoding: background, mask, foreground, JPEG planes.

use super::*;

/// Return the largest power-of-2 IW44 subsample factor for the given render
/// scale, allowing up to 1.5× upscaling in the compositor.
///
/// The compositor samples the decoded background at `pixel / subsample`, so a
/// decoded plane that is slightly smaller than the output is fine — the
/// compositor's nearest-neighbour lookup handles it naturally.  Allowing up to
/// 1.5× upscaling lets us pick a coarser subsample in many common cases
/// (e.g. 150 dpi from a 400 dpi source) and skip the high-frequency wavelet
/// bands, matching the partial-decode strategy used by DjVuLibre.
///
/// Examples (with 1.5× tolerance):
/// - scale=1.0  → 1 (full resolution)
/// - scale=0.5  → 2 (1.5/0.5=3.0 → 2)
/// - scale=0.375→ 4 (1.5/0.375=4.0 → 4)   ← was 2 before fix
/// - scale=0.25 → 4 (1.5/0.25=6.0 → 4)
/// - scale=0.1  → 8 (1.5/0.1=15 → capped at 8)
pub(super) fn best_iw44_subsample(scale: f32) -> u32 {
    if scale <= 0.0 || !scale.is_finite() || scale >= 1.0 {
        return 1;
    }
    // Allow up to 1.5× upscaling: the compositor handles the coordinate
    // division, so a slightly-too-small decoded plane is fine.
    // Round rather than truncate: pixel-rounding of width causes decode_scale
    // to differ from the true scale by up to 0.5/page_width (≈0.023% for a
    // 2260-px page), which can push 1.5/scale just below an integer and select
    // a 2× coarser subsample — e.g. subsample 2 instead of 4 for colorbook.
    let max_sub = (1.5_f32 / scale).round() as u32;
    let mut s = 1u32;
    while s * 2 <= max_sub {
        s *= 2;
    }
    s.min(8)
}

/// Max-pool 4× downsample of a bilevel mask.
///
/// Each output pixel is 1 if any bit in the corresponding 4×4 block of `src`
/// is set. Used by [`PageLayers::mask_sub4`] to build the 1/4-resolution mask
/// the compositor uses for sub=4 renders instead of `mask_box_any`.
#[cfg(feature = "std")]
pub(crate) fn downsample_mask_4x(src: &crate::bitmap::Bitmap) -> crate::bitmap::Bitmap {
    let out_w = src.width.div_ceil(4);
    let out_h = src.height.div_ceil(4);
    let mut out = crate::bitmap::Bitmap::new(out_w, out_h);
    for oy in 0..out_h {
        for ox in 0..out_w {
            'outer: for dy in 0..4u32 {
                for dx in 0..4u32 {
                    let sx = ox * 4 + dx;
                    let sy = oy * 4 + dy;
                    if sx < src.width && sy < src.height && src.get(sx, sy) {
                        out.set(ox, oy, true);
                        break 'outer;
                    }
                }
            }
        }
    }
    out
}

/// The page's render-tier `mask_sub4` layer, or `None` without `std`.
///
/// Wraps the `std`-only [`PageLayers`] cache so the compositor's sub=4 path
/// compiles identically with and without the `std` feature.
#[cfg(feature = "std")]
pub(super) fn page_mask_sub4(page: &DjVuPage) -> Option<std::sync::Arc<crate::bitmap::Bitmap>> {
    page.render_layers().mask_sub4(page)
}

/// Is `(plane_w, plane_h)` a legal BG44/FG44 reduction of the page's own
/// `(page_w, page_h)` (from the INFO chunk)?
///
/// Mirrors DjVuLibre's `DjVuFile::get_dpi` cross-check (message
/// `DjVuFile.corrupt_BG44`, "Corrupted data (Incorrect size in BG44
/// chunk)."): the IW44 plane must be an exact `ceil(page_dim / red)`
/// downsample of the page for a *single* common integer reduction factor
/// `red` in `1..=12` — i.e. the same `red` must satisfy width *and* height
/// simultaneously. A BG44 chunk's own header freely declares its width/height
/// (independent of the page's INFO chunk), so without this check a corrupted
/// or desynced INFO/BG44 pairing silently maps the plane onto the page using
/// mismatched per-axis ratios (see `bg_q24`/`fg_q44`, which compute `sx`/`sy`
/// independently) instead of being rejected — producing a visibly stretched/
/// distorted composite rather than a clean error, exactly the "no INFO-vs-
/// BG44-payload dimension cross-check" gap found by differential fuzzing
/// against `ddjvu` (round 45, PERF_EXPERIMENTS.md finding 2).
pub(super) fn iw44_reduction_is_legal(
    page_w: u32,
    page_h: u32,
    plane_w: u32,
    plane_h: u32,
) -> bool {
    if page_w == 0 || page_h == 0 || plane_w == 0 || plane_h == 0 {
        return false;
    }
    (1..=12u32).any(|red| page_w.div_ceil(red) == plane_w && page_h.div_ceil(red) == plane_h)
}

/// Decode background from BG44 chunks up to `max_chunks`.
///
/// `subsample` controls IW44 decode resolution: 1 = full, 2 = half, 4 = quarter.
/// Use `best_iw44_subsample(opts.decode_scale(page))` to pick an appropriate value.
///
/// When `max_chunks == usize::MAX`, the decoded wavelet image is fetched from
/// the page's [`PageLayers`] cache, avoiding repeated ZP arithmetic decode.
///
/// Returns `None` if there are no BG44 chunks.
/// `max_chunks = usize::MAX` means decode all chunks.
pub(super) fn decode_background_chunks(
    page: &DjVuPage,
    max_chunks: usize,
    subsample: u32,
) -> Result<Background, RenderError> {
    // Fast path: use a cached Iw44Image when all chunks are wanted.
    // For sub >= 4 we use the partial cache (first chunk only) — the high-frequency
    // refinement in later chunks is imperceptible at quarter-scale output, and skipping
    // them reduces cold ZP decode cost by ~4×.
    // For sub=1 (the most common case — full-resolution render) we also cache the
    // decoded RGB Pixmap, saving the 2–3 ms IDWT + YCbCr→RGB conversion per call.
    if max_chunks == usize::MAX {
        let bg44_chunks = page.bg44_chunks();
        if !bg44_chunks.is_empty() {
            if subsample == 1 {
                // Strict-mode error propagation: if BG44 failed to decode,
                // decoded_bg44() returns None; treat that as a hard error.
                let img = page
                    .decoded_bg44()
                    .ok_or(RenderError::Iw44(crate::Iw44Error::Invalid))?;
                // #811: a very large page is composited from bands of the
                // wavelet image; its whole RGB pixmap is never built (and
                // `bg_rgb_s1` never caches one).
                if let Some(band_rows) = img.rgb_band_rows() {
                    return Ok(Background::Banded {
                        image: img,
                        band_rows,
                    });
                }
                return Ok(Background::from(page.decoded_bg_rgb_s1()));
            }
            if subsample == 2 {
                // C5_COMPRESS: `PageLayers::downgrade` can clear `bg44` while
                // deliberately *keeping* an already-cached `bg_rgb_s2` (the
                // cheaper middle tier — see downgrade's doc comment). Check the
                // terminal cache first so that case stays warm: `bg_rgb_s2`'s
                // own initialiser already routes through `bg44(page)?`, so a
                // populated `Some` here can only follow a prior successful
                // decode — no need to force `decoded_bg44()` again.
                if let Some(cached) = page.decoded_bg_rgb_s2() {
                    return Ok(Background::Whole(cached));
                }
                // Same memoization as sub=1 for the common 150-from-300-DPI render.
                // Strict-mode error propagation: if BG44 failed to decode,
                // decoded_bg44() returns None; treat that as a hard error.
                let _ = page
                    .decoded_bg44()
                    .ok_or(RenderError::Iw44(crate::Iw44Error::Invalid))?;
                return Ok(Background::from(page.decoded_bg_rgb_s2()));
            }
            if subsample == 4 {
                // C5_COMPRESS: mirrors the subsample==2 short-circuit above for
                // `bg_rgb_s4` / `bg44_partial`.
                if let Some(cached) = page.decoded_bg_rgb_s4() {
                    return Ok(Background::Whole(cached));
                }
                // Cache the sub=4 RGB conversion (built from the partial image,
                // matching the sub>=4 path) so repeated thumbnail / downscale
                // renders skip the IDWT + YCbCr->RGB conversion.
                let _ = page
                    .decoded_bg44_partial()
                    .ok_or(RenderError::Iw44(crate::Iw44Error::Invalid))?;
                return Ok(Background::from(page.decoded_bg_rgb_s4()));
            }
            // subsample > 4 (subsample == 4 returned above). THUMB_PARTIAL_MEMO:
            // reuse an already-cached partial image, but do not *populate* the
            // cache from here. A partial `Iw44Image` is exactly as large as a
            // full one (see `PageLayers::bg44_partial_cached`) and this branch
            // caches nothing it derives, so memoising it made a thumbnail sweep
            // retain the whole book's backgrounds for no repeat saving.
            #[cfg(feature = "std")]
            if let Some(cached) = page.cached_bg_rgb_subhi(subsample) {
                return Ok(Background::Whole(cached));
            }
            let img = if subsample >= 4 {
                #[cfg(feature = "std")]
                {
                    // Decoded here and dropped with this call when the page has
                    // no cached partial image. `no_std` has no layer cache at
                    // all, so it keeps the plain accessor (a stub returning
                    // `None`).
                    match page.cached_bg44_partial() {
                        Some(cached) => Some(cached),
                        None => decode_bg44_partial(page).map(Arc::new),
                    }
                }
                #[cfg(not(feature = "std"))]
                {
                    page.decoded_bg44_partial()
                }
            } else {
                page.decoded_bg44()
            };
            let img = img.ok_or(RenderError::Iw44(crate::Iw44Error::Invalid))?;
            let rgb = Arc::new(img.to_rgb_subsample(subsample)?);
            #[cfg(feature = "std")]
            if subsample > 4 {
                page.store_bg_rgb_subhi(subsample, rgb.clone());
            }
            return Ok(Background::Whole(rgb));
        }
        // No BG44 chunks — fall through to the JPEG fallback below.
    } else {
        let bg44_chunks = page.bg44_chunks();
        if !bg44_chunks.is_empty() {
            let mut img = Iw44Image::new();
            for chunk_data in bg44_chunks.iter().take(max_chunks) {
                #[cfg(test)]
                count_bg44_chunk_decode();
                img.decode_chunk(chunk_data)?;
            }
            // Same dimension cross-check as the cached path in `PageLayers::bg44`.
            if !iw44_reduction_is_legal(
                page.width() as u32,
                page.height() as u32,
                img.width,
                img.height,
            ) {
                return Err(RenderError::Iw44(crate::Iw44Error::Invalid));
            }
            return Background::from_iw44(img, subsample);
        }
    }

    // Fall back to JPEG-encoded background if present.
    #[cfg(feature = "std")]
    if let Some(pm) = decode_bgjp(page)? {
        return Ok(Background::Whole(Arc::new(pm)));
    }

    Ok(Background::None)
}

/// How many BG44 chunks a full-detail render (`max_chunks == usize::MAX`)
/// decodes at `subsample`: all of them, or only the first at subsample 4 and
/// above, where the later chunks refine detail too fine to show.
///
/// The strict path follows it through the first-chunk `bg44_partial` cache in
/// [`decode_background_chunks`]; the permissive path reads it here, so both
/// modes decode the same background on an intact page and may share tiles.
pub(super) fn full_detail_chunks(max_chunks: usize, subsample: u32) -> usize {
    if max_chunks == usize::MAX && subsample >= 4 {
        1
    } else {
        max_chunks
    }
}

/// Permissive variant: decode BG44 chunks until the first error, then stop.
///
/// Returns whatever was decoded so far (may be blurry / incomplete).
/// Returns `None` only when there are no BG44 chunks at all or even the
/// first chunk fails to produce a valid image.
pub(super) fn decode_background_chunks_permissive(
    page: &DjVuPage,
    max_chunks: usize,
    subsample: u32,
) -> Background {
    let max_chunks = full_detail_chunks(max_chunks, subsample);
    let bg44_chunks = page.bg44_chunks();
    if !bg44_chunks.is_empty() {
        let mut img = Iw44Image::new();
        let wanted = bg44_chunks.len().min(max_chunks);
        // `decoded` is the number of chunks decoded before the first error,
        // which is exactly the failing chunk's `enumerate` index.
        for (decoded, chunk_data) in bg44_chunks.iter().take(max_chunks).enumerate() {
            if img.decode_chunk(chunk_data).is_err() {
                // stop on first error, use what we have
                record_recovery(RecoveredLayer::Background, {
                    #[cfg(feature = "std")]
                    {
                        format!(
                            "BG44 truncated at chunk {} of {wanted}; \
                                 kept {decoded} decoded chunk(s)",
                            decoded + 1
                        )
                    }
                    #[cfg(not(feature = "std"))]
                    {
                        let _ = (decoded, wanted);
                        ""
                    }
                });
                break;
            }
        }
        return Background::from_iw44(img, subsample).unwrap_or(Background::None);
    }

    // Fall back to JPEG-encoded background if present.
    #[cfg(feature = "std")]
    {
        Background::from(decode_bgjp(page).ok().flatten().map(Arc::new))
    }
    #[cfg(not(feature = "std"))]
    Background::None
}

/// Decode the JB2 mask (Sjbz chunk) without blit tracking.
///
/// Uses the page-level cache (`decoded_mask`) so that repeated renders of the
/// same page (e.g. at different DPI levels) skip the ZP arithmetic decode.
/// Returns the cached handle (zero-copy) on a cache hit, and a freshly
/// decoded bitmap on a cold decode or when no Sjbz chunk is present.
pub(super) fn decode_mask(
    page: &DjVuPage,
) -> Result<Option<Arc<crate::bitmap::Bitmap>>, RenderError> {
    match page.decoded_mask() {
        Some(bm) => Ok(Some(bm)),
        None if page.find_chunk(b"Sjbz").is_some() => {
            // Cache miss means decode failed; propagate via fresh decode for the error.
            page.extract_mask()
                .map_err(RenderError::from)
                .map(|opt| opt.map(Arc::new))
        }
        None => Ok(None),
    }
}

/// Decode the JB2 mask with per-pixel blit index tracking.
///
/// Delegates to [`DjVuPage::extract_mask_indexed`] so that the shared DJVI
/// dictionary (`shared_djbz`) is used as a fallback when there is no inline
/// Djbz chunk.
/// A decoded indexed mask: the JB2 bitmap plus its per-pixel blit-index map.
///
/// Both halves are shared handles. The page cache stores exactly this pair, so
/// a cache hit hands out the buffers without a copy, and the cache can drop its
/// own reference while a render still holds one.
pub(crate) type IndexedMask = (Arc<crate::bitmap::Bitmap>, Arc<Vec<i32>>);

pub(super) fn decode_mask_indexed(page: &DjVuPage) -> Result<Option<IndexedMask>, RenderError> {
    match page.decoded_mask_indexed() {
        Some(pair) => Ok(Some((pair.0.clone(), pair.1.clone()))),
        // Cache miss with a mask chunk present means decode failed; re-run to
        // surface the error (mirrors `decode_mask`). A genuine no-chunk page
        // returns Ok(None) without a re-decode.
        None if page.find_chunk(b"Sjbz").is_some() || page.find_chunk(b"Smmr").is_some() => page
            .extract_mask_indexed()
            .map_err(RenderError::from)
            .map(|opt| opt.map(|(bm, blit)| (Arc::new(bm), Arc::new(blit)))),
        None => Ok(None),
    }
}

/// Decode the FGbz foreground palette with per-blit color indices.
pub(super) fn decode_fg_palette_full(page: &DjVuPage) -> Result<Option<FgbzPalette>, RenderError> {
    let fgbz = match page.find_chunk(b"FGbz") {
        Some(data) => data,
        None => return Ok(None),
    };

    let pal = parse_fgbz(fgbz)?;
    if pal.colors.is_empty() {
        return Ok(None);
    }
    Ok(Some(pal))
}

/// Decode the FG44 foreground layer.
///
/// Uses the page-level cache (`decoded_fg44`) so that repeated renders skip
/// the IW44 ZP decode. Falls back to FGjp (JPEG) when no FG44 chunks are present.
pub(super) fn decode_fg44(page: &DjVuPage) -> Result<Option<Arc<Pixmap>>, RenderError> {
    let fg44_chunks = page.fg44_chunks();
    if !fg44_chunks.is_empty() {
        return match page.decoded_fg44() {
            Some(pm) => Ok(Some(pm)),
            // `decoded_fg44()` is a shared cache used by both strict and
            // permissive callers, so it swallows the underlying decode error
            // and returns `None`. With chunks present, a cache miss can only
            // mean decode failed (never "no foreground") — re-run the decode
            // for the real error and propagate it (mirrors `decode_mask`'s
            // Sjbz handling below, round 577). Permissive callers already wrap
            // this call in `.ok().flatten()`, recovering the old "no
            // foreground" behavior.
            None => page
                .extract_foreground()
                .map_err(RenderError::from)
                .map(|opt| opt.map(Arc::new)),
        };
    }

    // Fall back to JPEG-encoded foreground if present.
    #[cfg(feature = "std")]
    if let Some(pm) = decode_fgjp(page)? {
        return Ok(Some(Arc::new(pm)));
    }

    Ok(None)
}

/// The page layers decoded for a full (non-progressive) composite: background,
/// foreground palette, mask, optional indexed blit map, and the FG44/FGjp
/// foreground pixmap.
pub(super) struct DecodedLayers {
    pub(super) bg: Background,
    pub(super) fg_palette: Option<FgbzPalette>,
    pub(super) mask: Option<Arc<crate::bitmap::Bitmap>>,
    pub(super) blit_map: Option<Arc<Vec<i32>>>,
    pub(super) fg44: Option<Arc<Pixmap>>,
}

/// The page background a composite reads (#811).
pub(super) enum Background {
    /// No background layer: the compositor paints white.
    None,
    /// The whole background as one RGB pixmap — the ordinary case.
    Whole(Arc<Pixmap>),
    /// A page too large to hold its background as one RGB pixmap. The
    /// compositor pulls `band_rows` output rows at a time from the wavelet
    /// image with [`Iw44Image::rgb_rows`] and never holds more than one band;
    /// see [`for_each_bg_band`].
    Banded {
        image: Arc<Iw44Image>,
        band_rows: u32,
    },
}

impl Background {
    /// The whole pixmap, when the background is held whole.
    pub(super) fn whole(&self) -> Option<&Pixmap> {
        match self {
            Background::Whole(px) => Some(px),
            _ => None,
        }
    }

    /// `true` when there is any background at all.
    pub(super) fn is_some(&self) -> bool {
        !matches!(self, Background::None)
    }

    /// The background a freshly decoded (uncached) wavelet image gives at
    /// `subsample`: banded when the image asks for it at full resolution,
    /// else converted whole.
    pub(super) fn from_iw44(img: Iw44Image, subsample: u32) -> Result<Self, RenderError> {
        Self::from_shared_iw44(&Arc::new(img), subsample)
    }

    /// [`Self::from_iw44`] for an image the caller keeps (the streaming
    /// [`ProgressiveDecoder`] refines the same image chunk after chunk).
    pub(super) fn from_shared_iw44(
        img: &Arc<Iw44Image>,
        subsample: u32,
    ) -> Result<Self, RenderError> {
        if subsample == 1
            && let Some(band_rows) = img.rgb_band_rows()
        {
            return Ok(Background::Banded {
                image: img.clone(),
                band_rows,
            });
        }
        Ok(Background::Whole(Arc::new(
            img.to_rgb_subsample(subsample)?,
        )))
    }
}

impl From<Option<Arc<Pixmap>>> for Background {
    fn from(px: Option<Arc<Pixmap>>) -> Self {
        px.map_or(Background::None, Background::Whole)
    }
}

/// The rows of a colour plane the compositor reads: the whole plane, or one
/// band of its rows when the page is too large to hold whole (#811).
///
/// Row lookups take plane coordinates either way, so the compositor is the
/// same code for both. `height` is the whole plane's, which keeps the row
/// clamping at the plane's edge rather than the band's.
#[derive(Clone, Copy)]
pub(super) struct PlaneView<'a> {
    pub(super) px: &'a Pixmap,
    /// Height of the whole plane; `px.height` when the plane is held whole.
    pub(super) height: u32,
    /// The plane row held in row 0 of `px`.
    pub(super) row0: u32,
}

impl<'a> PlaneView<'a> {
    pub(super) fn whole(px: &'a Pixmap) -> Self {
        PlaneView {
            px,
            height: px.height,
            row0: 0,
        }
    }

    /// One band of a plane `height` rows tall, holding plane rows
    /// `row0..row0 + px.height`.
    pub(super) fn band(px: &'a Pixmap, height: u32, row0: u32) -> Self {
        PlaneView { px, height, row0 }
    }

    #[inline]
    pub(super) fn width(&self) -> u32 {
        self.px.width
    }

    #[inline]
    pub(super) fn height(&self) -> u32 {
        self.height
    }

    /// Plane row `y` as RGBA bytes, or an empty slice when it is not in
    /// memory. Every caller clamps `y` to the plane first; a row outside the
    /// band would mean [`bg_rows_needed`] planned the band wrong, which debug
    /// builds report rather than paint as black.
    #[inline]
    pub(super) fn row(&self, y: u32) -> &'a [u8] {
        let i = y.wrapping_sub(self.row0) as usize;
        debug_assert!(
            y >= self.row0 && i < self.px.height as usize,
            "plane row {y} is outside the band {}..{}",
            self.row0,
            self.row0 + self.px.height
        );
        let stride = self.px.width as usize * 4;
        i.checked_mul(stride)
            .and_then(|off| self.px.data.get(off..off + stride))
            .unwrap_or(&[])
    }
}

/// Decode every layer needed for a full composite at `bg_subsample` — the one
/// home for the permissive-vs-strict decode decision.
///
/// In permissive mode each step swallows errors (`.ok().flatten()`) and the
/// background stops at the first corrupt chunk; in strict mode any decode error
/// propagates. The returned `mask` already has `opts.bold` dilation applied,
/// since both callers do that immediately after decoding.
///
/// Every [`Composite`] with foreground layers decodes through this, so the
/// whole-page, row, buffer, region, tile, and progressive renders share one
/// decode; `max_chunks` sets how much of the background a render reads.
pub(super) fn decode_layers(
    page: &DjVuPage,
    opts: &RenderOptions,
    bg_subsample: u32,
    bg_chunk_limit: usize,
) -> Result<DecodedLayers, RenderError> {
    // #607 (round 89 follow-up): an eligible sub>=4 render skips the full JB2
    // decode entirely, whether `mask_sub4` is already warm or still cold.
    // Eligibility mirrors `sub4_mask` (no bold dilation, no FGbz
    // palette — those need full-resolution mask semantics); the compositor
    // then reads only the sub4 plane, so output is pixel-identical to the
    // full-decode path by construction.
    //
    // `mask_sub4(page)` (rather than the passive `mask_sub4_cached()`) is what
    // makes this fire on a *cold* first render too: when nothing is cached yet
    // it decodes straight to 1/4 resolution via `extract_mask_sub4` instead of
    // decoding the full-resolution canvas and downsampling it afterward — the
    // 12.6 MB `extract_mask` allocation round 89 flagged in the thumbnail
    // sweep. A warm full-resolution mask (from a prior sub=1 render of this
    // page) is still reused by downsampling it in place, never re-decoded.
    //
    // Restricted to full-background decodes (`bg_chunk_limit == usize::MAX`):
    // the progressive path composites with the mask returned *here* (it never
    // consults `sub4_mask`), so handing it a maskless layer set made
    // `render_progressive` frames silently drop the text layer whenever this
    // cache happened to be warm — output depended on cache warmth (#691
    // slice 3 regression test `render_progressive_ignores_mask_sub4_warmth`).
    #[cfg(feature = "std")]
    if bg_chunk_limit == usize::MAX
        && bg_subsample >= 4
        && opts.bold == 0
        && page.find_chunk(b"FGbz").is_none()
        && page.render_layers().mask_sub4(page).is_some()
    {
        let (bg, fg44) = if opts.permissive {
            (
                decode_background_chunks_permissive(page, bg_chunk_limit, bg_subsample),
                permissive_layer(decode_fg44(page), RecoveredLayer::Foreground),
            )
        } else {
            (
                decode_background_chunks(page, bg_chunk_limit, bg_subsample)?,
                decode_fg44(page)?,
            )
        };
        return Ok(DecodedLayers {
            bg,
            fg_palette: None,
            mask: None,
            blit_map: None,
            fg44,
        });
    }

    let bg;
    let fg_palette;
    let mask;
    let blit_map;
    let fg44;

    if opts.permissive {
        bg = decode_background_chunks_permissive(page, bg_chunk_limit, bg_subsample);
        fg_palette = permissive_layer(
            decode_fg_palette_full(page),
            RecoveredLayer::ForegroundPalette,
        );
        let indexed = if fg_palette.is_some() {
            permissive_layer(decode_mask_indexed(page), RecoveredLayer::Mask)
        } else {
            None
        };
        if let Some((bm, bm_map)) = indexed {
            mask = Some(bm);
            blit_map = Some(bm_map);
        } else {
            mask = permissive_layer(decode_mask(page), RecoveredLayer::Mask);
            blit_map = None;
        }
        fg44 = permissive_layer(decode_fg44(page), RecoveredLayer::Foreground);
    } else {
        // #440: background (BG44 ZP + IDWT) and foreground (JB2 mask + FG44) decode
        // touch disjoint OnceLock fields, so on a cold render they can run on two
        // rayon threads — the FG JB2 decode overlaps the BG ZP phase (when the pool
        // is otherwise idle, before IW44_PAR's IDWT join kicks in). Warm renders hit
        // the caches and return immediately, so the join is paid only when cold.
        #[cfg(feature = "parallel")]
        let (bg_res, fg_res) = rayon::join(
            || decode_background_chunks(page, bg_chunk_limit, bg_subsample),
            || decode_foreground_strict(page),
        );
        #[cfg(not(feature = "parallel"))]
        let (bg_res, fg_res) = (
            decode_background_chunks(page, bg_chunk_limit, bg_subsample),
            decode_foreground_strict(page),
        );
        bg = bg_res?;
        let fg = fg_res?;
        fg_palette = fg.fg_palette;
        mask = fg.mask;
        blit_map = fg.blit_map;
        fg44 = fg.fg44;
    }

    let mask = if opts.bold > 0 {
        mask.map(|m| Arc::new(Arc::unwrap_or_clone(m).dilate_n(opts.bold as u32)))
    } else {
        mask
    };

    Ok(DecodedLayers {
        bg,
        fg_palette,
        mask,
        blit_map,
        fg44,
    })
}

/// The foreground layers (mask + blit map, FGbz palette, FG44 colour) decoded in
/// strict mode, *before* any bold dilation.
pub(super) struct ForegroundLayers {
    pub(super) fg_palette: Option<FgbzPalette>,
    pub(super) mask: Option<Arc<crate::bitmap::Bitmap>>,
    pub(super) blit_map: Option<Arc<Vec<i32>>>,
    pub(super) fg44: Option<Arc<Pixmap>>,
}

/// Strict-mode decode of a page's foreground layers, shared by the full
/// [`decode_layers`] path and [`render_progressive`] (which decodes a partial
/// background but the same foreground).
///
/// Owns the "indexed mask when an FGbz palette is present, plain mask
/// otherwise" decision so the two callers cannot drift apart. Bold dilation is
/// applied by the caller, since `decode_layers` shares one dilation step across
/// its permissive and strict branches.
pub(super) fn decode_foreground_strict(page: &DjVuPage) -> Result<ForegroundLayers, RenderError> {
    let fg_palette = decode_fg_palette_full(page)?;
    let (mask, blit_map) = if fg_palette.is_some() {
        match decode_mask_indexed(page)? {
            Some((bm, bm_map)) => (Some(bm), Some(bm_map)),
            None => (None, None),
        }
    } else {
        (decode_mask(page)?, None)
    };
    let fg44 = decode_fg44(page)?;
    Ok(ForegroundLayers {
        fg_palette,
        mask,
        blit_map,
        fg44,
    })
}

/// Decode a BGjp (JPEG-encoded background) chunk into an RGB [`Pixmap`].
///
/// Returns `None` when the page has no `BGjp` chunk.
/// Only available with the `std` feature (requires `zune-jpeg`).
#[cfg(feature = "std")]
pub(super) fn decode_bgjp(page: &DjVuPage) -> Result<Option<Pixmap>, RenderError> {
    let data = match page.find_chunk(b"BGjp") {
        Some(d) => d,
        None => return Ok(None),
    };
    Ok(Some(decode_jpeg_to_pixmap(data)?))
}

/// Decode an FGjp (JPEG-encoded foreground) chunk into an RGB [`Pixmap`].
///
/// Returns `None` when the page has no `FGjp` chunk.
/// Only available with the `std` feature (requires `zune-jpeg`).
#[cfg(feature = "std")]
pub(super) fn decode_fgjp(page: &DjVuPage) -> Result<Option<Pixmap>, RenderError> {
    let data = match page.find_chunk(b"FGjp") {
        Some(d) => d,
        None => return Ok(None),
    };
    Ok(Some(decode_jpeg_to_pixmap(data)?))
}

/// Decode raw JPEG bytes into an RGBA [`Pixmap`].
///
/// Uses `zune-jpeg` for decoding. The JPEG is decoded to RGB and then
/// converted to RGBA (alpha = 255).
#[cfg(feature = "std")]
pub(super) fn decode_jpeg_to_pixmap(data: &[u8]) -> Result<Pixmap, RenderError> {
    use zune_jpeg::JpegDecoder;
    use zune_jpeg::zune_core::bytestream::ZCursor;

    let cursor = ZCursor::new(data);
    let mut decoder = JpegDecoder::new(cursor);
    decoder
        .decode_headers()
        .map_err(|e| RenderError::Jpeg(format!("{e:?}")))?;
    let info = decoder
        .info()
        .ok_or_else(|| RenderError::Jpeg("missing image info after decode_headers".to_owned()))?;
    let w = info.width as usize;
    let h = info.height as usize;
    let rgb = decoder
        .decode()
        .map_err(|e| RenderError::Jpeg(format!("{e:?}")))?;

    // zune-jpeg returns packed RGB; convert to RGBA with alpha = 255.
    let pixel_count = w * h;
    let rgb = if rgb.len() >= pixel_count * 3 {
        rgb
    } else {
        // Truncated JPEG — pad with zeros so rgb_to_rgba stays in bounds.
        let mut padded = rgb;
        padded.resize(pixel_count * 3, 0);
        padded
    };
    let mut rgba = vec![0u8; pixel_count * 4];
    rgb_to_rgba(&rgb[..pixel_count * 3], &mut rgba);
    Ok(Pixmap {
        width: w as u32,
        height: h as u32,
        data: rgba,
    })
}
