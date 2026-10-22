//! Gamma, fixed-point plane mapping, samplers, SIMD helpers and the anti-aliasing downscale.

use super::*;

// ── Gamma LUT ─────────────────────────────────────────────────────────────────

/// Standard sRGB / CRT display gamma assumed for rendering output.
pub(super) const DISPLAY_GAMMA: f32 = 2.2;

/// Precompute a gamma-correction look-up table for values 0..255.
///
/// Matches DjVuLibre's correction formula: the exponent is
/// `document_gamma / DISPLAY_GAMMA` so that documents created on a
/// standard gamma-2.2 device need no correction (identity LUT), while
/// documents from linear-light (gamma=1.0) sources are brightened to
/// compensate for the display gamma.
///
/// `lut[i] = round(255 * (i/255)^(gamma / DISPLAY_GAMMA))`
///
/// When `gamma <= 0.0`, not finite, or approximately equal to
/// `DISPLAY_GAMMA`, the LUT is the identity function (no correction).
pub(super) fn build_gamma_lut(gamma: f32) -> [u8; 256] {
    let mut lut = [0u8; 256];
    let exponent = if gamma <= 0.0 || !gamma.is_finite() {
        1.0_f32 // invalid — no correction
    } else {
        gamma / DISPLAY_GAMMA
    };
    if (exponent - 1.0).abs() < 1e-4 {
        // Identity
        for (i, v) in lut.iter_mut().enumerate() {
            *v = i as u8;
        }
        return lut;
    }
    for (i, v) in lut.iter_mut().enumerate() {
        let linear = i as f32 / 255.0;
        let corrected = linear.powf(exponent);
        *v = (corrected * 255.0 + 0.5) as u8;
    }
    lut
}

// ── Bilinear scaling (FRACBITS = 4) ──────────────────────────────────────────

/// Fixed-point fractional bits for bilinear scaling (1 << 4 = 16 subpixels).
pub(super) const FRACBITS: u32 = 4;
pub(super) const FRAC: u32 = 1 << FRACBITS;
pub(super) const FRAC_MASK: u32 = FRAC - 1;

/// Maps each byte value to 8 fg-mask bytes (MSB-first): 0xFF if bit set (fg), 0x00 otherwise.
pub(super) const MASK_EXPAND: [[u8; 8]; 256] = {
    let mut lut = [[0u8; 8]; 256];
    let mut b = 0usize;
    while b < 256 {
        let mut bit = 0usize;
        while bit < 8 {
            lut[b][bit] = if (b >> (7 - bit)) & 1 != 0 {
                0xFF
            } else {
                0x00
            };
            bit += 1;
        }
        b += 1;
    }
    lut
};

/// Maps each mask byte to 8 RGBA pixels for bilevel rendering (MSB-first, 300 DPI 1:1).
/// fg bit (1) → [0x00, 0x00, 0x00, 0xFF] (black); bg bit (0) → [0xFF, 0xFF, 0xFF, 0xFF] (white).
/// Table: 256 × 32 = 8 KiB (128 cache lines); persists in L2 across rows.
pub(super) const BILEVEL_RGBA: [[u8; 32]; 256] = {
    let mut lut = [[0u8; 32]; 256];
    let mut mb = 0usize;
    while mb < 256 {
        let mut bit = 0usize;
        while bit < 8 {
            let ch = if (mb >> (7 - bit)) & 1 != 0 {
                0u8
            } else {
                255u8
            };
            lut[mb][bit * 4] = ch;
            lut[mb][bit * 4 + 1] = ch;
            lut[mb][bit * 4 + 2] = ch;
            lut[mb][bit * 4 + 3] = 255;
            bit += 1;
        }
        mb += 1;
    }
    lut
};

// ── SIMD helpers ──────────────────────────────────────────────────────────────

/// Convert packed RGB bytes to packed RGBA with alpha = 255.
///
/// On x86_64 with SSSE3 (available on Core 2+, ~2006): processes 4 pixels per
/// `_mm_shuffle_epi8` + `_mm_or_si128`.  Falls back to scalar on older targets.
///
/// `src` must hold exactly `pixel_count * 3` bytes;
/// `dst` must hold exactly `pixel_count * 4` bytes.
#[cfg(feature = "std")]
#[allow(unsafe_code)]
#[inline]
pub(super) fn rgb_to_rgba(src: &[u8], dst: &mut [u8]) {
    let pixel_count = src.len() / 3;
    debug_assert_eq!(dst.len(), pixel_count * 4);

    #[cfg(target_arch = "x86_64")]
    if is_x86_feature_detected!("ssse3") {
        // SAFETY: feature detected; bounds are enforced by safe_chunks calculation.
        unsafe {
            // Only load 16 bytes where src has at least 16 bytes available:
            // chunk i reads src[i*12..i*12+16], so we need i*12+16 <= src.len().
            let safe_chunks = if src.len() >= 16 {
                ((src.len() - 16) / 12 + 1).min(pixel_count / 4)
            } else {
                0
            };
            rgb_to_rgba_ssse3(src, dst, pixel_count, safe_chunks);
        }
        return;
    }

    rgb_to_rgba_scalar(src, dst, 0, pixel_count);
}

#[cfg(all(feature = "std", target_arch = "x86_64"))]
#[allow(unsafe_code, unsafe_op_in_unsafe_fn)]
#[target_feature(enable = "ssse3")]
// SAFETY: caller guarantees SSSE3 availability; safe_chunks * 12 + 16 <= src.len()
// and safe_chunks * 16 <= dst.len() (enforced by rgb_to_rgba).
pub(super) unsafe fn rgb_to_rgba_ssse3(
    src: &[u8],
    dst: &mut [u8],
    pixel_count: usize,
    safe_chunks: usize,
) {
    use core::arch::x86_64::*;

    // Shuffle 12 packed RGB bytes into 16 RGBA bytes (4 pixels), zero in alpha slot.
    // _mm_set_epi8 arguments are byte 15 (highest) down to byte 0 (lowest).
    let shuf = _mm_set_epi8(
        -1, 11, 10, 9, // pixel 3: [R,G,B,0]
        -1, 8, 7, 6, // pixel 2
        -1, 5, 4, 3, // pixel 1
        -1, 2, 1, 0, // pixel 0
    );
    let alpha_or = _mm_set1_epi32(0xFF000000u32 as i32);

    for i in 0..safe_chunks {
        let v = _mm_loadu_si128(src.as_ptr().add(i * 12) as *const __m128i);
        _mm_storeu_si128(
            dst.as_mut_ptr().add(i * 16) as *mut __m128i,
            _mm_or_si128(_mm_shuffle_epi8(v, shuf), alpha_or),
        );
    }

    rgb_to_rgba_scalar(src, dst, safe_chunks * 4, pixel_count);
}

#[cfg(feature = "std")]
#[inline]
pub(super) fn rgb_to_rgba_scalar(src: &[u8], dst: &mut [u8], start: usize, end: usize) {
    for i in start..end {
        dst[i * 4] = src[i * 3];
        dst[i * 4 + 1] = src[i * 3 + 1];
        dst[i * 4 + 2] = src[i * 3 + 2];
        dst[i * 4 + 3] = 255;
    }
}

/// Generic Q24 ratios `(plane_w << 24) / page_w` and `(plane_h << 24) / page_h`
/// for converting page-space FRACBITS coords into plane-space FRACBITS coords.
/// Returns `(0, 0)` when `plane` is `None` or page dims are zero.  The public
/// FG/BG helpers below apply layer-specific cell-grid adjustments first.
#[inline]
pub(super) fn plane_q24(plane: Option<&Pixmap>, page_w: u32, page_h: u32) -> (u64, u64) {
    match plane {
        Some(p) if page_w > 0 && page_h > 0 => (
            ((p.width as u64) << 24) / page_w as u64,
            ((p.height as u64) << 24) / page_h as u64,
        ),
        _ => (0, 0),
    }
}

/// Map page-space fixed-point coordinates to BG44/FG44 plane-space by aligning
/// pixel centres instead of top-left corners.  This matches the usual image
/// resampling convention and reduces native-resolution drift against ddjvu for
/// non-integer page→plane ratios such as colorbook's 2260→754 BG scale.
///
/// A render at page size does not use it: there the BG follows
/// [`scaler_coord`] and the FG44 follows [`fg_native_frac`] (#831).
#[inline]
pub(super) fn map_plane_center_frac(page_frac: u32, q24: u64) -> u32 {
    let centered = (((page_frac as u64 + (FRAC / 2) as u64) * q24) >> 24) as u32;
    centered.saturating_sub(FRAC / 2)
}

#[inline]
pub(super) fn fg_q24(fg: Option<&Pixmap>, page_w: u32, page_h: u32) -> (u64, u64) {
    match fg {
        Some(p) if page_w > 0 && page_h > 0 && p.width > 0 && p.height > 0 => {
            // FG44 is a sparse foreground colour map.  Horizontally, the last
            // encoded column is often padding for a fixed-width colour cell
            // grid (e.g. 2260px page / 189px FG => 12px cells), so use the
            // inferred integer cell pitch instead of stretching across the
            // padded column.  Vertically, use the encoded plane ratio so the
            // bottom FG row remains reachable when the page height is not an
            // exact multiple of the cell pitch.
            let sx = page_w.div_ceil(p.width).max(1);
            (
                (1u64 << 24) / sx as u64,
                ((p.height as u64) << 24) / page_h as u64,
            )
        }
        _ => plane_q24(fg, page_w, page_h),
    }
}

/// `bg` is the background plane's `(width, height)` — the whole plane's, even
/// when the compositor holds only a band of its rows (#811).
#[inline]
pub(super) fn bg_q24(bg: Option<(u32, u32)>, page_w: u32, page_h: u32) -> (u64, u64) {
    match bg {
        Some((w, h)) if page_w > 0 && page_h > 0 && w > 0 && h > 0 => {
            // BG44 planes are cell grids too (usually page/3 for scans).  Use
            // the inferred integer subsample pitch so the padded right/bottom
            // edge cells do not stretch across the page during native render.
            let sx = page_w.div_ceil(w).max(1);
            let sy = page_h.div_ceil(h).max(1);
            ((1u64 << 24) / sx as u64, (1u64 << 24) / sy as u64)
        }
        _ => (0, 0),
    }
}

/// DjVuLibre's `compute_red`: the reduction `red` with
/// `ceil(page / red) == plane` on both axes, if one exists in `1..=12`
/// (DjVuLibre draws no background past 12).
pub(super) fn compute_red(page: (u32, u32), plane: (u32, u32)) -> Option<u32> {
    (1..=12u32).find(|&red| page.0.div_ceil(red) == plane.0 && page.1.div_ceil(red) == plane.1)
}

/// The background reduction when a page is rendered at its own size and the
/// background plane is smaller: DjVuLibre then enlarges the plane with
/// `GPixmapScaler` (#831). `0` for every other case, which keeps the
/// centre-aligned bilinear mapping of [`bg_q24`].
pub(super) fn native_bg_red(page: (u32, u32), full: (u32, u32), plane: (u32, u32)) -> u32 {
    if page != full {
        return 0;
    }
    match compute_red(page, plane) {
        Some(red) if red >= 2 => red,
        _ => 0,
    }
}

/// [`CompositeContext::fg_red`]: the FG44 reduction when the render is at
/// page size, else `0`.
pub(super) fn native_fg_red(page: (u32, u32), full: (u32, u32), fg: &Pixmap) -> u32 {
    if page != full {
        return 0;
    }
    compute_red(page, (fg.width, fg.height)).unwrap_or(0)
}

/// FG44 plane coordinates, in 1/16 pixels, of page pixel `(x, y)` on a
/// render at page size: the whole cell `(x / red, y / red)` with `y`
/// counted from the bottom, as DjVuLibre's `GPixmap::stencil` reads it.
/// Whole-pixel coordinates make the bilinear samplers return that cell.
#[inline]
pub(super) fn fg_native_frac(x: u32, y: u32, page_h: u32, red: u32, fg: &Pixmap) -> (u32, u32) {
    let from_bottom = page_h.saturating_sub(1).saturating_sub(y) / red;
    let row = fg.height.saturating_sub(1).saturating_sub(from_bottom);
    ((x / red) << FRACBITS, row << FRACBITS)
}

/// DjVuLibre's `GScaler::prepare_coord` for an enlargement by `red`: the
/// source position of output pixel `k` in 1/16 pixels. Both axes count from
/// the plane's origin — the left column, and the **bottom** row, since DjVu
/// coordinates grow upwards. The result can be slightly negative at the
/// first pixel and is clamped to the plane's last pixel at the far edge.
#[inline]
pub(super) fn scaler_coord(k: u32, red: u32, plane_len: u32) -> i32 {
    let beg = ((FRAC + red) / (2 * red)) as i32 - (FRAC / 2) as i32;
    let c = beg + ((red / 2 + k * FRAC) / red) as i32;
    c.min((plane_len.saturating_sub(1) * FRAC) as i32)
}

/// The column entry of [`scaler_coord`] for page column `x`.
#[inline]
pub(super) fn scaler_x(x: u32, red: u32, plane_w: u32) -> BilinearX {
    let c = scaler_coord(x, red, plane_w);
    let clamp = |v: i32| v.clamp(0, plane_w.saturating_sub(1) as i32) as u32;
    BilinearX {
        x0: clamp(c >> FRACBITS),
        x1: clamp((c >> FRACBITS) + 1),
        tx: (c & FRAC_MASK as i32) as u32,
    }
}

/// The two plane rows, as top-origin indices, that page row `y` blends,
/// and the weight of the second: `(lower, upper, f)`. `lower` is the row
/// nearer the bottom, so `upper <= lower`.
#[inline]
pub(super) fn scaler_rows(y: u32, page_h: u32, red: u32, plane_h: u32) -> (u32, u32, u32) {
    let from_bottom = page_h.saturating_sub(1).saturating_sub(y);
    let c = scaler_coord(from_bottom, red, plane_h);
    let last = plane_h.saturating_sub(1) as i32;
    let lower = (c >> FRACBITS).clamp(0, last) as u32;
    let upper = ((c >> FRACBITS) + 1).clamp(0, last) as u32;
    (
        last as u32 - lower,
        last as u32 - upper,
        (c & FRAC_MASK as i32) as u32,
    )
}

/// DjVuLibre's interpolation step, `lo + ((up - lo) * f + 8) >> 4`, with
/// an arithmetic shift. `GPixmapScaler` applies it vertically, rounds to
/// 8 bits, then applies it horizontally.
#[inline]
pub(super) fn scaler_lerp(lo: u32, up: u32, f: u32) -> u32 {
    // `lo + floor(x / 16)` equals `floor((16 * lo + x) / 16)`, and
    // `16 * lo + (up - lo) * f + 8` is never negative: the unsigned form
    // below is exact and has the shape of the bilinear blend.
    (lo * (FRAC - f) + up * f + FRAC / 2) >> FRACBITS
}

/// Sample a pixmap at fractional coordinates using bilinear interpolation.
///
/// Coordinates are in fixed-point: `fx = x * FRAC`, etc.
/// Returns (r, g, b).
#[inline]
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn sample_bilinear(pm: &Pixmap, fx: u32, fy: u32) -> (u8, u8, u8) {
    let x0 = (fx >> FRACBITS).min(pm.width.saturating_sub(1));
    let y0 = (fy >> FRACBITS).min(pm.height.saturating_sub(1));
    let x1 = (x0 + 1).min(pm.width.saturating_sub(1));
    let y1 = (y0 + 1).min(pm.height.saturating_sub(1));

    let tx = fx & FRAC_MASK; // 0..15
    let ty = fy & FRAC_MASK;

    let (r00, g00, b00) = pm.get_rgb(x0, y0);
    let (r10, g10, b10) = pm.get_rgb(x1, y0);
    let (r01, g01, b01) = pm.get_rgb(x0, y1);
    let (r11, g11, b11) = pm.get_rgb(x1, y1);

    let lerp = |a: u8, b: u8, c: u8, d: u8| -> u8 {
        let top = a as u32 * (FRAC - tx) + b as u32 * tx;
        let bot = c as u32 * (FRAC - tx) + d as u32 * tx;
        let numerator = top * (FRAC - ty) + bot * ty;
        // v ≤ (255*FRAC*FRAC + round) >> (2*FRACBITS) = 255 — no clamp needed.
        ((numerator + (1 << (2 * FRACBITS - 1))) >> (2 * FRACBITS)) as u8
    };

    (
        lerp(r00, r10, r01, r11),
        lerp(g00, g10, g01, g11),
        lerp(b00, b10, b01, b11),
    )
}

/// Bilinear sample using pre-fetched row slices (avoids repeated y-coord computation).
/// `ty` is the vertical fractional weight (0..FRAC-1). Row slices are RGBA (4 bytes/pixel).
#[inline]
pub(super) fn bilinear_from_rows(
    row0: &[u8],
    row1: &[u8],
    width: u32,
    fx: u32,
    ty: u32,
) -> (u8, u8, u8) {
    let w = width.saturating_sub(1) as usize;
    let x0 = (fx >> FRACBITS) as usize;
    let x0 = x0.min(w);
    let x1 = (x0 + 1).min(w);
    let tx = fx & FRAC_MASK;

    // Read 4 bytes (RGBA) per pixel — a single 32-bit load on all targets.
    let get = |row: &[u8], x: usize| -> (u8, u8, u8) {
        let off = x * 4;
        if let Some(q) = row.get(off..off + 4) {
            (q[0], q[1], q[2])
        } else {
            (0, 0, 0)
        }
    };
    let (r00, g00, b00) = get(row0, x0);
    let (r10, g10, b10) = get(row0, x1);
    let (r01, g01, b01) = get(row1, x0);
    let (r11, g11, b11) = get(row1, x1);

    // Precompute bilinear weights so ty/ity are absorbed into w01/w11 and never
    // need to be reloaded from the stack during per-channel accumulation.
    // Weights sum to FRAC*FRAC = 256, so result = dot/256 (>> 8).
    let itx = FRAC - tx;
    let ity = FRAC - ty;
    let w00 = itx * ity;
    let w10 = tx * ity;
    let w01 = itx * ty;
    let w11 = tx * ty;
    // max dot = 255 * 256 = 65280 + 128 ≤ u32 range; no clamp needed.
    let blend = |a: u8, b: u8, c: u8, d: u8| -> u8 {
        ((a as u32 * w00 + b as u32 * w10 + c as u32 * w01 + d as u32 * w11 + 128) >> 8) as u8
    };
    (
        blend(r00, r10, r01, r11),
        blend(g00, g10, g01, g11),
        blend(b00, b10, b01, b11),
    )
}

#[inline]
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn sample_nearest(pm: &Pixmap, fx: u32, fy: u32) -> (u8, u8, u8) {
    let x = ((fx + FRAC / 2) >> FRACBITS).min(pm.width.saturating_sub(1));
    let y = ((fy + FRAC / 2) >> FRACBITS).min(pm.height.saturating_sub(1));
    pm.get_rgb(x, y)
}

/// Area-average (box filter) sample: average all source pixels covered by the
/// output pixel's footprint.  Used when downscaling (scale < 1.0) for better
/// anti-aliasing and fewer moire patterns than bilinear.
///
/// `fx`, `fy` are the top-left corner of the output pixel in fixed-point.
/// `fx_step`, `fy_step` are the output pixel size in source coordinates.
#[inline]
pub(super) fn sample_area_avg(
    pm: &Pixmap,
    fx: u32,
    fy: u32,
    fx_step: u32,
    fy_step: u32,
) -> (u8, u8, u8) {
    let (x0, x1) = area_range(pm.width, fx, fx_step);
    let (y0, y1) = area_range(pm.height, fy, fy_step);
    sample_area_avg_bounds(PlaneView::whole(pm), x0, x1, y0, y1)
}

#[inline]
pub(super) fn sample_area_avg_bounds(
    pm: PlaneView<'_>,
    x0: u32,
    x1: u32,
    y0: u32,
    y1: u32,
) -> (u8, u8, u8) {
    let cols = (x1 - x0) as usize;
    let rows = (y1 - y0) as usize;

    // Fast path: 1x1 box -> direct read.
    if cols <= 1 && rows <= 1 {
        let off = x0 as usize * 4;
        return pm
            .row(y0)
            .get(off..off + 4)
            .map_or((0, 0, 0), |q| (q[0], q[1], q[2]));
    }

    let mut r_sum = 0u32;
    let mut g_sum = 0u32;
    let mut b_sum = 0u32;

    // One bounds check per row (not per pixel) to let the inner loop vectorize.
    for sy in y0..y1 {
        let x_off = x0 as usize * 4;
        if let Some(row) = pm.row(sy).get(x_off..x_off + cols * 4) {
            for chunk in row.as_chunks::<4>().0 {
                r_sum += chunk[0] as u32;
                g_sum += chunk[1] as u32;
                b_sum += chunk[2] as u32;
            }
        }
    }

    let count = (rows * cols) as u32;
    if count == 0 {
        return (255, 255, 255);
    }

    // Power-of-2 counts (count=4 at 2x downscale): replace UDIV with shifts.
    let half = count >> 1;
    if count.is_power_of_two() {
        let shift = count.trailing_zeros();
        (
            ((r_sum + half) >> shift) as u8,
            ((g_sum + half) >> shift) as u8,
            ((b_sum + half) >> shift) as u8,
        )
    } else {
        (
            ((r_sum + half) / count) as u8,
            ((g_sum + half) / count) as u8,
            ((b_sum + half) / count) as u8,
        )
    }
}

/// Coverage-weighted bilevel downscale: returns the fraction of set mask bits in
/// the output pixel's footprint as a gray value (0 = all background, 255 = all foreground).
///
/// Used by `composite_rows_bilevel_one` for anti-aliased text at downscale DPIs.
#[inline]
pub(super) fn mask_box_coverage(
    mask: &crate::bitmap::Bitmap,
    fx: u32,
    fy: u32,
    fx_step: u32,
    fy_step: u32,
) -> u8 {
    let x0 = (fx >> FRACBITS).min(mask.width.saturating_sub(1));
    let y0 = (fy >> FRACBITS).min(mask.height.saturating_sub(1));
    let x1 = ((fx + fx_step) >> FRACBITS).min(mask.width);
    let y1 = ((fy + fy_step) >> FRACBITS).min(mask.height);
    let total = (x1 - x0) * (y1 - y0);
    if total == 0 {
        return 0;
    }
    // Count foreground bits using byte-level popcount instead of individual bit reads.
    // MSB-first packing: pixel x is at bit (7 - x%8) of byte (x/8).
    // first_mask keeps pixels [x0, next-byte-boundary); end_mask keeps pixels before x1.
    let stride = mask.row_stride();
    let byte_lo = x0 as usize / 8;
    let byte_hi = (x1 as usize).div_ceil(8); // exclusive
    let first_mask = 0xFF_u8 >> (x0 % 8);
    let end_mask = if x1.is_multiple_of(8) {
        0xFF_u8
    } else {
        0xFF_u8 << (8 - x1 % 8)
    };
    let mut count = 0u32;
    if byte_hi == byte_lo + 1 {
        // Entire x-range fits in one byte.
        let combined = first_mask & end_mask;
        for sy in y0..y1 {
            count += (mask.data[sy as usize * stride + byte_lo] & combined).count_ones();
        }
    } else {
        for sy in y0..y1 {
            let row = &mask.data[sy as usize * stride..];
            count += (row[byte_lo] & first_mask).count_ones();
            for byte in row[(byte_lo + 1)..(byte_hi - 1)].iter() {
                count += byte.count_ones();
            }
            count += (row[byte_hi - 1] & end_mask).count_ones();
        }
    }
    // Widen to u64: on a large mask with aggressive downsampling a single box can
    // cover > 16.8 M foreground bits, where `count * 255` overflows u32 (wrong
    // value in release, panic in debug).
    ((count as u64 * 255 + total as u64 / 2) / total as u64) as u8
}

/// Bilinearly interpolate the JB2 mask's 0/255 bits as a continuous coverage
/// field, for anti-aliased glyph edges at **upscale** (zoom > 1).
///
/// Treats each set mask bit as full foreground coverage (255) and each clear
/// bit as full background coverage (0), then blends the four nearest mask
/// pixels the same way [`sample_bilinear`] blends a [`Pixmap`] — mirroring
/// `mask_box_coverage`'s box-average approach used at *downscale*, but for the
/// opposite direction.
///
/// Returns the interpolated foreground fraction, 0 (all background) ..= 255
/// (all foreground) — same convention as `mask_box_coverage`.
///
/// Opt-in via [`RenderOptions::mask_aa`]: DjVuLibre hard-edges the mask under
/// zoom, so this is a deliberate, judged divergence from the reference
/// renderer, not a faithfulness fix — the default (`mask_aa: false`) path
/// never calls this function.
#[inline]
pub(super) fn mask_bilinear_coverage(mask: &crate::bitmap::Bitmap, fx: u32, fy: u32) -> u8 {
    let x0 = (fx >> FRACBITS).min(mask.width.saturating_sub(1));
    let y0 = (fy >> FRACBITS).min(mask.height.saturating_sub(1));
    let x1 = (x0 + 1).min(mask.width.saturating_sub(1));
    let y1 = (y0 + 1).min(mask.height.saturating_sub(1));

    let tx = fx & FRAC_MASK;
    let ty = fy & FRAC_MASK;

    let bit = |x: u32, y: u32| -> u32 { if mask.get(x, y) { 255 } else { 0 } };
    let v00 = bit(x0, y0);
    let v10 = bit(x1, y0);
    let v01 = bit(x0, y1);
    let v11 = bit(x1, y1);

    let top = v00 * (FRAC - tx) + v10 * tx;
    let bot = v01 * (FRAC - tx) + v11 * tx;
    let numerator = top * (FRAC - ty) + bot * ty;
    // Same shape as sample_bilinear's lerp: v <= 255 — no clamp needed.
    ((numerator + (1 << (2 * FRACBITS - 1))) >> (2 * FRACBITS)) as u8
}

/// Find the center foreground pixel in a mask box for palette color lookup.
#[inline]
pub(super) fn mask_box_center_fg(
    mask: &crate::bitmap::Bitmap,
    fx: u32,
    fy: u32,
    fx_step: u32,
    fy_step: u32,
) -> (u32, u32) {
    // Use the center of the box
    let cx = (fx + fx_step / 2) >> FRACBITS;
    let cy = (fy + fy_step / 2) >> FRACBITS;
    (
        cx.min(mask.width.saturating_sub(1)),
        cy.min(mask.height.saturating_sub(1)),
    )
}

// ── Anti-aliasing downscale ──────────────────────────────────────────────────

/// Apply a 2×2 box-filter downscale pass for anti-aliasing.
///
/// If either dimension of `pm` is 1, the output dimension stays at 1.
pub(super) fn aa_downscale(pm: &Pixmap) -> Pixmap {
    let out_w = (pm.width / 2).max(1);
    let out_h = (pm.height / 2).max(1);
    let mut out = Pixmap::white(out_w, out_h);
    for y in 0..out_h {
        for x in 0..out_w {
            let sx = (x * 2).min(pm.width.saturating_sub(1));
            let sy = (y * 2).min(pm.height.saturating_sub(1));
            let sx1 = (sx + 1).min(pm.width.saturating_sub(1));
            let sy1 = (sy + 1).min(pm.height.saturating_sub(1));

            let (r00, g00, b00) = pm.get_rgb(sx, sy);
            let (r10, g10, b10) = pm.get_rgb(sx1, sy);
            let (r01, g01, b01) = pm.get_rgb(sx, sy1);
            let (r11, g11, b11) = pm.get_rgb(sx1, sy1);

            let avg = |a: u8, b: u8, c: u8, d: u8| -> u8 {
                ((a as u32 + b as u32 + c as u32 + d as u32 + 2) / 4) as u8
            };
            out.set_rgb(
                x,
                y,
                avg(r00, r10, r01, r11),
                avg(g00, g10, g01, g11),
                avg(b00, b10, b01, b11),
            );
        }
    }
    out
}

/// `window` of a page whose pixels from `origin` on are `src`, white where
/// the window leaves `src`.
pub(super) fn white_crop(src: &Pixmap, origin: (u32, u32), window: RenderRect) -> Pixmap {
    let mut pm = Pixmap::white(window.width, window.height);
    let x0 = window.x.max(origin.0);
    let y0 = window.y.max(origin.1);
    let x1 = window
        .x
        .saturating_add(window.width)
        .min(origin.0.saturating_add(src.width));
    let y1 = window
        .y
        .saturating_add(window.height)
        .min(origin.1.saturating_add(src.height));
    if x1 <= x0 || y1 <= y0 {
        return pm;
    }
    let row = (x1 - x0) as usize * 4;
    for y in y0..y1 {
        let src_at = ((y - origin.1) as usize * src.width as usize + (x0 - origin.0) as usize) * 4;
        let dst_at =
            ((y - window.y) as usize * window.width as usize + (x0 - window.x) as usize) * 4;
        pm.data[dst_at..dst_at + row].copy_from_slice(&src.data[src_at..src_at + row]);
    }
    pm
}
