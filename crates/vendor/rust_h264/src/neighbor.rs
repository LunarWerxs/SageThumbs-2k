//! CABAC/CAVLC neighbor context helpers.
//!
//! Functions for deriving neighbor-dependent context values used in
//! CABAC syntax element decoding and CAVLC nC computation.

use crate::residual::OFFSET_TO_BLOCK;

#[inline(always)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn cabac_amvd(
    mvd_store: &[[i16; 2]],
    mb_idx: usize,
    mb_width: usize,
    py: usize,
    px: usize,
    comp: usize, // comp: 0=x, 1=y
    mb_slice_id: &[u16],
    cur_slice_id: u16,
    mbaff: bool,
    _mb_field_decoding: &[bool],
) -> u32 {
    // Compute left and above neighbor MB indices (MBAFF-aware)
    let (has_left, left_mb_idx, has_above, above_mb_idx) = if !mbaff {
        let has_left = !mb_idx.is_multiple_of(mb_width);
        let left = if has_left { mb_idx - 1 } else { 0 };
        let has_above = mb_idx >= mb_width;
        let above = if has_above { mb_idx - mb_width } else { 0 };
        (has_left, left, has_above, above)
    } else {
        let pair_addr = mb_idx / 2;
        let has_left = !pair_addr.is_multiple_of(mb_width);
        let left = if has_left {
            (pair_addr - 1) * 2 + (mb_idx % 2)
        } else {
            0
        };
        let is_field = _mb_field_decoding[pair_addr];
        let has_above = if is_field {
            // Field-coded: above is same-field in above pair
            pair_addr >= mb_width
        } else {
            // Frame-coded: above is top of same pair (always available for bottom)
            !mb_idx.is_multiple_of(2) || pair_addr >= mb_width
        };
        let above = if !has_above {
            0
        } else if !mb_idx.is_multiple_of(2) && !is_field {
            mb_idx - 1 // Frame-coded: top of same pair
        } else {
            (pair_addr - mb_width) * 2 + 1
        };
        (has_left, left, has_above, above)
    };

    // Left neighbor
    let left_mvd = if px > 0 {
        // Within MB: block to the left at (py, px-4)
        let blk = OFFSET_TO_BLOCK[py / 4][(px - 4) / 4];
        mvd_store[mb_idx * 16 + blk][comp].unsigned_abs() as u32
    } else if has_left {
        // Left MB: rightmost column, same row
        if mb_slice_id[left_mb_idx] != cur_slice_id {
            0
        } else {
            let blk = OFFSET_TO_BLOCK[py / 4][3];
            mvd_store[left_mb_idx * 16 + blk][comp].unsigned_abs() as u32
        }
    } else {
        0
    };

    // Top neighbor
    let top_mvd = if py > 0 {
        let blk = OFFSET_TO_BLOCK[(py - 4) / 4][px / 4];
        mvd_store[mb_idx * 16 + blk][comp].unsigned_abs() as u32
    } else if has_above {
        if mb_slice_id[above_mb_idx] != cur_slice_id {
            0
        } else {
            let blk = OFFSET_TO_BLOCK[3][px / 4];
            mvd_store[above_mb_idx * 16 + blk][comp].unsigned_abs() as u32
        }
    } else {
        0
    };

    left_mvd + top_mvd
}

#[allow(clippy::too_many_arguments)]
/// CABAC ref_idx neighbor context for a partition.
#[inline(always)]
/// Returns (left_ref, top_ref) from neighbor blocks.
/// For B-slices, direct-mode neighbors are treated as ref=0 (spec 9.3.3.1.1.4).
pub(crate) fn cabac_neighbor_ref(
    ref_idx_store: &[i8],
    mb_idx: usize,
    mb_width: usize,
    py: usize,
    px: usize,
    mb_slice_id: &[u16],
    cur_slice_id: u16,
    _mb_is_direct: &[bool],
    blk_is_direct: &[bool],
    is_b_slice: bool,
    mbaff: bool,
    _mb_field_decoding: &[bool],
) -> (i8, i8) {
    // Compute left and above neighbor MB indices (MBAFF-aware)
    let (has_left, left_mb_idx, has_above, above_mb_idx) = if !mbaff {
        let has_left = !mb_idx.is_multiple_of(mb_width);
        let left = if has_left { mb_idx - 1 } else { 0 };
        let has_above = mb_idx >= mb_width;
        let above = if has_above { mb_idx - mb_width } else { 0 };
        (has_left, left, has_above, above)
    } else {
        let pair_addr = mb_idx / 2;
        let has_left = !pair_addr.is_multiple_of(mb_width);
        let left = if has_left {
            (pair_addr - 1) * 2 + (mb_idx % 2)
        } else {
            0
        };
        let is_field = _mb_field_decoding[pair_addr];
        let has_above = if is_field {
            // Field-coded: above is same-field in above pair
            pair_addr >= mb_width
        } else {
            // Frame-coded: above is top of same pair (always available for bottom)
            !mb_idx.is_multiple_of(2) || pair_addr >= mb_width
        };
        let above = if !has_above {
            0
        } else if !mb_idx.is_multiple_of(2) && !is_field {
            mb_idx - 1 // Frame-coded: top of same pair
        } else {
            (pair_addr - mb_width) * 2 + 1
        };
        (has_left, left, has_above, above)
    };

    let left_ref = if px > 0 {
        let blk = OFFSET_TO_BLOCK[py / 4][(px - 4) / 4];
        if is_b_slice && blk_is_direct[mb_idx * 16 + blk] {
            0
        } else {
            ref_idx_store[mb_idx * 16 + blk]
        }
    } else if has_left {
        if mb_slice_id[left_mb_idx] != cur_slice_id {
            -1
        } else {
            let blk = OFFSET_TO_BLOCK[py / 4][3];
            if is_b_slice && blk_is_direct[left_mb_idx * 16 + blk] {
                0
            } else {
                ref_idx_store[left_mb_idx * 16 + blk]
            }
        }
    } else {
        -1
    };

    let top_ref = if py > 0 {
        let blk = OFFSET_TO_BLOCK[(py - 4) / 4][px / 4];
        if is_b_slice && blk_is_direct[mb_idx * 16 + blk] {
            0
        } else {
            ref_idx_store[mb_idx * 16 + blk]
        }
    } else if has_above {
        if mb_slice_id[above_mb_idx] != cur_slice_id {
            -1
        } else {
            let blk = OFFSET_TO_BLOCK[3][px / 4];
            if is_b_slice && blk_is_direct[above_mb_idx * 16 + blk] {
                0
            } else {
                ref_idx_store[above_mb_idx * 16 + blk]
            }
        }
    } else {
        -1
    };

    (left_ref, top_ref)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn cabac_neighbor_nz_luma(
    nc_luma: &[u8],
    mb_idx: usize,
    mb_width: usize,
    blk: usize,
    is_left: bool,
    is_intra: bool,
    mb_slice_id: &[u16],
    cur_slice_id: u16,
    mbaff: bool,
    _mb_field_decoding: &[bool],
) -> bool {
    // Neighbor block indices: within-MB (>=0) or cross-MB (negative, encoded as -(blk+1))
    #[rustfmt::skip]
    const LEFT: [i8; 16] = [-6, 0, -8, 2, 1, 4, 3, 6, -14, 8, -16, 10, 9, 12, 11, 14];
    #[rustfmt::skip]
    const TOP: [i8; 16] = [-11, -12, 0, 1, -15, -16, 4, 5, 2, 3, 8, 9, 6, 7, 12, 13];

    let neighbor = if is_left { LEFT[blk] } else { TOP[blk] };

    if neighbor >= 0 {
        // Same MB
        nc_luma[mb_idx * 16 + neighbor as usize] > 0
    } else {
        // Cross-MB: decode the encoded block index
        let neighbor_blk = (-(neighbor + 1)) as usize;
        let neighbor_mb = if is_left {
            let has_left = if !mbaff {
                mb_idx.checked_rem(mb_width) != Some(0)
            } else {
                !(mb_idx / 2).is_multiple_of(mb_width)
            };
            if !has_left {
                return is_intra;
            }
            if !mbaff {
                mb_idx - 1
            } else {
                (mb_idx / 2 - 1) * 2 + (mb_idx % 2)
            }
        } else {
            let has_above = if !mbaff {
                mb_idx >= mb_width
            } else if _mb_field_decoding[mb_idx / 2] {
                (mb_idx / 2) >= mb_width
            } else {
                !mb_idx.is_multiple_of(2) || (mb_idx / 2) >= mb_width
            };
            if !has_above {
                return is_intra;
            }
            if !mbaff {
                mb_idx - mb_width
            } else if !mb_idx.is_multiple_of(2) && !_mb_field_decoding[mb_idx / 2] {
                mb_idx - 1 // Frame-coded: top of same pair
            } else {
                ((mb_idx / 2) - mb_width) * 2 + 1
            }
        };
        if mb_slice_id[neighbor_mb] != cur_slice_id {
            return is_intra;
        }
        nc_luma[neighbor_mb * 16 + neighbor_blk] > 0
    }
}

/// CABAC coded_block_flag neighbor lookup for chroma 4x4 blocks (4 blocks per MB).
#[allow(clippy::too_many_arguments)]
pub(crate) fn cabac_neighbor_nz_chroma(
    nc_chroma: &[u8],
    mb_idx: usize,
    mb_width: usize,
    blk: usize,
    is_left: bool,
    is_intra: bool,
    mb_slice_id: &[u16],
    cur_slice_id: u16,
    mbaff: bool,
    _mb_field_decoding: &[bool],
) -> bool {
    // Chroma block layout: 0=(0,0), 1=(0,4), 2=(4,0), 3=(4,4)
    // Left neighbors: blk0→left_mb blk1, blk1→blk0, blk2→left_mb blk3, blk3→blk2
    // Top neighbors: blk0→top_mb blk2, blk1→top_mb blk3, blk2→blk0, blk3→blk1
    let (same_mb_neighbor, cross_mb_blk) = if is_left {
        match blk {
            0 => (None, Some(1)),
            1 => (Some(0), None),
            2 => (None, Some(3)),
            3 => (Some(2), None),
            _ => (None, None),
        }
    } else {
        match blk {
            0 => (None, Some(2)),
            1 => (None, Some(3)),
            2 => (Some(0), None),
            3 => (Some(1), None),
            _ => (None, None),
        }
    };

    if let Some(nb) = same_mb_neighbor {
        nc_chroma[mb_idx * 4 + nb] > 0
    } else if let Some(nb) = cross_mb_blk {
        let neighbor_mb = if is_left {
            let has_left = if !mbaff {
                mb_idx.checked_rem(mb_width) != Some(0)
            } else {
                !(mb_idx / 2).is_multiple_of(mb_width)
            };
            if !has_left {
                return is_intra;
            }
            if !mbaff {
                mb_idx - 1
            } else {
                (mb_idx / 2 - 1) * 2 + (mb_idx % 2)
            }
        } else {
            let has_above = if !mbaff {
                mb_idx >= mb_width
            } else if _mb_field_decoding[mb_idx / 2] {
                (mb_idx / 2) >= mb_width
            } else {
                !mb_idx.is_multiple_of(2) || (mb_idx / 2) >= mb_width
            };
            if !has_above {
                return is_intra;
            }
            if !mbaff {
                mb_idx - mb_width
            } else if !mb_idx.is_multiple_of(2) && !_mb_field_decoding[mb_idx / 2] {
                mb_idx - 1 // Frame-coded: top of same pair
            } else {
                ((mb_idx / 2) - mb_width) * 2 + 1
            }
        };
        if mb_slice_id[neighbor_mb] != cur_slice_id {
            return is_intra;
        }
        nc_chroma[neighbor_mb * 4 + nb] > 0
    } else {
        false
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn predict_i4x4_mode(
    modes: &[u8],
    mb_idx: usize,
    mb_width: usize,
    blk_idx: usize,
    mb_slice_id: &[u16],
    cur_slice_id: u16,
    intra_avail: &[bool],
    mbaff: bool,
    mb_field_decoding: &[bool],
) -> u8 {
    // None = neighbor unavailable (picture boundary or non-I4x4 neighbor MB that
    // doesn't exist). When either is None, predicted mode defaults to DC (2).
    let mode_a = get_neighbor_i4x4_mode(
        modes,
        mb_idx,
        mb_width,
        blk_idx,
        true,
        mb_slice_id,
        cur_slice_id,
        intra_avail,
        mbaff,
        mb_field_decoding,
    );
    let mode_b = get_neighbor_i4x4_mode(
        modes,
        mb_idx,
        mb_width,
        blk_idx,
        false,
        mb_slice_id,
        cur_slice_id,
        intra_avail,
        mbaff,
        mb_field_decoding,
    );
    match (mode_a, mode_b) {
        (Some(a), Some(b)) => a.min(b),
        _ => 2, // DC when either neighbor is unavailable
    }
}

/// `intra_avail`: per-MB flag, true if the MB is available for intra prediction
/// (always true unless constrained_intra_pred_flag is set and the neighbor is inter).
#[allow(clippy::too_many_arguments)]
pub(crate) fn get_neighbor_i4x4_mode(
    modes: &[u8],
    mb_idx: usize,
    mb_width: usize,
    blk_idx: usize,
    is_left: bool,
    mb_slice_id: &[u16],
    cur_slice_id: u16,
    intra_avail: &[bool],
    mbaff: bool,
    _mb_field_decoding: &[bool],
) -> Option<u8> {
    // Block layout:  0  1 | 4  5
    //                2  3 | 6  7
    //               ------+------
    //                8  9 |12 13
    //               10 11 |14 15
    if is_left {
        match blk_idx {
            1 | 5 | 9 | 13 => Some(modes[mb_idx * 16 + blk_idx - 1]),
            3 | 7 | 11 | 15 => Some(modes[mb_idx * 16 + blk_idx - 1]),
            4 => Some(modes[mb_idx * 16 + 1]),
            6 => Some(modes[mb_idx * 16 + 3]),
            12 => Some(modes[mb_idx * 16 + 9]),
            14 => Some(modes[mb_idx * 16 + 11]),
            0 | 2 | 8 | 10 => {
                // Left edge of MB
                let has_left = if !mbaff {
                    !mb_idx.is_multiple_of(mb_width)
                } else {
                    !(mb_idx / 2).is_multiple_of(mb_width)
                };
                let left_mb = if !has_left {
                    0 // dummy, not used
                } else if !mbaff {
                    mb_idx - 1
                } else {
                    (mb_idx / 2 - 1) * 2 + (mb_idx % 2)
                };
                if has_left && mb_slice_id[left_mb] == cur_slice_id && intra_avail[left_mb] {
                    let left_blk = match blk_idx {
                        0 => 5,
                        2 => 7,
                        8 => 13,
                        10 => 15,
                        _ => unreachable!(),
                    };
                    Some(modes[left_mb * 16 + left_blk])
                } else {
                    None
                }
            }
            _ => None,
        }
    } else {
        match blk_idx {
            2 | 6 | 10 | 14 => Some(modes[mb_idx * 16 + blk_idx - 2]),
            3 | 7 | 11 | 15 => Some(modes[mb_idx * 16 + blk_idx - 2]),
            8 => Some(modes[mb_idx * 16 + 2]),
            9 => Some(modes[mb_idx * 16 + 3]),
            12 => Some(modes[mb_idx * 16 + 6]),
            13 => Some(modes[mb_idx * 16 + 7]),
            0 | 1 | 4 | 5 => {
                // Top edge of MB
                let has_above = if !mbaff {
                    mb_idx >= mb_width
                } else if _mb_field_decoding[mb_idx / 2] {
                    (mb_idx / 2) >= mb_width
                } else {
                    !mb_idx.is_multiple_of(2) || (mb_idx / 2) >= mb_width
                };
                let above_mb = if !has_above {
                    0 // dummy, not used
                } else if !mbaff {
                    mb_idx - mb_width
                } else if !mb_idx.is_multiple_of(2) && !_mb_field_decoding[mb_idx / 2] {
                    mb_idx - 1 // Frame-coded: top of same pair
                } else {
                    ((mb_idx / 2) - mb_width) * 2 + 1
                };
                if has_above && mb_slice_id[above_mb] == cur_slice_id && intra_avail[above_mb] {
                    let above_blk = match blk_idx {
                        0 => 10,
                        1 => 11,
                        4 => 14,
                        5 => 15,
                        _ => unreachable!(),
                    };
                    Some(modes[above_mb * 16 + above_blk])
                } else {
                    None // picture top boundary
                }
            }
            _ => None,
        }
    }
}

/// Compute nC for a 4x4 block from left (A) and above (B) neighbors.
/// H.264 spec 9.2.1: nC = average of neighbor total_coeff values.
#[allow(clippy::too_many_arguments)]
pub(crate) fn compute_nc(
    nc_array: &[u8],
    mb_idx: usize,
    mb_width: usize,
    blk_idx: usize,
    blks_per_mb: usize,
    mb_slice_id: &[u16],
    cur_slice_id: u16,
    mbaff: bool,
    _mb_field_decoding: &[bool],
) -> i32 {
    let (left_blk, left_in_mb) = if blks_per_mb == 16 {
        match blk_idx {
            0 => (5usize, false),
            2 => (7, false),
            8 => (13, false),
            10 => (15, false),
            4 => (1, true),
            6 => (3, true),
            12 => (9, true),
            14 => (11, true),
            1 => (0, true),
            3 => (2, true),
            5 => (4, true),
            7 => (6, true),
            9 => (8, true),
            11 => (10, true),
            13 => (12, true),
            15 => (14, true),
            _ => unreachable!(),
        }
    } else {
        match blk_idx {
            0 => (1, false),
            2 => (3, false),
            1 => (0, true),
            3 => (2, true),
            _ => unreachable!(),
        }
    };

    let nc_a: Option<u8> = if left_in_mb {
        Some(nc_array[mb_idx * blks_per_mb + left_blk])
    } else {
        let has_left = if !mbaff {
            !mb_idx.is_multiple_of(mb_width)
        } else {
            !(mb_idx / 2).is_multiple_of(mb_width)
        };
        if has_left {
            let left_nb = if !mbaff {
                mb_idx - 1
            } else {
                (mb_idx / 2 - 1) * 2 + (mb_idx % 2)
            };
            if mb_slice_id[left_nb] == cur_slice_id {
                Some(nc_array[left_nb * blks_per_mb + left_blk])
            } else {
                None
            }
        } else {
            None
        }
    };

    let (above_blk, above_in_mb) = if blks_per_mb == 16 {
        match blk_idx {
            0 => (10usize, false),
            1 => (11, false),
            4 => (14, false),
            5 => (15, false),
            2 => (0, true),
            3 => (1, true),
            6 => (4, true),
            7 => (5, true),
            8 => (2, true),
            9 => (3, true),
            10 => (8, true),
            11 => (9, true),
            12 => (6, true),
            13 => (7, true),
            14 => (12, true),
            15 => (13, true),
            _ => unreachable!(),
        }
    } else {
        match blk_idx {
            0 => (2, false),
            1 => (3, false),
            2 => (0, true),
            3 => (1, true),
            _ => unreachable!(),
        }
    };

    let nc_b: Option<u8> = if above_in_mb {
        Some(nc_array[mb_idx * blks_per_mb + above_blk])
    } else {
        let has_above = if !mbaff {
            mb_idx >= mb_width
        } else if _mb_field_decoding[mb_idx / 2] {
            (mb_idx / 2) >= mb_width
        } else {
            !mb_idx.is_multiple_of(2) || (mb_idx / 2) >= mb_width
        };
        if has_above {
            let above_nb = if !mbaff {
                mb_idx - mb_width
            } else if !mb_idx.is_multiple_of(2) && !_mb_field_decoding[mb_idx / 2] {
                mb_idx - 1 // Frame-coded: top of same pair
            } else {
                ((mb_idx / 2) - mb_width) * 2 + 1
            };
            if mb_slice_id[above_nb] == cur_slice_id {
                Some(nc_array[above_nb * blks_per_mb + above_blk])
            } else {
                None
            }
        } else {
            None
        }
    };

    match (nc_a, nc_b) {
        (Some(a), Some(b)) => (a as i32 + b as i32 + 1) >> 1,
        (Some(n), None) | (None, Some(n)) => n as i32,
        (None, None) => 0,
    }
}

/// Dequantize AC coefficients in a 4x4 block in raster order (skip DC at [0][0]).
pub(crate) fn dequant_4x4_ac_raster(block: &mut [i32; 16], qp: i32, scale: &[u8; 16]) {
    use crate::residual::ZIGZAG_4X4;

    let qp_per = qp / 6;
    let qp_rem = (qp % 6) as usize;

    const LEVEL_SCALE: [[i32; 3]; 6] = [
        [10, 13, 16],
        [11, 14, 18],
        [13, 16, 20],
        [14, 18, 23],
        [16, 20, 25],
        [18, 23, 29],
    ];

    for r in 0..4 {
        for c in 0..4 {
            if r == 0 && c == 0 {
                continue; // DC already handled
            }
            let idx = r * 4 + c;
            if block[idx] != 0 {
                let pc = (r & 1) + (c & 1);
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
