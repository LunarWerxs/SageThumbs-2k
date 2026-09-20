//! One tile: read, decompress (raw, RLE or zlib) and blit.

use super::*;

/// Read, decode, and blit (or accumulate, at `step > 1`) one tile into `out`/`acc`.
#[allow(
    clippy::too_many_arguments,
    reason = "one per already-threaded caller value"
)]
pub(super) fn decode_and_blit_tile<R: Read + Seek>(
    r: &mut R,
    head: &LayerHead,
    pro: &Prologue,
    tile_ptrs: &[u64],
    ti: usize,
    tptr: u64,
    tx: u32,
    ty: u32,
    tw: u32,
    th: u32,
    bpp: u32,
    bps: u32,
    step: u32,
    rw: u32,
    rh: u32,
    win: &mut Vec<u8>,
    scratch: &mut [u8],
    out: &mut RgbaImage,
    acc: &mut [[u32; 5]],
) -> Option<()> {
    let need = (tw * th * bpp) as usize;
    let window = tile_read_window(tile_ptrs, ti, tptr, need)?;
    read_at(r, tptr, window, win)?;
    let buf = scratch.get_mut(..need)?;
    decode_tile(win, 0, pro.compression, bpp, tw, th, buf)?;
    let tile = TileSamples {
        buf,
        bpp,
        bps,
        ltype: head.ltype,
        prec: pro.prec,
        colormap: &pro.colormap,
    };
    if step > 1 {
        blit_tile_scaled(acc, rw, rh, &tile, tx, ty, tw, th, step);
    } else {
        blit_tile(out, &tile, tx, ty, tw, th);
    }
    Some(())
}

/// Turn the premultiplied sums back into straight RGBA8.
///
/// Averaging STRAIGHT (non-premultiplied) colour is the classic edge artefact: a transparent
/// pixel still carries some colour, and letting it vote pulls a halo into everything next to
/// it. Summing `colour * alpha` and dividing by the summed alpha is the correct weighting,
/// which matters here because the whole point of this path is compositing layers with alpha.
pub(super) fn resolve_accumulator(out: &mut RgbaImage, acc: &[[u32; 5]]) {
    for (px, cell) in out.pixels_mut().zip(acc) {
        let taps = cell[4];
        if taps == 0 {
            *px = image::Rgba([0, 0, 0, 0]);
            continue;
        }
        let alpha_sum = cell[3];
        let a = (alpha_sum / taps).min(255) as u8;
        let chan = |sum: u32| -> u8 {
            // sum is Σ(colour*alpha); dividing by Σalpha un-premultiplies in one step.
            // `checked_div` covers the fully-transparent cell, where the sum is 0 too.
            (sum + alpha_sum / 2)
                .checked_div(alpha_sum)
                .unwrap_or(0)
                .min(255) as u8
        };
        *px = image::Rgba([chan(cell[0]), chan(cell[1]), chan(cell[2]), a]);
    }
}

/// Fill `dest` (tw*th*bpp bytes) with a tile's channel-interleaved, big-endian-sample
/// pixels, whatever the compression. NONE = raw; RLE = `bpp` byte-planes deinterleaved;
/// ZLIB = whole-tile zlib of the raw (already-interleaved) bytes.
pub(super) fn decode_tile(
    d: &[u8],
    off: usize,
    compression: u8,
    bpp: u32,
    tw: u32,
    th: u32,
    dest: &mut [u8],
) -> Option<()> {
    match compression {
        0 => decode_tile_raw(d, off, dest), // COMPRESS_NONE
        1 => decode_rle(d, off, bpp as usize, (tw * th) as usize, dest),
        2 => decode_tile_zlib(d, off, dest), // COMPRESS_ZLIB
        _ => None,
    }
}

/// Copy a raw (COMPRESS_NONE) tile of exactly `dest.len()` bytes at `off` into `dest`.
pub(super) fn decode_tile_raw(d: &[u8], off: usize, dest: &mut [u8]) -> Option<()> {
    let raw = d.get(off..off.checked_add(dest.len())?)?;
    dest.copy_from_slice(raw);
    Some(())
}

/// Inflate a COMPRESS_ZLIB tile into exactly `dest.len()` bytes; a short inflate declines.
pub(super) fn decode_tile_zlib(d: &[u8], off: usize, dest: &mut [u8]) -> Option<()> {
    use std::io::Read;
    let src = d.get(off..)?;
    let mut z = flate2::read::ZlibDecoder::new(src);
    let mut filled = 0usize;
    while filled < dest.len() {
        match z.read(&mut dest[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(_) => break,
        }
    }
    (filled == dest.len()).then_some(())
}
