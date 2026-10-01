/// 4x4 zigzag scan order: maps linear index to (row, col) within a 4x4 block.
pub const ZIGZAG_4X4: [(usize, usize); 16] = [
    (0, 0),
    (0, 1),
    (1, 0),
    (2, 0),
    (1, 1),
    (0, 2),
    (0, 3),
    (1, 2),
    (2, 1),
    (3, 0),
    (3, 1),
    (2, 2),
    (1, 3),
    (2, 3),
    (3, 2),
    (3, 3),
];

/// Inverse 4x4 Hadamard transform for I16x16 luma DC coefficients.
/// Input/output: 16 values arranged as 4x4 in raster order.
pub fn inverse_hadamard_4x4(dc: &mut [i32; 16]) {
    // Wrapping arithmetic: hostile streams can code coefficients near i32::MAX.
    use std::num::Wrapping as W;
    let mut tmp = [0i32; 16];

    // Horizontal transform
    for i in 0..4 {
        let a = W(dc[i * 4]);
        let b = W(dc[i * 4 + 1]);
        let c = W(dc[i * 4 + 2]);
        let d = W(dc[i * 4 + 3]);
        tmp[i * 4] = (a + b + c + d).0;
        tmp[i * 4 + 1] = (a + b - c - d).0;
        tmp[i * 4 + 2] = (a - b - c + d).0;
        tmp[i * 4 + 3] = (a - b + c - d).0;
    }

    // Vertical transform
    for j in 0..4 {
        let a = W(tmp[j]);
        let b = W(tmp[4 + j]);
        let c = W(tmp[8 + j]);
        let d = W(tmp[12 + j]);
        dc[j] = (a + b + c + d).0;
        dc[4 + j] = (a + b - c - d).0;
        dc[8 + j] = (a - b - c + d).0;
        dc[12 + j] = (a - b + c - d).0;
    }
}

/// Inverse 2x2 Hadamard transform for chroma DC coefficients.
pub fn inverse_hadamard_2x2(dc: &mut [i32; 4]) {
    use std::num::Wrapping as W;
    let (d0, d1, d2, d3) = (W(dc[0]), W(dc[1]), W(dc[2]), W(dc[3]));
    dc[0] = (d0 + d1 + d2 + d3).0;
    dc[1] = (d0 - d1 + d2 - d3).0;
    dc[2] = (d0 + d1 - d2 - d3).0;
    dc[3] = (d0 - d1 - d2 + d3).0;
}

/// Inverse 4x4 integer DCT transform (H.264 spec 8.5.12).
/// Operates in-place on 16 coefficients in raster order.
/// Horizontal pass (rows) first, then vertical pass (columns), per spec 8.5.12.1.
pub fn inverse_dct_4x4(block: &mut [i32; 16]) {
    // Use wrapping arithmetic — malformed coefficients can cause overflow
    use std::num::Wrapping as W;

    block[0] = block[0].wrapping_add(32);

    // First pass: rows
    for i in 0..4 {
        let s = i * 4;
        let z0 = (W(block[s]) + W(block[s + 2])).0;
        let z1 = (W(block[s]) - W(block[s + 2])).0;
        let z2 = (W(block[s + 1] >> 1) - W(block[s + 3])).0;
        let z3 = (W(block[s + 1]) + W(block[s + 3] >> 1)).0;

        block[s] = (W(z0) + W(z3)).0;
        block[s + 1] = (W(z1) + W(z2)).0;
        block[s + 2] = (W(z1) - W(z2)).0;
        block[s + 3] = (W(z0) - W(z3)).0;
    }

    // Second pass: columns, with >> 6 normalization
    for j in 0..4 {
        let z0 = (W(block[j]) + W(block[8 + j])).0;
        let z1 = (W(block[j]) - W(block[8 + j])).0;
        let z2 = (W(block[4 + j] >> 1) - W(block[12 + j])).0;
        let z3 = (W(block[4 + j]) + W(block[12 + j] >> 1)).0;

        block[j] = (W(z0) + W(z3)).0 >> 6;
        block[4 + j] = (W(z1) + W(z2)).0 >> 6;
        block[8 + j] = (W(z1) - W(z2)).0 >> 6;
        block[12 + j] = (W(z0) - W(z3)).0 >> 6;
    }
}

/// LevelScale factors from H.264 Table 8-13.
/// Indexed by [qp_rem][position_category] where position categories are:
/// 0: even row, even col — (0,0),(2,0),(0,2),(2,2)
/// 1: mixed parity — (0,1),(1,0),(0,3),(2,1), etc.
/// 2: odd row, odd col — (1,1),(3,1),(1,3),(3,3)
const LEVEL_SCALE: [[i32; 3]; 6] = [
    [10, 13, 16],
    [11, 14, 18],
    [13, 16, 20],
    [14, 18, 23],
    [16, 20, 25],
    [18, 23, 29],
];

/// Get the position category for a 4x4 block position (row, col).
/// Per spec Table 8-13: 0=even-even, 1=mixed parity, 2=odd-odd.
fn position_category(row: usize, col: usize) -> usize {
    (row & 1) + (col & 1)
}

/// Dequantize a 4x4 AC residual block in-place.
/// `scale` is the 4x4 scaling matrix (in scan order, default all-16 for flat scaling).
pub fn dequant_4x4(block: &mut [i32; 16], qp: i32, scale: &[u8; 16]) {
    let qp_per = qp / 6;
    let qp_rem = (qp % 6) as usize;

    for idx in 0..16 {
        if block[idx] != 0 {
            let (r, c) = ZIGZAG_4X4[idx];
            let v = LEVEL_SCALE[qp_rem][position_category(r, c)] * scale[idx] as i32;
            if qp_per >= 4 {
                block[idx] = block[idx].wrapping_mul(v).wrapping_shl((qp_per - 4) as u32);
            } else {
                block[idx] =
                    (block[idx].wrapping_mul(v).wrapping_add(1 << (3 - qp_per))) >> (4 - qp_per);
            }
        }
    }
}

/// Dequantize I16x16 luma DC coefficients after Hadamard.
/// Per spec 8.5.12.1, DC scaling uses scale[0] (the DC position of the scaling matrix).
pub fn dequant_luma_dc_i16x16(dc: &mut [i32; 16], qp: i32, scale_dc: u8) {
    let qp_per = qp / 6;
    let qp_rem = (qp % 6) as usize;
    let v = LEVEL_SCALE[qp_rem][0] * scale_dc as i32;

    if qp_per >= 6 {
        for d in dc.iter_mut() {
            *d = d.wrapping_mul(v).wrapping_shl((qp_per - 6) as u32);
        }
    } else {
        let round = 1 << (5 - qp_per);
        for d in dc.iter_mut() {
            *d = (d.wrapping_mul(v).wrapping_add(round)) >> (6 - qp_per);
        }
    }
}

/// Dequantize 4:2:0 chroma DC coefficients after Hadamard (spec 8.5.11.2):
/// `dcC = ((f * LevelScale4x4(qP % 6, 0, 0)) << (qP / 6)) >> 5`.
/// `qp` is QP'C (QPC + QpBdOffsetC). Uses scale[0] from the chroma scaling matrix.
/// Computed in i64 so a hostile coefficient cannot overflow before the shift.
pub fn dequant_chroma_dc(dc: &mut [i32; 4], qp: i32, scale_dc: u8) {
    let qp_per = (qp / 6) as u32;
    let qp_rem = (qp % 6) as usize;
    let v = (LEVEL_SCALE[qp_rem][0] * scale_dc as i32) as i64;
    for d in dc.iter_mut() {
        *d = (((*d as i64 * v) << qp_per) >> 5) as i32;
    }
}

/// QP_C lookup table from QP_I (H.264 Table 8-15).
/// qPI = clip3(-QpBdOffsetC, 51, QP_Y + chroma_qp_index_offset)
/// QP_C = qPI for qPI < 30, else QPC_TABLE[qPI]
pub const QPC_TABLE: [i32; 52] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25,
    26, 27, 28, 29, 29, 30, 31, 32, 32, 33, 34, 34, 35, 35, 36, 36, 37, 37, 37, 38, 38, 38, 39, 39,
    39, 39,
];

/// Compute QPC (spec 8.5.8, without QpBdOffsetC) from QPY and a chroma QP
/// index offset. The result may be negative for high bit depths; add
/// QpBdOffsetC to get QP'C for dequantisation.
pub fn chroma_qp(qp_y: i32, chroma_qp_index_offset: i32, qp_bd_offset_c: i32) -> i32 {
    let qpi = qp_y
        .saturating_add(chroma_qp_index_offset)
        .clamp(-qp_bd_offset_c, 51);
    if qpi < 30 {
        qpi
    } else {
        QPC_TABLE[qpi as usize]
    }
}

/// 4x4 field zigzag scan order (spec Table 8-13): used for field pictures and
/// field-coded MBs in MBAFF. Maps linear index to (row, col).
pub const ZIGZAG_4X4_FIELD: [(usize, usize); 16] = [
    (0, 0),
    (1, 0),
    (0, 1),
    (2, 0),
    (3, 0),
    (1, 1),
    (2, 1),
    (3, 1),
    (0, 2),
    (1, 2),
    (2, 2),
    (3, 2),
    (0, 3),
    (1, 3),
    (2, 3),
    (3, 3),
];

/// 8x8 field zigzag scan for CAVLC (spec Table 8-13, rearranged for CAVLC 4-quad decode).
/// Like ZIGZAG_8X8_CAVLC but using field scan order. Each group of 16 corresponds to
/// one 4x4 sub-block (TL, TR, BL, BR), with coefficients ordered by field scan position.
#[rustfmt::skip]
pub const ZIGZAG_8X8_CAVLC_FIELD: [usize; 64] = [
     0,  8,  1,  2, 16,  9, 24, 17,
    10,  3, 11, 25, 18, 26, 19, 27,
     4, 12,  5,  6, 13, 20, 28, 21,
    14,  7, 15, 22, 29, 30, 23, 31,
    32, 40, 33, 34, 41, 48, 56, 49,
    42, 35, 43, 50, 57, 58, 51, 59,
    36, 44, 37, 38, 45, 52, 60, 53,
    46, 39, 47, 54, 61, 62, 55, 63,
];

/// 8x8 field zigzag scan for CABAC (spec Table 8-13, field coding).
/// Value = row*8+col.
#[rustfmt::skip]
pub const ZIGZAG_8X8_CABAC_FIELD: [usize; 64] = [
     0,  8,  1,  2, 16,  9, 24, 17,
    10,  3,  4, 11, 25, 18, 32, 40,
    33, 26, 19, 12,  5,  6, 13, 20,
    27, 34, 41, 48, 56, 49, 42, 35,
    28, 21, 14,  7, 15, 22, 29, 36,
    43, 50, 57, 58, 51, 44, 37, 30,
    23, 31, 38, 45, 52, 59, 60, 53,
    46, 39, 47, 54, 61, 62, 55, 63,
];

/// 8x8 zigzag scan for CAVLC (spec Table 8-12, rearranged for CAVLC 4-quad decode).
/// Structured as 4 groups of 16: each group is one 4x4 sub-block's scan positions
/// within the full 8x8 block. Value = row * 8 + col.
#[rustfmt::skip]
pub const ZIGZAG_8X8_CAVLC: [usize; 64] = [
    // 4 groups of 16: each maps CAVLC scan position → raster position in 8x8
     0,  9, 17, 18, 12, 40, 27,  7, 35, 57, 29, 30, 58, 38, 53, 47,
     1,  2, 24, 11, 19, 48, 20, 14, 42, 50, 22, 37, 59, 31, 60, 55,
     8,  3, 32,  4, 26, 41, 13, 21, 49, 43, 15, 44, 52, 39, 61, 62,
    16, 10, 25,  5, 33, 34,  6, 28, 56, 36, 23, 51, 45, 46, 54, 63,
];

/// 8x8 zigzag scan for CABAC (standard zigzag, value = row*8+col).
#[rustfmt::skip]
pub const ZIGZAG_8X8_CABAC: [usize; 64] = [
     0,  1,  8, 16,  9,  2,  3, 10,
    17, 24, 32, 25, 18, 11,  4,  5,
    12, 19, 26, 33, 40, 48, 41, 34,
    27, 20, 13,  6,  7, 14, 21, 28,
    35, 42, 49, 56, 57, 50, 43, 36,
    29, 22, 15, 23, 30, 37, 44, 51,
    58, 59, 52, 45, 38, 31, 39, 46,
    53, 60, 61, 54, 47, 55, 62, 63,
];

/// LevelScale factors for 8x8 blocks (H.264 Table 8-14).
/// Indexed by [qp_rem][position_category_8x8].
/// 8x8 blocks have 6 position categories (vs 3 for 4x4).
const LEVEL_SCALE_8X8: [[i32; 6]; 6] = [
    [20, 18, 32, 19, 25, 24],
    [22, 19, 35, 21, 28, 26],
    [26, 23, 42, 24, 33, 31],
    [28, 25, 45, 26, 35, 33],
    [32, 28, 51, 30, 40, 38],
    [36, 32, 58, 34, 46, 43],
];

/// Maps (row%4)*4 + (col%4) to one of 6 position categories for 8x8 dequant.
/// From H.264 spec Table 8-14.
const DEQUANT_8X8_POS_CAT: [usize; 16] = [0, 3, 4, 3, 3, 1, 5, 1, 4, 5, 2, 5, 3, 1, 5, 1];

/// Dequantize an 8x8 residual block in raster order.
/// `block[r*8+c]` contains coefficients in raster positions.
/// `scale` is the 8x8 scaling matrix (64 values in raster order).
pub fn dequant_8x8(block: &mut [i32; 64], qp: i32, scale: &[u8; 64]) {
    let qp_per = qp / 6;
    let qp_rem = (qp % 6) as usize;

    for (idx, coeff) in block.iter_mut().enumerate() {
        if *coeff != 0 {
            let r = idx / 8;
            let c = idx % 8;
            let cat = DEQUANT_8X8_POS_CAT[(r % 4) * 4 + (c % 4)];
            let v = LEVEL_SCALE_8X8[qp_rem][cat] * scale[r * 8 + c] as i32;
            if qp_per >= 6 {
                *coeff = coeff.wrapping_mul(v).wrapping_shl((qp_per - 6) as u32);
            } else {
                *coeff = (coeff.wrapping_mul(v).wrapping_add(1 << (5 - qp_per))) >> (6 - qp_per);
            }
        }
    }
}

/// Inverse 8x8 integer DCT transform (H.264 spec 8.5.12).
/// Operates in-place on 64 coefficients in raster order (row-major, 8 per row).
/// Row pass first (within each row), then column pass with >> 6 normalization.
pub fn inverse_dct_8x8(block: &mut [i32; 64]) {
    // Use wrapping arithmetic throughout — malformed coefficients can cause overflow
    use std::num::Wrapping as W;

    block[0] = block[0].wrapping_add(32);

    // First pass: rows
    for i in 0..8 {
        let s = i * 8;
        let (b0, b2, b4, b6, b1, b3, b5, b7) = idct8_butterfly(
            block[s],
            block[s + 1],
            block[s + 2],
            block[s + 3],
            block[s + 4],
            block[s + 5],
            block[s + 6],
            block[s + 7],
        );
        block[s] = (W(b0) + W(b7)).0;
        block[s + 1] = (W(b2) + W(b5)).0;
        block[s + 2] = (W(b4) + W(b3)).0;
        block[s + 3] = (W(b6) + W(b1)).0;
        block[s + 4] = (W(b6) - W(b1)).0;
        block[s + 5] = (W(b4) - W(b3)).0;
        block[s + 6] = (W(b2) - W(b5)).0;
        block[s + 7] = (W(b0) - W(b7)).0;
    }

    // Second pass: columns, with >> 6 normalization
    for i in 0..8 {
        let (b0, b2, b4, b6, b1, b3, b5, b7) = idct8_butterfly(
            block[i],
            block[i + 8],
            block[i + 16],
            block[i + 24],
            block[i + 32],
            block[i + 40],
            block[i + 48],
            block[i + 56],
        );
        block[i] = (W(b0) + W(b7)).0 >> 6;
        block[i + 8] = (W(b2) + W(b5)).0 >> 6;
        block[i + 16] = (W(b4) + W(b3)).0 >> 6;
        block[i + 24] = (W(b6) + W(b1)).0 >> 6;
        block[i + 32] = (W(b6) - W(b1)).0 >> 6;
        block[i + 40] = (W(b4) - W(b3)).0 >> 6;
        block[i + 48] = (W(b2) - W(b5)).0 >> 6;
        block[i + 56] = (W(b0) - W(b7)).0 >> 6;
    }
}

/// 8x8 IDCT butterfly using wrapping arithmetic to avoid overflow panics.
#[allow(clippy::too_many_arguments)]
#[inline(always)]
fn idct8_butterfly(
    x0: i32,
    x1: i32,
    x2: i32,
    x3: i32,
    x4: i32,
    x5: i32,
    x6: i32,
    x7: i32,
) -> (i32, i32, i32, i32, i32, i32, i32, i32) {
    use std::num::Wrapping as W;
    let a0 = (W(x0) + W(x4)).0;
    let a2 = (W(x0) - W(x4)).0;
    let a4 = (W(x2 >> 1) - W(x6)).0;
    let a6 = (W(x6 >> 1) + W(x2)).0;

    let b0 = (W(a0) + W(a6)).0;
    let b2 = (W(a2) + W(a4)).0;
    let b4 = (W(a2) - W(a4)).0;
    let b6 = (W(a0) - W(a6)).0;

    let a1 = (W(0) - W(x3) + W(x5) - W(x7) - W(x7 >> 1)).0;
    let a3 = (W(x1) + W(x7) - W(x3) - W(x3 >> 1)).0;
    let a5 = (W(0) - W(x1) + W(x7) + W(x5) + W(x5 >> 1)).0;
    let a7 = (W(x3) + W(x5) + W(x1) + W(x1 >> 1)).0;

    let b1 = (W(a7 >> 2) + W(a1)).0;
    let b3 = (W(a3) + W(a5 >> 2)).0;
    let b5 = (W(a3 >> 2) - W(a5)).0;
    let b7 = (W(a7) - W(a1 >> 2)).0;

    (b0, b2, b4, b6, b1, b3, b5, b7)
}

/// Raster block index to (mb_row_offset, mb_col_offset) for luma 4x4 blocks.
/// Block ordering within a macroblock: inverse raster scan of 8x8 blocks,
/// then raster scan of 4x4 within each 8x8.
pub const BLOCK_INDEX_TO_OFFSET: [(usize, usize); 16] = [
    (0, 0),
    (0, 4),
    (4, 0),
    (4, 4), // block 0-3 (top-left 8x8)
    (0, 8),
    (0, 12),
    (4, 8),
    (4, 12), // block 4-7 (top-right 8x8)
    (8, 0),
    (8, 4),
    (12, 0),
    (12, 4), // block 8-11 (bottom-left 8x8)
    (8, 8),
    (8, 12),
    (12, 8),
    (12, 12), // block 12-15 (bottom-right 8x8)
];

/// Reverse lookup: `OFFSET_TO_BLOCK[row][col]` gives the block index for the
/// 4x4 block at grid position (row, col) where row/col are in 0..4.
/// Replaces O(16) linear scans of `BLOCK_INDEX_TO_OFFSET.iter().position()`.
pub const OFFSET_TO_BLOCK: [[usize; 4]; 4] = [
    [0, 1, 4, 5],     // row 0
    [2, 3, 6, 7],     // row 1
    [8, 9, 12, 13],   // row 2
    [10, 11, 14, 15], // row 3
];

/// coded_block_pattern mapping for I macroblocks (H.264 Table 9-4).
/// Index is the code_number from ue(v); value is the CBP.
/// Low 4 bits = luma CBP (one bit per 8x8 block), bits 4-5 = chroma CBP (0/1/2).
#[rustfmt::skip]
pub const CBP_INTRA_TABLE: [u8; 48] = [
    47, 31, 15,  0, 23, 27, 29, 30,  7, 11, 13, 14, 39, 43, 45, 46,
    16,  3,  5, 10, 12, 19, 21, 26, 28, 35, 37, 42, 44,  1,  2,  4,
     8, 17, 18, 20, 24,  6,  9, 22, 25, 32, 33, 34, 36, 40, 38, 41,
];

/// coded_block_pattern mapping for Inter macroblocks (H.264 Table 9-4b).
#[rustfmt::skip]
pub const CBP_INTER_TABLE: [u8; 48] = [
     0, 16,  1,  2,  4,  8, 32,  3,  5, 10, 12, 15, 47,  7, 11, 13,
    14,  6,  9, 31, 35, 37, 42, 44, 33, 34, 36, 40, 39, 43, 45, 46,
    17, 18, 20, 24, 19, 21, 26, 28, 23, 27, 29, 30, 22, 25, 38, 41,
];

/// Dequantize a full 4x4 block (including DC at position [0][0]) in raster order.
/// `scale` is the scaling matrix in scan order; mapped to raster via ZIGZAG_4X4.
pub fn dequant_4x4_full(block: &mut [i32; 16], qp: i32, scale: &[u8; 16]) {
    let qp_per = qp / 6;
    let qp_rem = (qp % 6) as usize;

    for r in 0..4 {
        for c in 0..4 {
            let idx = r * 4 + c;
            if block[idx] != 0 {
                let pc = position_category(r, c);
                // Find the scan-order index for this raster position to look up the scale
                let scan_idx = ZIGZAG_4X4
                    .iter()
                    .position(|&(zr, zc)| zr == r && zc == c)
                    .unwrap();
                let v = LEVEL_SCALE[qp_rem][pc] * scale[scan_idx] as i32;
                if qp_per >= 4 {
                    block[idx] = block[idx].wrapping_mul(v).wrapping_shl((qp_per - 4) as u32);
                } else {
                    block[idx] = (block[idx].wrapping_mul(v).wrapping_add(1 << (3 - qp_per)))
                        >> (4 - qp_per);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inverse_hadamard_4x4_all_same() {
        // If all DC values are the same, Hadamard should concentrate energy in [0]
        let mut dc = [5i32; 16];
        inverse_hadamard_4x4(&mut dc);
        assert_eq!(dc[0], 80); // 5 * 16
        for &v in &dc[1..] {
            assert_eq!(v, 0);
        }
    }

    #[test]
    fn test_inverse_hadamard_2x2() {
        let mut dc = [1, 0, 0, 0];
        inverse_hadamard_2x2(&mut dc);
        assert_eq!(dc, [1, 1, 1, 1]);
    }
}
