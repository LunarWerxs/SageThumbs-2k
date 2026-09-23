#![cfg(test)]

use super::*;

/// The reference: what the block average WAS, i.e. decode all sixteen texels through
/// `bcdec_rs` and average them. [`block_mean_fast`] must agree with this byte for byte,
/// on every block, or it is not an optimisation but a silent change to every DDS
/// thumbnail in the product.
fn slow_mean(blk: &[u8], block: Block) -> [u8; 4] {
    let mut tile = [0u8; 4 * 4 * 4];
    match block {
        Block::Bc1 => bcdec_rs::bc1(blk, &mut tile, 16),
        Block::Bc2 => bcdec_rs::bc2(blk, &mut tile, 16),
        Block::Bc3 => bcdec_rs::bc3(blk, &mut tile, 16),
        _ => unreachable!("only the palette formats have a fast path"),
    }
    let mut out = [0u8; 4];
    write_block_average(&tile, 0, 4, 4, &mut out);
    out
}

/// Deterministic pseudo-random blocks: a fixed-seed LCG, so a failure reproduces exactly
/// rather than "sometimes on CI".
fn blocks(seed: u32, n: usize, len: usize) -> Vec<Vec<u8>> {
    let mut s = seed;
    (0..n)
        .map(|_| {
            (0..len)
                .map(|_| {
                    s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (s >> 24) as u8
                })
                .collect()
        })
        .collect()
}

#[test]
fn the_fast_block_mean_matches_a_full_decode_exactly() {
    for (block, len) in [(Block::Bc1, 8), (Block::Bc2, 16), (Block::Bc3, 16)] {
        // Random blocks cover the ordinary case, including both BC1 endpoint orderings
        // and both BC3 alpha modes, since the seed bytes hit each about half the time.
        for blk in blocks(0x9E37_79B9, 20_000, len) {
            assert_eq!(
                block_mean_fast(&blk, block),
                Some(slow_mean(&blk, block)),
                "{block:?}: fast mean disagrees with the full decode for {blk:02X?}"
            );
        }

        // The boundaries a random sweep hits rarely or never. c0 == c1 is the flat block
        // AND the BC1A branch at once; all-zero and all-ones are the extremes; the two
        // endpoint orderings are the branch this whole function turns on.
        let mut edge: Vec<Vec<u8>> = vec![vec![0x00; len], vec![0xFF; len]];
        for (c0, c1) in [(0x0000u16, 0x0000u16), (0xFFFF, 0x0000), (0x0000, 0xFFFF)] {
            for idx in [0x0000_0000u32, 0xFFFF_FFFF, 0x1B1B_1B1B] {
                let mut b = vec![0u8; len];
                let off = if len == 16 { 8 } else { 0 };
                b[off..off + 2].copy_from_slice(&c0.to_le_bytes());
                b[off + 2..off + 4].copy_from_slice(&c1.to_le_bytes());
                b[off + 4..off + 8].copy_from_slice(&idx.to_le_bytes());
                edge.push(b);
            }
        }
        for blk in edge {
            assert_eq!(
                block_mean_fast(&blk, block),
                Some(slow_mean(&blk, block)),
                "{block:?}: fast mean disagrees on a boundary block {blk:02X?}"
            );
        }
    }
}

/// The formats that deliberately have NO fast path must say so, rather than returning a
/// wrong answer. BC7's colour comes from partitioned per-subset endpoints, so there is no
/// four-entry palette to weight; BC4/BC5 are already one or two channels.
#[test]
fn the_formats_without_a_palette_decline_the_fast_path() {
    for block in [
        Block::Bc4 { signed: false },
        Block::Bc5 { signed: false },
        Block::Bc6h { signed: false },
        Block::Bc7,
    ] {
        assert!(
            block_mean_fast(&[0xAB; 16], block).is_none(),
            "{block:?} has no palette and must not claim a fast mean"
        );
    }
}

/// A short buffer must decline, not panic. `blocks_rgba8` already bounds-checks its slice,
/// but this function indexes its own sub-ranges and runs on untrusted bytes in-process.
#[test]
fn a_truncated_block_declines_instead_of_panicking() {
    for len in 0..16usize {
        let b = vec![0xA5u8; len];
        for block in [Block::Bc1, Block::Bc2, Block::Bc3] {
            let _ = block_mean_fast(&b, block);
        }
    }
}
