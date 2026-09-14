//! Deterministic preprocessing for the DBNet text detector (#693).
//!
//! Turns a rendered page [`Pixmap`] into the `[1, 3, H, W]` f32 tensor the
//! PP-OCRv4 detection model expects:
//!
//! 1. [`det_input_size`] — pick the model input size from the page's actual
//!    extent: scale down so the long side fits [`DET_MAX_SIDE`], then round
//!    each side to the nearest multiple of [`DET_STRIDE`]. The size follows
//!    the page's aspect ratio — never one fixed canvas. DBNet's global
//!    pooling averages over the whole input, so padding a small page onto a
//!    large canvas would shift the probability map (see
//!    `docs/neural-ocr-design.md`).
//! 2. [`resize_bilinear_rgb`] — fixed-point (16.16) bilinear resample with
//!    pixel-center mapping. Integer arithmetic end-to-end, so the resampled
//!    bytes are bit-identical across platforms and runs.
//! 3. [`det_tensor`] — normalize with the ImageNet mean/std the model was
//!    trained with ([`DET_MEAN`], [`DET_STD`]) and lay out as CHW f32.
//!
//! Everything here is pure: no I/O, no model, no feature-gated behavior —
//! which is what makes the pipeline unit-testable without model weights.

use crate::pixmap::Pixmap;

/// Longest input side the detector accepts before we scale a page down.
pub const DET_MAX_SIDE: u32 = 960;

/// Both input sides must be multiples of this stride (DBNet downsamples ×32).
pub const DET_STRIDE: u32 = 32;

/// Per-channel RGB mean the detector was trained with (ImageNet).
pub const DET_MEAN: [f32; 3] = [0.485, 0.456, 0.406];

/// Per-channel RGB std the detector was trained with (ImageNet).
pub const DET_STD: [f32; 3] = [0.229, 0.224, 0.225];

/// Model input size for a page of `width × height` pixels.
///
/// Scales down (never up) so the long side is at most [`DET_MAX_SIDE`], then
/// rounds each side to the nearest multiple of [`DET_STRIDE`] (minimum one
/// stride). Integer arithmetic only — the same page always maps to the same
/// input size.
pub fn det_input_size(width: u32, height: u32) -> (u32, u32) {
    let long = width.max(height).max(1);
    let scale_dim = |dim: u32| -> u32 {
        let scaled = if long <= DET_MAX_SIDE {
            u64::from(dim)
        } else {
            // Round-to-nearest integer scaling: dim * MAX_SIDE / long.
            (u64::from(dim) * u64::from(DET_MAX_SIDE) + u64::from(long) / 2) / u64::from(long)
        };
        // Round to the nearest stride multiple, minimum one stride.
        let stride = u64::from(DET_STRIDE);
        let rounded = ((scaled + stride / 2) / stride) * stride;
        rounded.max(stride) as u32
    };
    (scale_dim(width), scale_dim(height))
}

/// One bilinear tap along one axis: two source indices and a 16.16 fraction.
struct AxisTap {
    i0: usize,
    i1: usize,
    /// Weight of `i1` in 16.16 fixed point (`0..=65535`).
    frac: u64,
}

/// Precompute bilinear taps for one axis with pixel-center mapping:
/// `src = (dst + 0.5) * src_len / dst_len - 0.5`, clamped to the valid range.
fn axis_taps(src_len: u32, dst_len: u32) -> Vec<AxisTap> {
    let src = i64::from(src_len);
    let dst = i64::from(dst_len);
    let max_pos = (src - 1) << 16;
    (0..dst)
        .map(|d| {
            // (d + 0.5) * src / dst - 0.5 in 16.16 fixed point.
            let pos = (((2 * d + 1) * src) << 15) / dst - (1 << 15);
            let pos = pos.clamp(0, max_pos);
            let i0 = (pos >> 16) as usize;
            AxisTap {
                i0,
                i1: (i0 + 1).min(src_len as usize - 1),
                frac: (pos & 0xffff) as u64,
            }
        })
        .collect()
}

/// Resample `src` to `dst_w × dst_h` with bilinear interpolation and return
/// packed RGB bytes (`dst_w * dst_h * 3`, row-major).
///
/// Fixed-point 16.16 arithmetic with round-to-nearest — bit-identical output
/// on every platform. Alpha is dropped (DjVu pages render opaque).
pub fn resize_bilinear_rgb(src: &Pixmap, dst_w: u32, dst_h: u32) -> Vec<u8> {
    assert!(dst_w > 0 && dst_h > 0, "destination size must be non-zero");
    let sw = src.width.max(1) as usize;
    let xtaps = axis_taps(src.width.max(1), dst_w);
    let ytaps = axis_taps(src.height.max(1), dst_h);

    let mut out = Vec::with_capacity(dst_w as usize * dst_h as usize * 3);
    for ty in &ytaps {
        let row0 = &src.data[ty.i0 * sw * 4..(ty.i0 * sw + sw) * 4];
        let row1 = &src.data[ty.i1 * sw * 4..(ty.i1 * sw + sw) * 4];
        let (fy, inv_fy) = (ty.frac, 65536 - ty.frac);
        for tx in &xtaps {
            let (fx, inv_fx) = (tx.frac, 65536 - tx.frac);
            let (p00, p01) = (tx.i0 * 4, tx.i1 * 4);
            for c in 0..3 {
                // Horizontal blend per row (fits in 24 bits), then vertical.
                let top = u64::from(row0[p00 + c]) * inv_fx + u64::from(row0[p01 + c]) * fx;
                let bot = u64::from(row1[p00 + c]) * inv_fx + u64::from(row1[p01 + c]) * fx;
                let v = (top * inv_fy + bot * fy + (1 << 31)) >> 32;
                out.push(v as u8);
            }
        }
    }
    out
}

/// Build the detector's input tensor data: resize the page to
/// `dst_w × dst_h`, normalize with [`DET_MEAN`]/[`DET_STD`], and lay out as
/// CHW f32 (`3 * dst_h * dst_w` values, C-major).
///
/// The caller wraps the buffer into `[1, 3, dst_h, dst_w]` for the model.
pub fn det_tensor(src: &Pixmap, dst_w: u32, dst_h: u32) -> Vec<f32> {
    let rgb = resize_bilinear_rgb(src, dst_w, dst_h);
    let plane = dst_w as usize * dst_h as usize;
    let mut out = vec![0.0f32; 3 * plane];
    for (i, px) in rgb.as_chunks::<3>().0.iter().enumerate() {
        for c in 0..3 {
            out[c * plane + i] = (f32::from(px[c]) / 255.0 - DET_MEAN[c]) / DET_STD[c];
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn det_input_size_keeps_small_pages_and_rounds_to_stride() {
        // Already stride-aligned and under the cap: unchanged.
        assert_eq!(det_input_size(640, 480), (640, 480));
        // Under the cap but unaligned: rounded to the nearest ×32.
        assert_eq!(det_input_size(650, 470), (640, 480));
        // Tiny page: clamped up to one stride, never zero.
        assert_eq!(det_input_size(10, 10), (32, 32));
    }

    #[test]
    fn det_input_size_scales_long_side_down_to_cap() {
        assert_eq!(det_input_size(1000, 500), (960, 480));
        // Extreme aspect ratio: short side still at least one stride.
        let (w, h) = det_input_size(4000, 40);
        assert_eq!(w, 960);
        assert_eq!(h, 32);
        // Never upscales: long side below the cap stays put.
        assert_eq!(det_input_size(320, 960), (320, 960));
    }

    #[test]
    fn det_input_size_is_deterministic_and_stride_aligned() {
        for &(w, h) in &[(1u32, 1u32), (123, 4567), (960, 960), (5000, 3000)] {
            let a = det_input_size(w, h);
            let b = det_input_size(w, h);
            assert_eq!(a, b);
            assert_eq!(a.0 % DET_STRIDE, 0);
            assert_eq!(a.1 % DET_STRIDE, 0);
            assert!(a.0 >= DET_STRIDE && a.1 >= DET_STRIDE);
        }
    }

    fn checker_2x2() -> Pixmap {
        // 2×2: (10,20,30) (50,60,70) / (90,100,110) (130,140,150)
        let mut pm = Pixmap::white(2, 2);
        let px = [
            [10u8, 20, 30],
            [50, 60, 70],
            [90, 100, 110],
            [130, 140, 150],
        ];
        for (i, rgb) in px.iter().enumerate() {
            pm.data[i * 4..i * 4 + 3].copy_from_slice(rgb);
        }
        pm
    }

    #[test]
    fn resize_same_size_is_identity() {
        let pm = checker_2x2();
        let rgb = resize_bilinear_rgb(&pm, 2, 2);
        let expected: Vec<u8> = pm
            .data
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|p| p[..3].to_vec())
            .collect();
        assert_eq!(rgb, expected);
    }

    #[test]
    fn resize_2x2_to_1x1_averages_all_pixels() {
        // Pixel-center mapping puts the single output pixel exactly between
        // all four inputs: the result is their rounded average.
        let rgb = resize_bilinear_rgb(&checker_2x2(), 1, 1);
        assert_eq!(rgb, vec![70, 80, 90]);
    }

    #[test]
    fn resize_is_deterministic() {
        let mut pm = Pixmap::white(17, 13);
        for (i, b) in pm.data.iter_mut().enumerate() {
            *b = (i * 7 % 251) as u8;
        }
        let a = resize_bilinear_rgb(&pm, 32, 32);
        let b = resize_bilinear_rgb(&pm, 32, 32);
        assert_eq!(a, b);
        assert_eq!(a.len(), 32 * 32 * 3);
    }

    #[test]
    fn det_tensor_normalizes_uniform_gray() {
        let mut pm = Pixmap::white(8, 8);
        for b in pm.data.iter_mut() {
            *b = 128;
        }
        let t = det_tensor(&pm, 32, 32);
        assert_eq!(t.len(), 3 * 32 * 32);
        let plane = 32 * 32;
        for c in 0..3 {
            let expected = (128.0 / 255.0 - DET_MEAN[c]) / DET_STD[c];
            for i in 0..plane {
                assert!((t[c * plane + i] - expected).abs() < 1e-6);
            }
        }
    }
}
