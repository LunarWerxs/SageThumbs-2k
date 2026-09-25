//! Block-compressed surfaces (BC1-BC5, BC7) to RGBA8, plus the per-block mean that serves a reduced decode.

use super::*;

/// Smallest surface worth reducing block-by-block rather than decoding whole. One
/// megapixel of BC1 is a 4 MB RGBA surface and about two milliseconds; the saving below
/// that is noise, and staying on the full path keeps a small targeted decode returning
/// exactly the mip level it selected.
pub(super) const AVG_MIN_PIXELS: u64 = 1 << 20;

pub(super) fn decode_rgba8(bytes: &[u8], s: &Surface, target: Option<u32>) -> Result<DynamicImage> {
    let src = surface(bytes, s)?;
    if let (Layout::Block(b), Some(t)) = (s.layout, target) {
        if let Some(img) = reduced_block_decode(src, s, b, t)? {
            return Ok(img);
        }
    }
    let len = out_buffer(s.width, s.height, 4)?;
    let mut out = vec![0u8; len];
    decode_layouts_rgba8(src, s, &mut out)?;
    apply_alpha_mode(&mut out, s.alpha_mode);
    rgba8_image(s.width, s.height, out)
}

/// ONE PIXEL PER 4x4 BLOCK, when the caller's target is small enough that the quarter-
/// size result still covers it. A block-compressed texture without a mip chain is the
/// one case `select_mip` cannot help with, and it is the common one: every DDS an
/// image editor exports has `dwMipMapCount = 1`, so a 12 MP BC1 texture decoded all
/// 750k blocks into a 48 MB surface and then threw 15/16 of it away in the fit. That
/// measured 180.5 ms against Windows' 24.8 ms, 7.3x and the worst block-format ratio
/// in the speed baseline.
///
/// This is NOT sampling: each block is still fully decoded, and the pixel written is
/// the MEAN of its in-bounds texels, which is exactly the 4x box reduction the later
/// fit would have performed anyway. So the picture is the same one, reached without
/// materialising a surface that is 16x larger than any use of it. The saving is the
/// scattered row writes into that surface and every later pass over it, not the block
/// decode itself.
///
/// Two gates, both load-bearing. The reduced grid must still COVER the target, so
/// nothing is ever upscaled: a 4000x3000 texture at a 256 px ask reduces to 1000x750
/// (fine), the same texture at a 1024 px preview-pane ask does not (1000 < 1024) and
/// takes the full path below. And the surface must be big enough for materialising it
/// to cost anything at all: below [`AVG_MIN_PIXELS`] the full decode is a couple of
/// milliseconds, there is nothing to win, and the level's own dimensions are the
/// answer mip selection is pinned to return. Full-fidelity callers pass `None` and are
/// untouched, exactly as with mip selection. `Ok(None)` when the reduction is not worth it.
pub(super) fn reduced_block_decode(
    src: &[u8],
    s: &Surface,
    b: Block,
    t: u32,
) -> Result<Option<DynamicImage>> {
    let bw = s.width.div_ceil(4);
    let bh = s.height.div_ceil(4);
    let px = u64::from(s.width) * u64::from(s.height);
    if matches!(b, Block::Bc6h { .. }) || bw.max(bh) < t.max(1) || px < AVG_MIN_PIXELS {
        return Ok(None);
    }
    let len = out_buffer(bw, bh, 4)?;
    let mut out = vec![0u8; len];
    blocks_rgba8(src, s.width, s.height, b, true, &mut out);
    apply_alpha_mode(&mut out, s.alpha_mode);
    Ok(Some(rgba8_image(bw, bh, out)?))
}

/// Run the layout-specific 8-bit decoder into `out`.
pub(super) fn decode_layouts_rgba8(src: &[u8], s: &Surface, out: &mut [u8]) -> Result<()> {
    match s.layout {
        Layout::Block(b) => blocks_rgba8(src, s.width, s.height, b, false, out),
        Layout::Masks(m) => masks_rgba8(src, s.width, s.height, m, out),
        Layout::Snorm8(n) => snorm_rgba8(src, s.width, s.height, n, 1, out),
        Layout::Snorm16(n) => snorm_rgba8(src, s.width, s.height, n, 2, out),
        Layout::Unorm16(n) => unorm16_rgba8(src, s.width, s.height, n, out),
        // is_float() routed these to decode_float.
        Layout::Half(_) | Layout::Float(_) | Layout::R11G11B10 | Layout::Rgb9E5 => {
            return Err(fail("float layout on the 8-bit path"))
        }
    }
    Ok(())
}

/// Wrap an RGBA8 byte buffer as a `DynamicImage`, or fail on a size mismatch.
pub(super) fn rgba8_image(width: u32, height: u32, out: Vec<u8>) -> Result<DynamicImage> {
    image::RgbaImage::from_raw(width, height, out)
        .map(DynamicImage::ImageRgba8)
        .ok_or_else(|| fail("buffer size mismatch"))
}

/// The fast-mean shortcut for a FULL (non-edge) block in average mode: compute its mean
/// without expanding all sixteen texels and write it to `out`, returning whether it did (the
/// caller then skips the normal decode for this block entirely).
///
/// Edge blocks fall through to the normal decode instead: their average covers only the
/// in-bounds texels, which an index histogram cannot distinguish. Byte-identical either way,
/// pinned by `dds_mean_tests::the_fast_block_mean_matches_a_full_decode_exactly`.
#[allow(clippy::too_many_arguments)]
pub(super) fn try_fast_block_mean(
    blk: &[u8],
    block: Block,
    average_blocks: bool,
    bx: usize,
    by: usize,
    bw: usize,
    width: u32,
    height: u32,
    out: &mut [u8],
) -> bool {
    let whole_block = (bx + 1) * 4 <= width as usize && (by + 1) * 4 <= height as usize;
    if !(average_blocks && whole_block) {
        return false;
    }
    let Some(mean) = block_mean_fast(blk, block) else {
        return false;
    };
    let dst = (by * bw + bx) * 4;
    if let Some(d) = out.get_mut(dst..dst + 4) {
        d.copy_from_slice(&mean);
    }
    true
}

/// Decode one block's 4×4 RGBA texels into `tile` (every arm writes all 16 pixels, so it never
/// carries a previous block's contents). Returns `false` for BC6H, a float-only format that
/// never reaches this 8-bit path, so the caller aborts the whole walk, matching the original's
/// unconditional `return`.
pub(super) fn decode_block_tile(blk: &[u8], block: Block, tile: &mut [u8]) -> bool {
    match block {
        Block::Bc1 => bcdec_rs::bc1(blk, tile, 16),
        Block::Bc2 => bcdec_rs::bc2(blk, tile, 16),
        Block::Bc3 => bcdec_rs::bc3(blk, tile, 16),
        // BC4/BC5 decode to 1 or 2 tightly packed channels; expand after.
        Block::Bc4 { signed } => {
            let mut one = [0u8; 16];
            bcdec_rs::bc4(blk, &mut one, 4, signed);
            for (i, v) in one.iter().enumerate() {
                tile[i * 4..i * 4 + 4].copy_from_slice(&[*v, *v, *v, 255]);
            }
        }
        Block::Bc5 { signed } => {
            let mut two = [0u8; 32];
            bcdec_rs::bc5(blk, &mut two, 8, signed);
            for i in 0..16 {
                // R,G,0: the third channel genuinely is not stored, and
                // this matches what ImageMagick renders for ATI2/BC5.
                tile[i * 4..i * 4 + 4].copy_from_slice(&[two[i * 2], two[i * 2 + 1], 0, 255]);
            }
        }
        Block::Bc7 => bcdec_rs::bc7(blk, tile, 16),
        // Float-only; never reaches the 8-bit path.
        Block::Bc6h { .. } => return false,
    }
    true
}

/// Write one decoded block's tile into `out`: its mean (average mode) or a direct tile copy
/// (full-resolution mode, clipped to the in-bounds part for edge blocks).
#[allow(clippy::too_many_arguments)]
pub(super) fn write_decoded_block(
    tile: &[u8; 4 * 4 * 4],
    average_blocks: bool,
    bx: usize,
    by: usize,
    bw: usize,
    width: u32,
    height: u32,
    row: usize,
    out: &mut [u8],
) {
    if average_blocks {
        let tw = 4.min(width as usize - bx * 4);
        let th = 4.min(height as usize - by * 4);
        write_block_average(tile, (by * bw + bx) * 4, tw, th, out);
    } else {
        copy_tile(tile, 4, bx, by, width, height, row, out);
    }
}

/// Walk the 4×4 block grid, decoding each into a scratch tile and copying the
/// in-bounds part out. The tile hop is what makes a texture whose dimensions are
/// not a multiple of 4 work — the last row/column of blocks is partly padding.
///
/// With `average_blocks`, `out` is instead the quarter-size grid and each block
/// contributes the mean of its in-bounds texels. Same walk, same block decode; only
/// what is written differs. See [`decode_rgba8`] for why.
pub(super) fn blocks_rgba8(
    src: &[u8],
    width: u32,
    height: u32,
    block: Block,
    average_blocks: bool,
    out: &mut [u8],
) {
    let bw = width.div_ceil(4) as usize;
    let bh = height.div_ceil(4) as usize;
    let bytes = block.block_bytes();
    let row = width as usize * 4;
    // 4×4 RGBA scratch, reused every iteration.
    let mut tile = [0u8; 4 * 4 * 4];
    for by in 0..bh {
        for bx in 0..bw {
            let off = (by * bw + bx) * bytes;
            let Some(blk) = src.get(off..off + bytes) else {
                return;
            };
            if try_fast_block_mean(blk, block, average_blocks, bx, by, bw, width, height, out) {
                continue;
            }
            if !decode_block_tile(blk, block, &mut tile) {
                return;
            }
            write_decoded_block(&tile, average_blocks, bx, by, bw, width, height, row, out);
        }
    }
}

/// The four RGBA colours a BC1/BC2/BC3 colour block resolves to.
///
/// **This mirrors `bcdec_rs::color_block` exactly, and it has to.** The whole point of
/// [`block_mean_fast`] is to produce a byte-identical answer without expanding sixteen
/// texels, so every magic constant here is copied from that function rather than re-derived
/// from the "2/3 of c0 plus 1/3 of c1" description — those are fixed-point reciprocals with
/// their own rounding, and a plausible-looking recomputation lands a level or two off on
/// most blocks. `dds_mean_tests::the_fast_block_mean_matches_a_full_decode_exactly` is what
/// keeps that true; treat a failure there as "the fast path is wrong", never as "the
/// tolerance needs widening".
///
/// `only_opaque` is BC2/BC3, whose colour block has no punch-through index because the alpha
/// arrives separately.
pub(super) fn color_palette(cb: &[u8], only_opaque: bool) -> [[u8; 4]; 4] {
    let c0 = u16::from_le_bytes([cb[0], cb[1]]);
    let c1 = u16::from_le_bytes([cb[2], cb[3]]);
    let (r0, g0, b0) = (
        (c0 as u32 >> 11) & 0x1F,
        (c0 as u32 >> 5) & 0x3F,
        c0 as u32 & 0x1F,
    );
    let (r1, g1, b1) = (
        (c1 as u32 >> 11) & 0x1F,
        (c1 as u32 >> 5) & 0x3F,
        c1 as u32 & 0x1F,
    );

    let expand = |r: u32, g: u32, b: u32| {
        [
            ((r * 527 + 23) >> 6) as u8,
            ((g * 259 + 33) >> 6) as u8,
            ((b * 527 + 23) >> 6) as u8,
            255,
        ]
    };
    let mut pal = [[0u8; 4]; 4];
    pal[0] = expand(r0, g0, b0);
    pal[1] = expand(r1, g1, b1);

    if c0 > c1 || only_opaque {
        pal[2] = [
            (((2 * r0 + r1) * 351 + 61) >> 7) as u8,
            (((2 * g0 + g1) * 2763 + 1039) >> 11) as u8,
            (((2 * b0 + b1) * 351 + 61) >> 7) as u8,
            255,
        ];
        pal[3] = [
            (((r0 + r1 * 2) * 351 + 61) >> 7) as u8,
            (((g0 + g1 * 2) * 2763 + 1039) >> 11) as u8,
            (((b0 + b1 * 2) * 351 + 61) >> 7) as u8,
            255,
        ];
    } else {
        // BC1A: one interpolated colour and one fully transparent index.
        pal[2] = [
            (((r0 + r1) * 1053 + 125) >> 8) as u8,
            (((g0 + g1) * 4145 + 1019) >> 11) as u8,
            (((b0 + b1) * 1053 + 125) >> 8) as u8,
            255,
        ];
        pal[3] = [0; 4];
    }
    pal
}

/// The mean of a FULL 4x4 BC1/BC2/BC3 block, computed from its endpoints and an index
/// histogram instead of expanding all sixteen texels and averaging them.
///
/// This is the block-average fast path's own fast path. Reducing a mipless 12 MP texture to
/// one pixel per block already avoids materialising 12 MP (2026-08-19), but it still ran every
/// block through `bcdec_rs` to build sixteen RGBA pixels that were immediately summed and
/// thrown away — 64 bytes written and read back per block, 750k times, for four numbers.
/// Here the palette is built once per block (identical arithmetic, see [`color_palette`]) and
/// each entry is multiplied by how many texels select it.
///
/// `None` for anything else: BC4/BC5 are cheap already (one or two channels, no palette), and
/// BC7's colour comes from one of eight partitioned modes with per-subset endpoints, so there
/// is no small palette to weight — it keeps the full decode. Edge blocks also return here
/// through the caller, because a partial block must average only its in-bounds texels and the
/// histogram cannot see which those are.
pub(super) fn block_mean_fast(blk: &[u8], block: Block) -> Option<[u8; 4]> {
    let (cb, only_opaque) = match block {
        Block::Bc1 => (blk.get(0..8)?, false),
        // BC2/BC3 put the alpha block first; the colour block is the second half.
        Block::Bc2 | Block::Bc3 => (blk.get(8..16)?, true),
        _ => return None,
    };
    let pal = color_palette(cb, only_opaque);
    let mut acc = palette_weighted_sum(&pal, cb);

    // BC2/BC3 overwrite alpha per texel, so the palette's 255s are discarded rather than
    // averaged in.
    match block {
        Block::Bc2 => acc[3] = bc2_alpha_sum(blk),
        Block::Bc3 => acc[3] = bc3_alpha_sum(blk)?,
        _ => {}
    }

    // Same rounding as `write_block_average` with n = 16, so a flat block round-trips.
    Some([
        ((acc[0] + 8) / 16) as u8,
        ((acc[1] + 8) / 16) as u8,
        ((acc[2] + 8) / 16) as u8,
        ((acc[3] + 8) / 16) as u8,
    ])
}

/// HISTOGRAM FIRST, then four weighted adds — not sixteen four-channel accumulations.
/// The obvious shape (`for each texel { acc += pal[idx] }`) measured THREE TIMES SLOWER
/// than simply expanding the block through `bcdec_rs`, because sixteen fresh array
/// iterators per block defeat the vectoriser. Counting into four bins and multiplying
/// once per palette entry is the same arithmetic with a sixteenth of the loop overhead.
pub(super) fn palette_weighted_sum(pal: &[[u8; 4]; 4], cb: &[u8]) -> [u32; 4] {
    let mut indices = u32::from_le_bytes([cb[4], cb[5], cb[6], cb[7]]);
    let mut hist = [0u32; 4];
    for _ in 0..16 {
        hist[(indices & 3) as usize] += 1;
        indices >>= 2;
    }
    let mut acc = [0u32; 4];
    for (n, colour) in hist.iter().zip(pal) {
        acc[0] += n * u32::from(colour[0]);
        acc[1] += n * u32::from(colour[1]);
        acc[2] += n * u32::from(colour[2]);
        acc[3] += n * u32::from(colour[3]);
    }
    acc
}

/// The summed 8-bit alpha of a full BC2 block, each texel's 4-bit alpha replicated ×17.
pub(super) fn bc2_alpha_sum(blk: &[u8]) -> u32 {
    (0..4)
        .map(|i| {
            let a = u16::from_le_bytes([blk[i * 2], blk[i * 2 + 1]]);
            (0..4)
                .map(|j| u32::from((a >> (4 * j)) & 0x0F) * 17)
                .sum::<u32>()
        })
        .sum()
}

/// The summed 8-bit alpha of a full BC3 block through its 8-entry interpolated palette.
///
/// Written out rather than derived from a loop counter, transcribed line for line
/// from `bcdec_rs::smooth_alpha_block`. A first attempt DID compute the weights
/// from the index and had both branches off by one; the equality test caught it,
/// but a table that can simply be compared against the source cannot drift at all.
pub(super) fn bc3_alpha_sum(blk: &[u8]) -> Option<u32> {
    let (a0, a1) = (u32::from(blk[0]), u32::from(blk[1]));
    let alpha: [u32; 8] = if a0 > a1 {
        [
            a0,
            a1,
            (6 * a0 + a1 + 1) / 7,
            (5 * a0 + 2 * a1 + 1) / 7,
            (4 * a0 + 3 * a1 + 1) / 7,
            (3 * a0 + 4 * a1 + 1) / 7,
            (2 * a0 + 5 * a1 + 1) / 7,
            (a0 + 6 * a1 + 1) / 7,
        ]
    } else {
        [
            a0,
            a1,
            (4 * a0 + a1 + 1) / 5,
            (3 * a0 + 2 * a1 + 1) / 5,
            (2 * a0 + 3 * a1 + 1) / 5,
            (a0 + 4 * a1 + 1) / 5,
            0x00,
            0xFF,
        ]
    };
    let mut bits = u64::from_le_bytes(blk.get(0..8)?.try_into().ok()?) >> 16;
    let mut sum = 0u32;
    for _ in 0..16 {
        sum += alpha[(bits & 0x07) as usize];
        bits >>= 3;
    }
    Some(sum)
}

/// Reduce one decoded 4x4 tile to a single RGBA pixel: the mean of its `w` by `h`
/// in-bounds texels. Padding texels in an edge block are excluded, so a texture whose
/// dimensions are not a multiple of 4 does not average undefined bytes into its last
/// row or column. Rounded, not truncated, so a flat block round-trips to its own colour.
pub(super) fn write_block_average(
    tile: &[u8; 4 * 4 * 4],
    dst: usize,
    w: usize,
    h: usize,
    out: &mut [u8],
) {
    let n = (w * h).max(1) as u32;
    let mut acc = [0u32; 4];
    for y in 0..h {
        for x in 0..w {
            let p = (y * 4 + x) * 4;
            for (a, v) in acc.iter_mut().zip(&tile[p..p + 4]) {
                *a += *v as u32;
            }
        }
    }
    if let Some(d) = out.get_mut(dst..dst + 4) {
        for (c, a) in d.iter_mut().zip(acc) {
            *c = ((a + n / 2) / n) as u8;
        }
    }
}

/// Copy the in-bounds pixels of one decoded 4×4 tile into the output image.
#[allow(clippy::too_many_arguments)]
pub(super) fn copy_tile(
    tile: &[u8],
    channels: usize,
    bx: usize,
    by: usize,
    width: u32,
    height: u32,
    row: usize,
    out: &mut [u8],
) {
    let px = bx * 4;
    let py = by * 4;
    let w = 4.min(width as usize - px);
    let h = 4.min(height as usize - py);
    for y in 0..h {
        let src = (y * 4) * channels;
        let dst = (py + y) * row + px * channels;
        let n = w * channels;
        if let (Some(s), Some(d)) = (tile.get(src..src + n), out.get_mut(dst..dst + n)) {
            d.copy_from_slice(s);
        }
    }
}
