//! The inverse discrete wavelet transform (Annex F), 5/3 reversible and 9/7 irreversible.
//!
//! Reduced-resolution decoding lives here: to produce the image at resolution level `r` we
//! stop after `r` reconstruction steps. The subbands above were never decoded, so each
//! skipped level removes three quarters of the remaining coefficients AND all of the tier-1
//! work that would have produced them. That is the whole reason this module exists.
//!
//! Follows the spec's 2D_SR shape literally: interleave the four subbands onto the reference
//! grid, filter every row, then filter every column. Doing it in that order (rather than the
//! fused thing it is tempting to write) is what keeps odd image and tile offsets aligned —
//! a half-sample error per level compounds into a visible smear over six levels.

/// One subband's coefficients on its own grid.
#[derive(Clone)]
pub(super) struct SubBand {
    pub w: usize,
    pub h: usize,
    pub data: Vec<f32>,
}

impl SubBand {
    pub fn empty(w: usize, h: usize) -> Self {
        SubBand {
            w,
            h,
            data: vec![0.0; w.saturating_mul(h)],
        }
    }
    #[inline]
    fn get(&self, x: usize, y: usize) -> f32 {
        if x < self.w && y < self.h {
            self.data[y * self.w + x]
        } else {
            0.0
        }
    }
}

/// 9/7 irreversible lifting coefficients (Table F.4).
const ALPHA: f32 = -1.586_134_3;
const BETA: f32 = -0.052_980_118;
const GAMMA: f32 = 0.882_911_1;
const DELTA: f32 = 0.443_506_85;
const K: f32 = 1.230_174_1;

/// Symmetric (whole-sample) extension of `v` at absolute position `i`, where `v[0]` sits at
/// absolute `i0`. This is PSE from Annex F; without it every subband edge ripples.
#[inline]
fn ext(v: &[f32], i0: isize, i: isize) -> f32 {
    let n = v.len() as isize;
    if n <= 0 {
        return 0.0;
    }
    if n == 1 {
        return v[0];
    }
    let mut k = i - i0;
    let period = 2 * (n - 1);
    k = k.rem_euclid(period);
    if k >= n {
        k = period - k;
    }
    v[k as usize]
}

/// 1D inverse lifting on an already-interleaved signal whose first sample is at absolute
/// index `i0`. Parity of the absolute index decides low-pass (even) vs high-pass (odd).
fn filtr_1d(sig: &mut [f32], i0: usize, reversible: bool) {
    let n = sig.len();
    if n == 0 {
        return;
    }
    let i0i = i0 as isize;
    if n == 1 {
        // A single sample is low-pass if it sits on an even index, else it is a lone
        // high-pass coefficient and carries half its value (F.3.7).
        if i0 % 2 != 0 {
            sig[0] /= 2.0;
        }
        return;
    }

    if reversible {
        // 5/3: even samples first, then odd, using the just-updated evens.
        let src = sig.to_vec();
        let mut even = src.clone();
        for (k, slot) in even.iter_mut().enumerate() {
            let i = i0i + k as isize;
            if i % 2 == 0 {
                let a = ext(&src, i0i, i - 1);
                let b = ext(&src, i0i, i + 1);
                *slot = src[k] - ((a + b + 2.0) / 4.0).floor();
            }
        }
        let mut out = even.clone();
        for (k, slot) in out.iter_mut().enumerate() {
            let i = i0i + k as isize;
            if i % 2 != 0 {
                let a = ext(&even, i0i, i - 1);
                let b = ext(&even, i0i, i + 1);
                *slot = src[k] + ((a + b) / 2.0).floor();
            }
        }
        sig.copy_from_slice(&out);
    } else {
        // 9/7: undo the K scaling, then the four lifting steps in reverse.
        for (k, s) in sig.iter_mut().enumerate() {
            let i = i0i + k as isize;
            *s = if i % 2 == 0 { *s * K } else { *s / K };
        }
        for (even_step, coef) in [(true, DELTA), (false, GAMMA), (true, BETA), (false, ALPHA)] {
            let src = sig.to_vec();
            for (k, s) in sig.iter_mut().enumerate() {
                let i = i0i + k as isize;
                if (i % 2 == 0) == even_step {
                    let a = ext(&src, i0i, i - 1);
                    let b = ext(&src, i0i, i + 1);
                    *s = src[k] - coef * (a + b);
                }
            }
        }
    }
}

/// Reconstruct one resolution level from an LL band plus its HL/LH/HH detail bands.
///
/// `(x0, y0)` is the top-left of the OUTPUT on the reference grid. The output size is
/// derived from it and the band sizes, so a tile whose origin is odd still lands correctly.
pub(super) fn reconstruct(
    ll: &SubBand,
    hl: &SubBand,
    lh: &SubBand,
    hh: &SubBand,
    x0: usize,
    y0: usize,
    reversible: bool,
) -> SubBand {
    let w = ll.w + hl.w;
    let h = ll.h + lh.h;
    let mut out = SubBand::empty(w, h);
    if w == 0 || h == 0 {
        return out;
    }

    // 2D_INTERLEAVE (F.3.3): even/even from LL, odd/even from HL, even/odd from LH,
    // odd/odd from HH, indexed relative to each band's own origin.
    for y in 0..h {
        let gy = y0 + y;
        let by = if gy % 2 == 0 {
            gy / 2 - y0.div_ceil(2)
        } else {
            gy / 2 - y0 / 2
        };
        for x in 0..w {
            let gx = x0 + x;
            let bx = if gx % 2 == 0 {
                gx / 2 - x0.div_ceil(2)
            } else {
                gx / 2 - x0 / 2
            };
            let v = match (gx % 2 == 0, gy % 2 == 0) {
                (true, true) => ll.get(bx, by),
                (false, true) => hl.get(bx, by),
                (true, false) => lh.get(bx, by),
                (false, false) => hh.get(bx, by),
            };
            out.data[y * w + x] = v;
        }
    }

    // HOR_SR then VER_SR.
    let mut row = vec![0.0f32; w];
    for y in 0..h {
        row.copy_from_slice(&out.data[y * w..(y + 1) * w]);
        filtr_1d(&mut row, x0, reversible);
        out.data[y * w..(y + 1) * w].copy_from_slice(&row);
    }
    let mut col = vec![0.0f32; h];
    for x in 0..w {
        for (y, c) in col.iter_mut().enumerate() {
            *c = out.data[y * w + x];
        }
        filtr_1d(&mut col, y0, reversible);
        for (y, c) in col.iter().enumerate() {
            out.data[y * w + x] = *c;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A flat image must survive a round of reconstruction unchanged: all detail bands are
    /// zero, so the LL value should propagate to every output sample. This catches the
    /// interleave-parity bugs that otherwise show up only as a subtly smeared thumbnail.
    #[test]
    fn flat_ll_reconstructs_flat() {
        for &(x0, y0) in &[(0usize, 0usize), (1, 0), (0, 1), (3, 5)] {
            for &reversible in &[true, false] {
                let ll = SubBand {
                    w: 4,
                    h: 4,
                    data: vec![100.0; 16],
                };
                let (hl, lh, hh) = (
                    SubBand::empty(4, 4),
                    SubBand::empty(4, 4),
                    SubBand::empty(4, 4),
                );
                let out = reconstruct(&ll, &hl, &lh, &hh, x0, y0, reversible);
                assert_eq!(out.w, 8);
                assert_eq!(out.h, 8);
                for (i, v) in out.data.iter().enumerate() {
                    assert!(
                        (*v - 100.0).abs() < 1.5,
                        "flat reconstruction drifted at {i}: {v} (origin {x0},{y0}, rev={reversible})"
                    );
                }
            }
        }
    }
}
