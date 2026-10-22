//! Backwards-compatible pixmap module.
//!
//! The implementation lives in the standalone `djvu-pixmap` crate. This module
//! preserves the historical `djvu_rs::pixmap::{Pixmap, GrayPixmap}` path and
//! hosts [`scale_lanczos3`], the context-free Lanczos-3 image resampler shared
//! by the render paths (it has no DjVu semantics, so it belongs with the pixmap
//! type rather than on the render interface).

pub use djvu_pixmap::{GrayPixmap, Pixmap, PixmapError};

// `vec!` / `Vec` are not in the no_std prelude; bring them in from `alloc` (the
// std prelude already provides them). Matches the cfg-gated import pattern used
// by other modules.
#[cfg(not(feature = "std"))]
use alloc::{vec, vec::Vec};

/// Lanczos-3 kernel: `sinc(x) * sinc(x/3)` for `|x| < 3`, 0 otherwise.
///
/// Uses the normalised sinc: `sinc(x) = sin(π x) / (π x)`, `sinc(0) = 1`.
#[inline]
fn lanczos3_kernel(x: f32) -> f32 {
    let ax = x.abs();
    if ax >= 3.0 {
        return 0.0;
    }
    if ax < 1e-6 {
        return 1.0;
    }
    let pi_x = core::f32::consts::PI * ax;
    let sinc_x = pi_x.sin() / pi_x;
    let pi_x3 = pi_x / 3.0;
    let sinc_x3 = pi_x3.sin() / pi_x3;
    sinc_x * sinc_x3
}

/// One axis of a Lanczos-3 rescale from `src_len` to `dst_len` pixels: where
/// output pixel `o` sits on the source, and which source pixels it reads.
///
/// The one place both passes and [`lanczos3_source_window`] take their taps
/// from, so a windowed rescale reads exactly the pixels a whole one does.
#[derive(Clone, Copy)]
struct Axis {
    scale: f32,
    /// Kernel half-width in source pixels.
    support: i32,
    src_len: u32,
}

impl Axis {
    fn new(src_len: u32, dst_len: u32) -> Self {
        let scale = src_len as f32 / dst_len as f32;
        Self {
            scale,
            support: (3.0_f32 * scale.max(1.0)).ceil() as i32,
            src_len,
        }
    }

    /// The source position of output pixel `o`.
    fn centre(&self, o: u32) -> f32 {
        (o as f32 + 0.5) * self.scale - 0.5
    }

    /// The source pixels `lo..=hi` that output pixel at `centre` reads.
    fn taps(&self, centre: f32) -> (i32, i32) {
        let c = centre.floor() as i32;
        (
            (c - self.support + 1).max(0),
            (c + self.support).min(self.src_len as i32 - 1),
        )
    }

    /// The weight of source pixel `s` for output pixel at `centre`.
    fn weight(&self, s: i32, centre: f32) -> f32 {
        lanczos3_kernel((s as f32 - centre) / self.scale.max(1.0))
    }

    /// The source pixels `lo..hi` that output pixels `o..o + len` read.
    fn span(&self, o: u32, len: u32) -> (u32, u32) {
        let (lo, _) = self.taps(self.centre(o));
        let (_, hi) = self.taps(self.centre(o + len - 1));
        (lo as u32, (hi + 1).max(lo) as u32)
    }
}

/// Scale `src` to `dst_w × dst_h` using separable Lanczos-3 resampling.
///
/// Two-pass implementation:
/// 1. Horizontal pass: `src_w × src_h` → `dst_w × src_h` intermediate.
/// 2. Vertical pass: `dst_w × src_h` → `dst_w × dst_h` output.
///
/// Only RGBA pixmaps are handled (alpha is passed through unchanged at 255).
///
/// # Errors
///
/// [`PixmapError`] when the intermediate `dst_w × src_h` buffer or the
/// `dst_w × dst_h` output exceeds [`Pixmap::MAX_PIXELS`]. The caller decides
/// whether that is a render limit or a bug in its own size arithmetic.
#[cfg_attr(not(feature = "std"), allow(dead_code))]
pub(crate) fn scale_lanczos3(src: &Pixmap, dst_w: u32, dst_h: u32) -> Result<Pixmap, PixmapError> {
    // Short-circuit: nothing to scale.
    if src.width == dst_w && src.height == dst_h {
        return Ok(src.clone());
    }
    if dst_w == 0 || dst_h == 0 {
        return Pixmap::try_white(dst_w.max(1), dst_h.max(1));
    }
    scale_lanczos3_window(
        src,
        (0, 0),
        (src.width, src.height),
        (dst_w, dst_h),
        (0, 0, dst_w, dst_h),
    )
}

/// The part of a `src_full` image that a Lanczos-3 rescale to `dst_full`
/// reads for the output window `(x, y, width, height)`: `(x, y, width,
/// height)` on the source. The window must be non-empty and inside
/// `dst_full`.
pub(crate) fn lanczos3_source_window(
    src_full: (u32, u32),
    dst_full: (u32, u32),
    window: (u32, u32, u32, u32),
) -> (u32, u32, u32, u32) {
    let (x0, x1) = Axis::new(src_full.0, dst_full.0).span(window.0, window.2);
    let (y0, y1) = Axis::new(src_full.1, dst_full.1).span(window.1, window.3);
    (x0, y0, x1 - x0, y1 - y0)
}

/// The output window `(x, y, width, height)` of a Lanczos-3 rescale of a
/// `src_full` image to `dst_full`, from `src`: the part of the source at
/// `origin` that covers [`lanczos3_source_window`] of that window.
///
/// Byte-identical to the same window cut from [`scale_lanczos3`] of the whole
/// source: each output pixel sums the same source pixels with the same
/// weights in the same order. The cost scales with the window, not the page.
///
/// # Errors
///
/// [`PixmapError`] when the intermediate buffer or the output exceeds
/// [`Pixmap::MAX_PIXELS`].
pub(crate) fn scale_lanczos3_window(
    src: &Pixmap,
    origin: (u32, u32),
    src_full: (u32, u32),
    dst_full: (u32, u32),
    window: (u32, u32, u32, u32),
) -> Result<Pixmap, PixmapError> {
    let (win_x, win_y, win_w, win_h) = window;
    let h_axis = Axis::new(src_full.0, dst_full.0);
    let v_axis = Axis::new(src_full.1, dst_full.1);
    // The source rows the window reads; the horizontal pass filters only these.
    let (row0, row1) = v_axis.span(win_y, win_h);
    debug_assert!(
        origin.1 <= row0 && row1 - origin.1 <= src.height,
        "the source must cover the rows the window reads"
    );

    // ── Horizontal pass ───────────────────────────────────────────────────────
    // Map each output column `ox` to a source position, then sum the Lanczos-3
    // kernel over the contributing source columns.
    //
    // The horizontal weight `lanczos3_kernel((sx - cx)/h_scale)` and the
    // normaliser depend only on the output column `ox` (via `cx`), never on the
    // row `oy`. Precompute, once, the contributor start `x0` + kernel weights +
    // norm for every output column, then the per-row loop is a pure weighted
    // sum. This hoists the sin-heavy kernel evaluation out of the `src_h` row
    // loop — the same idea that made the #448 vertical pass ~22% faster — and
    // combines it with row-pointer indexing so the source/destination rows are
    // read without per-pixel `get_rgb`/`set_rgb` bounds checks. Bit-identical:
    // identical weights summed in identical order with the identical norm.
    let dw = win_w as usize;
    let sw = src.width as usize;
    struct HCol {
        x0: usize,
        weights: Vec<f32>,
        norm: f32,
    }
    let hcols: Vec<HCol> = (win_x..win_x + win_w)
        .map(|ox| {
            let cx = h_axis.centre(ox);
            let (x0, x1) = h_axis.taps(cx);
            debug_assert!(
                origin.0 as i32 <= x0 && x1 < (origin.0 + src.width) as i32,
                "the source must cover the columns the window reads"
            );
            let mut weights = Vec::with_capacity((x1 - x0 + 1).max(0) as usize);
            let mut w_sum = 0.0_f32;
            for sx in x0..=x1 {
                let w = h_axis.weight(sx, cx);
                weights.push(w);
                w_sum += w;
            }
            let norm = if w_sum.abs() > 1e-6 { 1.0 / w_sum } else { 1.0 };
            HCol {
                x0: (x0 - origin.0 as i32) as usize,
                weights,
                norm,
            }
        })
        .collect();

    let mut mid = Pixmap::try_new(win_w, row1 - row0, 255, 255, 255, 255)?;

    // Per-output-row horizontal filter. Rows are independent (each reads its own
    // `src` row + the shared `hcols`, writes its own `mid` row), so the loop
    // parallelises over rows with no shared mutable state. Bit-identical either
    // way: the per-pixel weighted sum is over the same contributors in the same
    // order regardless of which thread runs the row.
    // Accumulate all four RGBA channels of each output pixel into one `[f32; 4]`
    // read from four contiguous source bytes per tap, so LLVM widens the tap
    // multiply-add to a single 4-lane FMA instead of three scalar ops on a
    // stride-4 deinterleave. The alpha lane accumulates the source's constant 255
    // and is ignored on output. Bit-identical: each RGB channel sums the same taps
    // in the same order with the same norm.
    let h_row = |my: usize, mid_row: &mut [u8]| {
        let sy = (row0 - origin.1) as usize + my;
        let src_row = &src.data[sy * sw * 4..(sy + 1) * sw * 4];
        for (ox, col) in hcols.iter().enumerate() {
            let mut acc = [0.0_f32; 4];
            for (i, &w) in col.weights.iter().enumerate() {
                let base = (col.x0 + i) * 4;
                let px = &src_row[base..base + 4];
                acc[0] += px[0] as f32 * w;
                acc[1] += px[1] as f32 * w;
                acc[2] += px[2] as f32 * w;
                acc[3] += px[3] as f32 * w;
            }
            let ob = ox * 4;
            mid_row[ob] = (acc[0] * col.norm).round().clamp(0.0, 255.0) as u8;
            mid_row[ob + 1] = (acc[1] * col.norm).round().clamp(0.0, 255.0) as u8;
            mid_row[ob + 2] = (acc[2] * col.norm).round().clamp(0.0, 255.0) as u8;
            // mid_row[ob + 3] stays 255 (from Pixmap::new alpha init).
        }
    };
    #[cfg(feature = "parallel")]
    {
        use rayon::prelude::*;
        mid.data
            .par_chunks_mut(dw * 4)
            .enumerate()
            .for_each(|(my, mid_row)| h_row(my, mid_row));
    }
    #[cfg(not(feature = "parallel"))]
    for (my, mid_row) in mid.data.chunks_mut(dw * 4).enumerate() {
        h_row(my, mid_row);
    }

    // ── Vertical pass ─────────────────────────────────────────────────────────
    // #448: the vertical weight depends only on (oy, sy), not ox, so hoist the
    // `lanczos3_kernel` evaluations out of the per-column loop (LLVM cannot LICM
    // the opaque `f32::sin` calls). Accumulate row-major into per-column buffers so
    // `mid` is read sequentially instead of striding by `dst_w*4` per sy. The
    // per-column sum is over the same `sy` values in the same order, so the result
    // is bit-identical to the column-major version.
    let mut out = Pixmap::try_new(win_w, win_h, 255, 255, 255, 255)?;

    // Per-output-row vertical filter, writing directly into `out_row`. Output
    // rows are independent; the only per-row mutable state is the three column
    // accumulators, so each worker keeps its own scratch (reused across the rows
    // it processes). Bit-identical to the sequential version: each output pixel
    // sums the same `sy` contributors in the same order.
    // Accumulate into a single **interleaved** RGBA `f32` buffer (`acc[ox*4+c]`)
    // rather than three separate `acc_r/g/b` column arrays. The inner `sy` loop is
    // then a contiguous SAXPY `acc[i] += mid_row[i] * w` over `dw*4` elements,
    // which LLVM auto-vectorises optimally — the previous stride-4 deinterleave
    // (reading `row[base], row[base+1], row[base+2]` into three arrays) inhibited
    // it. The alpha lane accumulates `mid`'s constant 255 and is ignored on output.
    // Bit-identical: each output channel still sums the same `sy` contributors in
    // the same order with the same norm.
    let v_row = |oy: usize, out_row: &mut [u8], acc: &mut [f32]| {
        let cy = v_axis.centre(win_y + oy as u32);
        let (y0, y1) = v_axis.taps(cy);

        acc.iter_mut().for_each(|v| *v = 0.0);
        let mut w_sum = 0.0_f32;

        for sy in y0..=y1 {
            let w = v_axis.weight(sy, cy);
            w_sum += w;
            let my = (sy as u32 - row0) as usize;
            let row = &mid.data[my * dw * 4..(my + 1) * dw * 4];
            for (a, &s) in acc.iter_mut().zip(row.iter()) {
                *a += s as f32 * w;
            }
        }

        let norm = if w_sum.abs() > 1e-6 { 1.0 / w_sum } else { 1.0 };
        for ox in 0..dw {
            let ob = ox * 4;
            out_row[ob] = (acc[ob] * norm).round().clamp(0.0, 255.0) as u8;
            out_row[ob + 1] = (acc[ob + 1] * norm).round().clamp(0.0, 255.0) as u8;
            out_row[ob + 2] = (acc[ob + 2] * norm).round().clamp(0.0, 255.0) as u8;
            // out_row[ob + 3] stays 255 (from Pixmap::new alpha init).
        }
    };

    #[cfg(feature = "parallel")]
    {
        use rayon::prelude::*;
        out.data.par_chunks_mut(dw * 4).enumerate().for_each_init(
            || vec![0.0_f32; dw * 4],
            |acc, (oy, out_row)| {
                v_row(oy, out_row, acc);
            },
        );
    }
    #[cfg(not(feature = "parallel"))]
    {
        let mut acc = vec![0.0_f32; dw * 4];
        for (oy, out_row) in out.data.chunks_mut(dw * 4).enumerate() {
            v_row(oy, out_row, &mut acc);
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #815: an output above `Pixmap::MAX_PIXELS` is refused, not returned
    /// as an empty pixmap.
    #[test]
    fn scale_lanczos3_refuses_oversized_output() {
        let src = Pixmap::white(2, 2);
        let err = scale_lanczos3(&src, 10000, 10000).unwrap_err();
        assert!(matches!(
            err,
            PixmapError::TooLarge {
                width: 10000,
                height: 10000,
                ..
            }
        ));
    }

    /// `lanczos3_kernel(0)` == 1.0 (unity at origin).
    #[test]
    fn lanczos3_kernel_unity_at_zero() {
        assert!((lanczos3_kernel(0.0) - 1.0).abs() < 1e-5);
    }

    /// `lanczos3_kernel` is zero outside |x| ≥ 3.
    #[test]
    fn lanczos3_kernel_zero_outside_support() {
        assert_eq!(lanczos3_kernel(3.0), 0.0);
        assert_eq!(lanczos3_kernel(-3.5), 0.0);
        assert_eq!(lanczos3_kernel(10.0), 0.0);
    }

    /// `scale_lanczos3` preserves dimensions.
    #[test]
    fn scale_lanczos3_correct_dimensions() {
        let src = Pixmap::white(100, 80);
        let dst = scale_lanczos3(&src, 50, 40).expect("fits the pixmap limit");
        assert_eq!(dst.width, 50);
        assert_eq!(dst.height, 40);
    }

    /// `scale_lanczos3` returns a clone when source and target match.
    #[test]
    fn scale_lanczos3_noop_when_same_size() {
        let src = Pixmap::try_new(4, 4, 200, 100, 50, 255).expect("fits the pixmap limit");
        let dst = scale_lanczos3(&src, 4, 4).expect("fits the pixmap limit");
        assert_eq!(dst.width, 4);
        assert_eq!(dst.height, 4);
        assert_eq!(dst.data, src.data);
    }

    /// Scaling a solid-color pixmap with Lanczos-3 preserves the color.
    #[test]
    fn scale_lanczos3_preserves_solid_color() {
        // Solid red 20×20 → 10×10
        let src = Pixmap::try_new(20, 20, 200, 0, 0, 255).expect("fits the pixmap limit");
        let dst = scale_lanczos3(&src, 10, 10).expect("fits the pixmap limit");
        assert_eq!(dst.width, 10);
        assert_eq!(dst.height, 10);
        // All output pixels should be close to red (200, 0, 0).
        for chunk in dst.data.as_chunks::<4>().0 {
            let (r, g, b) = (chunk[0], chunk[1], chunk[2]);
            assert!(
                (r as i32 - 200).abs() <= 5 && g <= 5 && b <= 5,
                "expected near-red (200,0,0), got ({r},{g},{b})"
            );
        }
    }

    /// A windowed rescale from just the source it reads is byte-identical
    /// to the same window of the whole rescale, down and up, at every edge.
    #[test]
    fn scale_lanczos3_window_matches_whole_crop() {
        let (sw, sh) = (37u32, 23u32);
        let mut src = Pixmap::white(sw, sh);
        for (i, b) in src.data.iter_mut().enumerate() {
            if i % 4 != 3 {
                *b = (i.wrapping_mul(2_654_435_761) >> 7) as u8;
            }
        }
        for (dw, dh) in [(12u32, 9u32), (74, 46), (50, 11), (5, 60)] {
            let whole = scale_lanczos3(&src, dw, dh).unwrap();
            for (x, y, w, h) in [
                (0, 0, 1, 1),
                (dw - 1, dh - 1, 1, 1),
                (0, 0, dw, dh),
                (dw / 3, dh / 4, dw / 2, dh / 2),
                (1, dh / 2, dw - 1, 1),
            ] {
                let (ox, oy, ow, oh) = lanczos3_source_window((sw, sh), (dw, dh), (x, y, w, h));
                assert!(ox + ow <= sw && oy + oh <= sh);
                let mut part = Pixmap::white(ow, oh);
                for row in 0..oh {
                    let from = (((oy + row) * sw + ox) * 4) as usize;
                    let to = (row * ow * 4) as usize;
                    part.data[to..to + ow as usize * 4]
                        .copy_from_slice(&src.data[from..from + ow as usize * 4]);
                }
                let got = scale_lanczos3_window(&part, (ox, oy), (sw, sh), (dw, dh), (x, y, w, h))
                    .unwrap();
                assert_eq!((got.width, got.height), (w, h));
                for row in 0..h {
                    let from = (((y + row) * dw + x) * 4) as usize;
                    let to = (row * w * 4) as usize;
                    assert_eq!(
                        got.data[to..to + w as usize * 4],
                        whole.data[from..from + w as usize * 4],
                        "{dw}x{dh} window {x},{y} {w}x{h} row {row}"
                    );
                }
            }
        }
    }

    #[test]
    fn scale_lanczos3_zero_dst_dimension_returns_white_fallback() {
        let src = Pixmap::white(10, 10);
        // dst_w=0 → Pixmap::white(max(0,1)=1, 5)
        let dst = scale_lanczos3(&src, 0, 5).expect("fits the pixmap limit");
        assert_eq!(dst.width, 1);
        assert_eq!(dst.height, 5);
        // dst_h=0 → Pixmap::white(8, max(0,1)=1)
        let dst2 = scale_lanczos3(&src, 8, 0).expect("fits the pixmap limit");
        assert_eq!(dst2.width, 8);
        assert_eq!(dst2.height, 1);
    }
}
