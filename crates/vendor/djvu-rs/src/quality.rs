//! Perceptual image-quality metrics (PSNR, SSIM, colour) for render experiments.
//!
//! The existing pixel-diff tooling (`examples/interop_pixdiff.rs`) reports the
//! *arithmetic* per-pixel RGB difference distribution versus DjVuLibre. That
//! answers "did the output drift?" but not "is it perceptually better/worse?",
//! which is exactly the judgement the deferred quality experiments need
//! (linear-light blending, mask-upscale AA, Lanczos-vs-area-avg downscale,
//! bicubic FG upsampling, gamma-correct downscale — see PERF_EXPERIMENTS.md
//! round-5 QUALITY_AA and the round-8 QUALITY-GATED list).
//!
//! This module adds the standard perceptual metrics so a change can be
//! judged against a reference image with a single number:
//!
//! - [`psnr`] — peak signal-to-noise ratio in dB. Higher is closer; ∞ (returned
//!   as [`f64::INFINITY`]) for identical inputs. Correlates loosely with quality
//!   but is a pure error metric.
//! - [`ssim`] — structural similarity index in `[-1, 1]` (1.0 = identical).
//!   Models luminance, contrast and structure the way the human visual system
//!   roughly does, so it catches blur / ringing / blockiness that PSNR misses.
//!
//! Both operate on the **luma** of RGBA [`Pixmap`]s (alpha ignored) so colour
//! and grayscale renders compare on the same perceptual channel. A convenience
//! [`compare`] returns both plus MSE in one pass-pair.
//!
//! # Colour blindness of the luma-only metrics — [`compare_color`]
//!
//! `psnr`/`ssim`/`compare` are **structurally colour-blind**: two renders that
//! agree on luma but disagree on hue or saturation score as identical. This
//! bit the round-31 `FGBZ_MEDIANCUT` experiment (PERF_EXPERIMENTS.md) —
//! aggressive foreground-palette quantisation visibly washed out coloured
//! text, but the D1 harness reported no drop, forcing a fall-back to manual
//! crop inspection. [`compare_color`] closes that gap:
//!
//! - Converts both images to **YCbCr** (ITU-R BT.601, the same colour model
//!   DjVu itself stores IW44 planes in — see `djvu-iw44::encode::rgb_to_ycbcr`)
//!   and runs the same windowed SSIM independently on the `Cb` and `Cr`
//!   chroma planes, alongside the existing luma `Y` SSIM.
//! - Reports a **weighted combined SSIM** (`0.8·Y + 0.1·Cb + 0.1·Cr`), the
//!   luma-dominant weighting standard in colour-video quality metrics —
//!   matches the perceptual reality DjVu's own chroma subsampling and
//!   chroma-delay design already assume (`CHROMA_BILINEAR` #422,
//!   `chroma_delay` in `Iw44EncodeOptions`).
//! - Reports mean/max **ΔE76** (Euclidean distance in CIE L\*a\*b\*, the
//!   classic "perceptually uniform colour difference" metric) so a colour
//!   drift can be quoted in the same units printed shops/paint charts use.
//!
//! ## Interpreting the numbers
//!
//! *Chroma SSIM (`ssim_cb`/`ssim_cr`/`ssim_combined`)* reads the same way as
//! luma SSIM: 1.0 = identical, and values noticeably below the luma SSIM on
//! the same pair mean the difference lives mostly in colour, not structure —
//! exactly the FGBZ_MEDIANCUT scenario. A combined score within ~0.001 of the
//! luma-only [`ssim`] means the change is colour-neutral; a combined score
//! more than ~0.01 below the luma score means colour is doing the damage.
//!
//! *ΔE76* (per CIE guidance, carried over unchanged from colour-reproduction
//! practice): `< 1.0` is imperceptible to the human eye; `1–2` is only
//! detectable by a trained observer under ideal conditions; `2–10` is
//! perceptible at a glance; `> 10` is an obviously different colour. `delta_e_max`
//! catches localised colour damage (e.g. one washed-out palette entry) that
//! `delta_e_mean` can dilute away over a large page.
//!
//! # Reference-vs-ideal workflow
//!
//! The most useful mode for an *intentional* quality change is to compare the
//! render against the **pre-compression source**, not against another decoder:
//! take a known image, encode it to DjVu, render it back, and measure SSIM/PSNR
//! against the original. A change that raises SSIM-vs-source is genuinely better,
//! independent of whether it drifts from DjVuLibre. `examples/quality_harness.rs`
//! drives exactly this, now with [`compare_color`] columns alongside luma.

#[cfg(feature = "std")]
use crate::pixmap::{GrayPixmap, Pixmap};

/// Rec.601 luma of an RGB triple, matching [`Pixmap::to_gray8`]'s fixed-point
/// weights (306 + 601 + 117 = 1024).
#[inline]
fn luma(r: u8, g: u8, b: u8) -> f64 {
    (r as u32 * 306 + g as u32 * 601 + b as u32 * 117) as f64 / 1024.0
}

/// Extract the luma plane (`f64`, one sample per pixel) from an RGBA byte buffer.
#[cfg(feature = "std")]
fn luma_plane(rgba: &[u8]) -> Vec<f64> {
    rgba.chunks_exact(4)
        .map(|p| luma(p[0], p[1], p[2]))
        .collect()
}

// ---------------------------------------------------------------------------
// Colour-aware metrics (QUALITY_COLOR)
// ---------------------------------------------------------------------------

/// ITU-R BT.601 `Cb` (blue-difference) chroma of an RGB triple, centred at 128.
///
/// Standard full-range JPEG/BT.601 coefficients (not DjVu's internal IW44
/// `b - g` shortcut, which is a lossless-reversible encoding trick rather than
/// a perceptual chroma axis). Result is in `0.0..=255.0` for valid RGB input.
#[inline]
fn chroma_cb(r: u8, g: u8, b: u8) -> f64 {
    128.0 - 0.168_736 * r as f64 - 0.331_264 * g as f64 + 0.5 * b as f64
}

/// ITU-R BT.601 `Cr` (red-difference) chroma of an RGB triple, centred at 128.
#[inline]
fn chroma_cr(r: u8, g: u8, b: u8) -> f64 {
    128.0 + 0.5 * r as f64 - 0.418_688 * g as f64 - 0.081_312 * b as f64
}

/// Extract the `Cb` plane (`f64`, one sample per pixel) from an RGBA byte buffer.
#[cfg(feature = "std")]
fn cb_plane(rgba: &[u8]) -> Vec<f64> {
    rgba.chunks_exact(4)
        .map(|p| chroma_cb(p[0], p[1], p[2]))
        .collect()
}

/// Extract the `Cr` plane (`f64`, one sample per pixel) from an RGBA byte buffer.
#[cfg(feature = "std")]
fn cr_plane(rgba: &[u8]) -> Vec<f64> {
    rgba.chunks_exact(4)
        .map(|p| chroma_cr(p[0], p[1], p[2]))
        .collect()
}

/// sRGB electro-optical transfer function inverse: gamma-encoded `0..=255`
/// channel to linear-light `0.0..=1.0`. Standard sRGB piecewise curve.
#[inline]
fn srgb_to_linear(c: u8) -> f64 {
    let c = c as f64 / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// CIE 1931 D65 reference white (standard sRGB working space illuminant).
const D65_XN: f64 = 0.950_47;
const D65_YN: f64 = 1.0;
const D65_ZN: f64 = 1.088_83;

/// CIE L\*a\*b\* forward companding function (`f(t)` in the standard definition).
#[inline]
fn lab_f(t: f64) -> f64 {
    const DELTA: f64 = 6.0 / 29.0;
    if t > DELTA * DELTA * DELTA {
        t.cbrt()
    } else {
        t / (3.0 * DELTA * DELTA) + 4.0 / 29.0
    }
}

/// Convert one sRGB pixel to CIE L\*a\*b\* (D65 white point).
///
/// Standard sRGB → linear → XYZ (D65) → Lab pipeline. `L*` in `0..=100`,
/// `a*`/`b*` roughly in `-128..=127` for in-gamut sRGB colours.
fn rgb_to_lab(r: u8, g: u8, b: u8) -> (f64, f64, f64) {
    let (rl, gl, bl) = (srgb_to_linear(r), srgb_to_linear(g), srgb_to_linear(b));
    // sRGB (D65) linear RGB → XYZ matrix (IEC 61966-2-1).
    let x = rl * 0.412_456_4 + gl * 0.357_576_1 + bl * 0.180_437_5;
    let y = rl * 0.212_672_9 + gl * 0.715_152_2 + bl * 0.072_175_0;
    let z = rl * 0.019_333_9 + gl * 0.119_192_0 + bl * 0.950_304_1;
    let fx = lab_f(x / D65_XN);
    let fy = lab_f(y / D65_YN);
    let fz = lab_f(z / D65_ZN);
    let l = 116.0 * fy - 16.0;
    let a = 500.0 * (fx - fy);
    let bb = 200.0 * (fy - fz);
    (l, a, bb)
}

/// CIE76 colour difference: Euclidean distance between two Lab triples.
#[inline]
fn delta_e76(a: (f64, f64, f64), b: (f64, f64, f64)) -> f64 {
    let (dl, da, db) = (a.0 - b.0, a.1 - b.1, a.2 - b.2);
    (dl * dl + da * da + db * db).sqrt()
}

/// A colour-aware comparison of two same-size RGBA images: per-channel
/// (Y/Cb/Cr) SSIM, a luma-dominant combined score, and CIE76 ΔE colour
/// difference. See the module docs for interpretation guidance.
#[cfg(feature = "std")]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColorQualityReport {
    /// Luma-channel SSIM — identical value/definition to [`ssim`].
    pub ssim_y: f64,
    /// `Cb` (blue-difference chroma) channel SSIM.
    pub ssim_cb: f64,
    /// `Cr` (red-difference chroma) channel SSIM.
    pub ssim_cr: f64,
    /// Weighted combined SSIM: `0.8·Y + 0.1·Cb + 0.1·Cr`. The luma-dominant
    /// weighting standard for colour-video quality metrics (chroma is only
    /// ~1/8 as perceptually salient as luminance to the human visual system;
    /// the same asymmetry DjVu's own chroma subsampling/delay design assumes).
    pub ssim_combined: f64,
    /// Mean CIE76 ΔE (Euclidean CIE L\*a\*b\* distance) over all pixels.
    pub delta_e_mean: f64,
    /// Maximum per-pixel CIE76 ΔE — catches localised colour damage that
    /// `delta_e_mean` can dilute away over a large page.
    pub delta_e_max: f64,
}

/// A one-shot quality comparison of two same-size images.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QualityReport {
    /// Mean squared error over the luma channel (0.0 = identical).
    pub mse: f64,
    /// Peak signal-to-noise ratio in dB ([`f64::INFINITY`] if identical).
    pub psnr_db: f64,
    /// Structural similarity index in `[-1, 1]` (1.0 = identical).
    pub ssim: f64,
}

/// Mean squared error between two equal-length luma planes.
fn mse_planes(a: &[f64], b: &[f64]) -> f64 {
    debug_assert_eq!(a.len(), b.len());
    if a.is_empty() {
        return 0.0;
    }
    let sum: f64 = a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum();
    sum / a.len() as f64
}

/// PSNR in dB from a mean squared error over an 8-bit signal (peak = 255).
///
/// Returns [`f64::INFINITY`] when `mse == 0` (identical images).
pub fn psnr_from_mse(mse: f64) -> f64 {
    if mse <= 0.0 {
        return f64::INFINITY;
    }
    10.0 * (255.0f64 * 255.0 / mse).log10()
}

/// Windowed SSIM over two equal-size luma planes.
///
/// Uses 8×8 non-overlapping windows (the widely used fast approximation of the
/// canonical 11×11 Gaussian sliding window) with the standard 8-bit stabilising
/// constants `C1 = (0.01·255)²`, `C2 = (0.03·255)²`, and averages the per-window
/// SSIM. Windows that would run past the right/bottom edge are anchored back
/// inside the image so every pixel is covered. For images smaller than one
/// window the whole image is treated as a single window.
fn ssim_planes(a: &[f64], b: &[f64], width: usize, height: usize) -> f64 {
    const C1: f64 = (0.01 * 255.0) * (0.01 * 255.0);
    const C2: f64 = (0.03 * 255.0) * (0.03 * 255.0);
    const WIN: usize = 8;

    if width == 0 || height == 0 {
        return 1.0;
    }

    let win_w = WIN.min(width);
    let win_h = WIN.min(height);
    let n = (win_w * win_h) as f64;

    // Top-left origins of the non-overlapping windows, clamped so the last
    // window in each axis ends exactly at the edge (covers the remainder).
    let origins = |len: usize, win: usize| -> Vec<usize> {
        let mut v = Vec::new();
        let mut o = 0usize;
        loop {
            v.push(o.min(len - win));
            if o + win >= len {
                break;
            }
            o += win;
        }
        v
    };
    let xs = origins(width, win_w);
    let ys = origins(height, win_h);

    let mut sum_ssim = 0.0;
    let mut count = 0.0;
    for &oy in &ys {
        for &ox in &xs {
            let (mut sa, mut sb, mut saa, mut sbb, mut sab) = (0.0, 0.0, 0.0, 0.0, 0.0);
            for wy in 0..win_h {
                let row = (oy + wy) * width + ox;
                for wx in 0..win_w {
                    let va = a[row + wx];
                    let vb = b[row + wx];
                    sa += va;
                    sb += vb;
                    saa += va * va;
                    sbb += vb * vb;
                    sab += va * vb;
                }
            }
            let mu_a = sa / n;
            let mu_b = sb / n;
            // Population variance/covariance (divide by n) — standard for SSIM.
            let var_a = saa / n - mu_a * mu_a;
            let var_b = sbb / n - mu_b * mu_b;
            let cov = sab / n - mu_a * mu_b;
            let num = (2.0 * mu_a * mu_b + C1) * (2.0 * cov + C2);
            let den = (mu_a * mu_a + mu_b * mu_b + C1) * (var_a + var_b + C2);
            sum_ssim += num / den;
            count += 1.0;
        }
    }
    if count == 0.0 { 1.0 } else { sum_ssim / count }
}

/// PSNR (dB) between two RGBA [`Pixmap`]s over their luma channel.
///
/// # Panics
/// Panics if the two pixmaps differ in dimensions.
#[cfg(feature = "std")]
pub fn psnr(a: &Pixmap, b: &Pixmap) -> f64 {
    assert_eq!(
        (a.width, a.height),
        (b.width, b.height),
        "psnr: dimension mismatch"
    );
    let (pa, pb) = (luma_plane(&a.data), luma_plane(&b.data));
    psnr_from_mse(mse_planes(&pa, &pb))
}

/// SSIM in `[-1, 1]` between two RGBA [`Pixmap`]s over their luma channel.
///
/// # Panics
/// Panics if the two pixmaps differ in dimensions.
#[cfg(feature = "std")]
pub fn ssim(a: &Pixmap, b: &Pixmap) -> f64 {
    assert_eq!(
        (a.width, a.height),
        (b.width, b.height),
        "ssim: dimension mismatch"
    );
    let (pa, pb) = (luma_plane(&a.data), luma_plane(&b.data));
    ssim_planes(&pa, &pb, a.width as usize, a.height as usize)
}

/// Full [`QualityReport`] (MSE + PSNR + SSIM) for two RGBA [`Pixmap`]s.
///
/// # Panics
/// Panics if the two pixmaps differ in dimensions.
#[cfg(feature = "std")]
pub fn compare(a: &Pixmap, b: &Pixmap) -> QualityReport {
    assert_eq!(
        (a.width, a.height),
        (b.width, b.height),
        "compare: dimension mismatch"
    );
    let (pa, pb) = (luma_plane(&a.data), luma_plane(&b.data));
    let mse = mse_planes(&pa, &pb);
    QualityReport {
        mse,
        psnr_db: psnr_from_mse(mse),
        ssim: ssim_planes(&pa, &pb, a.width as usize, a.height as usize),
    }
}

/// [`QualityReport`] for two grayscale [`GrayPixmap`]s (one byte per pixel).
///
/// # Panics
/// Panics if the two pixmaps differ in dimensions.
#[cfg(feature = "std")]
pub fn compare_gray(a: &GrayPixmap, b: &GrayPixmap) -> QualityReport {
    assert_eq!(
        (a.width, a.height),
        (b.width, b.height),
        "compare_gray: dimension mismatch"
    );
    let pa: Vec<f64> = a.data.iter().map(|&v| v as f64).collect();
    let pb: Vec<f64> = b.data.iter().map(|&v| v as f64).collect();
    let mse = mse_planes(&pa, &pb);
    QualityReport {
        mse,
        psnr_db: psnr_from_mse(mse),
        ssim: ssim_planes(&pa, &pb, a.width as usize, a.height as usize),
    }
}

/// Colour-aware [`ColorQualityReport`] for two RGBA [`Pixmap`]s: per-channel
/// (Y/Cb/Cr) SSIM plus mean/max CIE76 ΔE. See the module docs for how this
/// differs from (and complements) [`compare`], which is luma-only.
///
/// # Panics
/// Panics if the two pixmaps differ in dimensions.
#[cfg(feature = "std")]
pub fn compare_color(a: &Pixmap, b: &Pixmap) -> ColorQualityReport {
    assert_eq!(
        (a.width, a.height),
        (b.width, b.height),
        "compare_color: dimension mismatch"
    );
    let (w, h) = (a.width as usize, a.height as usize);

    let (ya, yb) = (luma_plane(&a.data), luma_plane(&b.data));
    let ssim_y = ssim_planes(&ya, &yb, w, h);

    let (cba, cbb) = (cb_plane(&a.data), cb_plane(&b.data));
    let ssim_cb = ssim_planes(&cba, &cbb, w, h);

    let (cra, crb) = (cr_plane(&a.data), cr_plane(&b.data));
    let ssim_cr = ssim_planes(&cra, &crb, w, h);

    let ssim_combined = 0.8 * ssim_y + 0.1 * ssim_cb + 0.1 * ssim_cr;

    let mut delta_e_sum = 0.0f64;
    let mut delta_e_max = 0.0f64;
    let mut count = 0usize;
    for (pa, pb) in a.data.chunks_exact(4).zip(b.data.chunks_exact(4)) {
        let lab_a = rgb_to_lab(pa[0], pa[1], pa[2]);
        let lab_b = rgb_to_lab(pb[0], pb[1], pb[2]);
        let de = delta_e76(lab_a, lab_b);
        delta_e_sum += de;
        if de > delta_e_max {
            delta_e_max = de;
        }
        count += 1;
    }
    let delta_e_mean = if count == 0 {
        0.0
    } else {
        delta_e_sum / count as f64
    };

    ColorQualityReport {
        ssim_y,
        ssim_cb,
        ssim_cr,
        ssim_combined,
        delta_e_mean,
        delta_e_max,
    }
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, r: u8, g: u8, b: u8) -> Pixmap {
        Pixmap::new(w, h, r, g, b, 255)
    }

    #[test]
    fn identical_images_are_perfect() {
        let a = solid(40, 30, 120, 130, 140);
        let rep = compare(&a, &a);
        assert_eq!(rep.mse, 0.0);
        assert!(rep.psnr_db.is_infinite());
        assert!((rep.ssim - 1.0).abs() < 1e-9, "ssim {}", rep.ssim);
    }

    #[test]
    fn psnr_from_mse_known_value() {
        // MSE = 1 → PSNR = 10·log10(65025) ≈ 48.13 dB.
        assert!((psnr_from_mse(1.0) - 48.1308).abs() < 1e-3);
        assert!(psnr_from_mse(0.0).is_infinite());
    }

    #[test]
    fn constant_offset_lowers_psnr_predictably() {
        // Two solid fields differing by a constant luma of 10 → MSE = 100.
        let a = solid(32, 32, 100, 100, 100);
        let b = solid(32, 32, 110, 110, 110);
        let rep = compare(&a, &b);
        assert!((rep.mse - 100.0).abs() < 1e-6, "mse {}", rep.mse);
        assert!((rep.psnr_db - psnr_from_mse(100.0)).abs() < 1e-9);
        // A pure luminance shift on flat fields still has high structural
        // similarity (contrast + structure terms are unchanged).
        assert!(rep.ssim > 0.9, "ssim {}", rep.ssim);
    }

    #[test]
    fn noise_reduces_ssim_more_than_a_flat_shift() {
        let w = 64u32;
        let h = 64u32;
        let base = solid(w, h, 128, 128, 128);

        // A flat +20 shift everywhere.
        let shifted = solid(w, h, 148, 148, 148);

        // Structured corruption: a pixel-level checkerboard so every window
        // sees high-variance damage (none stay unchanged).
        let mut noisy = base.clone();
        for y in 0..h {
            for x in 0..w {
                if (x + y) % 2 == 0 {
                    noisy.set_rgb(x, y, 20, 20, 20);
                } else {
                    noisy.set_rgb(x, y, 235, 235, 235);
                }
            }
        }

        let ssim_shift = ssim(&base, &shifted);
        let ssim_noise = ssim(&base, &noisy);
        assert!(
            ssim_noise < ssim_shift,
            "structural damage {ssim_noise} should score below a flat shift {ssim_shift}"
        );
        assert!(
            ssim_noise < 0.2,
            "pixel-checkerboard damage ssim {ssim_noise}"
        );
    }

    #[test]
    fn tiny_images_handled() {
        // Smaller than one 8×8 window — must not panic and must be perfect.
        let a = solid(3, 2, 10, 20, 30);
        let rep = compare(&a, &a);
        assert!((rep.ssim - 1.0).abs() < 1e-9);
        assert!(rep.psnr_db.is_infinite());
    }

    #[test]
    fn gray_and_rgba_luma_agree() {
        let a = solid(24, 24, 90, 90, 90);
        let b = solid(24, 24, 100, 100, 100);
        let rgba = compare(&a, &b);
        let gray = compare_gray(&a.to_gray8(), &b.to_gray8());
        assert!((rgba.mse - gray.mse).abs() < 1.0);
        assert!((rgba.ssim - gray.ssim).abs() < 1e-3);
    }

    // ---- QUALITY_COLOR ------------------------------------------------------

    /// Same-luma, wildly-different-hue test: (200,104,255) "lavender" and
    /// (0,255,3) "green" were solved so `306·r + 601·g + 117·b` (the fixed-point
    /// luma weights [`luma`] uses) is within 0.07 of each other, but the RGB
    /// triples themselves share no channel in common. A luma-only metric must
    /// report near-perfect similarity; the colour metric must not.
    #[test]
    fn same_luma_shifted_chroma_flags_where_luma_ssim_cannot() {
        let w = 32u32;
        let h = 32u32;
        let lavender = solid(w, h, 200, 104, 255);
        let green = solid(w, h, 0, 255, 3);

        // Sanity: the two solid colours really are near-isoluminant.
        let ly = luma(200, 104, 255);
        let lg = luma(0, 255, 3);
        assert!(
            (ly - lg).abs() < 0.1,
            "fixture not isoluminant: {ly} vs {lg}"
        );

        // Luma-only SSIM: structurally "identical" (both flat fields, equal mean).
        let luma_ssim = ssim(&lavender, &green);
        assert!(
            luma_ssim > 0.999,
            "luma-only SSIM should be ≈1.0 for isoluminant colours, got {luma_ssim}"
        );

        // Colour-aware metric must flag the hue shift luma-SSIM missed.
        let rep = compare_color(&lavender, &green);
        assert!(
            rep.ssim_cb < 0.8,
            "Cb SSIM should drop sharply on a hue shift, got {}",
            rep.ssim_cb
        );
        assert!(
            rep.ssim_cr < 0.8,
            "Cr SSIM should drop sharply on a hue shift, got {}",
            rep.ssim_cr
        );
        assert!(
            rep.ssim_combined < luma_ssim - 0.05,
            "combined SSIM ({}) should read well below luma-only SSIM ({luma_ssim})",
            rep.ssim_combined
        );
        // Lavender vs green is a dramatic colour difference — ΔE76 should be huge
        // (for reference, >10 is "obviously different colour" per CIE guidance).
        assert!(
            rep.delta_e_mean > 100.0,
            "ΔE76 should be large for lavender-vs-green, got {}",
            rep.delta_e_mean
        );
        assert!(
            (rep.delta_e_mean - rep.delta_e_max).abs() < 1e-6,
            "flat fields: mean == max"
        );
    }

    #[test]
    fn identical_images_have_perfect_color_score() {
        let a = solid(24, 24, 60, 130, 200);
        let rep = compare_color(&a, &a);
        assert!((rep.ssim_y - 1.0).abs() < 1e-9);
        assert!((rep.ssim_cb - 1.0).abs() < 1e-9);
        assert!((rep.ssim_cr - 1.0).abs() < 1e-9);
        assert!((rep.ssim_combined - 1.0).abs() < 1e-9);
        assert!(rep.delta_e_mean < 1e-6, "delta_e_mean {}", rep.delta_e_mean);
        assert!(rep.delta_e_max < 1e-6, "delta_e_max {}", rep.delta_e_max);
    }

    #[test]
    fn ssim_y_matches_luma_only_ssim() {
        // ColorQualityReport.ssim_y must agree exactly with the standalone
        // luma-only `ssim()` — it is documented as the same value.
        let a = solid(40, 30, 120, 130, 140);
        let b = solid(40, 30, 100, 90, 80);
        let luma_ssim = ssim(&a, &b);
        let rep = compare_color(&a, &b);
        assert!((rep.ssim_y - luma_ssim).abs() < 1e-12);
    }

    #[test]
    fn delta_e_zero_for_identical_pixel() {
        let lab = rgb_to_lab(128, 64, 200);
        assert!(delta_e76(lab, lab) < 1e-9);
    }

    #[test]
    fn delta_e_white_black_is_max_lightness_gap() {
        // Pure white vs pure black in Lab: L*=100 vs L*=0, a*=b*=0 for both
        // (neutral greys sit on the a*=b*=0 axis) → ΔE76 == 100.
        let white = rgb_to_lab(255, 255, 255);
        let black = rgb_to_lab(0, 0, 0);
        assert!(
            (white.1).abs() < 1e-3,
            "white a* should be ~0, got {}",
            white.1
        );
        assert!(
            (white.2).abs() < 1e-3,
            "white b* should be ~0, got {}",
            white.2
        );
        assert!((black.0).abs() < 1e-6, "black L* should be ~0");
        let de = delta_e76(white, black);
        assert!((de - 100.0).abs() < 1e-3, "ΔE76(white, black) = {de}");
    }

    #[test]
    fn tiny_images_handled_by_color_metric() {
        // Smaller than one 8×8 SSIM window — must not panic and must be perfect.
        let a = solid(3, 2, 10, 20, 30);
        let rep = compare_color(&a, &a);
        assert!((rep.ssim_combined - 1.0).abs() < 1e-9);
    }
}
