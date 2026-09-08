//! Backwards-compatible pixmap module.
//!
//! The implementation lives in the standalone `djvu-pixmap` crate. This module
//! preserves the historical `djvu_rs::pixmap::{Pixmap, GrayPixmap}` path and
//! hosts [`scale_lanczos3`], the context-free Lanczos-3 image resampler shared
//! by the render paths (it has no DjVu semantics, so it belongs with the pixmap
//! type rather than on the render interface).

pub use djvu_pixmap::{GrayPixmap, Pixmap};

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

/// Scale `src` to `dst_w × dst_h` using separable Lanczos-3 resampling.
///
/// Two-pass implementation:
/// 1. Horizontal pass: `src_w × src_h` → `dst_w × src_h` intermediate.
/// 2. Vertical pass: `dst_w × src_h` → `dst_w × dst_h` output.
///
/// Only RGBA pixmaps are handled (alpha is passed through unchanged at 255).
pub(crate) fn scale_lanczos3(src: &Pixmap, dst_w: u32, dst_h: u32) -> Pixmap {
    let src_w = src.width;
    let src_h = src.height;

    // Short-circuit: nothing to scale.
    if src_w == dst_w && src_h == dst_h {
        return src.clone();
    }
    if dst_w == 0 || dst_h == 0 {
        return Pixmap::white(dst_w.max(1), dst_h.max(1));
    }

    // ── Horizontal pass ───────────────────────────────────────────────────────
    // Map each output column `ox` (0..dst_w) to a source position, then sum
    // the Lanczos-3 kernel over the contributing source columns.
    let h_scale = src_w as f32 / dst_w as f32;
    let h_support = (3.0_f32 * h_scale.max(1.0)).ceil() as i32; // kernel half-width in src pixels

    // The horizontal weight `lanczos3_kernel((sx - cx)/h_scale)` and the
    // normaliser depend only on the output column `ox` (via `cx`), never on the
    // row `oy`. Precompute, once, the contributor start `x0` + kernel weights +
    // norm for every output column, then the per-row loop is a pure weighted
    // sum. This hoists the sin-heavy kernel evaluation out of the `src_h` row
    // loop — the same idea that made the #448 vertical pass ~22% faster — and
    // combines it with row-pointer indexing so the source/destination rows are
    // read without per-pixel `get_rgb`/`set_rgb` bounds checks. Bit-identical:
    // identical weights summed in identical order with the identical norm.
    let dw = dst_w as usize;
    let sw = src_w as usize;
    struct HCol {
        x0: usize,
        weights: Vec<f32>,
        norm: f32,
    }
    let hcols: Vec<HCol> = (0..dst_w)
        .map(|ox| {
            let cx = (ox as f32 + 0.5) * h_scale - 0.5;
            let x0 = (cx.floor() as i32 - h_support + 1).max(0);
            let x1 = (cx.floor() as i32 + h_support).min(src_w as i32 - 1);
            let mut weights = Vec::with_capacity((x1 - x0 + 1).max(0) as usize);
            let mut w_sum = 0.0_f32;
            for sx in x0..=x1 {
                let w = lanczos3_kernel((sx as f32 - cx) / h_scale.max(1.0));
                weights.push(w);
                w_sum += w;
            }
            let norm = if w_sum.abs() > 1e-6 { 1.0 / w_sum } else { 1.0 };
            HCol {
                x0: x0 as usize,
                weights,
                norm,
            }
        })
        .collect();

    let mut mid = Pixmap::new(dst_w, src_h, 255, 255, 255, 255);

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
    let h_row = |oy: usize, mid_row: &mut [u8]| {
        let src_row = &src.data[oy * sw * 4..(oy + 1) * sw * 4];
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
            .for_each(|(oy, mid_row)| h_row(oy, mid_row));
    }
    #[cfg(not(feature = "parallel"))]
    for oy in 0..src_h as usize {
        let mid_row = &mut mid.data[oy * dw * 4..(oy + 1) * dw * 4];
        h_row(oy, mid_row);
    }

    // ── Vertical pass ─────────────────────────────────────────────────────────
    let v_scale = src_h as f32 / dst_h as f32;
    let v_support = (3.0_f32 * v_scale.max(1.0)).ceil() as i32;

    // #448: the vertical weight depends only on (oy, sy), not ox, so hoist the
    // `lanczos3_kernel` evaluations out of the per-column loop (LLVM cannot LICM
    // the opaque `f32::sin` calls). Accumulate row-major into per-column buffers so
    // `mid` is read sequentially instead of striding by `dst_w*4` per sy. The
    // per-column sum is over the same `sy` values in the same order, so the result
    // is bit-identical to the column-major version.
    let mut out = Pixmap::new(dst_w, dst_h, 255, 255, 255, 255);

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
        let cy = (oy as f32 + 0.5) * v_scale - 0.5;
        let y0 = (cy.floor() as i32 - v_support + 1).max(0);
        let y1 = (cy.floor() as i32 + v_support).min(src_h as i32 - 1);

        acc.iter_mut().for_each(|v| *v = 0.0);
        let mut w_sum = 0.0_f32;

        for sy in y0..=y1 {
            let w = lanczos3_kernel((sy as f32 - cy) / v_scale.max(1.0));
            w_sum += w;
            let row = &mid.data[sy as usize * dw * 4..(sy as usize + 1) * dw * 4];
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
        for oy in 0..dst_h as usize {
            let out_row = &mut out.data[oy * dw * 4..(oy + 1) * dw * 4];
            v_row(oy, out_row, &mut acc);
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let dst = scale_lanczos3(&src, 50, 40);
        assert_eq!(dst.width, 50);
        assert_eq!(dst.height, 40);
    }

    /// `scale_lanczos3` returns a clone when source and target match.
    #[test]
    fn scale_lanczos3_noop_when_same_size() {
        let src = Pixmap::new(4, 4, 200, 100, 50, 255);
        let dst = scale_lanczos3(&src, 4, 4);
        assert_eq!(dst.width, 4);
        assert_eq!(dst.height, 4);
        assert_eq!(dst.data, src.data);
    }

    /// Scaling a solid-color pixmap with Lanczos-3 preserves the color.
    #[test]
    fn scale_lanczos3_preserves_solid_color() {
        // Solid red 20×20 → 10×10
        let src = Pixmap::new(20, 20, 200, 0, 0, 255);
        let dst = scale_lanczos3(&src, 10, 10);
        assert_eq!(dst.width, 10);
        assert_eq!(dst.height, 10);
        // All output pixels should be close to red (200, 0, 0).
        for chunk in dst.data.chunks_exact(4) {
            let (r, g, b) = (chunk[0], chunk[1], chunk[2]);
            assert!(
                (r as i32 - 200).abs() <= 5 && g <= 5 && b <= 5,
                "expected near-red (200,0,0), got ({r},{g},{b})"
            );
        }
    }

    #[test]
    fn scale_lanczos3_zero_dst_dimension_returns_white_fallback() {
        let src = Pixmap::white(10, 10);
        // dst_w=0 → Pixmap::white(max(0,1)=1, 5)
        let dst = scale_lanczos3(&src, 0, 5);
        assert_eq!(dst.width, 1);
        assert_eq!(dst.height, 5);
        // dst_h=0 → Pixmap::white(8, max(0,1)=1)
        let dst2 = scale_lanczos3(&src, 8, 0);
        assert_eq!(dst2.width, 8);
        assert_eq!(dst2.height, 1);
    }
}
