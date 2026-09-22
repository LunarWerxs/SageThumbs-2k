//! Uncompressed surfaces - bit-masked, SNORM and UNORM16 channels - to RGBA8, and the alpha-mode fix-ups.

use super::*;

pub(super) fn masks_rgba8(src: &[u8], width: u32, height: u32, m: Masks, out: &mut [u8]) {
    let pitch = (width as usize * m.bpp as usize).div_ceil(8);
    let step = (m.bpp / 8) as usize;
    let ch = [
        Channel::new(m.r),
        Channel::new(m.g),
        Channel::new(m.b),
        Channel::new(m.a),
    ];
    for y in 0..height as usize {
        let line = y * pitch;
        for x in 0..width as usize {
            let Some(px) = src.get(line + x * step..line + x * step + step) else {
                return;
            };
            let p = mask_pixel(px, &ch, m);
            let dst = (y * width as usize + x) * 4;
            if let Some(d) = out.get_mut(dst..dst + 4) {
                d.copy_from_slice(&p);
            }
        }
    }
}

/// Unpack one masked pixel from its little-endian bytes into RGBA.
pub(super) fn mask_pixel(px: &[u8], ch: &[Channel; 4], m: Masks) -> [u8; 4] {
    let mut v = 0u32;
    for (i, b) in px.iter().enumerate() {
        v |= (*b as u32) << (8 * i);
    }
    let r = ch[0].get(v);
    let (g, b) = if m.grey {
        (r, r)
    } else {
        (ch[1].get(v), ch[2].get(v))
    };
    let a = if m.a == 0 { 255 } else { ch[3].get(v) };
    [r, g, b, a]
}

/// One bit-mask channel, pre-resolved to a shift and a scale so the per-pixel loop
/// stays cheap.
#[derive(Clone, Copy)]
pub(super) struct Channel {
    pub(super) shift: u32,
    pub(super) mask: u32,
    /// Number of bits the channel occupies (0 = channel absent).
    pub(super) bits: u32,
}

impl Channel {
    pub(super) fn new(mask: u32) -> Self {
        if mask == 0 {
            return Self {
                shift: 0,
                mask: 0,
                bits: 0,
            };
        }
        let shift = mask.trailing_zeros();
        Self {
            shift,
            mask,
            bits: (mask >> shift).count_ones(),
        }
    }

    /// Extract and scale to 8-bit. Narrow channels are bit-replicated (5-bit 31 →
    /// 255, not 248) so a 565 texture reaches full white.
    pub(super) fn get(self, v: u32) -> u8 {
        if self.bits == 0 {
            return 0;
        }
        let raw = (v & self.mask) >> self.shift;
        if self.bits >= 8 {
            (raw >> (self.bits - 8)) as u8
        } else {
            let max = (1u32 << self.bits) - 1;
            ((raw * 255 + max / 2) / max) as u8
        }
    }
}

/// Walk `count` pixels of `step` bytes each, handing `f` the pixel index and its bytes.
/// A pixel that runs past the end of `src` stops the walk, so a surface that is present
/// only in part renders what it has instead of failing.
pub(super) fn each_pixel(src: &[u8], count: usize, step: usize, mut f: impl FnMut(usize, &[u8])) {
    for i in 0..count {
        let Some(px) = src.get(i * step..i * step + step) else {
            return;
        };
        f(i, px);
    }
}

/// Signed-normalized integer channels, remapped from [-1, 1] to [0, 255] so the
/// negative half is visible rather than clamped flat.
pub(super) fn snorm_rgba8(
    src: &[u8],
    width: u32,
    height: u32,
    channels: u8,
    size: usize,
    out: &mut [u8],
) {
    let n = channels as usize;
    each_pixel(src, width as usize * height as usize, n * size, |i, px| {
        let mut c = [0u8; 4];
        for (k, slot) in c.iter_mut().enumerate().take(n) {
            let raw = if size == 1 {
                px[k] as i8 as f32 / 127.0
            } else {
                i16::from_le_bytes([px[k * 2], px[k * 2 + 1]]) as f32 / 32767.0
            };
            *slot = ((raw.clamp(-1.0, 1.0) * 0.5 + 0.5) * 255.0 + 0.5) as u8;
        }
        write_channels(out, i, n, c[0], c[1], c[2], c[3]);
    });
}

pub(super) fn unorm16_rgba8(src: &[u8], width: u32, height: u32, channels: u8, out: &mut [u8]) {
    let n = channels as usize;
    each_pixel(src, width as usize * height as usize, n * 2, |i, px| {
        let mut c = [0u8; 4];
        for (k, slot) in c.iter_mut().enumerate().take(n) {
            *slot = (u16::from_le_bytes([px[k * 2], px[k * 2 + 1]]) >> 8) as u8;
        }
        write_channels(out, i, n, c[0], c[1], c[2], c[3]);
    });
}

/// Expand an `n`-channel pixel to RGBA: 1 → grey, 2 → R,G,0, 3 → RGB, 4 → RGBA.
pub(super) fn write_channels(out: &mut [u8], i: usize, n: usize, c0: u8, c1: u8, c2: u8, c3: u8) {
    let px = match n {
        1 => [c0, c0, c0, 255],
        2 => [c0, c1, 0, 255],
        3 => [c0, c1, c2, 255],
        _ => [c0, c1, c2, c3],
    };
    if let Some(d) = out.get_mut(i * 4..i * 4 + 4) {
        d.copy_from_slice(&px);
    }
}

/// `DDS_HEADER_DXT10.miscFlags2` can declare the stored alpha premultiplied (undo
/// it, or every semi-transparent pixel renders too dark over the checkerboard) or
/// meaningless (force opaque, or the thumbnail is invisible).
pub(super) fn apply_alpha_mode(out: &mut [u8], mode: u32) {
    match mode {
        ALPHA_MODE_PREMULTIPLIED => crate::decode::svg::unpremultiply_rgba(out),
        ALPHA_MODE_OPAQUE => force_opaque_alpha(out),
        _ => {}
    }
}

/// Force every pixel opaque when the alpha channel is declared meaningless.
pub(super) fn force_opaque_alpha(out: &mut [u8]) {
    let (chunks, _) = out.as_chunks_mut::<4>();
    for px in chunks {
        px[3] = 255;
    }
}
