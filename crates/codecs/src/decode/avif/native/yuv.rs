//! Y'CbCr to R'G'B': the matrix and range an AVIF declares, applied to a decoded AV1 picture.
//! The output keeps the file's own transfer and primaries; turning those into sRGB (an ICC
//! profile, or the HDR tone map) is the caller's step, shared with every other format.

use super::av1::{Frame, Layout};

/// How the three planes combine into R'G'B' (ITU-T H.273 `MatrixCoefficients`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Matrix {
    /// 0: the planes ARE G, B, R.
    Identity,
    /// 8: YCgCo.
    YCgCo,
    /// Every luma/colour-difference matrix, by its two weights.
    Weights { kr: f32, kb: f32 },
}

/// The matrix for code point `mc`. Unspecified (2) and anything this does not model read as
/// BT.601, which is what libavif does and what an encoder that writes no matrix meant.
pub(super) fn matrix_for(mc: u16) -> Matrix {
    let weights = |kr, kb| Matrix::Weights { kr, kb };
    match mc {
        0 => Matrix::Identity,
        1 => weights(0.2126, 0.0722),
        4 => weights(0.30, 0.11),
        7 => weights(0.212, 0.087),
        8 => Matrix::YCgCo,
        9 | 10 => weights(0.2627, 0.0593),
        _ => weights(0.299, 0.114),
    }
}

/// The sample scaling of one picture: offsets and spans for luma and chroma at its bit depth.
#[derive(Clone, Copy)]
struct Scale {
    y_off: f32,
    y_span: f32,
    c_mid: f32,
    c_span: f32,
}

impl Scale {
    fn new(bits: u8, full_range: bool) -> Self {
        let max = ((1u32 << bits) - 1) as f32;
        let unit = (1u32 << bits.saturating_sub(8)) as f32;
        let c_mid = (1u32 << (bits - 1)) as f32;
        if full_range {
            Scale {
                y_off: 0.0,
                y_span: max,
                c_mid,
                c_span: max,
            }
        } else {
            Scale {
                y_off: 16.0 * unit,
                y_span: 219.0 * unit,
                c_mid,
                c_span: 224.0 * unit,
            }
        }
    }
}

#[inline]
fn to_rgb(m: Matrix, y: f32, cb: f32, cr: f32) -> [f32; 3] {
    match m {
        Matrix::Identity => [cr + 0.5, y, cb + 0.5],
        Matrix::YCgCo => {
            let t = y - cb;
            [t + cr, y + cb, t - cr]
        }
        Matrix::Weights { kr, kb } => {
            let r = y + 2.0 * (1.0 - kr) * cr;
            let b = y + 2.0 * (1.0 - kb) * cb;
            let g = (y - kr * r - kb * b) / (1.0 - kr - kb);
            [r, g, b]
        }
    }
}

/// Call `put(x, y, [r, g, b])` for every pixel of `frame` reduced by `step` (1 = every pixel;
/// `n` = one output pixel per `n` x `n` block, the block's average), each channel in 0..=1
/// (clamped). At full size chroma is taken from the sample covering the pixel (nearest), which
/// a thumbnail cannot tell from interpolation; reduced, luma and chroma are both averaged over
/// the block before the matrix, which is exact because the matrix is linear.
pub(super) fn convert(
    frame: &Frame,
    matrix: Matrix,
    full_range: bool,
    step: u32,
    mut put: impl FnMut(u32, u32, [f32; 3]),
) {
    let scale = Scale::new(frame.bits, full_range);
    // The identity matrix scales every plane like luma: they are R, G and B.
    let chroma = |s: &Scale, v: f32| {
        if matrix == Matrix::Identity {
            (v - s.y_off) / s.y_span - 0.5
        } else {
            (v - s.c_mid) / s.c_span
        }
    };
    let luma = |s: &Scale, v: f32| (v - s.y_off) / s.y_span;
    let (sx, sy) = match frame.layout {
        Layout::I420 => (1, 1),
        Layout::I422 => (1, 0),
        _ => (0, 0),
    };
    let step = step.max(1);
    for oy in 0..frame.height.div_ceil(step) {
        let (y0, y1) = (oy * step, ((oy + 1) * step).min(frame.height));
        let lrows: Vec<_> = (y0..y1).map(|y| frame.row(0, y)).collect();
        let crows = if frame.layout == Layout::Mono {
            Vec::new()
        } else {
            ((y0 >> sy)..=((y1 - 1) >> sy))
                .map(|cy| (frame.row(1, cy), frame.row(2, cy)))
                .collect()
        };
        for ox in 0..frame.width.div_ceil(step) {
            let (x0, x1) = (ox * step, ((ox + 1) * step).min(frame.width));
            let (mut sum, mut n) = (0f32, 0f32);
            for r in &lrows {
                for x in x0..x1 {
                    sum += f32::from(r.get(x as usize));
                }
                n += (x1 - x0) as f32;
            }
            let yv = luma(&scale, sum / n);
            if crows.is_empty() {
                let v = yv.clamp(0.0, 1.0);
                put(ox, oy, [v, v, v]);
                continue;
            }
            let (cx0, cx1) = (x0 >> sx, (x1 - 1) >> sx);
            let (mut su, mut sv, mut cn) = (0f32, 0f32, 0f32);
            for (u, v) in &crows {
                for cx in cx0..=cx1 {
                    su += f32::from(u.get(cx as usize));
                    sv += f32::from(v.get(cx as usize));
                }
                cn += (cx1 - cx0 + 1) as f32;
            }
            let rgb = to_rgb(matrix, yv, chroma(&scale, su / cn), chroma(&scale, sv / cn));
            put(ox, oy, rgb.map(|ch| ch.clamp(0.0, 1.0)));
        }
    }
}

/// Call `put(x, y, a)` for every sample of an alpha picture (its luma plane) reduced by `step`
/// the same way [`convert`] reduces colour, in 0..=1.
pub(super) fn convert_alpha(
    frame: &Frame,
    full_range: bool,
    step: u32,
    mut put: impl FnMut(u32, u32, f32),
) {
    let scale = Scale::new(frame.bits, full_range);
    let step = step.max(1);
    for oy in 0..frame.height.div_ceil(step) {
        let (y0, y1) = (oy * step, ((oy + 1) * step).min(frame.height));
        let rows: Vec<_> = (y0..y1).map(|y| frame.row(0, y)).collect();
        for ox in 0..frame.width.div_ceil(step) {
            let (x0, x1) = (ox * step, ((ox + 1) * step).min(frame.width));
            let (mut sum, mut n) = (0f32, 0f32);
            for r in &rows {
                for x in x0..x1 {
                    sum += f32::from(r.get(x as usize));
                }
                n += (x1 - x0) as f32;
            }
            put(
                ox,
                oy,
                ((sum / n - scale.y_off) / scale.y_span).clamp(0.0, 1.0),
            );
        }
    }
}
