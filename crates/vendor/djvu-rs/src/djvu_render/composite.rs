//! The compositor: context, background bands, row bodies.

use super::*;

/// A sub-rectangle within the full rendered output.
///
/// Used by [`render_region`] to select which portion of the page to render.
/// `x` and `y` are pixel offsets within the output at `opts.width × opts.height`
/// resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderRect {
    /// X offset in output pixels.
    pub x: u32,
    /// Y offset in output pixels.
    pub y: u32,
    /// Width of the output region in pixels.
    pub width: u32,
    /// Height of the output region in pixels.
    pub height: u32,
}

/// All decoded layers and options passed to the compositor.
///
/// `Clone`/`Copy`: every field is a reference or a `Copy` primitive, so
/// [`render_region_tiled`] can stamp out one context per tile (only
/// `offset_x`/`offset_y`/`out_w`/`out_h` differ) without rebuilding the q24 /
/// gamma-LUT plumbing each time.
#[derive(Clone, Copy)]
pub(super) struct CompositeContext<'a> {
    pub(super) opts: &'a RenderOptions,
    pub(super) page_w: u32,
    pub(super) page_h: u32,
    pub(super) bg: Option<PlaneView<'a>>,
    /// Q24 ratio for converting page-space FRACBITS coordinates to BG-plane
    /// FRACBITS coordinates.  Uses the inferred integer BG44 cell pitch so
    /// padded edge cells do not stretch across the native render.  `0` when
    /// `bg` is `None`.
    pub(super) bg_x_q24: u64,
    pub(super) bg_y_q24: u64,
    /// [`native_bg_red`]: when non-zero the render is at page size and the
    /// background is enlarged exactly like DjVuLibre's `GPixmapScaler`
    /// instead of through `bg_x_q24`/`bg_y_q24`.
    pub(super) bg_red: u32,
    pub(super) mask: Option<&'a crate::bitmap::Bitmap>,
    /// `mask_sub.trailing_zeros()` where mask_sub is 1 (full-res) or 4 (1/4-res).
    /// Using a shift instead of division avoids a UDIV instruction in the hot path.
    pub(super) mask_shift: u32,
    pub(super) fg_palette: Option<&'a FgbzPalette>,
    /// Per-pixel blit index map (same dimensions as mask). `-1` = no blit.
    pub(super) blit_map: Option<&'a [i32]>,
    pub(super) fg44: Option<&'a Pixmap>,
    /// Q24 ratio for converting page-space FRACBITS coordinates to FG44-space
    /// FRACBITS coordinates.  The horizontal ratio uses the inferred integer
    /// foreground colour-cell pitch; the vertical ratio uses the encoded plane
    /// height so the bottom row remains reachable.  `0` when `fg44` is `None`.
    pub(super) fg_x_q24: u64,
    pub(super) fg_y_q24: u64,
    /// The FG44 reduction on a render at page size (`0` otherwise). DjVuLibre
    /// then paints each foreground pixel from its nearest cell, counting rows
    /// from the bottom (`GPixmap::stencil`, #831); see [`fg_native_frac`].
    pub(super) fg_red: u32,
    pub(super) gamma_lut: &'a [u8; 256],
    /// True when gamma_lut is the identity mapping (lut[i] == i for all i).
    pub(super) gamma_is_identity: bool,
    /// X offset within the full render (for region renders; 0 for full page).
    pub(super) offset_x: u32,
    /// Y offset within the full render (for region renders; 0 for full page).
    pub(super) offset_y: u32,
    /// Output width (may be smaller than opts.width for region renders).
    pub(super) out_w: u32,
    /// Output height (may be smaller than opts.height for region renders).
    pub(super) out_h: u32,
}

#[derive(Clone, Copy)]
pub(super) struct AreaAvgX {
    pub(super) fx: u32,
    pub(super) bg_x0: u32,
    pub(super) bg_x1: u32,
}

// Exclusive upper bounds: the output pixel covers source [x0, x1) x [y0, y1).
// The old inclusive formula gave a 3x3 box at 2x downscale because x1 landed on
// the first pixel of the next output cell; exclusive gives the correct 2x2.
pub(super) fn area_range(limit: u32, f: u32, step: u32) -> (u32, u32) {
    (
        (f >> FRACBITS).min(limit.saturating_sub(1)),
        ((f + step) >> FRACBITS).min(limit),
    )
}

pub(super) fn precompute_area_avg_x(
    ctx: &CompositeContext<'_>,
    fx_step: u32,
    bg_fx_step: u32,
) -> Vec<AreaAvgX> {
    let mut xs = Vec::with_capacity(ctx.out_w as usize);
    let bg_w = ctx.bg.map_or(0, |bg| bg.width());
    for ox in 0..ctx.out_w {
        let fx = (ox + ctx.offset_x) * fx_step;
        let bg_fx = ((fx as u64 * ctx.bg_x_q24) >> 24) as u32;
        let (bg_x0, bg_x1) = area_range(bg_w, bg_fx, bg_fx_step);
        xs.push(AreaAvgX { fx, bg_x0, bg_x1 });
    }
    xs
}

/// Per-output-column bg sampling data for the bilinear (upscale / 1:1) path:
/// clamped source columns `x0`/`x1` and the horizontal fractional weight `tx`.
/// Column mapping never depends on the row, so this is computed once per
/// render instead of once per pixel — the AreaAvgX analog for upscaling.
#[derive(Clone, Copy)]
pub(super) struct BilinearX {
    pub(super) x0: u32,
    pub(super) x1: u32,
    pub(super) tx: u32,
}

/// A row's background source in [`zoom_row`].
#[derive(Clone, Copy)]
pub(super) enum BgRow<'s> {
    /// DjVuLibre's vertical pass, rounded to 8 bits, and its first plane
    /// column (#831). `bg_at` applies the horizontal pass per pixel.
    Scaled(&'s [[u16; 4]], u32),
    /// The vertical pre-blend, the plane width, and its first column.
    Blend(&'s [[u16; 4]], u32, u32),
}

/// Build the per-column table for [`composite_rows_bilinear_one`]. Walks the
/// exact Q48 fixed-point accumulator of the in-loop fallback (`bg_fx_q`), so
/// table lookups and the fallback produce byte-identical coordinates.
/// When [`CompositeContext::bg_red`] is set, the table holds the
/// [`scaler_x`] entries instead, and the fallback is not used.
pub(super) fn precompute_bilinear_x(
    ctx: &CompositeContext<'_>,
    fx_step: u32,
) -> Option<Vec<BilinearX>> {
    let bg = ctx.bg?;
    if ctx.bg_red != 0 {
        return Some(
            (0..ctx.out_w)
                .map(|ox| scaler_x(ox + ctx.offset_x, ctx.bg_red, bg.width()))
                .collect(),
        );
    }
    let clamp_w = bg.width().saturating_sub(1);
    let bg_fx_step_q: u64 = fx_step as u64 * ctx.bg_x_q24;
    let mut bg_fx_q: u64 = (ctx.offset_x as u64 * fx_step as u64 + FRAC as u64 / 2) * ctx.bg_x_q24;
    let mut xs = Vec::with_capacity(ctx.out_w as usize);
    for _ in 0..ctx.out_w {
        let bg_fx = ((bg_fx_q >> 24) as u32).saturating_sub(FRAC / 2);
        let x0 = (bg_fx >> FRACBITS).min(clamp_w);
        xs.push(BilinearX {
            x0,
            x1: (x0 + 1).min(clamp_w),
            tx: bg_fx & FRAC_MASK,
        });
        bg_fx_q = bg_fx_q.wrapping_add(bg_fx_step_q);
    }
    Some(xs)
}

impl<'a> CompositeContext<'a> {
    /// Build a composite context from already-decoded layers.
    ///
    /// This is the single home for the per-render wiring that was copy-pasted
    /// across all five render entry points: the q24 cell-pitch arithmetic (where
    /// the #199 BG/FG alignment fix lives), the page-dimension lookup, and the
    /// gamma-LUT / offset / output-size plumbing. Callers decode their layers
    /// (full or partial background, optionally sub-4 mask) and hand them here so
    /// a q24 / offset / gamma bug can only ever be fixed in one place.
    ///
    /// The `(mask, mask_shift)` pair is passed in rather than derived because it
    /// varies with the [`Detail`]: a full-detail composite may swap in the
    /// 1/4-resolution mask via [`sub4_mask`], while coarse and progressive
    /// composites always read the full-resolution mask.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn from_layers(
        page: &DjVuPage,
        opts: &'a RenderOptions,
        bg: Option<PlaneView<'a>>,
        mask: Option<&'a crate::bitmap::Bitmap>,
        mask_shift: u32,
        fg_palette: Option<&'a FgbzPalette>,
        blit_map: Option<&'a [i32]>,
        fg44: Option<&'a Pixmap>,
        gamma_lut: &'a [u8; 256],
        offset: (u32, u32),
        out: (u32, u32),
    ) -> Self {
        let page_w = page.width() as u32;
        let page_h = page.height() as u32;
        let (fg_x_q24, fg_y_q24) = fg_q24(fg44, page_w, page_h);
        let fg_red = fg44.map_or(0, |f| {
            native_fg_red((page_w, page_h), (opts.width, opts.height), f)
        });
        let (bg_x_q24, bg_y_q24) = bg_q24(bg.map(|b| (b.width(), b.height())), page_w, page_h);
        let bg_red = bg.map_or(0, |b| {
            native_bg_red(
                (page_w, page_h),
                (opts.width, opts.height),
                (b.width(), b.height()),
            )
        });
        CompositeContext {
            opts,
            page_w,
            page_h,
            bg,
            bg_x_q24,
            bg_y_q24,
            bg_red,
            mask,
            mask_shift,
            fg_palette,
            blit_map,
            fg44,
            fg_x_q24,
            fg_y_q24,
            fg_red,
            gamma_lut,
            gamma_is_identity: gamma_lut.iter().enumerate().all(|(i, &v)| v == i as u8),
            offset_x: offset.0,
            offset_y: offset.1,
            out_w: out.0,
            out_h: out.1,
        }
    }

    /// The same context reading `bg` instead — a band of the plane, or a
    /// different band. `bg` must report the whole plane's size so the
    /// plane pitch stays what [`Self::from_layers`] computed.
    pub(super) fn with_bg<'b>(&self, bg: Option<PlaneView<'b>>) -> CompositeContext<'b>
    where
        'a: 'b,
    {
        let (bg_x_q24, bg_y_q24) = bg_q24(
            bg.map(|b| (b.width(), b.height())),
            self.page_w,
            self.page_h,
        );
        let bg_red = bg.map_or(0, |b| {
            native_bg_red(
                (self.page_w, self.page_h),
                (self.opts.width, self.opts.height),
                (b.width(), b.height()),
            )
        });
        CompositeContext {
            bg,
            bg_x_q24,
            bg_y_q24,
            bg_red,
            ..*self
        }
    }
}

/// Pick the mask and its shift for a full-detail composite.
///
/// At background subsample ≥ 4 (and only when no bold dilation or FGbz palette
/// is in play, since those need full-resolution lookups) the compositor reads a
/// pre-downsampled 1/4-resolution mask — one bit lookup per output pixel instead
/// of 4–9. The returned shift is `mask_sub.trailing_zeros()` (2 for the 1/4-res
/// mask, 0 for the full-res mask).
///
/// The 1/4-resolution mask is a shared handle out of the page cache: the cache
/// can drop its own copy at any time (see `CacheSlot`), so the composite keeps
/// the buffer alive through the handle.
#[cfg(feature = "std")]
pub(super) fn sub4_mask(
    page: &DjVuPage,
    bg_subsample: u32,
    opts: &RenderOptions,
    full_mask: Option<Arc<crate::bitmap::Bitmap>>,
    fg_palette: Option<&FgbzPalette>,
) -> (Option<Arc<crate::bitmap::Bitmap>>, u32) {
    if bg_subsample >= 4 && opts.bold == 0 && fg_palette.is_none() {
        (page_mask_sub4(page), 2)
    } else {
        (full_mask, 0)
    }
}

#[cfg(not(feature = "std"))]
pub(super) fn sub4_mask(
    _page: &DjVuPage,
    _bg_subsample: u32,
    _opts: &RenderOptions,
    full_mask: Option<Arc<crate::bitmap::Bitmap>>,
    _fg_palette: Option<&FgbzPalette>,
) -> (Option<Arc<crate::bitmap::Bitmap>>, u32) {
    (full_mask, 0)
}

/// Look up the palette color for a foreground pixel at (px, py).
///
/// Uses the blit map to find the per-glyph blit index, then maps it through
/// the FGbz index table to get the final color. Falls back to `palette[0]` when
/// no index table is present, and to black when lookup fails.
#[inline]
pub(super) fn lookup_palette_color(
    pal: &FgbzPalette,
    blit_map: Option<&[i32]>,
    mask: Option<&crate::bitmap::Bitmap>,
    px: u32,
    py: u32,
) -> PaletteColor {
    if let Some(bm) = blit_map
        && let Some(m) = mask
    {
        let mi = py as usize * m.width as usize + px as usize;
        if mi < bm.len() {
            let blit_idx = bm[mi];
            if blit_idx >= 0 {
                if !pal.indices.is_empty() {
                    // Two-level indirection: blit_idx → color_idx → color
                    let bi = blit_idx as usize;
                    if bi < pal.indices.len() {
                        let ci = pal.indices[bi] as usize;
                        if ci < pal.colors.len() {
                            return pal.colors[ci];
                        }
                    }
                } else {
                    // No index table: use blit_idx directly as color index
                    let ci = blit_idx as usize;
                    if ci < pal.colors.len() {
                        return pal.colors[ci];
                    }
                }
            }
        }
    }
    // Fallback: first palette color or black
    pal.colors.first().copied().unwrap_or_default()
}

/// The background-plane rows the compositor reads for output rows `rows`
/// (absolute rows of the `full_w × full_h` render), as `lo..hi` (#811).
///
/// Mirrors the row arithmetic of [`composite_rows_bilinear_one`] and
/// [`composite_rows_area_avg_one`] for the first and last output row; both
/// mappings are monotone, so the rows in between fall inside. The bilinear
/// path reads rows `y0` and `y0 + 1` (clamped); the area path reads
/// `[y0, y1)` with at least `y0` itself.
pub(super) fn bg_rows_needed(
    (page_w, page_h): (u32, u32),
    (full_w, full_h): (u32, u32),
    plane: (u32, u32),
    rows: core::ops::Range<u32>,
) -> (u32, u32) {
    let plane_h = plane.1;
    if rows.is_empty() || plane_h == 0 {
        return (0, 0);
    }
    let fx_step = ((page_w as u64 * FRAC as u64) / full_w.max(1) as u64) as u32;
    let fy_step = ((page_h as u64 * FRAC as u64) / full_h.max(1) as u64) as u32;
    let (_, bg_y_q24) = bg_q24(Some(plane), page_w, page_h);
    let first = rows.start;
    let last = rows.end - 1;
    if fx_step > FRAC || fy_step > FRAC {
        let bg_fy_step = ((fy_step as u64 * bg_y_q24) >> 24) as u32;
        let row_of = |oy: u32| (((oy * fy_step) as u64 * bg_y_q24) >> 24) as u32;
        let (lo, _) = area_range(plane_h, row_of(first), bg_fy_step);
        let (y0, y1) = area_range(plane_h, row_of(last), bg_fy_step);
        (lo, y1.max(y0 + 1))
    } else if let red @ 1.. = native_bg_red((page_w, page_h), (full_w, full_h), plane) {
        // Rows grow downwards while the scaler counts from the bottom:
        // the first output row reads the band's top, the last its bottom.
        let (_, lo, _) = scaler_rows(first, page_h, red, plane_h);
        let (hi, _, _) = scaler_rows(last, page_h, red, plane_h);
        (lo, hi + 1)
    } else {
        let clamp_h = plane_h - 1;
        let row_of =
            |oy: u32| (map_plane_center_frac(oy * fy_step, bg_y_q24) >> FRACBITS).min(clamp_h);
        let lo = row_of(first);
        let hi = (row_of(last) + 1).min(clamp_h) + 1;
        (lo, hi)
    }
}

/// The background plane columns that output columns `cols` read, widened by
/// every sampler's reach, so that a band decoded with only these columns
/// ([`Iw44Image::rgb_window`]) holds every pixel the composite reads.
///
/// Mirrors the column arithmetic of the compositor, truncated `fx_step`
/// included: at `FRACBITS = 4` that truncation moves a column far from the
/// page origin by several plane pixels. The reach adds one output pixel's
/// footprint in the plane (the area average reads that much), the bilinear
/// and scaler neighbour, and a pixel of rounding on each side.
pub(super) fn bg_cols_needed(
    page_w: u32,
    full_w: u32,
    plane_w: u32,
    cols: core::ops::Range<u32>,
) -> core::ops::Range<u32> {
    if cols.is_empty() || plane_w == 0 {
        return 0..0;
    }
    let fx_step = (u64::from(page_w) * u64::from(FRAC)) / u64::from(full_w.max(1));
    let (bg_x_q24, _) = bg_q24(Some((plane_w, 1)), page_w, 1);
    let to_plane =
        |ox: u32| (((u64::from(ox) * fx_step + u64::from(FRAC / 2)) * bg_x_q24) >> 24) >> FRACBITS;
    let reach = (((fx_step * bg_x_q24) >> 24) >> FRACBITS) + 2;
    let lo = to_plane(cols.start)
        .saturating_sub(reach)
        .min(u64::from(plane_w));
    let hi = (to_plane(cols.end) + reach + 1).min(u64::from(plane_w));
    lo as u32..hi as u32
}

/// How many output rows one background band covers, so that the plane rows
/// [`bg_rows_needed`] asks for stay within `band_rows` — the memory budget
/// [`Iw44Image::rgb_band_rows`] sized. `offset_y`/`out_h` are the output
/// rows of the whole composite.
pub(super) fn bg_band_out_rows(
    page: (u32, u32),
    full: (u32, u32),
    plane: (u32, u32),
    offset_y: u32,
    out_h: u32,
    band_rows: u32,
) -> u32 {
    let fy_step = ((page.1 as u64 * FRAC as u64) / full.1.max(1) as u64) as u32;
    let (_, bg_y_q24) = bg_q24(Some(plane), page.0, page.1);
    // Plane rows per output row, Q(FRACBITS + 24); a few rows of slack for
    // the clamped neighbour rows the samplers read.
    let per_out = (fy_step as u64 * bg_y_q24).max(1);
    let rows = ((band_rows.saturating_sub(4) as u64) << (FRACBITS + 24)) / per_out;
    let mut rows = (rows.min(out_h as u64) as u32).max(1);
    // Safety net: never exceed the budget, whatever rounding did above.
    loop {
        let (lo, hi) = bg_rows_needed(page, full, plane, offset_y..offset_y + rows);
        if hi - lo <= band_rows || rows == 1 {
            return rows;
        }
        rows = (rows * 7 / 8).max(1);
    }
}

/// Run `f` once per background band of a composite (#811).
///
/// For a whole (or missing) background this is one call with the ordinary
/// context. For a [`Background::Banded`] page the output rows are walked in
/// bands: each band pulls exactly the plane rows it reads with
/// [`Iw44Image::rgb_rows`], composites through a context whose `offset_y`
/// and `out_h` are narrowed to that band, and is dropped before the next
/// one, so the peak memory is one band, not the whole background pixmap.
/// `f` receives the context and the band's first output row relative to
/// `out`; it writes those rows of its own output.
#[allow(clippy::too_many_arguments)]
pub(super) fn for_each_bg_band<F>(
    page: &DjVuPage,
    opts: &RenderOptions,
    bg: &Background,
    mask: Option<&crate::bitmap::Bitmap>,
    mask_shift: u32,
    fg_palette: Option<&FgbzPalette>,
    blit_map: Option<&[i32]>,
    fg44: Option<&Pixmap>,
    gamma_lut: &[u8; 256],
    offset: (u32, u32),
    out: (u32, u32),
    mut f: F,
) -> Result<(), RenderError>
where
    F: FnMut(&CompositeContext<'_>, u32) -> Result<(), RenderError>,
{
    let (image, band_rows) = match bg {
        Background::Banded { image, band_rows } => (image, *band_rows),
        _ => {
            let ctx = CompositeContext::from_layers(
                page,
                opts,
                bg.whole().map(PlaneView::whole),
                mask,
                mask_shift,
                fg_palette,
                blit_map,
                fg44,
                gamma_lut,
                offset,
                out,
            );
            return f(&ctx, 0);
        }
    };
    let plane = (image.width, image.height);
    let page_dims = (page.width() as u32, page.height() as u32);
    let full = (opts.width, opts.height);
    let template = CompositeContext::from_layers(
        page, opts, None, mask, mask_shift, fg_palette, blit_map, fg44, gamma_lut, offset, out,
    );
    let cols = bg_cols_needed(page_dims.0, full.0, plane.0, offset.0..offset.0 + out.0);
    let mut oy0 = 0u32;
    while oy0 < out.1 {
        let rows = bg_band_out_rows(
            page_dims,
            full,
            plane,
            offset.1 + oy0,
            out.1 - oy0,
            band_rows,
        );
        let oy1 = oy0 + rows;
        let (lo, hi) = bg_rows_needed(page_dims, full, plane, offset.1 + oy0..offset.1 + oy1);
        let band = image.rgb_window(lo..hi, cols.clone())?;
        let view = PlaneView::band(&band, plane.1, lo);
        let mut ctx = template.with_bg(Some(view));
        ctx.offset_y = offset.1 + oy0;
        ctx.out_h = rows;
        f(&ctx, oy0)?;
        oy0 = oy1;
    }
    Ok(())
}

/// Composite one page into `buf` (RGBA, pre-allocated) using the given context.
///
/// This is a zero-allocation render path when `buf` is already the right size.
/// For region renders, `ctx.out_w`/`ctx.out_h` give the output dimensions and
/// `ctx.offset_x`/`ctx.offset_y` give the starting offset within the full render.
///
/// Iterates the output rows over the same single-row composite bodies used by
/// [`composite_rows`], writing each row directly into its slice of `buf` with no
/// intermediate copy. The two paths therefore share one per-pixel decision tree.
pub(super) fn composite_into(
    ctx: &CompositeContext<'_>,
    buf: &mut [u8],
) -> Result<(), RenderError> {
    let full_w = ctx.opts.width;
    let full_h = ctx.opts.height;

    // Fixed-point step: how many source pixels per full-render output pixel
    let fx_step = ((ctx.page_w as u64 * FRAC as u64) / full_w.max(1) as u64) as u32;
    let fy_step = ((ctx.page_h as u64 * FRAC as u64) / full_h.max(1) as u64) as u32;

    let row_stride = ctx.out_w as usize * 4;

    // Bilevel fast path: JB2-only page (no IW44 bg, no FG44, no palette).
    // Skips bilinear sampling and gamma LUT — just white fill + black mask writes.
    // Writes alpha=255 inline; returns early before the general compositor loop.
    if ctx.bg.is_none() && ctx.fg44.is_none() && ctx.fg_palette.is_none() {
        #[cfg(feature = "parallel")]
        {
            use rayon::prelude::*;
            let n = ctx.out_h as usize * row_stride;
            buf[..n]
                .par_chunks_exact_mut(row_stride)
                .enumerate()
                .for_each(|(oy, row)| {
                    composite_rows_bilevel_one(ctx, oy as u32, fx_step, fy_step, row);
                });
        }
        #[cfg(not(feature = "parallel"))]
        for (oy, row) in buf[..ctx.out_h as usize * row_stride]
            .chunks_exact_mut(row_stride)
            .enumerate()
        {
            composite_rows_bilevel_one(ctx, oy as u32, fx_step, fy_step, row);
        }
        return Ok(());
    }

    let downscale = fx_step > FRAC || fy_step > FRAC;
    // Precompute bg-space step for the area-average path (avoids per-pixel multiply).
    let bg_fx_step = ((fx_step as u64 * ctx.bg_x_q24) >> 24) as u32;
    let bg_fy_step = ((fy_step as u64 * ctx.bg_y_q24) >> 24) as u32;
    let area_avg_x = downscale.then(|| precompute_area_avg_x(ctx, fx_step, bg_fx_step));
    let bilinear_x = (!downscale)
        .then(|| precompute_bilinear_x(ctx, fx_step))
        .flatten();

    #[cfg(feature = "parallel")]
    {
        use rayon::prelude::*;
        let n = ctx.out_h as usize * row_stride;
        buf[..n]
            .par_chunks_exact_mut(row_stride)
            .enumerate()
            .for_each_init(Vec::new, |vblend, (oy, row)| {
                if downscale {
                    composite_rows_area_avg_one(
                        ctx,
                        oy as u32,
                        fx_step,
                        fy_step,
                        bg_fx_step,
                        bg_fy_step,
                        row,
                        area_avg_x.as_deref(),
                    );
                } else {
                    composite_rows_bilinear_one(
                        ctx,
                        oy as u32,
                        fx_step,
                        fy_step,
                        row,
                        bilinear_x.as_deref(),
                        vblend,
                    );
                }
            });
    }
    #[cfg(not(feature = "parallel"))]
    {
        let mut vblend = Vec::new();
        for (oy, row) in buf[..ctx.out_h as usize * row_stride]
            .chunks_exact_mut(row_stride)
            .enumerate()
        {
            if downscale {
                composite_rows_area_avg_one(
                    ctx,
                    oy as u32,
                    fx_step,
                    fy_step,
                    bg_fx_step,
                    bg_fy_step,
                    row,
                    area_avg_x.as_deref(),
                );
            } else {
                composite_rows_bilinear_one(
                    ctx,
                    oy as u32,
                    fx_step,
                    fy_step,
                    row,
                    bilinear_x.as_deref(),
                    &mut vblend,
                );
            }
        }
    }

    // All render paths (bilevel, bilinear, area-average) write alpha=255 inline;
    // no separate fill_alpha_255 post-pass is needed.

    Ok(())
}

/// Drive the composite hot path row-by-row, calling `sink(row_index, &row_rgba)`
/// once per output row.
///
/// Each call to `sink` receives a 4-byte-per-pixel RGBA slice of width
/// `ctx.out_w`.  A single scratch row is allocated up-front (not per-row), so
/// peak additional heap use is `out_w * 4` bytes regardless of page height.
///
/// This is the streaming primitive behind [`render_streaming`]; callers
/// that already hold a flat output buffer should prefer [`composite_into`],
/// which writes directly without an intermediate copy.
pub(super) fn composite_rows<F>(ctx: &CompositeContext<'_>, mut sink: F) -> Result<(), RenderError>
where
    F: FnMut(usize, &[u8]),
{
    let full_w = ctx.opts.width;
    let full_h = ctx.opts.height;

    // Fixed-point step: how many source pixels per full-render output pixel.
    let fx_step = ((ctx.page_w as u64 * FRAC as u64) / full_w.max(1) as u64) as u32;
    let fy_step = ((ctx.page_h as u64 * FRAC as u64) / full_h.max(1) as u64) as u32;

    let row_stride = ctx.out_w as usize * 4;

    // Bilevel fast path: JB2-only page (no IW44 bg, no FG44, no palette).
    if ctx.bg.is_none() && ctx.fg44.is_none() && ctx.fg_palette.is_none() {
        let mut row_buf = vec![0u8; row_stride];
        for oy in 0..ctx.out_h {
            composite_rows_bilevel_one(ctx, oy, fx_step, fy_step, &mut row_buf);
            sink(oy as usize, &row_buf);
        }
        return Ok(());
    }

    let mut row_buf = vec![0u8; row_stride];
    let downscale = fx_step > FRAC || fy_step > FRAC;

    // Precompute bg-space step for area-average path (avoids per-pixel multiply).
    let bg_fx_step = ((fx_step as u64 * ctx.bg_x_q24) >> 24) as u32;
    let bg_fy_step = ((fy_step as u64 * ctx.bg_y_q24) >> 24) as u32;
    let area_avg_x = downscale.then(|| precompute_area_avg_x(ctx, fx_step, bg_fx_step));
    let bilinear_x = (!downscale)
        .then(|| precompute_bilinear_x(ctx, fx_step))
        .flatten();
    let mut vblend = Vec::new();

    for oy in 0..ctx.out_h {
        if downscale {
            composite_rows_area_avg_one(
                ctx,
                oy,
                fx_step,
                fy_step,
                bg_fx_step,
                bg_fy_step,
                &mut row_buf,
                area_avg_x.as_deref(),
            );
        } else {
            composite_rows_bilinear_one(
                ctx,
                oy,
                fx_step,
                fy_step,
                &mut row_buf,
                bilinear_x.as_deref(),
                &mut vblend,
            );
        }
        sink(oy as usize, &row_buf);
    }

    Ok(())
}

/// Write one bilevel row into `row_buf`.
#[inline]
pub(super) fn composite_rows_bilevel_one(
    ctx: &CompositeContext<'_>,
    oy: u32,
    fx_step: u32,
    fy_step: u32,
    row_buf: &mut [u8],
) {
    let mask = match ctx.mask {
        Some(m) => m,
        None => {
            for chunk in row_buf.as_chunks_mut::<4>().0 {
                chunk[0] = 255;
                chunk[1] = 255;
                chunk[2] = 255;
                chunk[3] = 255;
            }
            return;
        }
    };

    // 1:1 scale fast path.
    if fx_step == FRAC && fy_step == FRAC {
        let stride = mask.row_stride();
        // Same shape as the column clamp below: `mask.data` holds `mask.height`
        // rows, not `page_h` rows. An INFO chunk that declares a page taller
        // than the bilevel mask it ships walked `py` past the end of the data
        // and panicked on the range. Clamp to the mask's own height too, and
        // take the row through `get`, so a short or empty mask renders white
        // instead of unwinding.
        //
        // Both bounds checks for this row are here, once, rather than per
        // pixel: `mask_row` is exactly `stride` bytes and `stride` is
        // `ceil(mask.width / 8)`, so once a column is clamped to
        // `mask.width - 1` its byte index cannot leave the row. That is what
        // lets the fallback loop below keep indexing directly. A zero-width
        // mask has no byte to read at all, so it leaves with the empty row.
        let py = (oy + ctx.offset_y)
            .min(ctx.page_h.saturating_sub(1))
            .min(mask.height.saturating_sub(1)) as usize;
        let Some(mask_row) = mask.data.get(py * stride..(py + 1) * stride) else {
            row_buf.fill(255);
            return;
        };
        if mask_row.is_empty() {
            row_buf.fill(255);
            return;
        }

        // I3: whole-row white fast path. If the mask row has no foreground bits (page
        // margins, blank inter-line gaps — typically 25-35% of rows in text scans),
        // fill the output row with white in one NEON-vectorised store instead of running
        // the per-pixel bit-extraction loop.
        if !mask_row.iter().any(|&b| b != 0) {
            row_buf.fill(255);
            return;
        }

        // P2: BILEVEL_RGBA table fast path. One table lookup per mask byte produces
        // 8 pre-packed RGBA pixels (32 bytes); copy_from_slice compiles to 2 NEON vst1
        // stores, replacing 8×16 scalar instructions. Works whenever offset_x is
        // byte-aligned (offset_x % 8 == 0) so output byte `i` maps to source mask byte
        // `offset_x/8 + i` with no per-pixel bit shuffle — covers full-page renders
        // (offset_x==0) and byte-aligned `render_region` viewports. Guard
        // `offset_x + out_w <= mask.width` keeps the source index in bounds and makes
        // the `.min(page_w-1)` clamp a no-op (mask.width == page_w for the 1:1 mask).
        let out_w = row_buf.len() / 4;
        let ox0 = ctx.offset_x as usize;
        if ctx.offset_x.is_multiple_of(8) && ox0 + out_w <= mask.width as usize {
            let mb0 = ox0 / 8; // first source mask byte (0 for full-page renders)
            let nb_full = out_w / 8; // number of full mask bytes (8 pixels each)
            let nb_rem = out_w % 8; // trailing pixels from a partial mask byte
            for byte_idx in 0..nb_full {
                let mb = mask_row[mb0 + byte_idx];
                let src = &BILEVEL_RGBA[mb as usize];
                row_buf[byte_idx * 32..(byte_idx + 1) * 32].copy_from_slice(src);
            }
            if nb_rem > 0 {
                let mb = mask_row[mb0 + nb_full];
                let src = &BILEVEL_RGBA[mb as usize];
                let base = nb_full * 32;
                row_buf[base..base + nb_rem * 4].copy_from_slice(&src[..nb_rem * 4]);
            }
            return;
        }

        // Fallback: branchless per-pixel expansion with .min() clamp for partial/offset views.
        //
        // `mask_row` is `mask.width` pixels wide, not `page_w` wide: an INFO chunk that
        // declares a page wider than the bilevel mask it ships (a fuzzed or malformed file)
        // used to walk `px` past the end of `mask_row` here and panic on the index. The
        // fast path above already guards this (`ox0 + out_w <= mask.width`); clamp to the
        // mask's own width too.
        //
        // The clamp is the bounds check, and it is the only one this loop needs:
        // `last_col <= mask.width - 1` and `mask_row` is `ceil(mask.width / 8)`
        // bytes, so `px >> 3` is always a byte of this row. Reading it through
        // `get(..).map_or(..)` instead cost 5.7 % on `render_region_bilevel` —
        // the per-pixel branch is what the word "branchless" above is about.
        let last_col = ctx
            .page_w
            .saturating_sub(1)
            .min(mask.width.saturating_sub(1));
        for (ox, pixel) in row_buf.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let px = (ox as u32 + ctx.offset_x).min(last_col) as usize;
            let is_fg = ((mask_row[px >> 3] >> (7 - (px & 7))) & 1) as u32;
            let ch = (is_fg.wrapping_sub(1) & 0xFF) as u8; // 0 when fg, 255 when bg
            pixel[0] = ch;
            pixel[1] = ch;
            pixel[2] = ch;
            pixel[3] = 255;
        }
        return;
    }

    let downscale = fx_step > FRAC || fy_step > FRAC;
    let fy = (oy + ctx.offset_y) * fy_step;
    let py = (fy >> FRACBITS).min(ctx.page_h.saturating_sub(1));

    // I3-downscale: all-white band fast path for the anti-aliased path. The
    // source mask band [y0, y1) for this output row is row-invariant (fy is
    // fixed), so if it has no foreground bits every output pixel's coverage is 0
    // → white. One NEON-vectorised scan replaces out_w `mask_box_coverage` calls
    // (each of which itself scans the band). Only `mask_shift == 0` is covered —
    // the max-pool sub-path indexes the mask at a coarser resolution. The y range
    // matches `mask_box_coverage` exactly; `y1 <= y0` is the degenerate
    // zero-coverage case (also white).
    if downscale && ctx.mask_shift == 0 {
        let stride = mask.row_stride();
        let y0 = (fy >> FRACBITS).min(mask.height.saturating_sub(1)) as usize;
        let y1 = ((fy + fy_step) >> FRACBITS).min(mask.height) as usize;
        if y1 <= y0 || !mask.data[y0 * stride..y1 * stride].iter().any(|&b| b != 0) {
            row_buf.fill(255);
            return;
        }
    }

    for (ox, pixel) in row_buf.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let fx = (ox as u32 + ctx.offset_x) * fx_step;
        let px = (fx >> FRACBITS).min(ctx.page_w.saturating_sub(1));

        // ch = 0 → black (foreground), 255 → white (background).
        // At downscale, count the fraction of foreground mask bits in the output pixel's
        // footprint (anti-aliased). At 1:1, use the exact mask bit (binary).
        let ch = if downscale {
            if ctx.mask_shift > 0 {
                // Subsampled max-pool mask: one boolean covers the footprint already.
                let dpx = fx >> (FRACBITS + ctx.mask_shift);
                let dpy = fy >> (FRACBITS + ctx.mask_shift);
                if dpx < mask.width && dpy < mask.height && mask.get(dpx, dpy) {
                    0u8
                } else {
                    255u8
                }
            } else {
                // Anti-aliased: coverage fraction → gray.
                255 - mask_box_coverage(mask, fx, fy, fx_step, fy_step)
            }
        } else if ctx.opts.mask_aa && ctx.mask_shift == 0 {
            // D_AA_ZOOM (opt-in): this branch is only reachable past the exact
            // 1:1 early return above, so `!downscale` here means a genuine
            // upscale (zoom > 1) in at least one axis. Bilinearly interpolate
            // the mask's 0/255 bits instead of the hard nearest-bit lookup —
            // smooths glyph edges under zoom. Default `mask_aa: false` never
            // takes this branch, so the fast nearest path below is untouched.
            255 - mask_bilinear_coverage(mask, fx, fy)
        } else if px < mask.width && py < mask.height && mask.get(px, py) {
            0u8
        } else {
            255u8
        };
        pixel[0] = ch;
        pixel[1] = ch;
        pixel[2] = ch;
        pixel[3] = 255;
    }
}

/// The background pixel of one output column for [`BgRow::Scaled`]: the
/// horizontal half of DjVuLibre's `GPixmapScaler` (#831). The #831 arm always
/// has a column table (`precompute_bilinear_x`).
#[inline(always)]
pub(super) fn bg_scaled_pixel(
    vert: &[[u16; 4]],
    col_start: u32,
    e: Option<BilinearX>,
) -> (u8, u8, u8) {
    let Some(e) = e else {
        return (255, 255, 255);
    };
    let v0 = vert
        .get(e.x0.wrapping_sub(col_start) as usize)
        .copied()
        .unwrap_or([0; 4]);
    let v1 = vert
        .get(e.x1.wrapping_sub(col_start) as usize)
        .copied()
        .unwrap_or([0; 4]);
    let f = |i: usize| scaler_lerp(v0[i] as u32, v1[i] as u32, e.tx) as u8;
    (f(0), f(1), f(2))
}

/// The background pixel of one output column for [`BgRow::Blend`]: the
/// horizontal half of the separable blend. `e` is the column's table entry;
/// without one the column comes from the Q48 accumulator `bg_fx_q`, the same
/// walk `precompute_bilinear_x` replicates.
#[inline(always)]
pub(super) fn bg_blend_pixel(
    vb_row: &[[u16; 4]],
    bg_w: u32,
    col_start: u32,
    e: Option<BilinearX>,
    bg_fx_q: u64,
) -> (u8, u8, u8) {
    let e = e.unwrap_or_else(|| {
        let bg_fx = ((bg_fx_q >> 24) as u32).saturating_sub(FRAC / 2);
        let clamp_w = bg_w.saturating_sub(1);
        let x0 = (bg_fx >> FRACBITS).min(clamp_w);
        BilinearX {
            x0,
            x1: (x0 + 1).min(clamp_w),
            tx: bg_fx & FRAC_MASK,
        }
    });
    // `col_start` shifts full-bg column indices into the windowed row.
    let v0 = vb_row
        .get(e.x0.saturating_sub(col_start) as usize)
        .copied()
        .unwrap_or([0; 4]);
    let v1 = vb_row
        .get(e.x1.saturating_sub(col_start) as usize)
        .copied()
        .unwrap_or([0; 4]);
    let itx = FRAC - e.tx;
    let f = |i: usize| ((v0[i] as u32 * itx + v1[i] as u32 * e.tx + 128) >> 8) as u8;
    (f(0), f(1), f(2))
}

/// Write one bilinear row into `row_buf` (upscale / 1:1).
///
/// A 1:1 row goes to [`native_row_bg_mask`] or [`native_row`]; every other
/// row goes to [`zoom_row`].
///
/// `bx` is the optional per-column table from [`precompute_bilinear_x`]
/// (`None` falls back to the in-loop fixed-point walk — byte-identical).
/// `vblend` is caller-owned scratch for the vertically pre-blended bg row;
/// reusing it across rows avoids a per-row allocation.
///
/// `inline(never)`: inlined into `composite_into`, this loop compiles to
/// code 11–14% slower on native-size renders (COMPOSITE_BILINEAR_NOINLINE in
/// `PERF_EXPERIMENTS.md`).
#[inline(never)]
pub(super) fn composite_rows_bilinear_one(
    ctx: &CompositeContext<'_>,
    oy: u32,
    fx_step: u32,
    fy_step: u32,
    row_buf: &mut [u8],
    bx: Option<&[BilinearX]>,
    vblend: &mut Vec<u16>,
) {
    // 1:1 fast path: fx and fy land on exact pixel centres (tx = ty = 0), so
    // bilinear interpolation degrades to nearest-neighbour. Guard on the bg
    // plane ratio too: if bg is at subsample > 1, the bg coordinates are not
    // integer-aligned even at native scale and bilinear blending is needed.
    if fx_step == FRAC && fy_step == FRAC && ctx.bg_x_q24 == (1 << 24) && ctx.bg_y_q24 == (1 << 24)
    {
        let (fy, py) = row_page_y(ctx, oy, fy_step);
        // Extra-tight path for the common corpus case: bg present, mask
        // present, no palette, no FG44, zero horizontal offset.
        if ctx.offset_x == 0
            && ctx.fg_palette.is_none()
            && ctx.fg44.is_none()
            && let Some(bg) = ctx.bg
        {
            native_row_bg_mask(ctx, bg, py, row_buf);
        } else {
            native_row(ctx, fy, py, fx_step, row_buf);
        }
    } else {
        zoom_row(ctx, oy, fx_step, fy_step, row_buf, bx, vblend);
    }
}

/// The page-space fixed-point y of output row `oy` and its page row.
#[inline(always)]
fn row_page_y(ctx: &CompositeContext<'_>, oy: u32, fy_step: u32) -> (u32, u32) {
    let fy = (oy + ctx.offset_y) * fy_step;
    (fy, (fy >> FRACBITS).min(ctx.page_h.saturating_sub(1)))
}

/// Mask row `py` and the mask width, or `None` below the mask. Hoisted out
/// of the pixel loop: `py` is row-invariant, so this saves a `y*stride`
/// multiply per pixel.
#[inline(always)]
fn mask_row_at<'a>(ctx: &CompositeContext<'a>, py: u32) -> Option<(&'a [u8], u32)> {
    ctx.mask.and_then(|m| {
        if py >= m.height {
            return None;
        }
        let stride = m.row_stride();
        m.data.get(py as usize * stride..).map(|row| (row, m.width))
    })
}

/// True when a mask row has no foreground bit (page margins, blank gaps
/// between text lines), or there is no mask row at all.
#[inline(always)]
fn mask_row_blank(mask_row: Option<(&[u8], u32)>) -> bool {
    match mask_row {
        None => true,
        Some((mask_row, mask_w)) => {
            let nb = (mask_w as usize).div_ceil(8).min(mask_row.len());
            mask_row[..nb].iter().all(|&b| b == 0)
        }
    }
}

/// A 1:1 row with a background, no palette, no FG44 and no horizontal
/// offset — the common corpus case. Precompute the bg row and mask row
/// slices so the inner loop only touches sequential memory with no
/// per-pixel coordinate mapping calls.
#[inline(always)]
fn native_row_bg_mask(ctx: &CompositeContext<'_>, bg: PlaneView<'_>, py: u32, row_buf: &mut [u8]) {
    let bg_row = bg.row(py.min(bg.height().saturating_sub(1)));
    let lut = &ctx.gamma_lut;

    if let Some(mask) = ctx.mask {
        // Has mask: check each pixel for foreground (black).
        let mask_stride = mask.row_stride();
        let mask_py = py.min(mask.height.saturating_sub(1)) as usize;
        let mask_row = mask.data.get(mask_py * mask_stride..).unwrap_or(&[]);

        // A2: pre-expand mask bits to bytes via LUT, then branchless blend.
        let bg_max_px = (bg.width() as usize).saturating_sub(1);
        let mask_limit = mask.width as usize;
        let out_w = row_buf.len() / 4;
        // D1: hoist gamma identity check outside the pixel loop.
        macro_rules! a2_has_mask_loop {
            ($write:expr) => {
                for mb_idx in 0..out_w.div_ceil(8) {
                    let mb = mask_row.get(mb_idx).copied().unwrap_or(0);
                    let exp = &MASK_EXPAND[mb as usize];
                    for j in 0..8usize {
                        let ox = mb_idx * 8 + j;
                        if ox >= out_w {
                            break;
                        }
                        let fg_m = if ox < mask_limit { exp[j] } else { 0u8 };
                        let px = ox.min(bg_max_px);
                        let off = px * 4;
                        let pixel = &mut row_buf[ox * 4..(ox + 1) * 4];
                        let (r, g, b) = if let Some(q) = bg_row.get(off..off + 4) {
                            (q[0] & !fg_m, q[1] & !fg_m, q[2] & !fg_m)
                        } else {
                            (!fg_m, !fg_m, !fg_m)
                        };
                        $write(pixel, r, g, b);
                    }
                }
            };
        }
        if ctx.gamma_is_identity {
            a2_has_mask_loop!(|pixel: &mut [u8], r, g, b| {
                pixel[0] = r;
                pixel[1] = g;
                pixel[2] = b;
                pixel[3] = 255;
            });
        } else {
            a2_has_mask_loop!(|pixel: &mut [u8], r, g, b| {
                pixel[0] = lut[r as usize];
                pixel[1] = lut[g as usize];
                pixel[2] = lut[b as usize];
                pixel[3] = 255;
            });
        }
    } else {
        // No mask: pure background copy with gamma correction.
        // D1: hoist gamma identity check outside the pixel loop.
        if ctx.gamma_is_identity {
            let out_w = row_buf.len() / 4;
            // E1: when bg covers the full output width, bulk-copy the row
            // via memcpy — bg Pixmap always has alpha=255 from YCbCr decode.
            if bg.width() as usize >= out_w {
                row_buf[..out_w * 4].copy_from_slice(&bg_row[..out_w * 4]);
            } else {
                for (ox, pixel) in row_buf.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                    let px = ox.min((bg.width() as usize).saturating_sub(1));
                    let off = px * 4;
                    if let Some(q) = bg_row.get(off..off + 4) {
                        pixel[0] = q[0];
                        pixel[1] = q[1];
                        pixel[2] = q[2];
                    } else {
                        pixel[0] = 255;
                        pixel[1] = 255;
                        pixel[2] = 255;
                    }
                    pixel[3] = 255;
                }
            }
        } else {
            for (ox, pixel) in row_buf.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let px = ox.min((bg.width() as usize).saturating_sub(1));
                let off = px * 4;
                if let Some(q) = bg_row.get(off..off + 4) {
                    pixel[0] = lut[q[0] as usize];
                    pixel[1] = lut[q[1] as usize];
                    pixel[2] = lut[q[2] as usize];
                } else {
                    pixel[0] = 255;
                    pixel[1] = 255;
                    pixel[2] = 255;
                }
                pixel[3] = 255;
            }
        }
    }
}

/// A general 1:1 row (offset, palette, or FG44 present): nearest-neighbour.
#[inline(always)]
fn native_row(ctx: &CompositeContext<'_>, fy: u32, py: u32, fx_step: u32, row_buf: &mut [u8]) {
    let (page_w, page_h) = (ctx.page_w, ctx.page_h);
    // C2: Pre-hoist FG44 y-rows (row-invariant, analogous to bg_rows in B-series path).
    // Eliminates per-fg-pixel: map_plane_center_frac(fy), y0/y1/ty computation, row lookups.
    let fg_rows_1x1 = ctx.fg44.filter(|_| ctx.fg_palette.is_none()).map(|fg| {
        let fg_fy = if ctx.fg_red != 0 {
            fg_native_frac(0, py, page_h, ctx.fg_red, fg).1
        } else {
            map_plane_center_frac(fy, ctx.fg_y_q24)
        };
        let y0 = (fg_fy >> FRACBITS).min(fg.height.saturating_sub(1)) as usize;
        let y1 = (y0 + 1).min(fg.height.saturating_sub(1) as usize);
        let ty = fg_fy & FRAC_MASK;
        let stride = fg.width as usize * 4;
        let row0 = fg.data.get(y0 * stride..).unwrap_or(&[]);
        let row1 = fg.data.get(y1 * stride..).unwrap_or(&[]);
        (row0, row1, fg.width, ty)
    });
    // C2b: Pre-hoist bg row slice (bg_x_q24 == bg_y_q24 == 1<<24 guaranteed by outer
    // condition, so bg_fx == fx and the bg row index == py clamped to bg.height).
    let bg_row_1x1 = ctx
        .bg
        .map(|bg| (bg.row(py.min(bg.height().saturating_sub(1))), bg.width()));
    // C3: Pre-hoist mask row (py is row-invariant; eliminates y*stride multiply per pixel).
    let mask_row_1x1 = mask_row_at(ctx, py);

    // F2: whole-row background fast path.
    // If the mask row has no foreground bits (page margins, blank inter-line gaps — typically
    // 30-40% of rows in text documents), bulk-copy from bg_row instead of dispatching
    // per-pixel between FG44 bilinear and BG44 lookup.
    if mask_row_blank(mask_row_1x1) && native_row_blank(ctx, bg_row_1x1, row_buf) {
        return;
    }

    // G1: Pre-expand the mask row from bit-packed to per-pixel bytes via MASK_EXPAND LUT.
    // Reduces per-pixel mask check from ~7 ops (shift, bounds-check, bit-extract) to a
    // single byte load + compare. Buffer covers up to 600 DPI A4/US-letter (≤4096px);
    // oversized pages fall through to the original bit-extraction path.
    let mut g1_buf = [0u8; G1_MAX];
    let g1_mask = expand_mask_row(mask_row_1x1, &mut g1_buf);

    for (ox, pixel) in row_buf.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let fx = (ox as u32 + ctx.offset_x) * fx_step;
        let px = (fx >> FRACBITS).min(page_w.saturating_sub(1));

        // G1 fast path: single byte load. Falls back to bit-extraction when g1_mask
        // is empty (no mask, or page wider than G1_MAX). LLVM hoists the is_empty()
        // branch as loop-invariant and generates two loop versions.
        let is_fg = if g1_mask.is_empty() {
            mask_row_1x1.is_some_and(|(row, mask_w)| {
                let pxu = px as usize;
                pxu < mask_w as usize
                    && (row.get(pxu >> 3).copied().unwrap_or(0) >> (7 - (pxu & 7))) & 1 != 0
            })
        } else {
            g1_mask.get(px as usize).copied().unwrap_or(0) != 0
        };

        let (r, g, b) = if is_fg {
            if let Some(pal) = ctx.fg_palette {
                let color = lookup_palette_color(pal, ctx.blit_map, ctx.mask, px, py);
                (color.r, color.g, color.b)
            } else if let Some((fg_row0, fg_row1, fg_w, fg_ty)) = fg_rows_1x1 {
                let fg_fx = match px.checked_div(ctx.fg_red) {
                    Some(cell) => cell << FRACBITS,
                    None => map_plane_center_frac(fx, ctx.fg_x_q24),
                };
                bilinear_from_rows(fg_row0, fg_row1, fg_w, fg_fx, fg_ty)
            } else {
                (0, 0, 0)
            }
        } else if let Some((bg_row, bg_w)) = bg_row_1x1 {
            let bx = (px as usize).min(bg_w.saturating_sub(1) as usize);
            let off = bx * 4;
            bg_row
                .get(off..off + 4)
                .map_or((255, 255, 255), |q| (q[0], q[1], q[2]))
        } else {
            (255, 255, 255)
        };

        if ctx.gamma_is_identity {
            pixel[0] = r;
            pixel[1] = g;
            pixel[2] = b;
        } else {
            pixel[0] = ctx.gamma_lut[r as usize];
            pixel[1] = ctx.gamma_lut[g as usize];
            pixel[2] = ctx.gamma_lut[b as usize];
        }
        pixel[3] = 255;
    }
}

/// F2: write a 1:1 row whose mask row is blank. Returns `false`, writing
/// nothing, in the edge case where the output reaches past the background
/// (`out_w` clamped beyond `bg_w`); the caller then runs the per-pixel loop.
#[inline(always)]
fn native_row_blank(
    ctx: &CompositeContext<'_>,
    bg_row_1x1: Option<(&[u8], u32)>,
    row_buf: &mut [u8],
) -> bool {
    if ctx.gamma_is_identity {
        let out_w = row_buf.len() / 4;
        let offset_x = ctx.offset_x as usize;
        if let Some((bg_row, bg_w)) = bg_row_1x1 {
            if offset_x + out_w <= bg_w as usize {
                row_buf.copy_from_slice(&bg_row[offset_x * 4..(offset_x + out_w) * 4]);
                return true;
            }
            // Edge case (out_w clamped beyond bg_w): fall through to per-pixel loop.
        } else {
            row_buf.fill(255);
            return true;
        }
    } else {
        // #443: F2 for non-identity gamma. An all-bg row is just the gamma
        // LUT applied to the bg row (or to white when there is no bg) — a
        // sequential LUT pass that skips the G1 pre-expansion + per-pixel
        // dispatch. Byte-identical to the per-pixel loop for these rows.
        let out_w = row_buf.len() / 4;
        let offset_x = ctx.offset_x as usize;
        let lut = &ctx.gamma_lut;
        if let Some((bg_row, bg_w)) = bg_row_1x1 {
            if offset_x + out_w <= bg_w as usize {
                let src = &bg_row[offset_x * 4..(offset_x + out_w) * 4];
                for (chunk, s) in row_buf
                    .as_chunks_mut::<4>()
                    .0
                    .iter_mut()
                    .zip(src.as_chunks::<4>().0)
                {
                    chunk[0] = lut[s[0] as usize];
                    chunk[1] = lut[s[1] as usize];
                    chunk[2] = lut[s[2] as usize];
                    chunk[3] = 255;
                }
                return true;
            }
            // Edge case (out_w clamped beyond bg_w): fall through.
        } else {
            let white = lut[255];
            for chunk in row_buf.as_chunks_mut::<4>().0 {
                chunk[0] = white;
                chunk[1] = white;
                chunk[2] = white;
                chunk[3] = 255;
            }
            return true;
        }
    }
    false
}

/// G1: the widest mask row [`expand_mask_row`] expands — 600 DPI
/// A4/US-letter.
const G1_MAX: usize = 4096;

/// G1: expand a bit-packed mask row into one byte per pixel in `g1_buf`.
/// Empty when there is no mask or the page is wider than [`G1_MAX`]; the
/// caller then falls back to bit extraction.
#[inline(always)]
fn expand_mask_row<'b>(mask_row: Option<(&[u8], u32)>, g1_buf: &'b mut [u8; G1_MAX]) -> &'b [u8] {
    if let Some((mask_row, mask_w)) = mask_row {
        let mw = mask_w as usize;
        if mw <= G1_MAX {
            let nb = mw.div_ceil(8);
            for (i, &mb) in mask_row[..nb].iter().enumerate() {
                let exp = &MASK_EXPAND[mb as usize];
                let base = i * 8;
                // Write 8 bytes; g1_mask = &g1_buf[..mw] prevents reads past mask_w.
                g1_buf[base..base + 8].copy_from_slice(exp);
            }
            &g1_buf[..mw]
        } else {
            &g1_buf[..0] // page too wide: use fallback bit-extraction below
        }
    } else {
        &g1_buf[..0] // no mask: all pixels are background
    }
}

/// A zoomed row, or a 1:1 row over a subsampled background: bilinear.
///
/// `inline(never)`: inlined into [`composite_rows_bilinear_one`], zoomed
/// renders ran 2–4% slower (COMPOSITE_BILINEAR_SPLIT in
/// `PERF_EXPERIMENTS.md`).
#[inline(never)]
fn zoom_row(
    ctx: &CompositeContext<'_>,
    oy: u32,
    fx_step: u32,
    fy_step: u32,
    row_buf: &mut [u8],
    bx: Option<&[BilinearX]>,
    vblend: &mut Vec<u16>,
) {
    let (page_w, page_h) = (ctx.page_w, ctx.page_h);
    let (fy, py) = row_page_y(ctx, oy, fy_step);

    // B1: replace the per-pixel u64 mul for bg_fx with an exact u64
    // accumulator (add per pixel instead of multiply).
    // bg_fx_q tracks (page_frac + FRAC/2) * bg_x_q24 in Q48; >> 24 gives the
    // FRAC-fixed-point coordinate; subtract FRAC/2 to get the centered sample pos.
    let bg_fx_step_q: u64 = fx_step as u64 * ctx.bg_x_q24;
    let mut bg_fx_q: u64 = (ctx.offset_x as u64 * fx_step as u64 + FRAC as u64 / 2) * ctx.bg_x_q24;

    // B2b: pre-hoist mask row slice for py (eliminates y*stride multiply per pixel).
    let mask_hoist = mask_row_at(ctx, py);

    // #435: row-level all-bg fast path (F2 analog for the B-series path). Pre-scan
    // the hoisted mask row once; if it has no foreground bits (blank margins /
    // inter-line gaps), `is_fg` is constant-false for the whole row, so the
    // `!mask_all_bg &&` short-circuit lets LLVM unswitch the loop and drop the
    // per-pixel bit-extraction. Unlike F2 the bg pixels still need per-pixel
    // resampling, so this saves only the is_fg check (not the whole bg copy).
    let mask_all_bg = mask_row_blank(mask_hoist);

    // B2/B3: precompute bg row slices (y0/y1 are row-invariant), then run the
    // vertical half of the separable bilinear blend once per bg column: with
    // oy fixed, ty and both source rows never change across the row, so
    // v = p0*ity + p1*ty (<= 255*FRAC, exact in u16) is shared by every output
    // pixel sampling that column. The horizontal half later computes
    // (v0*itx + v1*tx + 128) >> 8, which expands to the original 4-term dot
    // product of `bilinear_from_rows` — byte-identical by algebra.
    //
    // The pre-blend only covers the bg columns this row actually samples
    // ([col_start, col_end], from the monotonic accumulator's endpoints) —
    // a region render must not pay for the full bg width (#region bench).
    //
    // #831: a native render of a reduced background takes the other arm,
    // DjVuLibre's `GPixmapScaler`: the vertical pass, rounded to 8 bits,
    // over the columns the row reads; `bg_at` then applies the horizontal
    // pass to each pixel that shows the background.
    let bg_src = match ctx.bg {
        None => None,
        Some(bg) if ctx.bg_red != 0 => {
            Some(scaled_bg_row(ctx, bg, oy, row_buf.len() / 4, bx, vblend))
        }
        Some(bg) => Some(blend_bg_row(ctx, bg, fy, bg_fx_q, bg_fx_step_q, vblend)),
    };

    // D_AA_ZOOM (opt-in): this function is only invoked when `!downscale`
    // (composite_into/composite_rows dispatch downscale to the area-average
    // path), but that includes an exact page-level 1:1 render whose *bg*
    // plane is subsampled (bg_x_q24/bg_y_q24 != 1<<24) — very common for
    // scanned BG44 pages — which fails the 1:1 test in
    // `composite_rows_bilinear_one` and lands here too. Mask AA must only kick in on a genuine
    // zoom (upscale in at least one axis), never on that native 1:1 case, so
    // gate on `fx_step`/`fy_step` directly rather than reusing `!downscale`.
    let mask_upscale =
        ctx.opts.mask_aa && ctx.mask_shift == 0 && (fx_step < FRAC || fy_step < FRAC);

    // The loop is expanded once per `BgRow` variant, so each copy calls its
    // background sampler directly. A per-pixel match on the variant cost
    // 11-13 % on zoomed renders (#831).
    macro_rules! pixel_loop {
        ($bg_at:expr) => {{
            let bg_at = $bg_at;
            for (ox, pixel) in row_buf.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let fx = (ox as u32 + ctx.offset_x) * fx_step;
                let px = (fx >> FRACBITS).min(page_w.saturating_sub(1));

                // `coverage` generalises the binary `is_fg` lookup to a 0..=255
                // foreground fraction: 0 = fully background, 255 = fully foreground,
                // matching `mask_bilinear_coverage`'s convention. With `mask_aa`
                // disabled (default) it only ever takes the values 0 or 255 via the
                // exact same nearest-bit test as before, and the two special cases
                // below reproduce the original is_fg true/false branches exactly —
                // byte-identical output.
                let coverage: u8 = if mask_upscale {
                    if mask_all_bg {
                        0
                    } else {
                        ctx.mask.map_or(0, |m| mask_bilinear_coverage(m, fx, fy))
                    }
                } else if !mask_all_bg
                    && mask_hoist.is_some_and(|(mask_row, mask_w)| {
                        let pxu = px as usize;
                        pxu < mask_w as usize
                            && (mask_row.get(pxu >> 3).copied().unwrap_or(0) >> (7 - (pxu & 7))) & 1
                                != 0
                    })
                {
                    255
                } else {
                    0
                };

                let (r, g, b) = if coverage == 0 {
                    bg_at(ox, bg_fx_q)
                } else {
                    let (fr, fg_g, fb) = if let Some(pal) = ctx.fg_palette {
                        let color = lookup_palette_color(pal, ctx.blit_map, ctx.mask, px, py);
                        (color.r, color.g, color.b)
                    } else if let Some(fg) = ctx.fg44 {
                        let (fg_fx, fg_fy) = if ctx.fg_red != 0 {
                            fg_native_frac(px, py, page_h, ctx.fg_red, fg)
                        } else {
                            (
                                map_plane_center_frac(fx, ctx.fg_x_q24),
                                map_plane_center_frac(fy, ctx.fg_y_q24),
                            )
                        };
                        sample_bilinear(fg, fg_fx, fg_fy)
                    } else {
                        (0, 0, 0)
                    };
                    if coverage == 255 {
                        (fr, fg_g, fb)
                    } else {
                        // Partial coverage (mask_aa only): blend fg/bg proportionally
                        // to the interpolated mask coverage for a smoothed glyph edge.
                        let (br, bg_g, bb) = bg_at(ox, bg_fx_q);
                        let cov = coverage as u32;
                        let inv = 255 - cov;
                        let blend = |f: u8, b: u8| -> u8 {
                            ((f as u32 * cov + b as u32 * inv + 127) / 255) as u8
                        };
                        (blend(fr, br), blend(fg_g, bg_g), blend(fb, bb))
                    }
                };

                // D1: skip LUT scatter reads when gamma is the identity mapping.
                if ctx.gamma_is_identity {
                    pixel[0] = r;
                    pixel[1] = g;
                    pixel[2] = b;
                } else {
                    pixel[0] = ctx.gamma_lut[r as usize];
                    pixel[1] = ctx.gamma_lut[g as usize];
                    pixel[2] = ctx.gamma_lut[b as usize];
                }
                pixel[3] = 255;
                bg_fx_q = bg_fx_q.wrapping_add(bg_fx_step_q);
            }
        }};
    }
    match bg_src {
        None => pixel_loop!(|_: usize, _: u64| (255, 255, 255)),
        Some(BgRow::Scaled(vert, col_start)) => pixel_loop!(|ox: usize, _: u64| {
            bg_scaled_pixel(vert, col_start, bx.and_then(|t| t.get(ox).copied()))
        }),
        Some(BgRow::Blend(vb_row, bg_w, col_start)) => pixel_loop!(|ox: usize, q: u64| {
            bg_blend_pixel(
                vb_row,
                bg_w,
                col_start,
                bx.and_then(|t| t.get(ox).copied()),
                q,
            )
        }),
    }
}

/// The #831 arm of [`zoom_row`]: DjVuLibre's `GPixmapScaler` vertical pass,
/// rounded to 8 bits, over the plane columns the row reads.
#[inline(always)]
fn scaled_bg_row<'v>(
    ctx: &CompositeContext<'_>,
    bg: PlaneView<'_>,
    oy: u32,
    out_w: usize,
    bx: Option<&[BilinearX]>,
    vblend: &'v mut Vec<u16>,
) -> BgRow<'v> {
    let red = ctx.bg_red;
    let (lower, upper, f) = scaler_rows(oy + ctx.offset_y, ctx.page_h, red, bg.height());
    let entry = |ox: usize| {
        bx.and_then(|t| t.get(ox).copied())
            .unwrap_or_else(|| scaler_x(ox as u32 + ctx.offset_x, red, bg.width()))
    };
    let col_start = entry(0).x0;
    let col_end = entry(out_w.saturating_sub(1)).x1.max(col_start);
    let ncols = (col_end - col_start + 1) as usize;
    vblend.clear();
    vblend.resize(ncols * 4, 0);
    let (r0, r1) = (bg.row(lower), bg.row(upper));
    let span = col_start as usize * 4..(col_end as usize + 1) * 4;
    if let (Some(a), Some(b)) = (r0.get(span.clone()), r1.get(span)) {
        for ((v, &a), &b) in vblend.iter_mut().zip(a).zip(b) {
            *v = scaler_lerp(a as u32, b as u32, f) as u16;
        }
    } else {
        for (i, v) in vblend.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            // Truncated rows (partial/streaming decode) read as zeros,
            // as in the bilinear arm below.
            let off = (col_start as usize + i) * 4;
            let p0 = r0.get(off..off + 4);
            let p1 = r1.get(off..off + 4);
            for ch in 0..3 {
                let a = p0.map_or(0, |q| q[ch] as u32);
                let b = p1.map_or(0, |q| q[ch] as u32);
                v[ch] = scaler_lerp(a, b, f) as u16;
            }
        }
    }
    BgRow::Scaled(vblend.as_chunks::<4>().0, col_start)
}

/// The bilinear arm of [`zoom_row`]: the vertical half of the separable
/// blend over the plane columns the row reads.
#[inline(always)]
fn blend_bg_row<'v>(
    ctx: &CompositeContext<'_>,
    bg: PlaneView<'_>,
    fy: u32,
    bg_fx_q: u64,
    bg_fx_step_q: u64,
    vblend: &'v mut Vec<u16>,
) -> BgRow<'v> {
    let bg_fy = map_plane_center_frac(fy, ctx.bg_y_q24);
    let clamp_h = bg.height().saturating_sub(1);
    let y0 = (bg_fy >> FRACBITS).min(clamp_h);
    let y1 = (y0 + 1).min(clamp_h);
    let ty = bg_fy & FRAC_MASK;
    let ity = FRAC - ty;
    let row0 = bg.row(y0);
    let row1 = bg.row(y1);
    let clamp_w = bg.width().saturating_sub(1);
    let fx_at = |q: u64| ((q >> 24) as u32).saturating_sub(FRAC / 2);
    let col_start = (fx_at(bg_fx_q) >> FRACBITS).min(clamp_w);
    let last_q =
        bg_fx_q.wrapping_add(bg_fx_step_q.wrapping_mul(ctx.out_w.saturating_sub(1) as u64));
    let col_end = ((fx_at(last_q) >> FRACBITS).min(clamp_w) + 1).min(clamp_w);
    let ncols = (col_end - col_start + 1) as usize;
    vblend.clear();
    vblend.resize(ncols * 4, 0);
    for (i, v) in vblend.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        // Truncated rows (partial/streaming decode) contribute zeros,
        // exactly like bilinear_from_rows' out-of-range corners.
        let off = (col_start as usize + i) * 4;
        let p0 = row0.get(off..off + 4);
        let p1 = row1.get(off..off + 4);
        for ch in 0..4 {
            let a = p0.map_or(0, |q| q[ch] as u32);
            let b = p1.map_or(0, |q| q[ch] as u32);
            v[ch] = (a * ity + b * ty) as u16;
        }
    }
    BgRow::Blend(vblend.as_chunks::<4>().0, bg.width(), col_start)
}

/// Write one area-average row into `row_buf` (downscale).
#[inline]
#[allow(clippy::too_many_arguments)]
pub(super) fn composite_rows_area_avg_one(
    ctx: &CompositeContext<'_>,
    oy: u32,
    fx_step: u32,
    fy_step: u32,
    bg_fx_step: u32,
    bg_fy_step: u32,
    row_buf: &mut [u8],
    area_avg_x: Option<&[AreaAvgX]>,
) {
    let fy = (oy + ctx.offset_y) * fy_step;
    let bg_fy = ((fy as u64 * ctx.bg_y_q24) >> 24) as u32;
    let bg_y = ctx.bg.map(|bg| area_range(bg.height(), bg_fy, bg_fy_step));

    // #438: row-level all-bg fast path (F2/I3 analog for the area-avg path). The
    // mask footprint's y-band [y0, y1) is row-invariant; if it has no foreground
    // bits, `mask_box_any` would return false for every output pixel, so the
    // `!mask_all_bg &&` short-circuit skips the per-pixel footprint scan entirely.
    // Only the `mask_shift == 0` (mask_box_any) path is covered — the max-pool
    // sub-path indexes a coarser mask. The y range matches `mask_box_any`.
    let mask_all_bg = ctx.mask_shift == 0
        && ctx.mask.is_none_or(|m| {
            let stride = m.row_stride();
            let y0 = (fy >> FRACBITS).min(m.height.saturating_sub(1)) as usize;
            let y1 = ((fy + fy_step) >> FRACBITS).min(m.height) as usize;
            y1 <= y0 || !m.data[y0 * stride..y1 * stride].iter().any(|&b| b != 0)
        });

    for (ox, pixel) in row_buf.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let fallback;
        let ax = if let Some(ax) = area_avg_x.and_then(|xs| xs.get(ox)) {
            *ax
        } else {
            let fx = (ox as u32 + ctx.offset_x) * fx_step;
            let bg_fx = ((fx as u64 * ctx.bg_x_q24) >> 24) as u32;
            let (bg_x0, bg_x1) = area_range(ctx.bg.map_or(0, |bg| bg.width()), bg_fx, bg_fx_step);
            fallback = AreaAvgX { fx, bg_x0, bg_x1 };
            fallback
        };
        let fx = ax.fx;

        // #439: anti-aliased colour downscale. `coverage` (0..255) is the fraction
        // of the output pixel's footprint that is foreground; blend fg/bg
        // proportionally so colour text edges get a smooth gradient instead of the
        // blocky halos the old binary `mask_box_any` produced (colour analog of the
        // AA experiment for bilevel). The max-pool sub-path (mask_shift > 0) stays
        // binary. #438's `mask_all_bg` skips the coverage scan for blank rows.
        let coverage: u8 = if mask_all_bg {
            0
        } else if let Some(m) = ctx.mask {
            if ctx.mask_shift > 0 {
                let px = fx >> (FRACBITS + ctx.mask_shift);
                let pym = fy >> (FRACBITS + ctx.mask_shift);
                if px < m.width && pym < m.height && m.get(px, pym) {
                    255
                } else {
                    0
                }
            } else {
                mask_box_coverage(m, fx, fy, fx_step, fy_step)
            }
        } else {
            0
        };

        let bg_sample = || -> (u8, u8, u8) {
            // `bg_y` is `Some` exactly when `ctx.bg` is.
            match (ctx.bg, bg_y) {
                (Some(bg), Some((bg_y0, bg_y1))) => {
                    sample_area_avg_bounds(bg, ax.bg_x0, ax.bg_x1, bg_y0, bg_y1)
                }
                _ => (255, 255, 255),
            }
        };
        let fg_sample = || -> (u8, u8, u8) {
            if let Some(pal) = ctx.fg_palette {
                let (cx, cy) = mask_box_center_fg(ctx.mask.unwrap(), fx, fy, fx_step, fy_step);
                let color = lookup_palette_color(pal, ctx.blit_map, ctx.mask, cx, cy);
                (color.r, color.g, color.b)
            } else if let Some(fg) = ctx.fg44 {
                let fg_fx = ((fx as u64 * ctx.fg_x_q24) >> 24) as u32;
                let fg_fy = ((fy as u64 * ctx.fg_y_q24) >> 24) as u32;
                let fg_fx_step = ((fx_step as u64 * ctx.fg_x_q24) >> 24) as u32;
                let fg_fy_step = ((fy_step as u64 * ctx.fg_y_q24) >> 24) as u32;
                sample_area_avg(fg, fg_fx, fg_fy, fg_fx_step, fg_fy_step)
            } else {
                (0, 0, 0)
            }
        };

        let (r, g, b) = if coverage == 0 {
            bg_sample()
        } else if coverage == 255 {
            fg_sample()
        } else {
            // Partially covered edge pixel: blend fg over bg by coverage.
            let (fr, fg_, fb) = fg_sample();
            let (br, bg_, bb) = bg_sample();
            let c = coverage as u32;
            let ic = 255 - c;
            let mix = |f: u8, b: u8| ((c * f as u32 + ic * b as u32 + 127) / 255) as u8;
            (mix(fr, br), mix(fg_, bg_), mix(fb, bb))
        };

        if ctx.gamma_is_identity {
            pixel[0] = r;
            pixel[1] = g;
            pixel[2] = b;
        } else {
            pixel[0] = ctx.gamma_lut[r as usize];
            pixel[1] = ctx.gamma_lut[g as usize];
            pixel[2] = ctx.gamma_lut[b as usize];
        }
        pixel[3] = 255;
    }
}
