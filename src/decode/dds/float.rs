//! HDR surfaces (BC6H, half and float channels, R11G11B10, RGB9E5) to linear RGB32F.

use super::masks::each_pixel;
use super::*;

/// The float layouts decode to linear `Rgb32F`, which the caller tone-maps through
/// the same Reinhard + sRGB transfer EXR and Radiance HDR already use — so a BC6H
/// texture thumbnails on the compact (no-ImageMagick) install too.
pub(super) fn decode_float(bytes: &[u8], s: &Surface) -> Result<DynamicImage> {
    let src = surface(bytes, s)?;
    let len = out_buffer(s.width, s.height, 3 * 4)? / 4;
    let mut out = vec![0f32; len];
    match s.layout {
        Layout::Block(Block::Bc6h { signed }) => {
            blocks_bc6h(src, s.width, s.height, signed, &mut out)
        }
        Layout::Half(n) => half_rgb32f(src, s.width, s.height, n, &mut out),
        Layout::Float(n) => float_rgb32f(src, s.width, s.height, n, &mut out),
        Layout::R11G11B10 => r11g11b10_rgb32f(src, s.width, s.height, &mut out),
        Layout::Rgb9E5 => rgb9e5_rgb32f(src, s.width, s.height, &mut out),
        _ => return Err(fail("non-float layout on the HDR path")),
    }
    image::Rgb32FImage::from_raw(s.width, s.height, out)
        .map(DynamicImage::ImageRgb32F)
        .ok_or_else(|| fail("buffer size mismatch"))
}

pub(super) fn blocks_bc6h(src: &[u8], width: u32, height: u32, signed: bool, out: &mut [f32]) {
    let bw = width.div_ceil(4) as usize;
    let bh = height.div_ceil(4) as usize;
    let row = width as usize * 3;
    let mut tile = [0f32; 4 * 4 * 3];
    for by in 0..bh {
        for bx in 0..bw {
            let off = (by * bw + bx) * 16;
            let Some(blk) = src.get(off..off + 16) else {
                return;
            };
            bcdec_rs::bc6h_float(blk, &mut tile, 12, signed);
            let px = bx * 4;
            let py = by * 4;
            let w = 4.min(width as usize - px);
            let h = 4.min(height as usize - py);
            for y in 0..h {
                let s = y * 12;
                let d = (py + y) * row + px * 3;
                let n = w * 3;
                if let (Some(sl), Some(dl)) = (tile.get(s..s + n), out.get_mut(d..d + n)) {
                    dl.copy_from_slice(sl);
                }
            }
        }
    }
}

/// Reconstruct an IEEE half. `f16::from_bits` is not stable on this toolchain and
/// the `half` crate is not in the tree, so this is the classic shift-and-fix-up.
pub(super) fn half_to_f32(h: u16) -> f32 {
    let sign = ((h >> 15) & 1) as u32;
    let exp = ((h >> 10) & 0x1F) as u32;
    let man = (h & 0x3FF) as u32;
    let bits = match exp {
        0 if man == 0 => sign << 31,
        // Subnormal: shift the mantissa up until the implicit 1 appears, then bias
        // the exponent by however many shifts that took (113 is 127 - 24 + 10).
        0 => {
            let mut shifts = 0u32;
            let mut m = man;
            while m & 0x400 == 0 {
                m <<= 1;
                shifts += 1;
            }
            (sign << 31) | ((113 - shifts) << 23) | ((m & 0x3FF) << 13)
        }
        // Inf / NaN.
        31 => (sign << 31) | (0xFF << 23) | (man << 13),
        _ => (sign << 31) | ((exp + 127 - 15) << 23) | (man << 13),
    };
    f32::from_bits(bits)
}

pub(super) fn half_rgb32f(src: &[u8], width: u32, height: u32, channels: u8, out: &mut [f32]) {
    let n = channels as usize;
    each_pixel(src, width as usize * height as usize, n * 2, |i, px| {
        let mut c = [0f32; 3];
        for (k, slot) in c.iter_mut().enumerate().take(n.min(3)) {
            *slot = half_to_f32(u16::from_le_bytes([px[k * 2], px[k * 2 + 1]]));
        }
        write_rgb(out, i, n, c);
    });
}

pub(super) fn float_rgb32f(src: &[u8], width: u32, height: u32, channels: u8, out: &mut [f32]) {
    let n = channels as usize;
    each_pixel(src, width as usize * height as usize, n * 4, |i, px| {
        let mut c = [0f32; 3];
        for (k, slot) in c.iter_mut().enumerate().take(n.min(3)) {
            let b = &px[k * 4..k * 4 + 4];
            *slot = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        }
        write_rgb(out, i, n, c);
    });
}

/// 11/11/10 unsigned floats (5-bit exponents, no sign) packed into 32 bits.
pub(super) fn r11g11b10_rgb32f(src: &[u8], width: u32, height: u32, out: &mut [f32]) {
    let unpack = |bits: u32, mantissa_bits: u32| -> f32 {
        let exp = bits >> mantissa_bits;
        let man = bits & ((1 << mantissa_bits) - 1);
        let scale = 1.0 / (1u32 << mantissa_bits) as f32;
        match exp {
            0 => man as f32 * scale * (2f32).powi(-14),
            31 => f32::INFINITY,
            _ => (1.0 + man as f32 * scale) * (2f32).powi(exp as i32 - 15),
        }
    };
    each_pixel(src, width as usize * height as usize, 4, |i, px| {
        let v = u32::from_le_bytes([px[0], px[1], px[2], px[3]]);
        write_rgb(
            out,
            i,
            3,
            [
                unpack(v & 0x7FF, 6),
                unpack((v >> 11) & 0x7FF, 6),
                unpack((v >> 22) & 0x3FF, 5),
            ],
        );
    });
}

/// Three 9-bit mantissas sharing one 5-bit exponent.
pub(super) fn rgb9e5_rgb32f(src: &[u8], width: u32, height: u32, out: &mut [f32]) {
    each_pixel(src, width as usize * height as usize, 4, |i, px| {
        let v = u32::from_le_bytes([px[0], px[1], px[2], px[3]]);
        // exponent bias 15, mantissa denominator 2^9
        let scale = (2f32).powi(((v >> 27) & 0x1F) as i32 - 15 - 9);
        write_rgb(
            out,
            i,
            3,
            [
                (v & 0x1FF) as f32 * scale,
                ((v >> 9) & 0x1FF) as f32 * scale,
                ((v >> 18) & 0x1FF) as f32 * scale,
            ],
        );
    });
}

/// Expand an `n`-channel float pixel to RGB: 1 → grey, 2 → R,G,0, else RGB.
pub(super) fn write_rgb(out: &mut [f32], i: usize, n: usize, c: [f32; 3]) {
    let px = match n {
        1 => [c[0], c[0], c[0]],
        2 => [c[0], c[1], 0.0],
        _ => c,
    };
    if let Some(d) = out.get_mut(i * 3..i * 3 + 3) {
        d.copy_from_slice(&px);
    }
}
