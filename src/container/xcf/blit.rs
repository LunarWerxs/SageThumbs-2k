//! From decoded tile samples to canvas pixels: the scaled blit, precision conversion and compositing.

use super::*;

/// One decoded tile: its interleaved sample bytes plus the sample format they are stored in.
///
/// Every per-pixel path below ([`blit_tile`], [`blit_tile_scaled`], [`accumulate_cell`]) needs
/// exactly these, so they travel as one value instead of being repeated in each parameter list.
pub(super) struct TileSamples<'a> {
    /// Tile bytes, `tw * th * bpp` long.
    pub(super) buf: &'a [u8],
    /// Bytes per pixel (all channels interleaved).
    pub(super) bpp: u32,
    /// Bytes per sample.
    pub(super) bps: u32,
    /// XCF layer type: 0 RGB, 1 RGBA, 2 GRAY, 3 GRAYA, 4 INDEXED, 5 INDEXEDA.
    pub(super) ltype: u32,
    /// Sample precision.
    pub(super) prec: Precision,
    /// Palette entries, used by the indexed layer types.
    pub(super) colormap: &'a [[u8; 3]],
}

/// Convert a decoded tile's interleaved samples to RGBA8 and paint it into `out`.
pub(super) fn blit_tile(
    out: &mut RgbaImage,
    tile: &TileSamples<'_>,
    tx: u32,
    ty: u32,
    tw: u32,
    th: u32,
) {
    let buf = tile.buf;
    let bpp = tile.bpp as usize;
    let bps = tile.bps as usize;
    for row in 0..th {
        for col in 0..tw {
            let pi = (row * tw + col) as usize * bpp;
            let Some(px) = buf.get(pi..pi + bpp) else {
                continue;
            };
            let rgba = sample_to_rgba(px, bps, tile.ltype, tile.prec, tile.colormap);
            out.put_pixel(tx + col, ty + row, image::Rgba(rgba));
        }
    }
}

/// Taps per axis inside one output cell. Sixteen samples per pixel is a good box filter and a
/// bounded one: at step 23 a full box would read 529 source pixels per output pixel, which is
/// the cost this whole path exists to avoid, while point-sampling a single one aliases badly
/// on exactly the detailed images people notice. Cells smaller than this sample every pixel.
pub(super) const MAX_TAPS: u32 = 4;

/// [`blit_tile`] onto a grid reduced by `step`, accumulating premultiplied sums.
///
/// It iterates OUTPUT cells and reaches back for taps, rather than iterating source pixels and
/// mapping them forward. That is the whole saving: the work becomes proportional to the tile's
/// footprint in the output (about 3x3 cells at step 23) times [`MAX_TAPS`] squared, instead of
/// to the tile's 4096 pixels.
#[allow(
    clippy::too_many_arguments,
    reason = "one per already-threaded caller value"
)]
pub(super) fn blit_tile_scaled(
    acc: &mut [[u32; 5]],
    rw: u32,
    rh: u32,
    tile: &TileSamples<'_>,
    tx: u32,
    ty: u32,
    tw: u32,
    th: u32,
    step: u32,
) {
    let (cx0, cx1) = (tx / step, (tx + tw - 1) / step);
    let (cy0, cy1) = (ty / step, (ty + th - 1) / step);
    for cy in cy0..=cy1.min(rh.saturating_sub(1)) {
        // This cell's source rows, clipped to the tile we actually hold.
        let sy0 = (cy * step).max(ty);
        let sy1 = ((cy + 1) * step).min(ty + th);
        if sy0 >= sy1 {
            continue;
        }
        for cx in cx0..=cx1.min(rw.saturating_sub(1)) {
            accumulate_cell(acc, rw, cx, cy, sy0, sy1, tile, tx, ty, tw, step);
        }
    }
}

/// Accumulate one output cell's premultiplied colour sums from evenly spaced taps across the
/// cell's source span, clipped to the tile we actually hold.
#[allow(clippy::too_many_arguments)]
pub(super) fn accumulate_cell(
    acc: &mut [[u32; 5]],
    rw: u32,
    cx: u32,
    cy: u32,
    sy0: u32,
    sy1: u32,
    tile: &TileSamples<'_>,
    tx: u32,
    ty: u32,
    tw: u32,
    step: u32,
) {
    let buf = tile.buf;
    let bpp = tile.bpp as usize;
    let bps = tile.bps as usize;
    let span_y = sy1 - sy0;
    let ny = span_y.min(MAX_TAPS);
    let sx0 = (cx * step).max(tx);
    let sx1 = ((cx + 1) * step).min(tx + tw);
    if sx0 >= sx1 {
        return;
    }
    let span_x = sx1 - sx0;
    let nx = span_x.min(MAX_TAPS);
    let Some(cell) = acc.get_mut((cy as usize) * (rw as usize) + cx as usize) else {
        return;
    };
    for j in 0..ny {
        // Evenly spaced across the span rather than the first N, so a cell that
        // straddles a tile edge still samples the whole width it covers.
        let sy = sy0 + span_y * j / ny;
        for i in 0..nx {
            let sx = sx0 + span_x * i / nx;
            let pi = ((sy - ty) as usize * tw as usize + (sx - tx) as usize) * bpp;
            let Some(px) = buf.get(pi..pi + bpp) else {
                continue;
            };
            let rgba = sample_to_rgba(px, bps, tile.ltype, tile.prec, tile.colormap);
            let a = u32::from(rgba[3]);
            cell[0] += u32::from(rgba[0]) * a;
            cell[1] += u32::from(rgba[1]) * a;
            cell[2] += u32::from(rgba[2]) * a;
            cell[3] += a;
            cell[4] += 1;
        }
    }
}

/// One pixel's raw sample bytes → RGBA8, per layer type + precision (+ colormap for indexed).
pub(super) fn sample_to_rgba(
    px: &[u8],
    bps: usize,
    ltype: u32,
    prec: Precision,
    colormap: &[[u8; 3]],
) -> [u8; 4] {
    // Read the nth channel's sample and normalize to [0,1]; color channels get sRGB applied
    // when the file stores LINEAR light (alpha is always linear, never transformed).
    let chan = |n: usize, is_color: bool| -> f32 {
        let s = px
            .get(n * bps..n * bps + bps)
            .map(|b| prec.normalize(b))
            .unwrap_or(0.0);
        if is_color && prec.linear {
            linear_to_srgb(s)
        } else {
            s
        }
    };
    let to8 = |x: f32| (x.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;

    match ltype {
        0 => [
            to8(chan(0, true)),
            to8(chan(1, true)),
            to8(chan(2, true)),
            255,
        ], // RGB
        1 => [
            to8(chan(0, true)),
            to8(chan(1, true)),
            to8(chan(2, true)),
            to8(chan(3, false)),
        ], // RGBA
        2 => {
            let g = to8(chan(0, true));
            [g, g, g, 255]
        }
        3 => {
            let g = to8(chan(0, true));
            [g, g, g, to8(chan(1, false))]
        }
        4 | 5 => {
            // Indexed: sample 0 is a raw palette index (1 byte); IndexedA adds an alpha byte.
            let idx = *px.first().unwrap_or(&0) as usize;
            let [r, g, b] = colormap.get(idx).copied().unwrap_or([0, 0, 0]);
            let a = if ltype == 5 {
                *px.get(bps).unwrap_or(&255)
            } else {
                255
            };
            [r, g, b, a]
        }
        _ => [0, 0, 0, 0],
    }
}

/// Alpha-composite `layer` over `canvas` (NORMAL mode) at the layer's offset, scaling the
/// source alpha by the layer opacity. Straight (non-premultiplied) over.
pub(super) fn composite(canvas: &mut RgbaImage, layer: &Layer) {
    let (cw, ch) = (canvas.width() as i64, canvas.height() as i64);
    let src = &layer.px;
    for sy in 0..src.height() {
        let dy = layer.oy as i64 + sy as i64;
        if dy < 0 || dy >= ch {
            continue;
        }
        for sx in 0..src.width() {
            let dx = layer.ox as i64 + sx as i64;
            if dx < 0 || dx >= cw {
                continue;
            }
            let s = src.get_pixel(sx, sy).0;
            let sa = (s[3] as f32 / 255.0) * layer.opacity;
            if sa <= 0.0 {
                continue;
            }
            blend_pixel(canvas, dx as u32, dy as u32, s, sa);
        }
    }
}

/// Blend source pixel `s` with premultiply-alpha `sa` over the existing canvas pixel at
/// (`dx`, `dy`), writing the straight-alpha result back in place.
pub(super) fn blend_pixel(canvas: &mut RgbaImage, dx: u32, dy: u32, s: [u8; 4], sa: f32) {
    let d = canvas.get_pixel(dx, dy).0;
    let da = d[3] as f32 / 255.0;
    let oa = sa + da * (1.0 - sa);
    if oa <= 0.0 {
        return;
    }
    let mix = |sc: u8, dc: u8| -> u8 {
        let s = sc as f32 / 255.0;
        let dd = dc as f32 / 255.0;
        let o = (s * sa + dd * da * (1.0 - sa)) / oa;
        (o.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
    };
    canvas.put_pixel(
        dx,
        dy,
        image::Rgba([
            mix(s[0], d[0]),
            mix(s[1], d[1]),
            mix(s[2], d[2]),
            (oa * 255.0 + 0.5) as u8,
        ]),
    );
}

/// Precision descriptor: how wide a sample is, whether it's float, and whether the stored
/// values are linear-light (needing sRGB encoding for display) or already perceptual.
#[derive(Clone, Copy)]
pub(super) struct Precision {
    pub(super) float: bool,
    pub(super) linear: bool,
}

impl Precision {
    /// Map an XCF precision word to (float?, linear?). v7+ uses the 100..=750 scheme; older
    /// files (or unknown words) are treated as 8-bit perceptual, which is the common case.
    pub(super) fn from_word(w: u32) -> Self {
        // Linear codes end in 00, perceptual/gamma codes end in 50. Float starts at 500.
        let linear = w.is_multiple_of(100) && w >= 100;
        let float = w >= 500;
        Precision { float, linear }
    }

    /// Normalize one sample's bytes (big-endian) to [0,1]. `bps` = bytes per sample.
    pub(super) fn normalize(self, b: &[u8]) -> f32 {
        if self.float {
            match b.len() {
                2 => half_to_f32(u16::from_be_bytes([b[0], b[1]])).clamp(0.0, 1.0),
                4 => f32::from_be_bytes([b[0], b[1], b[2], b[3]]).clamp(0.0, 1.0),
                8 => f64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
                    .clamp(0.0, 1.0) as f32,
                _ => 0.0,
            }
        } else {
            match b.len() {
                1 => b[0] as f32 / 255.0,
                2 => u16::from_be_bytes([b[0], b[1]]) as f32 / 65535.0,
                4 => u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as f32 / 4294967295.0,
                _ => 0.0,
            }
        }
    }
}

/// Standard linear-light → sRGB transfer (for files stored in a linear precision).
pub(super) fn linear_to_srgb(x: f32) -> f32 {
    if x <= 0.0031308 {
        x * 12.92
    } else {
        1.055 * x.powf(1.0 / 2.4) - 0.055
    }
}

/// Minimal IEEE half-precision → f32 (for the 16-bit-float XCF precisions).
pub(super) fn half_to_f32(h: u16) -> f32 {
    let sign = (h >> 15) & 1;
    let exp = (h >> 10) & 0x1f;
    let mant = h & 0x3ff;
    let f = match exp {
        0 => (mant as f32) * 2f32.powi(-24),
        0x1f => {
            if mant == 0 {
                f32::INFINITY
            } else {
                f32::NAN
            }
        }
        _ => (1.0 + mant as f32 / 1024.0) * 2f32.powi(exp as i32 - 15),
    };
    if sign == 1 {
        -f
    } else {
        f
    }
}
