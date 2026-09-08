//! Photometric foreground/background segmentation.
//!
//! Splits a full-resolution RGBA [`Pixmap`] into a bilevel mask and a
//! sub-sampled background pixmap, the inputs the layered DjVu encoder
//! needs for `Sjbz` + `BG44` (and eventually `FG44` / `FGbz`).
//!
//! The default remains the original deterministic fixed-luminance threshold.
//! Optional knobs add adaptive Sauvola binarisation and conservative background
//! inpainting for fully masked BG blocks.

use crate::bitmap::Bitmap;
use crate::pixmap::Pixmap;

/// Binarisation method used by [`segment_page`].
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub enum Binarization {
    /// Fixed BT.601 luminance threshold from [`SegmentOptions::threshold`].
    #[default]
    Fixed,
    /// Sauvola local adaptive threshold.
    ///
    /// `window` is clamped to at least 3 pixels. `k` is typically in
    /// `0.2..=0.5`; non-finite values fall back to `0.34`.
    Sauvola { window: u32, k: f32 },
}

/// Knobs for [`segment_page`].
#[derive(Debug, Clone, Copy)]
pub struct SegmentOptions {
    /// Luminance cut-off for fixed-threshold masks: pixels with `Y < threshold`
    /// become foreground (black, `1`). BT.601 weights.
    ///
    /// Ignored by [`Binarization::Sauvola`].
    pub threshold: u8,
    /// Background sub-sample factor — output BG dimensions are
    /// `ceil(width / bg_subsample) × ceil(height / bg_subsample)`.
    /// Saturated to `>= 1`. DjVuLibre default: 12.
    pub bg_subsample: u32,
    /// Opt-in content-adaptive background subsample (#569). When `true`,
    /// `bg_subsample` is treated as the *ceiling* and the effective factor is
    /// chosen per page from the measured background detail (variance of the
    /// non-mask pixels at cell level): flat paper stays at the ceiling
    /// (smallest BG44), detailed/photo backgrounds drop to 6 or 3. `false`
    /// (default) keeps the historical fixed factor — byte-identical output.
    pub adaptive_bg_subsample: bool,
    /// Mask-generation method. Defaults to [`Binarization::Fixed`] to preserve
    /// the deterministic historical encoder output.
    pub binarization: Binarization,
    /// When true, a BG block that is fully covered by foreground mask is filled
    /// from the nearest neighbouring unmasked pixels instead of falling back to
    /// the masked block mean. This prevents solid ink from becoming a black BG
    /// cell under text strokes.
    pub bg_inpaint: bool,
    /// When true, fully-masked BG cells are filled by **harmonic diffusion** of
    /// the confident (unmasked-derived) cells — a Laplace/Jacobi relaxation that
    /// solves for the smoothest interpolation. Being maximally smooth, it injects
    /// the least high-frequency wavelet energy, so the IW44 background codes to
    /// fewer bytes than either the ink-colour fallback or the ring-average
    /// [`Self::bg_inpaint`]. Masked cells are covered by the foreground layer, so
    /// their value is invisible; smoothing them is a pure encoder-side size win.
    /// Takes precedence over `bg_inpaint` when both are set.
    pub bg_diffuse: bool,
    /// Opt-in skew correction (#592). Estimates the page's small-angle skew
    /// (projection-profile sharpness maximization over ±5°) and, when it
    /// exceeds ~0.15°, rotates the source upright (bilinear, white fill)
    /// before binarization. Skew is expensive for JB2 — measured +36…+384%
    /// Sjbz at 1° — while OCR is nearly insensitive. Off by default: like
    /// despeckle this intentionally changes output pixels, so it is an
    /// enhancement lever, not a fidelity-preserving one.
    pub deskew: bool,
    /// Opt-in block-level text-vs-photo classification (#562). Mixed layouts
    /// (text + continuous-tone photos on one page) shred photo areas into
    /// mask speckle under per-pixel binarization: Sjbz bloats with noise
    /// components and the photo loses continuous tone. When `true`, 32×32
    /// blocks whose statistics say "photo" (mid-tone-dominated, few sharp
    /// luma edges — the per-block analogue of `classify_content`, #570) are
    /// removed from the mask so the region routes wholly to the background
    /// layer. Pure-text pages classify all-text and produce byte-identical
    /// output. Default `false`.
    pub block_classify: bool,
}

impl Default for SegmentOptions {
    fn default() -> Self {
        Self {
            threshold: 128,
            bg_subsample: 12,
            adaptive_bg_subsample: false,
            binarization: Binarization::Fixed,
            bg_inpaint: false,
            bg_diffuse: false,
            deskew: false,
            block_classify: false,
        }
    }
}

impl SegmentOptions {
    /// Archival-grade profile: a denser background sample grid
    /// (`bg_subsample = 6` vs the default 12), other knobs at their defaults.
    ///
    /// This is the single source of truth for the `Archival` background
    /// resolution — `EncodeQuality::default_segment_options` and the CLI both
    /// route through it instead of re-spelling the `bg_subsample: 6` literal.
    pub fn archival() -> Self {
        Self {
            bg_subsample: 6,
            adaptive_bg_subsample: false,
            ..Self::default()
        }
    }
}

/// Result of [`segment_page`].
pub struct SegmentedPage {
    /// Full-resolution bilevel mask. `true` = foreground/ink.
    pub mask: Bitmap,
    /// Sub-sampled background pixmap, mean-colour per block of the non-mask
    /// source pixels. Fully masked blocks either fall back to their full-block
    /// mean (default) or, with [`SegmentOptions::bg_inpaint`], to neighbouring
    /// unmasked pixels.
    pub bg: Pixmap,
}

#[derive(Debug, Clone, Copy, Default)]
struct ColorAccum {
    r: u64,
    g: u64,
    b: u64,
    n: u64,
}

impl ColorAccum {
    fn add(&mut self, r: u8, g: u8, b: u8) {
        self.r += u64::from(r);
        self.g += u64::from(g);
        self.b += u64::from(b);
        self.n += 1;
    }

    fn color(self) -> Option<(u8, u8, u8)> {
        if self.n == 0 {
            return None;
        }
        Some((
            (self.r / self.n) as u8,
            (self.g / self.n) as u8,
            (self.b / self.n) as u8,
        ))
    }
}

/// Pick the effective background subsample from measured background detail
/// (#569): per 12×12 cell, the luma spread (max−min) of unmasked pixels;
/// the fraction of cells with spread > 24 is the detail statistic.
///
/// Flat paper (text pages) → the ceiling (usually 12, smallest BG44);
/// moderately detailed backgrounds → 6; photo/texture-heavy → 3. Sampled at
/// a row stride so the pass is a small fraction of segmentation cost.
fn choose_bg_subsample(rgba: &Pixmap, mask: &Bitmap, ceiling: u32) -> u32 {
    let (w, h) = (rgba.width as usize, rgba.height as usize);
    const CELL: usize = 12;
    let cols = w.div_ceil(CELL);
    let rows = h.div_ceil(CELL);
    let mut spread_hi = 0usize;
    let mut cells = 0usize;
    // Sample every other cell row for speed.
    let mut cy = 0usize;
    while cy < rows {
        for cx in 0..cols {
            let x0 = cx * CELL;
            let y0 = cy * CELL;
            let x1 = (x0 + CELL).min(w);
            let y1 = (y0 + CELL).min(h);
            let mut lo = 255u8;
            let mut hi = 0u8;
            let mut seen = false;
            for y in y0..y1 {
                let row = &rgba.data[y * w * 4..(y + 1) * w * 4];
                for x in x0..x1 {
                    if mask.get(x as u32, y as u32) {
                        continue;
                    }
                    let l = luminance(row[x * 4], row[x * 4 + 1], row[x * 4 + 2]);
                    lo = lo.min(l);
                    hi = hi.max(l);
                    seen = true;
                }
            }
            if seen {
                cells += 1;
                if hi - lo > 24 {
                    spread_hi += 1;
                }
            }
        }
        cy += 2;
    }
    if cells == 0 {
        return ceiling;
    }
    let detail_pct = spread_hi * 100 / cells;
    if detail_pct < 5 {
        ceiling
    } else if detail_pct < 30 {
        6.min(ceiling)
    } else {
        3.min(ceiling)
    }
}

#[inline]
fn luminance(r: u8, g: u8, b: u8) -> u8 {
    (((r as u32) * 306 + (g as u32) * 601 + (b as u32) * 117) >> 10) as u8
}

/// Estimate the page's small-angle skew in degrees (#592).
///
/// Projection-profile sharpness maximization: binarize a strided luminance
/// sample, then for each candidate angle accumulate ink counts into sheared
/// row buckets (`row = y + x·tanθ`) and score the profile by the sum of
/// squared adjacent-row differences — sharpest when text baselines align
/// with the projection. Coarse-to-fine sweep over ±5° (0.5° → 0.1° → 0.02°).
///
/// Returns the **correction angle**: rotating the page by the returned value
/// (via the internal small-angle rotation) aligns the text baselines.
pub fn estimate_skew(rgba: &Pixmap) -> f32 {
    let (w, h) = (rgba.width as usize, rgba.height as usize);
    if w < 64 || h < 64 {
        return 0.0;
    }
    // Dark-pixel sample: every row (row resolution is the estimator's angular
    // sensitivity — striding rows would plateau the score for small angles),
    // strided columns for cost.
    let stride = 2usize;
    let mut ink: Vec<(u32, u32)> = Vec::new();
    for y in 0..h {
        let row = &rgba.data[y * w * 4..(y * w + w) * 4];
        for x in (0..w).step_by(stride) {
            let p = &row[x * 4..x * 4 + 3];
            if luminance(p[0], p[1], p[2]) < 128 {
                ink.push((x as u32, y as u32));
            }
        }
    }
    if ink.len() < 256 {
        return 0.0; // not enough ink for a meaningful profile
    }

    let score = |deg: f32| -> f64 {
        let t = deg.to_radians().tan();
        let bins = h + (w as f32 * t.abs()) as usize + 2;
        let mut hist = vec![0u32; bins];
        let last = bins - 1;
        let off = if t < 0.0 { w as f32 * -t } else { 0.0 };
        for &(x, y) in &ink {
            let r = (y as f32 + x as f32 * t + off) as usize;
            hist[r.min(last)] += 1;
        }
        hist.windows(2)
            .map(|p| {
                let d = p[1] as f64 - p[0] as f64;
                d * d
            })
            .sum()
    };

    // Ties (score plateaus happen on small pages where a sub-bucket shear
    // moves nothing) resolve to the plateau centre, not its first edge.
    let sweep = |centre: f32, half: f32, step: f32| -> f32 {
        let mut best = f64::MIN;
        let (mut first, mut last_a) = (centre, centre);
        let mut a = centre - half;
        while a <= centre + half + 1e-6 {
            let sc = score(a);
            if sc > best {
                best = sc;
                first = a;
                last_a = a;
            } else if sc == best {
                last_a = a;
            }
            a += step;
        }
        (first + last_a) / 2.0
    };

    let c = sweep(0.0, 5.0, 0.5);
    let c = sweep(c, 0.5, 0.1);
    let c = sweep(c, 0.1, 0.02);

    // Sub-step refinement: a residual of even 0.02° after correction costs
    // more than the alignment recovers (double resampling + micro-shear), so
    // interpolate the score peak with a parabola through the final three
    // sweep points instead of stopping at the grid.
    let step = 0.02f32;
    let (sl, sc, sr) = (score(c - step), score(c), score(c + step));
    let denom = sl - 2.0 * sc + sr;
    if denom.abs() > f64::EPSILON {
        let shift = 0.5 * (sl - sr) / denom;
        c + step * (shift as f32).clamp(-1.0, 1.0)
    } else {
        c
    }
}

/// Rotate `src` by `deg` degrees around its centre (bilinear inverse mapping,
/// white fill) — the small-angle correction used by
/// [`SegmentOptions::deskew`].
fn rotate_small(src: &Pixmap, deg: f32) -> Pixmap {
    let rad = deg.to_radians();
    let (sn, cs) = rad.sin_cos();
    let (w, h) = (src.width as i32, src.height as i32);
    let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
    let mut out = Pixmap::white(src.width, src.height);
    for y in 0..h {
        for x in 0..w {
            let dx = x as f32 - cx;
            let dy = y as f32 - cy;
            let sx = cs * dx + sn * dy + cx;
            let sy = -sn * dx + cs * dy + cy;
            let x0f = sx.floor();
            let y0f = sy.floor();
            if x0f < 0.0 || y0f < 0.0 || x0f as i32 + 1 >= w || y0f as i32 + 1 >= h {
                continue; // white fill
            }
            let (fx, fy) = (sx - x0f, sy - y0f);
            let (x0, y0) = (x0f as usize, y0f as usize);
            let idx = |xx: usize, yy: usize| (yy * src.width as usize + xx) * 4;
            let di = (y as usize * src.width as usize + x as usize) * 4;
            for ch in 0..3 {
                let p00 = src.data[idx(x0, y0) + ch] as f32;
                let p10 = src.data[idx(x0 + 1, y0) + ch] as f32;
                let p01 = src.data[idx(x0, y0 + 1) + ch] as f32;
                let p11 = src.data[idx(x0 + 1, y0 + 1) + ch] as f32;
                let v = p00 * (1.0 - fx) * (1.0 - fy)
                    + p10 * fx * (1.0 - fy)
                    + p01 * (1.0 - fx) * fy
                    + p11 * fx * fy;
                out.data[di + ch] = v.round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    out
}

/// Block edge length for the #562 text-vs-photo classifier.
const CLASSIFY_BLOCK: usize = 32;

/// Classify `CLASSIFY_BLOCK`-sized blocks as photo (continuous-tone) and clear
/// the mask inside them (#562).
///
/// Per-block features mirror the page-level `classify_content` (#570), whose
/// corpus calibration showed photos at ~0.04% sharp horizontal luma edges vs
/// ≥0.36% for text, and mid-tone domination for continuous tone: a block is
/// "photo" when non-white pixels (luma ≤ 223) exceed 60% of the block while sharp
/// neighbour deltas (>64) stay under 1.5% of pairs — the continuous-tone
/// signature — **or** when the binarized mask flips ink↔paper on more than a
/// quarter of horizontal neighbour pairs, the halftone-dot signature (dot
/// grids shred into transitions every 1–2 px; text edges flip an order of
/// magnitude less). The block map is smoothed with a 3×3 majority vote so
/// isolated misclassifications (a photo block of large type, a textured photo
/// corner) don't punch holes either way.
fn clear_photo_blocks(mask: &mut Bitmap, rgba: &Pixmap) {
    let (w, h) = (rgba.width as usize, rgba.height as usize);
    let bw = w.div_ceil(CLASSIFY_BLOCK);
    let bh = h.div_ceil(CLASSIFY_BLOCK);
    if bw == 0 || bh == 0 {
        return;
    }

    let mut photo = vec![false; bw * bh];
    for by in 0..bh {
        for bx in 0..bw {
            let x0 = bx * CLASSIFY_BLOCK;
            let y0 = by * CLASSIFY_BLOCK;
            let x1 = (x0 + CLASSIFY_BLOCK).min(w);
            let y1 = (y0 + CLASSIFY_BLOCK).min(h);
            let mut midtone = 0u32;
            let mut sharp = 0u32;
            let mut flips = 0u32;
            let mut pairs = 0u32;
            let mut n = 0u32;
            for y in y0..y1 {
                let row = &rgba.data[y * w * 4..(y * w + w) * 4];
                let mut prev: Option<u32> = None;
                let mut prev_ink: Option<bool> = None;
                for x in x0..x1 {
                    let p = &row[x * 4..x * 4 + 3];
                    let l = luminance(p[0], p[1], p[2]) as u32;
                    if l <= 223 {
                        midtone += 1;
                    }
                    let ink = mask.get(x as u32, y as u32);
                    if let Some(pl) = prev {
                        pairs += 1;
                        if pl.abs_diff(l) > 64 {
                            sharp += 1;
                        }
                        if prev_ink == Some(!ink) {
                            flips += 1;
                        }
                    }
                    prev = Some(l);
                    prev_ink = Some(ink);
                    n += 1;
                }
            }
            let n = n.max(1);
            let pairs = pairs.max(1);
            let continuous = midtone * 5 > n * 3 && sharp * 200 < pairs * 3;
            let halftone = flips * 4 > pairs;
            photo[by * bw + bx] = continuous || halftone;
        }
    }

    // 3×3 majority smoothing.
    let smoothed: Vec<bool> = (0..bw * bh)
        .map(|i| {
            let (bx, by) = (i % bw, i / bw);
            let mut yes = 0u32;
            let mut total = 0u32;
            for dy in -1i32..=1 {
                for dx in -1i32..=1 {
                    let (nx, ny) = (bx as i32 + dx, by as i32 + dy);
                    if nx >= 0 && ny >= 0 && (nx as usize) < bw && (ny as usize) < bh {
                        total += 1;
                        if photo[ny as usize * bw + nx as usize] {
                            yes += 1;
                        }
                    }
                }
            }
            yes * 2 > total
        })
        .collect();

    for by in 0..bh {
        for bx in 0..bw {
            if !smoothed[by * bw + bx] {
                continue;
            }
            let x1 = ((bx + 1) * CLASSIFY_BLOCK).min(w);
            let y1 = ((by + 1) * CLASSIFY_BLOCK).min(h);
            for y in (by * CLASSIFY_BLOCK)..y1 {
                for x in (bx * CLASSIFY_BLOCK)..x1 {
                    mask.set(x as u32, y as u32, false);
                }
            }
        }
    }
}

/// Minimum estimated skew worth correcting; below it the resampling blur
/// costs more than the alignment recovers.
const DESKEW_MIN_DEG: f32 = 0.15;

/// Segment an RGBA page into a bilevel mask + sub-sampled background.
///
/// Empty input (`width == 0` or `height == 0`) returns empty outputs.
pub fn segment_page(rgba: &Pixmap, opts: &SegmentOptions) -> SegmentedPage {
    // #592: opt-in skew correction — estimate, and re-run on the rotated
    // source when the page is measurably skewed.
    if opts.deskew {
        let correction = estimate_skew(rgba);
        if correction.abs() >= DESKEW_MIN_DEG {
            let upright = rotate_small(rgba, correction);
            let mut inner = *opts;
            inner.deskew = false; // single correction pass
            return segment_page(&upright, &inner);
        }
    }

    let w = rgba.width;
    let h = rgba.height;

    let mut mask = Bitmap::new(w, h);
    if w == 0 || h == 0 {
        return SegmentedPage {
            mask,
            bg: Pixmap::default(),
        };
    }

    match opts.binarization {
        Binarization::Fixed => fill_fixed_mask(&mut mask, rgba, opts.threshold),
        Binarization::Sauvola { window, k } => {
            let luma = luminance_plane(rgba);
            fill_sauvola_mask(&mut mask, &luma, w, h, window, k);
        }
    }

    // #562: block-level text-vs-photo classification — photo blocks leave the
    // mask entirely so their continuous tone survives in the background.
    if opts.block_classify {
        clear_photo_blocks(&mut mask, rgba);
    }

    // #569: content-adaptive background subsample — measured detail of the
    // non-mask pixels picks the effective factor, with `opts.bg_subsample`
    // as the ceiling. Runs after the mask so under-ink pixels don't count
    // as "background detail".
    let sub = if opts.adaptive_bg_subsample {
        choose_bg_subsample(rgba, &mask, opts.bg_subsample.max(1))
    } else {
        opts.bg_subsample.max(1)
    };

    let bw = w.div_ceil(sub);
    let bh = h.div_ceil(sub);
    let mut bg = Pixmap::white(bw, bh);

    // Each BG cell's colour is an independent block-mean over the (mask-excluded)
    // source pixels of its `sub × sub` block — cells never read each other, only
    // the shared read-only `rgba`/`mask`. So the BG-cell fill is embarrassingly
    // parallel; with the `parallel` feature, split `bg` into disjoint mutable row
    // slices and fill them concurrently. Same pixels, same colour per cell →
    // byte-identical to the sequential nested loop. (This is the bulk of
    // `segment_page`: `block_mean` collectively scans the whole page.)
    #[cfg(feature = "parallel")]
    {
        use rayon::prelude::*;
        let bwu = bw as usize;
        bg.data
            .par_chunks_mut(bwu * 4)
            .enumerate()
            .for_each(|(by, bg_row)| {
                let by = by as u32;
                for bx in 0..bw {
                    let (r, g, b) = bg_cell_color(rgba, &mask, opts, sub, w, h, bw, bh, bx, by);
                    let o = bx as usize * 4;
                    bg_row[o] = r;
                    bg_row[o + 1] = g;
                    bg_row[o + 2] = b;
                }
            });
    }

    #[cfg(not(feature = "parallel"))]
    for by in 0..bh {
        for bx in 0..bw {
            let (r, g, b) = bg_cell_color(rgba, &mask, opts, sub, w, h, bw, bh, bx, by);
            bg.set_rgb(bx, by, r, g, b);
        }
    }

    if opts.bg_diffuse {
        diffuse_masked_cells(&mut bg, rgba, &mask, sub, w, h);
    }

    SegmentedPage { mask, bg }
}

/// Overwrite every fully-masked BG cell with the harmonic (smoothest)
/// interpolation of the confident cells, minimising the wavelet energy the IW44
/// background codec must spend on invisible pixels.
///
/// A BG cell is *confident* when its `sub × sub` source block holds at least one
/// unmasked pixel (so its colour is real background); the fill loop above already
/// wrote those. The remaining *masked* cells are entirely covered by foreground
/// ink, so their value is never rendered — we are free to pick whatever codes
/// smallest. The smoothest such choice is the solution of Laplace's equation with
/// the confident cells as Dirichlet boundary, approximated here by Gauss-Seidel
/// relaxation (in-place, so it converges roughly twice as fast as Jacobi).
fn diffuse_masked_cells(bg: &mut Pixmap, rgba: &Pixmap, mask: &Bitmap, sub: u32, w: u32, h: u32) {
    let bw = bg.width as usize;
    let bh = bg.height as usize;
    if bw == 0 || bh == 0 {
        return;
    }

    // Confidence grid: a cell is fixed iff its block has any unmasked pixel.
    let mut confident = vec![false; bw * bh];
    let mut any_masked = false;
    for by in 0..bh {
        for bx in 0..bw {
            let x0 = bx as u32 * sub;
            let x1 = (x0 + sub).min(w);
            let y0 = by as u32 * sub;
            let y1 = (y0 + sub).min(h);
            let c = block_mean(rgba, mask, x0, x1, y0, y1, true).is_some();
            confident[by * bw + bx] = c;
            any_masked |= !c;
        }
    }
    if !any_masked {
        return;
    }

    // Per-channel f32 working buffers seeded from the current BG (confident cells
    // hold their real colour; masked cells start at the mean of all confident
    // cells for faster convergence).
    let mut plane = [
        vec![0f32; bw * bh],
        vec![0f32; bw * bh],
        vec![0f32; bw * bh],
    ];
    let mut seed = [0f64; 3];
    let mut nconf = 0u64;
    for i in 0..bw * bh {
        let px = &bg.data[i * 4..i * 4 + 3];
        for c in 0..3 {
            plane[c][i] = px[c] as f32;
        }
        if confident[i] {
            nconf += 1;
            for c in 0..3 {
                seed[c] += px[c] as f64;
            }
        }
    }
    if nconf == 0 {
        return; // no boundary to diffuse from; leave the fallback fills as-is
    }
    let seed = [
        (seed[0] / nconf as f64) as f32,
        (seed[1] / nconf as f64) as f32,
        (seed[2] / nconf as f64) as f32,
    ];
    for i in 0..bw * bh {
        if !confident[i] {
            for c in 0..3 {
                plane[c][i] = seed[c];
            }
        }
    }

    // Gauss-Seidel relaxation: each masked cell ← average of its 4-neighbours
    // (edges drop the missing neighbour). Cap iterations at the grid's larger
    // dimension (enough for a fill to propagate across the widest masked span)
    // and stop early once the largest per-cell update falls below 0.5/255.
    let max_iters = bw.max(bh).clamp(16, 512);
    for _ in 0..max_iters {
        let mut max_delta = 0f32;
        for by in 0..bh {
            for bx in 0..bw {
                let i = by * bw + bx;
                if confident[i] {
                    continue;
                }
                for p in plane.iter_mut() {
                    let mut sum = 0f32;
                    let mut n = 0f32;
                    if bx > 0 {
                        sum += p[i - 1];
                        n += 1.0;
                    }
                    if bx + 1 < bw {
                        sum += p[i + 1];
                        n += 1.0;
                    }
                    if by > 0 {
                        sum += p[i - bw];
                        n += 1.0;
                    }
                    if by + 1 < bh {
                        sum += p[i + bw];
                        n += 1.0;
                    }
                    let nv = sum / n;
                    let d = (nv - p[i]).abs();
                    if d > max_delta {
                        max_delta = d;
                    }
                    p[i] = nv;
                }
            }
        }
        if max_delta < 0.5 {
            break;
        }
    }

    // Write the relaxed values back into the masked cells (confident cells keep
    // their exact original colour — visible background is untouched).
    for i in 0..bw * bh {
        if confident[i] {
            continue;
        }
        for (c, p) in plane.iter().enumerate() {
            bg.data[i * 4 + c] = p[i].round().clamp(0.0, 255.0) as u8;
        }
    }
}

/// Colour of a single BG cell `(bx, by)`: the mask-excluded block mean, falling
/// back to inpainting (when enabled) then the full-block mean then white. Pure
/// function of the read-only inputs, so it is safe to call from parallel workers.
#[allow(clippy::too_many_arguments)]
fn bg_cell_color(
    rgba: &Pixmap,
    mask: &Bitmap,
    opts: &SegmentOptions,
    sub: u32,
    w: u32,
    h: u32,
    bw: u32,
    bh: u32,
    bx: u32,
    by: u32,
) -> (u8, u8, u8) {
    let x0 = bx * sub;
    let x1 = (x0 + sub).min(w);
    let y0 = by * sub;
    let y1 = (y0 + sub).min(h);
    block_mean(rgba, mask, x0, x1, y0, y1, true)
        .or_else(|| {
            opts.bg_inpaint
                .then(|| inpaint_block_mean(rgba, mask, bx, by, sub, bw, bh))
                .flatten()
        })
        .or_else(|| block_mean(rgba, mask, x0, x1, y0, y1, false))
        .unwrap_or((255, 255, 255))
}

fn luminance_plane(rgba: &Pixmap) -> Vec<u8> {
    let mut luma = Vec::with_capacity((rgba.width * rgba.height) as usize);
    for y in 0..rgba.height {
        for x in 0..rgba.width {
            let (r, g, b) = rgba.get_rgb(x, y);
            luma.push(luminance(r, g, b));
        }
    }
    luma
}

fn fill_fixed_mask(mask: &mut Bitmap, rgba: &Pixmap, threshold: u8) {
    let threshold = u32::from(threshold);
    // Row-slice the packed RGBA rows (`chunks_exact(4)`) instead of a per-pixel
    // `rgba.get_rgb` (bounds check + `(y*width+x)*4` multiply) and set mask bits
    // directly in the row byte (`|= 0x80 >> (x&7)`) instead of `mask.set` (which
    // recomputes `y*stride + x/8` per call). `mask` starts cleared, so OR-ing in
    // only the foreground bits is byte-identical. Same PS2-class win.
    let w = rgba.width as usize;
    let mstride = mask.row_stride();
    for y in 0..mask.height as usize {
        let src = &rgba.data[y * w * 4..(y + 1) * w * 4];
        let mrow = &mut mask.data[y * mstride..(y + 1) * mstride];
        for (x, px) in src.chunks_exact(4).enumerate() {
            if u32::from(luminance(px[0], px[1], px[2])) < threshold {
                mrow[x >> 3] |= 0x80 >> (x & 7);
            }
        }
    }
}

fn fill_sauvola_mask(mask: &mut Bitmap, luma: &[u8], w: u32, h: u32, window: u32, k: f32) {
    let window = window.max(3);
    let radius = window / 2;
    let k = if k.is_finite() { k } else { 0.34 };
    let k = k.clamp(0.0, 1.0);
    let (sum, sum_sq) = integral_luma(luma, w, h);
    let stride = w as usize + 1;

    #[cfg(feature = "parallel")]
    {
        use rayon::prelude::*;
        let mask_stride = mask.row_stride();
        mask.data
            .par_chunks_mut(mask_stride)
            .enumerate()
            .for_each(|(y, row)| {
                fill_sauvola_row(row, luma, &sum, &sum_sq, stride, w, h, radius, k, y as u32);
            });
    }

    #[cfg(not(feature = "parallel"))]
    fill_sauvola_mask_sequential(mask, luma, &sum, &sum_sq, stride, w, h, radius, k);
}

#[allow(clippy::too_many_arguments)]
#[cfg_attr(feature = "parallel", allow(dead_code))]
fn fill_sauvola_mask_sequential(
    mask: &mut Bitmap,
    luma: &[u8],
    sum: &[u64],
    sum_sq: &[u64],
    stride: usize,
    w: u32,
    h: u32,
    radius: u32,
    k: f32,
) {
    let mask_stride = mask.row_stride();
    for (y, row) in mask.data.chunks_mut(mask_stride).enumerate() {
        fill_sauvola_row(row, luma, sum, sum_sq, stride, w, h, radius, k, y as u32);
    }
}

#[allow(clippy::too_many_arguments)]
fn fill_sauvola_row(
    mask_row: &mut [u8],
    luma: &[u8],
    sum: &[u64],
    sum_sq: &[u64],
    stride: usize,
    w: u32,
    h: u32,
    radius: u32,
    k: f32,
    y: u32,
) {
    let y0 = y.saturating_sub(radius);
    let y1 = (y + radius + 1).min(h);
    for x in 0..w {
        let x0 = x.saturating_sub(radius);
        let x1 = (x + radius + 1).min(w);
        let area = f64::from((x1 - x0) * (y1 - y0));
        let s = rect_sum(sum, stride, x0, y0, x1, y1) as f64;
        let ss = rect_sum(sum_sq, stride, x0, y0, x1, y1) as f64;
        let mean = s / area;
        let variance = (ss / area - mean * mean).max(0.0);
        let stddev = variance.sqrt();
        let threshold = mean * (1.0 + f64::from(k) * (stddev / 128.0 - 1.0));
        let idx = (y * w + x) as usize;
        if f64::from(luma[idx]) < threshold {
            mask_row[x as usize >> 3] |= 0x80 >> (x & 7);
        }
    }
}

fn integral_luma(luma: &[u8], w: u32, h: u32) -> (Vec<u64>, Vec<u64>) {
    let stride = w as usize + 1;
    let len = stride * (h as usize + 1);
    let mut sum = vec![0u64; len];
    let mut sum_sq = vec![0u64; len];

    for y in 0..h as usize {
        let mut row_sum = 0u64;
        let mut row_sum_sq = 0u64;
        for x in 0..w as usize {
            let v = u64::from(luma[y * w as usize + x]);
            row_sum += v;
            row_sum_sq += v * v;
            let dst = (y + 1) * stride + x + 1;
            sum[dst] = sum[dst - stride] + row_sum;
            sum_sq[dst] = sum_sq[dst - stride] + row_sum_sq;
        }
    }

    (sum, sum_sq)
}

fn rect_sum(integral: &[u64], stride: usize, x0: u32, y0: u32, x1: u32, y1: u32) -> u64 {
    let (x0, y0, x1, y1) = (x0 as usize, y0 as usize, x1 as usize, y1 as usize);
    integral[y1 * stride + x1] + integral[y0 * stride + x0]
        - integral[y0 * stride + x1]
        - integral[y1 * stride + x0]
}

fn block_mean(
    rgba: &Pixmap,
    mask: &Bitmap,
    x0: u32,
    x1: u32,
    y0: u32,
    y1: u32,
    unmasked_only: bool,
) -> Option<(u8, u8, u8)> {
    let mut acc = ColorAccum::default();
    // Row-slice the RGBA block rows instead of per-pixel `rgba.get_rgb` (index
    // multiply + bounds check), and read the mask row once per row instead of
    // per-pixel `mask.get` (which recomputes `y*stride`). Same pixels, same
    // accumulation order → byte-identical block mean. (PS2 class; block_mean is
    // called once per BG cell so it collectively scans the whole page.)
    let w = rgba.width as usize;
    let mstride = mask.row_stride();
    for y in y0..y1 {
        let ry = y as usize;
        let row = &rgba.data[(ry * w + x0 as usize) * 4..(ry * w + x1 as usize) * 4];
        if unmasked_only {
            let mrow = &mask.data[ry * mstride..(ry + 1) * mstride];
            for (i, px) in row.chunks_exact(4).enumerate() {
                let x = x0 as usize + i;
                if (mrow[x >> 3] >> (7 - (x & 7))) & 1 != 0 {
                    continue;
                }
                acc.add(px[0], px[1], px[2]);
            }
        } else {
            for px in row.chunks_exact(4) {
                acc.add(px[0], px[1], px[2]);
            }
        }
    }
    acc.color()
}

fn inpaint_block_mean(
    rgba: &Pixmap,
    mask: &Bitmap,
    bx: u32,
    by: u32,
    sub: u32,
    bw: u32,
    bh: u32,
) -> Option<(u8, u8, u8)> {
    let max_radius = bw.max(bh);
    for radius in 1..=max_radius {
        let bx0 = bx.saturating_sub(radius);
        let by0 = by.saturating_sub(radius);
        let bx1 = (bx + radius + 1).min(bw);
        let by1 = (by + radius + 1).min(bh);
        let mut acc = ColorAccum::default();

        for ny in by0..by1 {
            for nx in bx0..bx1 {
                let dx = nx.abs_diff(bx);
                let dy = ny.abs_diff(by);
                if dx.max(dy) != radius {
                    continue;
                }
                let x0 = nx * sub;
                let x1 = (x0 + sub).min(rgba.width);
                let y0 = ny * sub;
                let y1 = (y0 + sub).min(rgba.height);
                for y in y0..y1 {
                    for x in x0..x1 {
                        if !mask.get(x, y) {
                            let (r, g, b) = rgba.get_rgb(x, y);
                            acc.add(r, g, b);
                        }
                    }
                }
            }
        }

        if let Some(color) = acc.color() {
            return Some(color);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic text-like page: horizontal word-ish bars with pseudo-random
    /// (deterministic LCG) line spacing, word lengths and gaps — a strictly
    /// periodic pattern would alias the projection-profile estimator. Rotated
    /// by `deg` via the deskew rotation helper itself.
    fn striped_page(deg: f32) -> Pixmap {
        let mut pm = Pixmap::white(512, 512);
        let mut rng = 0x2545_F491u32;
        let mut next = move |m: u32| {
            rng = rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (rng >> 16) % m
        };
        let mut y = 24u32;
        while y + 10 < 488 {
            let mut x = 24 + next(20);
            while x + 12 < 488 {
                let wlen = 12 + next(40);
                for yy in y..y + 6 {
                    for xx in x..(x + wlen).min(488) {
                        pm.set_rgb(xx, yy, 0, 0, 0);
                    }
                }
                x += wlen + 6 + next(12);
            }
            y += 20 + next(9);
        }
        if deg == 0.0 {
            pm
        } else {
            rotate_small(&pm, deg)
        }
    }

    /// #592: the estimator recovers a synthetic 1° skew to within ±0.06° and
    /// reports ~0 on the upright page.
    #[test]
    fn estimate_skew_finds_synthetic_rotation() {
        assert!(estimate_skew(&striped_page(0.0)).abs() <= 0.06);
        let est = estimate_skew(&striped_page(1.0));
        assert!(
            (est + 1.0).abs() <= 0.06,
            "1° skew must estimate a ≈−1° correction, got {est}"
        );
    }

    /// #592: `deskew: true` on a skewed page yields a mask much closer to the
    /// upright mask than segmenting the skewed page directly; on an upright
    /// page it is a no-op (below the correction threshold).
    #[test]
    fn deskew_option_corrects_synthetic_skew() {
        let upright = striped_page(0.0);
        let skewed = striped_page(1.0);
        let opts_plain = SegmentOptions::default();
        let opts_deskew = SegmentOptions {
            deskew: true,
            ..SegmentOptions::default()
        };

        let ref_mask = segment_page(&upright, &opts_plain).mask;
        let diff = |m: &Bitmap| -> u64 {
            let mut d = 0u64;
            for y in 0..m.height {
                for x in 0..m.width {
                    if m.get(x, y) != ref_mask.get(x, y) {
                        d += 1;
                    }
                }
            }
            d
        };

        let skewed_diff = diff(&segment_page(&skewed, &opts_plain).mask);
        let fixed_diff = diff(&segment_page(&skewed, &opts_deskew).mask);
        assert!(
            fixed_diff * 2 < skewed_diff,
            "deskew must recover most of the skew: skewed {skewed_diff}, deskewed {fixed_diff}"
        );

        // Upright page: below the threshold → identical to the plain path.
        let a = segment_page(&upright, &opts_plain).mask;
        let b = segment_page(&upright, &opts_deskew).mask;
        assert_eq!(a.data, b.data, "deskew must be a no-op on an upright page");
    }

    /// #562: a page with a text half and a continuous-tone gradient half —
    /// `block_classify` must clear the mask in the gradient region and keep
    /// the text mask bit-identical.
    #[test]
    fn block_classify_clears_photo_keeps_text() {
        let mut pm = Pixmap::white(256, 256);
        // Text-ish bars in the top half.
        for y in (16..112).step_by(16) {
            for yy in y..y + 5 {
                for x in 16..240 {
                    pm.set_rgb(x, yy, 0, 0, 0);
                }
            }
        }
        // Continuous-tone diagonal gradient in the bottom half (mid-tones,
        // no sharp edges).
        for y in 128..256 {
            for x in 0..256 {
                let v = (60 + (x + y) / 4) as u8;
                pm.set_rgb(x, y, v, v, v);
            }
        }

        let plain = segment_page(&pm, &SegmentOptions::default()).mask;
        let classified = segment_page(
            &pm,
            &SegmentOptions {
                block_classify: true,
                ..SegmentOptions::default()
            },
        )
        .mask;

        let ink = |m: &Bitmap, y0: u32, y1: u32| -> u64 {
            (y0..y1)
                .map(|y| (0..256).filter(|&x| m.get(x, y)).count() as u64)
                .sum()
        };
        // Text half identical.
        for y in 0..112 {
            for x in 0..256 {
                assert_eq!(
                    plain.get(x, y),
                    classified.get(x, y),
                    "text mask must be untouched at ({x},{y})"
                );
            }
        }
        // Photo half: plain claims ink (gradient dips below the threshold),
        // classified clears it.
        assert!(ink(&plain, 128, 256) > 0, "gradient must binarize to ink");
        assert_eq!(ink(&classified, 128, 256), 0, "photo half must be cleared");
    }

    fn fill(pm: &mut Pixmap, r: u8, g: u8, b: u8) {
        for y in 0..pm.height {
            for x in 0..pm.width {
                pm.set_rgb(x, y, r, g, b);
            }
        }
    }

    #[test]
    fn all_white_page_yields_empty_mask() {
        let pm = Pixmap::white(24, 24);
        let seg = segment_page(&pm, &SegmentOptions::default());
        assert_eq!(seg.mask.width, 24);
        assert_eq!(seg.mask.height, 24);
        for y in 0..24 {
            for x in 0..24 {
                assert!(
                    !seg.mask.get(x, y),
                    "white pixel at ({x},{y}) should not be mask"
                );
            }
        }
        assert_eq!(seg.bg.width, 2);
        assert_eq!(seg.bg.height, 2);
        for chunk in seg.bg.data.chunks_exact(4) {
            assert_eq!(&chunk[..3], &[255, 255, 255]);
        }
    }

    #[test]
    fn all_black_page_yields_full_mask_and_black_bg_fallback() {
        let mut pm = Pixmap::white(12, 12);
        fill(&mut pm, 0, 0, 0);
        let seg = segment_page(&pm, &SegmentOptions::default());
        for y in 0..12 {
            for x in 0..12 {
                assert!(seg.mask.get(x, y));
            }
        }
        // Block fully masked → default remains the historical full-block mean.
        assert_eq!(seg.bg.width, 1);
        assert_eq!(seg.bg.height, 1);
        assert_eq!(&seg.bg.data[..3], &[0, 0, 0]);
    }

    #[test]
    fn threshold_boundary_is_strict() {
        let mut pm = Pixmap::white(4, 1);
        // Set lums: 0, 127, 128, 255 (gray triples)
        pm.set_rgb(0, 0, 0, 0, 0);
        pm.set_rgb(1, 0, 127, 127, 127);
        pm.set_rgb(2, 0, 128, 128, 128);
        pm.set_rgb(3, 0, 255, 255, 255);
        let seg = segment_page(
            &pm,
            &SegmentOptions {
                threshold: 128,
                bg_subsample: 1,
                adaptive_bg_subsample: false,
                ..SegmentOptions::default()
            },
        );
        assert!(seg.mask.get(0, 0));
        assert!(seg.mask.get(1, 0));
        assert!(!seg.mask.get(2, 0));
        assert!(!seg.mask.get(3, 0));
    }

    #[test]
    fn bg_excludes_mask_pixels() {
        // 4x4 block, sub=4: 1 ink pixel (value 0) in a sea of pale yellow
        // (BT.601 lum ≈ 222, above default threshold). Unmasked mean must
        // equal the BG colour exactly, not be pulled toward 0.
        let mut pm = Pixmap::white(4, 4);
        fill(&mut pm, 240, 230, 100);
        pm.set_rgb(1, 1, 0, 0, 0);
        let seg = segment_page(
            &pm,
            &SegmentOptions {
                threshold: 128,
                bg_subsample: 4,
                ..SegmentOptions::default()
            },
        );
        assert!(seg.mask.get(1, 1));
        assert!(!seg.mask.get(0, 0));
        assert_eq!(seg.bg.width, 1);
        assert_eq!(seg.bg.height, 1);
        let (r, g, b) = (seg.bg.data[0], seg.bg.data[1], seg.bg.data[2]);
        assert_eq!(
            (r, g, b),
            (240, 230, 100),
            "ink pixel should not contaminate BG mean"
        );
    }

    #[test]
    fn sauvola_handles_dark_background_and_light_ink() {
        // Synthetic mixed scan strip: left half is dark paper, right half is
        // bright paper. A fixed 128 threshold masks the dark paper and misses
        // the light-gray ink; Sauvola keys off local contrast instead.
        let mut pm = Pixmap::white(16, 8);
        for y in 0..8 {
            for x in 0..16 {
                let v = if x < 8 { 80 } else { 220 };
                pm.set_rgb(x, y, v, v, v);
            }
        }
        pm.set_rgb(3, 3, 40, 40, 40);
        pm.set_rgb(11, 3, 140, 140, 140);

        let fixed = segment_page(&pm, &SegmentOptions::default());
        let adaptive = segment_page(
            &pm,
            &SegmentOptions {
                binarization: Binarization::Sauvola { window: 7, k: 0.34 },
                ..SegmentOptions::default()
            },
        );

        let fixed_count = count_mask(&fixed.mask);
        let adaptive_count = count_mask(&adaptive.mask);
        assert!(fixed_count > 50, "fixed threshold masks the dark paper");
        assert!(
            adaptive_count < fixed_count / 2,
            "adaptive mask should be much sparser than fixed ({adaptive_count} vs {fixed_count})"
        );
        assert!(adaptive.mask.get(3, 3), "dark ink on dark paper");
        assert!(adaptive.mask.get(11, 3), "light ink on light paper");
        assert!(!adaptive.mask.get(1, 1), "dark paper is background");
        assert!(!adaptive.mask.get(9, 1), "bright paper is background");
    }

    #[cfg(feature = "parallel")]
    #[test]
    fn parallel_sauvola_mask_is_byte_identical_to_sequential() {
        // Non-byte-aligned width also checks that row padding remains untouched.
        let (w, h) = (131, 97);
        let luma: Vec<u8> = (0..w * h)
            .map(|i| ((i * 73 + (i / w) * 29) & 0xff) as u8)
            .collect();
        let mut parallel = Bitmap::new(w, h);
        fill_sauvola_mask(&mut parallel, &luma, w, h, 31, 0.34);

        let window = 31_u32;
        let radius = window / 2;
        let (sum, sum_sq) = integral_luma(&luma, w, h);
        let mut sequential = Bitmap::new(w, h);
        fill_sauvola_mask_sequential(
            &mut sequential,
            &luma,
            &sum,
            &sum_sq,
            w as usize + 1,
            w,
            h,
            radius,
            0.34,
        );

        assert_eq!(parallel.data, sequential.data);
    }

    /// #569: the adaptive chooser keeps flat backgrounds at the ceiling,
    /// drops detailed ones, and ignores under-mask pixels.
    #[test]
    fn adaptive_bg_subsample_tracks_background_detail() {
        // Flat white page -> ceiling.
        let flat = Pixmap::white(120, 120);
        let empty = Bitmap::new(120, 120);
        assert_eq!(choose_bg_subsample(&flat, &empty, 12), 12);

        // Noisy background -> 3.
        let mut noisy = Pixmap::white(120, 120);
        for y in 0..120 {
            for x in 0..120 {
                let v = ((x * 37 + y * 91) % 256) as u8;
                noisy.set_rgb(x, y, v, v, v);
            }
        }
        assert_eq!(choose_bg_subsample(&noisy, &empty, 12), 3);

        // The same noise fully under the mask does not count as background
        // detail -> ceiling.
        let mut inked = Bitmap::new(120, 120);
        for y in 0..120 {
            for x in 0..120 {
                inked.set_black(x, y);
            }
        }
        assert_eq!(choose_bg_subsample(&noisy, &inked, 12), 12);
    }

    fn count_mask(mask: &Bitmap) -> u32 {
        let mut n = 0;
        for y in 0..mask.height {
            for x in 0..mask.width {
                n += u32::from(mask.get(x, y));
            }
        }
        n
    }

    #[test]
    fn inpaint_fully_masked_bg_block_from_neighbors() {
        let mut pm = Pixmap::white(8, 4);
        for y in 0..4 {
            for x in 0..4 {
                pm.set_rgb(x, y, 0, 0, 0);
            }
            for x in 4..8 {
                pm.set_rgb(x, y, 210, 200, 160);
            }
        }

        let opts = SegmentOptions {
            threshold: 128,
            bg_subsample: 4,
            bg_inpaint: true,
            ..SegmentOptions::default()
        };
        let seg = segment_page(&pm, &opts);
        assert_eq!(seg.bg.width, 2);
        assert_eq!(seg.bg.height, 1);
        assert_eq!(seg.bg.get_rgb(0, 0), (210, 200, 160));
        assert_eq!(seg.bg.get_rgb(1, 0), (210, 200, 160));
    }

    #[test]
    fn empty_input_returns_empty_outputs() {
        let pm = Pixmap::default();
        let seg = segment_page(&pm, &SegmentOptions::default());
        assert_eq!(seg.mask.width, 0);
        assert_eq!(seg.mask.height, 0);
        assert_eq!(seg.bg.width, 0);
        assert_eq!(seg.bg.height, 0);
    }

    #[test]
    fn bg_dims_round_up() {
        let pm = Pixmap::white(13, 7);
        let seg = segment_page(
            &pm,
            &SegmentOptions {
                threshold: 128,
                bg_subsample: 12,
                ..SegmentOptions::default()
            },
        );
        assert_eq!(seg.bg.width, 2);
        assert_eq!(seg.bg.height, 1);
    }

    #[test]
    fn inpaint_all_masked_single_block_falls_back_to_white() {
        // 1×1 image entirely masked: inpaint_block_mean exhausts its radius loop
        // (only pixel is the center, never on the border ring) and returns None.
        let mut pm = Pixmap::white(1, 1);
        pm.set_rgb(0, 0, 0, 0, 0);
        let opts = SegmentOptions {
            threshold: 128,
            bg_subsample: 1,
            bg_inpaint: true,
            ..SegmentOptions::default()
        };
        let seg = segment_page(&pm, &opts);
        // block_mean with mask_excluded=false falls back to the actual pixel
        assert_eq!(seg.bg.get_rgb(0, 0), (0, 0, 0));
    }

    #[test]
    fn bg_subsample_zero_is_clamped_to_one() {
        let pm = Pixmap::white(3, 3);
        let seg = segment_page(
            &pm,
            &SegmentOptions {
                threshold: 128,
                bg_subsample: 0,
                ..SegmentOptions::default()
            },
        );
        assert_eq!(seg.bg.width, 3);
        assert_eq!(seg.bg.height, 3);
    }

    #[test]
    fn bg_diffuse_smooths_masked_cells_and_keeps_confident_cells() {
        // 4×1 blocks (bg_subsample = 1 → one cell per pixel). Left and right
        // pixels are red background (confident); the two middle pixels are ink
        // (fully masked). With diffusion the masked cells must interpolate
        // smoothly between the red neighbours instead of falling back to the
        // ink colour, and the confident cells must be left exactly as-is.
        // Boundary colour must be light enough to stay *unmasked* (luminance ≥
        // the 128 threshold): (230,150,150) has luma ≈ 174.
        let mut pm = Pixmap::white(4, 1);
        pm.set_rgb(0, 0, 230, 150, 150);
        pm.set_rgb(1, 0, 0, 0, 0); // ink → masked
        pm.set_rgb(2, 0, 0, 0, 0); // ink → masked
        pm.set_rgb(3, 0, 230, 150, 150);
        let opts = SegmentOptions {
            threshold: 128,
            bg_subsample: 1,
            bg_diffuse: true,
            ..SegmentOptions::default()
        };
        let seg = segment_page(&pm, &opts);
        assert_eq!(seg.bg.width, 4);
        // Confident endpoints untouched.
        assert_eq!(seg.bg.get_rgb(0, 0), (230, 150, 150));
        assert_eq!(seg.bg.get_rgb(3, 0), (230, 150, 150));
        // Masked interior converges to the (uniform) boundary value, never the
        // black ink fallback the default path would have produced.
        for x in 1..=2 {
            let (r, g, b) = seg.bg.get_rgb(x, 0);
            assert_eq!(
                (r, g, b),
                (230, 150, 150),
                "masked cell {x} should diffuse to the boundary, got {:?}",
                (r, g, b)
            );
        }

        // Without diffusion the same masked interior falls back to the ink
        // colour (black) — confirms the modes differ and diffusion is the win.
        let plain = SegmentOptions {
            threshold: 128,
            bg_subsample: 1,
            ..SegmentOptions::default()
        };
        let seg_plain = segment_page(&pm, &plain);
        assert_eq!(seg_plain.bg.get_rgb(1, 0), (0, 0, 0));
    }
}
