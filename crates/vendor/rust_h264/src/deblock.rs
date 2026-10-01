//! H.264 deblocking filter (spec section 8.7).
//!
//! Applied after all macroblocks in a slice are decoded. For each MB in raster
//! order, vertical edges are filtered left-to-right, then horizontal edges
//! top-to-bottom. Each edge consists of 4-sample segments filtered independently.

use crate::decoder::Frame;
use crate::residual::chroma_qp;
use crate::slice::SliceHeader;

/// Macroblock type for boundary strength derivation.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum MbType {
    #[default]
    Intra,
    Ipcm,
    Inter,
}

/// Per-macroblock metadata needed by the deblocking filter.
#[derive(Clone, Default)]
pub struct MbInfo {
    pub mb_type: MbType,
    pub qp_y: i32,
    /// Per-4x4-block motion vectors for L0 (quarter-pel). 16 entries in raster scan.
    pub mv_l0: [[i16; 2]; 16],
    /// Per-4x4-block motion vectors for L1 (quarter-pel). 16 entries in raster scan.
    pub mv_l1: [[i16; 2]; 16],
    /// Per-4x4-block reference indices for L0. -1 = unused.
    pub ref_idx_l0: [i8; 16],
    /// Per-4x4-block reference indices for L1. -1 = unused.
    pub ref_idx_l1: [i8; 16],
    /// Per-4x4-block reference picture POC for L0. Used for cross-list comparison.
    pub ref_poc_l0: [i32; 16],
    /// Per-4x4-block reference picture POC for L1.
    pub ref_poc_l1: [i32; 16],
    /// Per-4x4-block non-zero coefficient count. True if any coefficients were coded.
    pub nnz: [bool; 16],
    /// Number of reference lists used (1 for P-slice, 2 for B-slice, 0 for I-slice).
    pub list_count: u8,
    /// True if this MB uses 8x8 transform (High profile transform_size_8x8_flag).
    pub is_8x8dct: bool,
}

// H.264 Table 8-16a: alpha threshold indexed by indexA = clamp(0..51, QPavg + offset_a)
#[rustfmt::skip]
const ALPHA_TABLE: [i32; 52] = [
     0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,
     4,  4,  5,  6,  7,  8,  9, 10, 12, 13, 15, 17, 20, 22, 25, 28,
    32, 36, 40, 45, 50, 56, 63, 71, 80, 90,101,113,127,144,162,182,
   203,226,255,255,
];

// H.264 Table 8-16b: beta threshold
#[rustfmt::skip]
const BETA_TABLE: [i32; 52] = [
     0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,
     2,  2,  2,  3,  3,  3,  3,  4,  4,  4,  6,  6,  7,  7,  8,  8,
     9,  9, 10, 10, 11, 11, 12, 12, 13, 13, 14, 14, 15, 15, 16, 16,
    17, 17, 18, 18,
];

// H.264 Table 8-16c: tc0 indexed by [indexA][bS-1] for bS=1..3
#[rustfmt::skip]
const TC0_TABLE: [[i32; 3]; 52] = [
    [ 0, 0, 0], [ 0, 0, 0], [ 0, 0, 0], [ 0, 0, 0], [ 0, 0, 0], [ 0, 0, 0],
    [ 0, 0, 0], [ 0, 0, 0], [ 0, 0, 0], [ 0, 0, 0], [ 0, 0, 0], [ 0, 0, 0],
    [ 0, 0, 0], [ 0, 0, 0], [ 0, 0, 0], [ 0, 0, 0], [ 0, 0, 0], [ 0, 0, 1],
    [ 0, 0, 1], [ 0, 0, 1], [ 0, 0, 1], [ 0, 1, 1], [ 0, 1, 1], [ 1, 1, 1],
    [ 1, 1, 1], [ 1, 1, 1], [ 1, 1, 1], [ 1, 1, 2], [ 1, 1, 2], [ 1, 1, 2],
    [ 1, 1, 2], [ 1, 2, 3], [ 1, 2, 3], [ 2, 2, 3], [ 2, 2, 4], [ 2, 3, 4],
    [ 2, 3, 4], [ 3, 3, 5], [ 3, 4, 6], [ 3, 4, 6], [ 4, 5, 7], [ 4, 5, 8],
    [ 4, 6, 9], [ 5, 7,10], [ 6, 8,11], [ 6, 8,13], [ 7,10,14], [ 8,11,16],
    [ 9,12,18], [10,13,20], [11,15,23], [13,17,25],
];

/// Apply the deblocking filter to a fully decoded frame.
pub fn filter_frame(
    frame: &mut Frame,
    mb_info: &[MbInfo],
    mb_width: usize,
    _mb_height: usize,
    header: &SliceHeader,
    chroma_qp_index_offset: i32,
    second_chroma_qp_index_offset: i32,
) {
    filter_frame_params(
        frame,
        mb_info,
        mb_width,
        header.disable_deblocking_filter_idc,
        header.slice_alpha_c0_offset_div2,
        header.slice_beta_offset_div2,
        chroma_qp_index_offset,
        second_chroma_qp_index_offset,
    );
}

/// Apply deblocking with explicit parameters (no SliceHeader needed).
#[allow(clippy::needless_range_loop, clippy::too_many_arguments)]
pub fn filter_frame_params(
    frame: &mut Frame,
    mb_info: &[MbInfo],
    mb_width: usize,
    disable_deblocking_filter_idc: u32,
    slice_alpha_c0_offset_div2: i32,
    slice_beta_offset_div2: i32,
    chroma_qp_index_offset: i32,
    second_chroma_qp_index_offset: i32,
) {
    filter_frame_inner(
        frame,
        mb_info,
        mb_width,
        disable_deblocking_filter_idc,
        slice_alpha_c0_offset_div2,
        slice_beta_offset_div2,
        [chroma_qp_index_offset, second_chroma_qp_index_offset],
        false,
    );
}

/// Apply deblocking with MBAFF support.
#[allow(clippy::too_many_arguments)]
pub fn filter_frame_mbaff(
    frame: &mut Frame,
    mb_info: &[MbInfo],
    mb_width: usize,
    disable_deblocking_filter_idc: u32,
    slice_alpha_c0_offset_div2: i32,
    slice_beta_offset_div2: i32,
    chroma_qp_index_offset: i32,
    second_chroma_qp_index_offset: i32,
    mbaff: bool,
) {
    filter_frame_inner(
        frame,
        mb_info,
        mb_width,
        disable_deblocking_filter_idc,
        slice_alpha_c0_offset_div2,
        slice_beta_offset_div2,
        [chroma_qp_index_offset, second_chroma_qp_index_offset],
        mbaff,
    );
}

#[allow(clippy::needless_range_loop, clippy::too_many_arguments)]
fn filter_frame_inner(
    frame: &mut Frame,
    mb_info: &[MbInfo],
    mb_width: usize,
    disable_deblocking_filter_idc: u32,
    slice_alpha_c0_offset_div2: i32,
    slice_beta_offset_div2: i32,
    chroma_qp_index_offsets: [i32; 2],
    mbaff: bool,
) {
    if disable_deblocking_filter_idc == 1 {
        return;
    }

    let filter_offset_a = slice_alpha_c0_offset_div2.wrapping_mul(2);
    let filter_offset_b = slice_beta_offset_div2.wrapping_mul(2);
    let stride_y = frame.width as usize;
    let stride_c = (frame.width / 2) as usize;

    // High bit depth (spec 8.7.2.2): alpha, beta and tC0 table values are
    // scaled by 1 << (BitDepth - 8); p0'/q0' clip to Clip1Y / Clip1C.
    let bit_depth_y = (frame.bit_depth_luma as u32).clamp(8, 14);
    let bit_depth_c = (frame.bit_depth_chroma as u32).clamp(8, 14);
    let scale_y = 1i32 << (bit_depth_y - 8);
    let scale_c = 1i32 << (bit_depth_c - 8);
    let max_y = (1i32 << bit_depth_y) - 1;
    let max_c = (1i32 << bit_depth_c) - 1;
    let qp_bd_offset_c = 6 * (bit_depth_c as i32 - 8);
    // Per chroma plane (Cb, Cr): (indexA, alpha, beta) from each MB's QPC,
    // derived from its own QPY with that plane's chroma_qp_index_offset.
    let chroma_thresholds = |qp_p: i32, qp_q: i32| -> [(usize, i32, i32); 2] {
        chroma_qp_index_offsets.map(|offset| {
            let qp_c_p = chroma_qp(qp_p, offset, qp_bd_offset_c);
            let qp_c_q = chroma_qp(qp_q, offset, qp_bd_offset_c);
            let qp_c_avg = (qp_c_p + qp_c_q + 1) >> 1;
            let c_index_a = qp_c_avg.saturating_add(filter_offset_a).clamp(0, 51) as usize;
            let c_index_b = qp_c_avg.saturating_add(filter_offset_b).clamp(0, 51) as usize;
            (
                c_index_a,
                ALPHA_TABLE[c_index_a] * scale_c,
                BETA_TABLE[c_index_b] * scale_c,
            )
        })
    };

    // 4x4 block index layout within an MB (raster scan):
    //  0  1  4  5
    //  2  3  6  7
    //  8  9 12 13
    // 10 11 14 15
    // For vertical edges: blocks to the left of edge column
    // For horizontal edges: blocks above edge row

    for mb_idx in 0..mb_info.len() {
        // Compute spatial position and column/row for neighbor checks
        let (mb_x, mb_y, mb_col, mb_row) = if mbaff {
            let pair_addr = mb_idx / 2;
            let pair_col = pair_addr % mb_width;
            let pair_row = pair_addr / mb_width;
            let is_bottom = mb_idx % 2 != 0;
            (
                pair_col * 16,
                pair_row * 32 + if is_bottom { 16 } else { 0 },
                pair_col,
                pair_row * 2 + if is_bottom { 1 } else { 0 },
            )
        } else {
            let col = mb_idx % mb_width;
            let row = mb_idx / mb_width;
            (col * 16, row * 16, col, row)
        };
        let mb_q = &mb_info[mb_idx];

        // -- Vertical edges (left to right) --
        for edge in 0..4 {
            let edge_x = mb_x + edge * 4;
            if edge == 0 && mb_col == 0 {
                continue;
            }

            let is_mb_edge = edge == 0;

            // 8x8 transform: skip internal odd edges (spec 8.7.2.1).
            // Edges 1 and 3 fall inside 8x8 transform blocks, so no filtering.
            if !is_mb_edge && (edge & 1) != 0 && mb_q.is_8x8dct {
                continue;
            }

            // Left neighbor MB
            let left_mb_idx = if !is_mb_edge {
                mb_idx // internal edge, same MB
            } else if mbaff {
                // MBAFF: left = same position (top/bottom) in left pair
                let pair_addr = mb_idx / 2;
                (pair_addr - 1) * 2 + (mb_idx % 2)
            } else {
                mb_idx - 1
            };
            let mb_p = &mb_info[left_mb_idx];

            let qp_q = mb_q.qp_y;
            let qp_p = mb_p.qp_y;
            let qp_avg = (qp_p.wrapping_add(qp_q).wrapping_add(1)) >> 1;
            let index_a = qp_avg.saturating_add(filter_offset_a).clamp(0, 51) as usize;
            let index_b = qp_avg.saturating_add(filter_offset_b).clamp(0, 51) as usize;
            let alpha = ALPHA_TABLE[index_a] * scale_y;
            let beta = BETA_TABLE[index_b] * scale_y;

            // Chroma thresholds (computed once per edge, used for chroma segments)
            let c_thresholds = chroma_thresholds(qp_p, qp_q);

            // Compute bS for all 4 segments first
            let mut seg_bs = [0i32; 4];
            for seg in 0..4 {
                let blk_q = blk_idx(edge, seg);
                let blk_p = if is_mb_edge {
                    blk_idx(3, seg)
                } else {
                    blk_idx(edge - 1, seg)
                };
                seg_bs[seg] = derive_bs(mb_p, mb_q, blk_p, blk_q, is_mb_edge);
            }
            // Luma: filter each segment independently
            for seg in 0..4 {
                if seg_bs[seg] == 0 {
                    continue;
                }
                let tc0 = if seg_bs[seg] < 4 {
                    TC0_TABLE[index_a][(seg_bs[seg] - 1) as usize] * scale_y
                } else {
                    0
                };
                let y = mb_y + seg * 4;
                filter_edge_v(
                    &mut frame.y,
                    stride_y,
                    edge_x,
                    y,
                    4,
                    seg_bs[seg],
                    alpha,
                    beta,
                    tc0,
                    max_y,
                );
            }

            // Chroma: per-pixel bS from all 4 luma segments
            if edge % 2 == 0 {
                let c_edge_x = if mbaff {
                    (mb_idx / 2 % mb_width) * 8 + (edge / 2) * 4
                } else {
                    mb_col * 8 + (edge / 2) * 4
                };
                for cseg in 0..2 {
                    let cy = mb_y / 2 + cseg * 4;
                    let c_bs = [
                        seg_bs[cseg * 2],
                        seg_bs[cseg * 2],
                        seg_bs[cseg * 2 + 1],
                        seg_bs[cseg * 2 + 1],
                    ];
                    for (plane, &(c_index_a, c_alpha, c_beta)) in [&mut frame.u, &mut frame.v]
                        .into_iter()
                        .zip(c_thresholds.iter())
                    {
                        for i in 0..4 {
                            let pbs = c_bs[i];
                            if pbs == 0 {
                                continue;
                            }
                            let ptc0 = if pbs < 4 {
                                TC0_TABLE[c_index_a][(pbs - 1) as usize] * scale_c
                            } else {
                                0
                            };
                            filter_edge_v_chroma(
                                plane,
                                stride_c,
                                c_edge_x,
                                cy + i,
                                1,
                                pbs,
                                c_alpha,
                                c_beta,
                                ptc0,
                                max_c,
                            );
                        }
                    }
                }
            }
        }

        // -- Horizontal edges (top to bottom) --
        for edge in 0..4 {
            let edge_y = mb_y + edge * 4;

            let is_mb_edge = edge == 0;

            // Check if there's an above neighbor
            if is_mb_edge {
                if mbaff {
                    // Top MB of first pair row has no above
                    let pair_addr = mb_idx / 2;
                    let pair_row = pair_addr / mb_width;
                    if mb_idx % 2 == 0 && pair_row == 0 {
                        continue;
                    }
                } else if mb_row == 0 {
                    continue;
                }
            }

            // 8x8 transform: skip internal odd edges (spec 8.7.2.1).
            if !is_mb_edge && (edge & 1) != 0 && mb_q.is_8x8dct {
                continue;
            }

            // Above neighbor MB
            let above_mb_idx = if !is_mb_edge {
                mb_idx // internal edge, same MB
            } else if mbaff {
                if mb_idx % 2 != 0 {
                    // Bottom MB: above = top of same pair
                    mb_idx - 1
                } else {
                    // Top MB: above = bottom of above pair
                    let pair_addr = mb_idx / 2;
                    (pair_addr - mb_width) * 2 + 1
                }
            } else {
                mb_idx - mb_width
            };
            let mb_p = &mb_info[above_mb_idx];

            let qp_q = mb_q.qp_y;
            let qp_p = mb_p.qp_y;
            let qp_avg = (qp_p.wrapping_add(qp_q).wrapping_add(1)) >> 1;
            let index_a = qp_avg.saturating_add(filter_offset_a).clamp(0, 51) as usize;
            let index_b = qp_avg.saturating_add(filter_offset_b).clamp(0, 51) as usize;
            let alpha = ALPHA_TABLE[index_a] * scale_y;
            let beta = BETA_TABLE[index_b] * scale_y;

            let c_thresholds = chroma_thresholds(qp_p, qp_q);

            // Compute bS for all 4 segments first
            let mut seg_bs = [0i32; 4];
            for seg in 0..4 {
                let blk_q = blk_idx(seg, edge);
                let blk_p = if is_mb_edge {
                    blk_idx(seg, 3)
                } else {
                    blk_idx(seg, edge - 1)
                };
                seg_bs[seg] = derive_bs(mb_p, mb_q, blk_p, blk_q, is_mb_edge);
            }

            // Luma: filter each segment independently
            for seg in 0..4 {
                if seg_bs[seg] == 0 {
                    continue;
                }
                let tc0 = if seg_bs[seg] < 4 {
                    TC0_TABLE[index_a][(seg_bs[seg] - 1) as usize] * scale_y
                } else {
                    0
                };
                let x = mb_x + seg * 4;
                filter_edge_h(
                    &mut frame.y,
                    stride_y,
                    x,
                    edge_y,
                    4,
                    seg_bs[seg],
                    alpha,
                    beta,
                    tc0,
                    max_y,
                );
            }

            // Chroma: per-pixel bS from all 4 luma segments
            if edge % 2 == 0 {
                let c_edge_y = mb_y / 2 + (edge / 2) * 4;
                for cseg in 0..2 {
                    let cx = mb_x / 2 + cseg * 4;
                    let c_bs = [
                        seg_bs[cseg * 2],
                        seg_bs[cseg * 2],
                        seg_bs[cseg * 2 + 1],
                        seg_bs[cseg * 2 + 1],
                    ];
                    for (plane, &(c_index_a, c_alpha, c_beta)) in [&mut frame.u, &mut frame.v]
                        .into_iter()
                        .zip(c_thresholds.iter())
                    {
                        for i in 0..4 {
                            let pbs = c_bs[i];
                            if pbs == 0 {
                                continue;
                            }
                            let ptc0 = if pbs < 4 {
                                TC0_TABLE[c_index_a][(pbs - 1) as usize] * scale_c
                            } else {
                                0
                            };
                            filter_edge_h_chroma(
                                plane,
                                stride_c,
                                cx + i,
                                c_edge_y,
                                1,
                                pbs,
                                c_alpha,
                                c_beta,
                                ptc0,
                                max_c,
                            );
                        }
                    }
                }
            }
        }
    }
}

/// Convert (column, row) in 4x4-block units to raster-scan 4x4 block index.
/// Layout: col 0-1 = left 8x8, col 2-3 = right 8x8; row 0-1 = top 8x8, row 2-3 = bottom.
/// Raster: 0,1,4,5 / 2,3,6,7 / 8,9,12,13 / 10,11,14,15
fn blk_idx(col: usize, row: usize) -> usize {
    let x8 = col / 2; // 8x8 block column (0 or 1)
    let y8 = row / 2; // 8x8 block row (0 or 1)
    let x4 = col % 2; // position within 8x8
    let y4 = row % 2;
    (y8 * 2 + x8) * 4 + y4 * 2 + x4
}

/// Derive boundary strength for an edge between two 4x4 blocks (spec 8.7.2.1).
///
/// `mb_p`/`mb_q`: macroblock info for each side of the edge.
/// `blk_p`/`blk_q`: 4x4 block index (0-15 raster) within each MB.
/// `is_mb_edge`: true if the edge is on a macroblock boundary.
fn derive_bs(mb_p: &MbInfo, mb_q: &MbInfo, blk_p: usize, blk_q: usize, is_mb_edge: bool) -> i32 {
    let intra_p = mb_p.mb_type == MbType::Intra || mb_p.mb_type == MbType::Ipcm;
    let intra_q = mb_q.mb_type == MbType::Intra || mb_q.mb_type == MbType::Ipcm;

    if intra_p || intra_q {
        return if is_mb_edge { 4 } else { 3 };
    }

    // bS=2: either side has non-zero coded coefficients
    if mb_p.nnz[blk_p] || mb_q.nnz[blk_q] {
        return 2;
    }

    // bS=1 or 0: check reference indices and motion vectors
    if check_mv_diff(mb_p, mb_q, blk_p, blk_q) {
        return 1;
    }

    0
}

/// Check if two 4x4 blocks have different motion (spec 8.7.2.1 conditions for bS=1).
/// Returns true if reference pictures differ or |MV_diff| >= 4 in any component.
fn check_mv_diff(mb_p: &MbInfo, mb_q: &MbInfo, blk_p: usize, blk_q: usize) -> bool {
    let list_count = mb_p.list_count.max(mb_q.list_count);

    if list_count <= 1 {
        // P-slice: single list comparison by picture identity (POC).
        // With ref_pic_list_modification, different ref_idx values can map
        // to the same reference picture (e.g., [POC4, POC4, POC0]).
        let rp = mb_p.ref_idx_l0[blk_p];
        let rq = mb_q.ref_idx_l0[blk_q];
        if rp < 0 && rq < 0 {
            return false; // both unused
        }
        if (rp < 0) != (rq < 0) {
            return true; // one used, one not
        }
        if mb_p.ref_poc_l0[blk_p] != mb_q.ref_poc_l0[blk_q] {
            return true;
        }
        if mv_diff_ge4(mb_p.mv_l0[blk_p], mb_q.mv_l0[blk_q]) {
            return true;
        }
        return false;
    }

    // B-slice: two-list comparison by actual picture identity (POC).
    // Spec 8.7.2.1: compare reference pictures (not list indices) and MVs.
    // First check straight (p_L0 vs q_L0, p_L1 vs q_L1).
    // If not, check swapped (p_L0 vs q_L1, p_L1 vs q_L0).
    let straight_match = refs_and_mvs_match(
        mb_p.ref_idx_l0[blk_p],
        mb_p.ref_poc_l0[blk_p],
        mb_p.mv_l0[blk_p],
        mb_q.ref_idx_l0[blk_q],
        mb_q.ref_poc_l0[blk_q],
        mb_q.mv_l0[blk_q],
    ) && refs_and_mvs_match(
        mb_p.ref_idx_l1[blk_p],
        mb_p.ref_poc_l1[blk_p],
        mb_p.mv_l1[blk_p],
        mb_q.ref_idx_l1[blk_q],
        mb_q.ref_poc_l1[blk_q],
        mb_q.mv_l1[blk_q],
    );

    if straight_match {
        return false;
    }

    // Try swapped: p_L0 vs q_L1 and p_L1 vs q_L0
    let swapped_match = refs_and_mvs_match(
        mb_p.ref_idx_l0[blk_p],
        mb_p.ref_poc_l0[blk_p],
        mb_p.mv_l0[blk_p],
        mb_q.ref_idx_l1[blk_q],
        mb_q.ref_poc_l1[blk_q],
        mb_q.mv_l1[blk_q],
    ) && refs_and_mvs_match(
        mb_p.ref_idx_l1[blk_p],
        mb_p.ref_poc_l1[blk_p],
        mb_p.mv_l1[blk_p],
        mb_q.ref_idx_l0[blk_q],
        mb_q.ref_poc_l0[blk_q],
        mb_q.mv_l0[blk_q],
    );

    !swapped_match
}

/// Check if reference picture and MV match between two blocks.
/// Compares by POC (picture identity) rather than list index.
fn refs_and_mvs_match(
    ref_a: i8,
    poc_a: i32,
    mv_a: [i16; 2],
    ref_b: i8,
    poc_b: i32,
    mv_b: [i16; 2],
) -> bool {
    if ref_a < 0 && ref_b < 0 {
        return true; // both unused
    }
    if ref_a < 0 || ref_b < 0 {
        return false; // one used, one not
    }
    if poc_a != poc_b {
        return false; // different pictures
    }
    !mv_diff_ge4(mv_a, mv_b)
}

/// Returns true if |mv_a - mv_b| >= 4 in either component (quarter-pel).
#[inline]
fn mv_diff_ge4(mv_a: [i16; 2], mv_b: [i16; 2]) -> bool {
    (mv_a[0] - mv_b[0]).unsigned_abs() >= 4 || (mv_a[1] - mv_b[1]).unsigned_abs() >= 4
}

/// Filter one 4-sample segment of a vertical edge.
/// `x` is the column of q0; p samples are at x-1, x-2, x-3. q samples at x, x+1, x+2.
/// `y` is the row of the first of `count` consecutive rows to filter.
#[allow(clippy::too_many_arguments)]
fn filter_edge_v(
    plane: &mut [u16],
    stride: usize,
    x: usize,
    y: usize,
    count: usize,
    bs: i32,
    alpha: i32,
    beta: i32,
    tc0: i32,
    max: i32,
) {
    filter_edge_v_inner(plane, stride, x, y, count, bs, alpha, beta, tc0, max, false);
}

#[allow(clippy::too_many_arguments)]
fn filter_edge_v_chroma(
    plane: &mut [u16],
    stride: usize,
    x: usize,
    y: usize,
    count: usize,
    bs: i32,
    alpha: i32,
    beta: i32,
    tc0: i32,
    max: i32,
) {
    filter_edge_v_inner(plane, stride, x, y, count, bs, alpha, beta, tc0, max, true);
}

#[allow(clippy::too_many_arguments)]
fn filter_edge_v_inner(
    plane: &mut [u16],
    stride: usize,
    x: usize,
    y: usize,
    count: usize,
    bs: i32,
    alpha: i32,
    beta: i32,
    tc0: i32,
    max: i32,
    is_chroma: bool,
) {
    for i in 0..count {
        let row = y + i;
        let idx_q0 = row * stride + x;
        // Guard against frame buffer overrun from malformed streams
        if x < 3 || idx_q0 + 3 >= plane.len() {
            continue;
        }
        let idx_p0 = idx_q0 - 1;

        let p0 = plane[idx_p0] as i32;
        let p1 = plane[idx_p0 - 1] as i32;
        let q0 = plane[idx_q0] as i32;
        let q1 = plane[idx_q0 + 1] as i32;

        if !should_filter(p0, p1, q0, q1, alpha, beta) {
            continue;
        }

        let p2 = plane[idx_p0 - 2] as i32;
        let q2 = plane[idx_q0 + 2] as i32;

        if bs == 4 {
            if is_chroma {
                // Spec 8.7.2.4: chroma strong filter only modifies p0 and q0
                plane[idx_p0] = ((2 * p1 + p0 + q1 + 2) >> 2) as u16;
                plane[idx_q0] = ((2 * q1 + q0 + p1 + 2) >> 2) as u16;
            } else {
                let (np0, np1, np2, nq0, nq1, nq2) = strong_filter(
                    p0,
                    p1,
                    p2,
                    plane[idx_p0 - 3] as i32,
                    q0,
                    q1,
                    q2,
                    plane[idx_q0 + 3] as i32,
                    alpha,
                    beta,
                );
                plane[idx_p0] = np0 as u16;
                plane[idx_p0 - 1] = np1 as u16;
                plane[idx_p0 - 2] = np2 as u16;
                plane[idx_q0] = nq0 as u16;
                plane[idx_q0 + 1] = nq1 as u16;
                plane[idx_q0 + 2] = nq2 as u16;
            }
        } else if is_chroma {
            // Spec 8.7.2.3: chroma normal filter — tc = tc0 + 1, only p0/q0 modified
            let tc = tc0 + 1;
            let delta = ((((q0 - p0) << 2) + (p1 - q1) + 4) >> 3).clamp(-tc, tc);
            plane[idx_p0] = (p0 + delta).clamp(0, max) as u16;
            plane[idx_q0] = (q0 - delta).clamp(0, max) as u16;
        } else {
            let (np0, np1, nq0, nq1) = normal_filter(p0, p1, p2, q0, q1, q2, tc0, beta, max);
            plane[idx_p0] = np0 as u16;
            plane[idx_p0 - 1] = np1 as u16;
            plane[idx_q0] = nq0 as u16;
            plane[idx_q0 + 1] = nq1 as u16;
        }
    }
}

/// Filter one 4-sample segment of a horizontal edge.
/// `y` is the row of q0; p samples are at y-1, y-2, y-3. q samples at y, y+1, y+2.
#[allow(clippy::too_many_arguments)]
fn filter_edge_h(
    plane: &mut [u16],
    stride: usize,
    x: usize,
    y: usize,
    count: usize,
    bs: i32,
    alpha: i32,
    beta: i32,
    tc0: i32,
    max: i32,
) {
    filter_edge_h_inner(plane, stride, x, y, count, bs, alpha, beta, tc0, max, false);
}

#[allow(clippy::too_many_arguments)]
fn filter_edge_h_chroma(
    plane: &mut [u16],
    stride: usize,
    x: usize,
    y: usize,
    count: usize,
    bs: i32,
    alpha: i32,
    beta: i32,
    tc0: i32,
    max: i32,
) {
    filter_edge_h_inner(plane, stride, x, y, count, bs, alpha, beta, tc0, max, true);
}

#[allow(clippy::too_many_arguments)]
fn filter_edge_h_inner(
    plane: &mut [u16],
    stride: usize,
    x: usize,
    y: usize,
    count: usize,
    bs: i32,
    alpha: i32,
    beta: i32,
    tc0: i32,
    max: i32,
    is_chroma: bool,
) {
    for i in 0..count {
        let col = x + i;
        let idx_q0 = y * stride + col;
        // Guard against frame buffer overrun from malformed streams
        if y < 3 || idx_q0 + 3 * stride >= plane.len() {
            continue;
        }
        let idx_p0 = idx_q0 - stride;

        let p0 = plane[idx_p0] as i32;
        let p1 = plane[idx_p0 - stride] as i32;
        let q0 = plane[idx_q0] as i32;
        let q1 = plane[idx_q0 + stride] as i32;

        if !should_filter(p0, p1, q0, q1, alpha, beta) {
            continue;
        }

        let p2 = plane[idx_p0 - 2 * stride] as i32;
        let q2 = plane[idx_q0 + 2 * stride] as i32;

        if bs == 4 {
            if is_chroma {
                // Spec 8.7.2.4: chroma strong filter only modifies p0 and q0
                plane[idx_p0] = ((2 * p1 + p0 + q1 + 2) >> 2) as u16;
                plane[idx_q0] = ((2 * q1 + q0 + p1 + 2) >> 2) as u16;
            } else {
                let (np0, np1, np2, nq0, nq1, nq2) = strong_filter(
                    p0,
                    p1,
                    p2,
                    plane[idx_p0 - 3 * stride] as i32,
                    q0,
                    q1,
                    q2,
                    plane[idx_q0 + 3 * stride] as i32,
                    alpha,
                    beta,
                );
                plane[idx_p0] = np0 as u16;
                plane[idx_p0 - stride] = np1 as u16;
                plane[idx_p0 - 2 * stride] = np2 as u16;
                plane[idx_q0] = nq0 as u16;
                plane[idx_q0 + stride] = nq1 as u16;
                plane[idx_q0 + 2 * stride] = nq2 as u16;
            }
        } else if is_chroma {
            // Spec 8.7.2.3: chroma normal filter modifies only p0 and q0
            // with tc = tc0 + 1 (no ap/aq adjustment)
            let tc = tc0 + 1;
            let delta = ((((q0 - p0) << 2) + (p1 - q1) + 4) >> 3).clamp(-tc, tc);
            plane[idx_p0] = (p0 + delta).clamp(0, max) as u16;
            plane[idx_q0] = (q0 - delta).clamp(0, max) as u16;
        } else {
            let (np0, np1, nq0, nq1) = normal_filter(p0, p1, p2, q0, q1, q2, tc0, beta, max);
            plane[idx_p0] = np0 as u16;
            plane[idx_p0 - stride] = np1 as u16;
            plane[idx_q0] = nq0 as u16;
            plane[idx_q0 + stride] = nq1 as u16;
        }
    }
}

/// Check whether filtering should be applied (spec 8.7.2.3 condition).
#[inline]
fn should_filter(p0: i32, p1: i32, q0: i32, q1: i32, alpha: i32, beta: i32) -> bool {
    (p0 - q0).abs() < alpha && (p1 - p0).abs() < beta && (q1 - q0).abs() < beta
}

/// Normal filter for bS=1..3 (H.264 spec 8.7.2.3).
/// Returns (p0', p1', q0', q1').
#[allow(clippy::too_many_arguments)]
fn normal_filter(
    p0: i32,
    p1: i32,
    p2: i32,
    q0: i32,
    q1: i32,
    q2: i32,
    tc0: i32,
    beta: i32,
    max: i32,
) -> (i32, i32, i32, i32) {
    let ap = (p2 - p0).abs();
    let aq = (q2 - q0).abs();
    let avg = (p0 + q0 + 1) >> 1;

    // p1' and q1' are computed first; tc is incremented as a side effect
    let mut tc = tc0;

    let new_p1 = if ap < beta {
        tc += 1;
        if tc0 != 0 {
            p1 + (((p2 + avg) >> 1) - p1).clamp(-tc0, tc0)
        } else {
            p1
        }
    } else {
        p1
    };

    let new_q1 = if aq < beta {
        tc += 1;
        if tc0 != 0 {
            q1 + (((q2 + avg) >> 1) - q1).clamp(-tc0, tc0)
        } else {
            q1
        }
    } else {
        q1
    };

    // p0' and q0' use the final tc (after increments)
    let delta = ((((q0 - p0) << 2) + (p1 - q1) + 4) >> 3).clamp(-tc, tc);
    let new_p0 = (p0 + delta).clamp(0, max);
    let new_q0 = (q0 - delta).clamp(0, max);

    (new_p0, new_p1, new_q0, new_q1)
}

/// Strong filter for bS=4 (spec 8.7.2.4).
/// Returns (p0', p1', p2', q0', q1', q2').
#[allow(clippy::too_many_arguments)]
fn strong_filter(
    p0: i32,
    p1: i32,
    p2: i32,
    p3: i32,
    q0: i32,
    q1: i32,
    q2: i32,
    q3: i32,
    alpha: i32,
    beta: i32,
) -> (i32, i32, i32, i32, i32, i32) {
    let ap = (p2 - p0).abs();
    let aq = (q2 - q0).abs();
    let small_gap = (p0 - q0).abs() < ((alpha >> 2) + 2);

    // p-side
    let (np0, np1, np2) = if small_gap && ap < beta {
        (
            (p2 + 2 * p1 + 2 * p0 + 2 * q0 + q1 + 4) >> 3,
            (p2 + p1 + p0 + q0 + 2) >> 2,
            (2 * p3 + 3 * p2 + p1 + p0 + q0 + 4) >> 3,
        )
    } else {
        ((2 * p1 + p0 + q1 + 2) >> 2, p1, p2)
    };

    // q-side (mirror)
    let (nq0, nq1, nq2) = if small_gap && aq < beta {
        (
            (q2 + 2 * q1 + 2 * q0 + 2 * p0 + p1 + 4) >> 3,
            (q2 + q1 + q0 + p0 + 2) >> 2,
            (2 * q3 + 3 * q2 + q1 + q0 + p0 + 4) >> 3,
        )
    } else {
        ((2 * q1 + q0 + p1 + 2) >> 2, q1, q2)
    };

    (np0, np1, np2, nq0, nq1, nq2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_filter() {
        // Large difference → filter
        assert!(should_filter(100, 100, 200, 200, 255, 18));
        // p0-q0 difference >= alpha → no filter
        assert!(!should_filter(0, 0, 255, 255, 100, 18));
        // p1-p0 difference >= beta → no filter
        assert!(!should_filter(100, 80, 110, 110, 255, 5));
    }

    #[test]
    fn test_strong_filter_uniform() {
        // Uniform block: all 128. Filter should not change anything meaningful.
        let (p0, p1, p2, q0, q1, q2) =
            strong_filter(128, 128, 128, 128, 128, 128, 128, 128, 255, 18);
        assert_eq!((p0, p1, p2, q0, q1, q2), (128, 128, 128, 128, 128, 128));
    }

    #[test]
    fn test_strong_filter_step_edge() {
        // Sharp step: p-side=0, q-side=255
        let (p0, _p1, _p2, q0, _q1, _q2) = strong_filter(0, 0, 0, 0, 255, 255, 255, 255, 255, 18);
        // small_gap = |0-255| < (255/4 + 2) = 65 → false
        // ap = |0 - 0| = 0 < 18 → true, but small_gap is false
        // So weak path: p0' = (2*0 + 0 + 255 + 2) >> 2 = 64
        //               q0' = (2*255 + 255 + 0 + 2) >> 2 = 192 (actually 191)
        assert_eq!(p0, 64);
        // q0' = (510 + 255 + 0 + 2) >> 2 = 767 >> 2 = 191
        assert_eq!(q0, 191);
    }

    #[test]
    fn test_normal_filter() {
        // Small step across edge with bS=3, moderate QP
        let (p0, _p1, q0, _q1) = normal_filter(120, 120, 120, 130, 130, 130, 1, 18, 255);
        // ap = 0 < 18, aq = 0 < 18 → tc = 1 + 1 + 1 = 3
        // delta = ((10*4 + 120-130 + 4) >> 3) clamped to [-3,3]
        //       = (40 - 10 + 4) >> 3 = 34 >> 3 = 4, clamped to 3
        assert_eq!(p0, 123);
        assert_eq!(q0, 127);
    }

    #[test]
    fn test_filter_vertical_edge() {
        // 8x4 plane with a step at column 4
        let mut plane = vec![0u16; 8 * 4];
        let stride = 8;
        for row in 0..4 {
            for col in 0..4 {
                plane[row * stride + col] = 100;
            }
            for col in 4..8 {
                plane[row * stride + col] = 200;
            }
        }

        // bS=3, generous thresholds so filtering occurs
        filter_edge_v(&mut plane, stride, 4, 0, 4, 3, 255, 255, 3, 255);

        // After filtering, the step should be smoothed
        // p0 (col 3) should increase, q0 (col 4) should decrease
        for row in 0..4 {
            assert!(plane[row * stride + 3] > 100, "p0 should increase");
            assert!(plane[row * stride + 4] < 200, "q0 should decrease");
        }
    }

    #[test]
    fn test_filter_horizontal_edge() {
        // 4x8 plane with a step at row 4
        let mut plane = vec![0u16; 4 * 8];
        let stride = 4;
        for row in 0..4 {
            for col in 0..4 {
                plane[row * stride + col] = 100;
            }
        }
        for row in 4..8 {
            for col in 0..4 {
                plane[row * stride + col] = 200;
            }
        }

        filter_edge_h(&mut plane, stride, 0, 4, 4, 3, 255, 255, 3, 255);

        for col in 0..4 {
            assert!(plane[3 * stride + col] > 100, "p0 should increase");
            assert!(plane[4 * stride + col] < 200, "q0 should decrease");
        }
    }

    #[test]
    fn test_normal_filter_clips_to_bit_depth() {
        // 10-bit samples near the top of the range must clip to 1023, not 255.
        let (p0, _p1, q0, _q1) = normal_filter(1000, 1000, 1000, 1023, 1023, 1023, 40, 80, 1023);
        assert!(p0 > 1000 && p0 <= 1023);
        assert!(q0 <= 1023 && q0 > 255);
    }
}
