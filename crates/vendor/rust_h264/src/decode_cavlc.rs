//! CAVLC macroblock decode for non-skip MBs.

use crate::bitstream::BitstreamReader;
use crate::cavlc::parse_residual_block_cavlc;
use crate::deblock::{MbInfo, MbType};
use crate::error::DecodeError;
use crate::inter_pred;
use crate::intra_pred::predict_chroma_8x8;
use crate::mv_pred::{predict_mv, predict_mv_sub, ref_pic_safe, MbaffCtx};
use crate::neighbor::{compute_nc, dequant_4x4_ac_raster, predict_i4x4_mode};
use crate::residual::{
    dequant_4x4_full, dequant_8x8, dequant_chroma_dc, dequant_luma_dc_i16x16, inverse_dct_4x4,
    inverse_dct_8x8, inverse_hadamard_2x2, inverse_hadamard_4x4, BLOCK_INDEX_TO_OFFSET,
    CBP_INTER_TABLE, CBP_INTRA_TABLE, OFFSET_TO_BLOCK, ZIGZAG_4X4, ZIGZAG_4X4_FIELD,
    ZIGZAG_8X8_CAVLC, ZIGZAG_8X8_CAVLC_FIELD,
};
use crate::slice_context::{SliceContext, SliceParams};

impl SliceContext<'_> {
    /// Decode a single non-skip CAVLC macroblock (P inter, B inter, or intra).
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::needless_range_loop)]
    pub(crate) fn decode_cavlc_mb(
        &mut self,
        reader: &mut BitstreamReader,
        mb_idx: usize,
        mb_x: usize,
        mb_y: usize,
        sp: &SliceParams,
    ) -> Result<(), DecodeError> {
        // Select field or frame coefficient scan tables
        let field_scan = self.is_field_scan(mb_idx);
        let zigzag_4x4 = if field_scan {
            &ZIGZAG_4X4_FIELD
        } else {
            &ZIGZAG_4X4
        };
        let zigzag_8x8_cavlc = if field_scan {
            &ZIGZAG_8X8_CAVLC_FIELD
        } else {
            &ZIGZAG_8X8_CAVLC
        };

        let raw_mb_type = reader.read_ue()?;
        // For P slices, mb_type >= 5 means intra (subtract 5)
        // For B slices, mb_type >= 23 means intra (subtract 23)
        let inter_limit = if sp.is_p_slice {
            5
        } else if sp.is_b_slice {
            23
        } else {
            0
        };
        let (mb_type, is_inter) = if (sp.is_p_slice || sp.is_b_slice) && raw_mb_type < inter_limit {
            (raw_mb_type, true)
        } else {
            (raw_mb_type - inter_limit, false)
        };
        if is_inter && sp.is_b_slice {
            // === Inter (B) macroblock ===
            // Table 7-11: mb_type 0=B_Direct_16x16, 1=B_L0_16x16,
            // 2=B_L1_16x16, 3=B_Bi_16x16, 4-21=16x8/8x16 variants, 22=B_8x8
            let mut no_sub_less_8x8_b = true; // for transform_size_8x8_flag
            struct SubPart {
                x: usize,
                y: usize,
                w: usize,
                h: usize,
                ref_idx_l0: i8,
                ref_idx_l1: i8,
                mv_l0: [i16; 2],
                mv_l1: [i16; 2],
                pred_l0: bool,
                pred_l1: bool,
            }
            let mut sub_parts: Vec<SubPart> = Vec::new();

            match mb_type {
                1 | 2 => {
                    // B_L0_16x16 (mb_type=1) or B_L1_16x16 (mb_type=2)
                    let is_l0 = mb_type == 1;
                    let num_active = if is_l0 {
                        sp.num_ref_idx_l0_active
                    } else {
                        sp.num_ref_idx_l1_active
                    };

                    let ref_idx = if num_active > 1 {
                        reader.read_te(num_active - 1)? as i8
                    } else {
                        0
                    };
                    let mvd_x = reader.read_se()? as i16;
                    let mvd_y = reader.read_se()? as i16;

                    // MV prediction using the appropriate store
                    let (mv_store_ref, ref_store_ref): (&[[i16; 2]], &[i8]) = if is_l0 {
                        (self.mv_store_l0, self.ref_idx_store_l0)
                    } else {
                        (self.mv_store_l1, self.ref_idx_store_l1)
                    };
                    let (mvp_x, mvp_y) = predict_mv(
                        mv_store_ref,
                        ref_store_ref,
                        mb_idx,
                        self.mb_width as usize,
                        0,
                        16,
                        16,
                        ref_idx,
                        self.mb_slice_id,
                        self.this_slice_id,
                        MbaffCtx {
                            mbaff: self.mbaff,
                            mb_field_decoding: self.mb_field_decoding,
                        },
                    );
                    let mv = [mvp_x + mvd_x, mvp_y + mvd_y];

                    // Store MV/ref for all 4x4 blocks
                    for blk in 0..16 {
                        if is_l0 {
                            self.mv_store_l0[mb_idx * 16 + blk] = mv;
                            self.ref_idx_store_l0[mb_idx * 16 + blk] = ref_idx;
                        } else {
                            self.mv_store_l1[mb_idx * 16 + blk] = mv;
                            self.ref_idx_store_l1[mb_idx * 16 + blk] = ref_idx;
                        }
                    }

                    sub_parts.push(SubPart {
                        x: 0,
                        y: 0,
                        w: 16,
                        h: 16,
                        ref_idx_l0: if is_l0 { ref_idx } else { -1 },
                        ref_idx_l1: if is_l0 { -1 } else { ref_idx },
                        mv_l0: if is_l0 { mv } else { [0, 0] },
                        mv_l1: if is_l0 { [0, 0] } else { mv },
                        pred_l0: is_l0,
                        pred_l1: !is_l0,
                    });
                }
                0 => {
                    // B_Direct_16x16: derive MVs per 4x4 block via spatial or temporal direct
                    if !sp.direct_8x8_inference_flag {
                        no_sub_less_8x8_b = false;
                    }
                    self.derive_direct_mvs(mb_idx, 0, 16, sp);
                    // Build sub_parts for MC, coalescing blocks with identical MVs/refs
                    // per 8x8 sub-block (spec 8.4.1.2.1: direct mode applied per 8x8)
                    let base = mb_idx * 16;
                    for i8x8 in 0..4 {
                        let blk0 = i8x8 * 4; // top-left 4x4 block of this 8x8
                        let mv0 = self.mv_store_l0[base + blk0];
                        let mv1 = self.mv_store_l1[base + blk0];
                        let r0 = self.ref_idx_store_l0[base + blk0];
                        let r1 = self.ref_idx_store_l1[base + blk0];
                        // Check if all 4 blocks in this 8x8 have the same MV/ref
                        let uniform = (1..4).all(|sub| {
                            let b = blk0 + sub;
                            self.mv_store_l0[base + b] == mv0
                                && self.mv_store_l1[base + b] == mv1
                                && self.ref_idx_store_l0[base + b] == r0
                                && self.ref_idx_store_l1[base + b] == r1
                        });
                        let (blk_row, blk_col) = BLOCK_INDEX_TO_OFFSET[blk0];
                        if uniform {
                            sub_parts.push(SubPart {
                                x: blk_col,
                                y: blk_row,
                                w: 8,
                                h: 8,
                                ref_idx_l0: r0,
                                ref_idx_l1: r1,
                                mv_l0: mv0,
                                mv_l1: mv1,
                                pred_l0: r0 >= 0,
                                pred_l1: r1 >= 0,
                            });
                        } else {
                            for sub in 0..4 {
                                let b = blk0 + sub;
                                let (br, bc) = BLOCK_INDEX_TO_OFFSET[b];
                                let rl0 = self.ref_idx_store_l0[base + b];
                                let rl1 = self.ref_idx_store_l1[base + b];
                                sub_parts.push(SubPart {
                                    x: bc,
                                    y: br,
                                    w: 4,
                                    h: 4,
                                    ref_idx_l0: rl0,
                                    ref_idx_l1: rl1,
                                    mv_l0: self.mv_store_l0[base + b],
                                    mv_l1: self.mv_store_l1[base + b],
                                    pred_l0: rl0 >= 0,
                                    pred_l1: rl1 >= 0,
                                });
                            }
                        }
                    }
                }
                3 => {
                    // B_Bi_16x16: both L0 and L1, averaged
                    let ref_idx_l0 = if sp.num_ref_idx_l0_active > 1 {
                        reader.read_te(sp.num_ref_idx_l0_active - 1)? as i8
                    } else {
                        0
                    };
                    let ref_idx_l1 = if sp.num_ref_idx_l1_active > 1 {
                        reader.read_te(sp.num_ref_idx_l1_active - 1)? as i8
                    } else {
                        0
                    };

                    let mvd_l0_x = reader.read_se()? as i16;
                    let mvd_l0_y = reader.read_se()? as i16;
                    let (mvp_l0_x, mvp_l0_y) = predict_mv(
                        self.mv_store_l0,
                        self.ref_idx_store_l0,
                        mb_idx,
                        self.mb_width as usize,
                        0,
                        16,
                        16,
                        ref_idx_l0,
                        self.mb_slice_id,
                        self.this_slice_id,
                        MbaffCtx {
                            mbaff: self.mbaff,
                            mb_field_decoding: self.mb_field_decoding,
                        },
                    );
                    let mv_l0 = [mvp_l0_x + mvd_l0_x, mvp_l0_y + mvd_l0_y];

                    let mvd_l1_x = reader.read_se()? as i16;
                    let mvd_l1_y = reader.read_se()? as i16;
                    let (mvp_l1_x, mvp_l1_y) = predict_mv(
                        self.mv_store_l1,
                        self.ref_idx_store_l1,
                        mb_idx,
                        self.mb_width as usize,
                        0,
                        16,
                        16,
                        ref_idx_l1,
                        self.mb_slice_id,
                        self.this_slice_id,
                        MbaffCtx {
                            mbaff: self.mbaff,
                            mb_field_decoding: self.mb_field_decoding,
                        },
                    );
                    let mv_l1 = [mvp_l1_x + mvd_l1_x, mvp_l1_y + mvd_l1_y];

                    // Store both L0 and L1 MVs
                    for blk in 0..16 {
                        self.mv_store_l0[mb_idx * 16 + blk] = mv_l0;
                        self.ref_idx_store_l0[mb_idx * 16 + blk] = ref_idx_l0;
                        self.mv_store_l1[mb_idx * 16 + blk] = mv_l1;
                        self.ref_idx_store_l1[mb_idx * 16 + blk] = ref_idx_l1;
                    }

                    sub_parts.push(SubPart {
                        x: 0,
                        y: 0,
                        w: 16,
                        h: 16,
                        ref_idx_l0,
                        ref_idx_l1,
                        mv_l0,
                        mv_l1,
                        pred_l0: true,
                        pred_l1: true,
                    });
                }
                4..=21 => {
                    // B 16x8/8x16 variants (Table 7-11)
                    // Each type specifies partition size and per-partition pred direction
                    // Format: (part_w, part_h, [(pred_l0_0, pred_l1_0), (pred_l0_1, pred_l1_1)])
                    #[rustfmt::skip]
                        #[allow(clippy::type_complexity)]
                        const B_PART_TABLE: [(usize, usize, [(bool, bool); 2]); 18] = [
                            // mb_type 4-5: B_L0_L0
                            (16, 8, [(true,false),(true,false)]),  // 4
                            (8, 16, [(true,false),(true,false)]),  // 5
                            // mb_type 6-7: B_L1_L1
                            (16, 8, [(false,true),(false,true)]), // 6
                            (8, 16, [(false,true),(false,true)]), // 7
                            // mb_type 8-9: B_L0_L1
                            (16, 8, [(true,false),(false,true)]), // 8
                            (8, 16, [(true,false),(false,true)]), // 9
                            // mb_type 10-11: B_L1_L0
                            (16, 8, [(false,true),(true,false)]), // 10
                            (8, 16, [(false,true),(true,false)]), // 11
                            // mb_type 12-13: B_L0_Bi
                            (16, 8, [(true,false),(true,true)]),  // 12
                            (8, 16, [(true,false),(true,true)]),  // 13
                            // mb_type 14-15: B_L1_Bi
                            (16, 8, [(false,true),(true,true)]),  // 14
                            (8, 16, [(false,true),(true,true)]),  // 15
                            // mb_type 16-17: B_Bi_L0
                            (16, 8, [(true,true),(true,false)]),  // 16
                            (8, 16, [(true,true),(true,false)]),  // 17
                            // mb_type 18-19: B_Bi_L1
                            (16, 8, [(true,true),(false,true)]),  // 18
                            (8, 16, [(true,true),(false,true)]),  // 19
                            // mb_type 20-21: B_Bi_Bi
                            (16, 8, [(true,true),(true,true)]),   // 20
                            (8, 16, [(true,true),(true,true)]),   // 21
                        ];
                    let entry = B_PART_TABLE[(mb_type - 4) as usize];
                    let (part_w, part_h) = (entry.0, entry.1);
                    let pred_flags = entry.2;

                    // Parse ref_idx: for each list, for each partition (spec 7.3.5.1)
                    let mut part_ref_l0 = [-1i8; 2];
                    let mut part_ref_l1 = [-1i8; 2];
                    for p in 0..2 {
                        if pred_flags[p].0 && sp.num_ref_idx_l0_active > 1 {
                            part_ref_l0[p] = reader.read_te(sp.num_ref_idx_l0_active - 1)? as i8;
                        } else if pred_flags[p].0 {
                            part_ref_l0[p] = 0;
                        }
                    }
                    for p in 0..2 {
                        if pred_flags[p].1 && sp.num_ref_idx_l1_active > 1 {
                            part_ref_l1[p] = reader.read_te(sp.num_ref_idx_l1_active - 1)? as i8;
                        } else if pred_flags[p].1 {
                            part_ref_l1[p] = 0;
                        }
                    }

                    // Parse MVD: for each list, for each partition (spec 7.3.5.1)
                    // Order: L0 part0, L0 part1, L1 part0, L1 part1
                    let mut mv_l0_parts = [[0i16; 2]; 2];
                    let mut mv_l1_parts = [[0i16; 2]; 2];
                    for p in 0..2 {
                        if pred_flags[p].0 {
                            // Store ref_idx_l0 before predicting (partition 1 needs partition 0's data)
                            let (py_off, px_off) =
                                if part_h == 8 { (p * 8, 0) } else { (0, p * 8) };
                            for r in (0..part_h).step_by(4) {
                                for c in (0..part_w).step_by(4) {
                                    let lr = (py_off + r) / 4;
                                    let lc = (px_off + c) / 4;
                                    let blk = OFFSET_TO_BLOCK[lr][lc];
                                    self.ref_idx_store_l0[mb_idx * 16 + blk] = part_ref_l0[p];
                                }
                            }
                            let mvd_x = reader.read_se()? as i16;
                            let mvd_y = reader.read_se()? as i16;
                            let (mvp_x, mvp_y) = predict_mv(
                                self.mv_store_l0,
                                self.ref_idx_store_l0,
                                mb_idx,
                                self.mb_width as usize,
                                p,
                                part_w,
                                part_h,
                                part_ref_l0[p],
                                self.mb_slice_id,
                                self.this_slice_id,
                                MbaffCtx {
                                    mbaff: self.mbaff,
                                    mb_field_decoding: self.mb_field_decoding,
                                },
                            );
                            mv_l0_parts[p] = [mvp_x + mvd_x, mvp_y + mvd_y];
                            // Store MV immediately for partition 1 to read partition 0
                            for r in (0..part_h).step_by(4) {
                                for c in (0..part_w).step_by(4) {
                                    let lr = (py_off + r) / 4;
                                    let lc = (px_off + c) / 4;
                                    let blk = OFFSET_TO_BLOCK[lr][lc];
                                    self.mv_store_l0[mb_idx * 16 + blk] = mv_l0_parts[p];
                                }
                            }
                        }
                    }
                    for p in 0..2 {
                        if pred_flags[p].1 {
                            let (py_off, px_off) =
                                if part_h == 8 { (p * 8, 0) } else { (0, p * 8) };
                            for r in (0..part_h).step_by(4) {
                                for c in (0..part_w).step_by(4) {
                                    let lr = (py_off + r) / 4;
                                    let lc = (px_off + c) / 4;
                                    let blk = OFFSET_TO_BLOCK[lr][lc];
                                    self.ref_idx_store_l1[mb_idx * 16 + blk] = part_ref_l1[p];
                                }
                            }
                            let mvd_x = reader.read_se()? as i16;
                            let mvd_y = reader.read_se()? as i16;
                            let (mvp_x, mvp_y) = predict_mv(
                                self.mv_store_l1,
                                self.ref_idx_store_l1,
                                mb_idx,
                                self.mb_width as usize,
                                p,
                                part_w,
                                part_h,
                                part_ref_l1[p],
                                self.mb_slice_id,
                                self.this_slice_id,
                                MbaffCtx {
                                    mbaff: self.mbaff,
                                    mb_field_decoding: self.mb_field_decoding,
                                },
                            );
                            mv_l1_parts[p] = [mvp_x + mvd_x, mvp_y + mvd_y];
                            for r in (0..part_h).step_by(4) {
                                for c in (0..part_w).step_by(4) {
                                    let lr = (py_off + r) / 4;
                                    let lc = (px_off + c) / 4;
                                    let blk = OFFSET_TO_BLOCK[lr][lc];
                                    self.mv_store_l1[mb_idx * 16 + blk] = mv_l1_parts[p];
                                }
                            }
                        }
                    }

                    // Build sub_parts for MC
                    for p in 0..2 {
                        let (py_off, px_off) = if part_h == 8 { (p * 8, 0) } else { (0, p * 8) };
                        sub_parts.push(SubPart {
                            x: px_off,
                            y: py_off,
                            w: part_w,
                            h: part_h,
                            ref_idx_l0: part_ref_l0[p],
                            ref_idx_l1: part_ref_l1[p],
                            mv_l0: mv_l0_parts[p],
                            mv_l1: mv_l1_parts[p],
                            pred_l0: pred_flags[p].0,
                            pred_l1: pred_flags[p].1,
                        });
                    }
                }
                22 => {
                    // B_8x8: 4 sub-MBs, each with its own sub_mb_type
                    // Table 7-17: sub_mb_type 0-12
                    // Format: (sub_w, sub_h, pred_l0, pred_l1)
                    #[rustfmt::skip]
                        const B_SUB_TABLE: [(usize, usize, bool, bool); 13] = [
                            (8, 8, false, false), // 0: B_Direct_8x8
                            (8, 8, true, false),  // 1: B_L0_8x8
                            (8, 8, false, true),  // 2: B_L1_8x8
                            (8, 8, true, true),   // 3: B_Bi_8x8
                            (8, 4, true, false),  // 4: B_L0_8x4
                            (4, 8, true, false),  // 5: B_L0_4x8
                            (8, 4, false, true),  // 6: B_L1_8x4
                            (4, 8, false, true),  // 7: B_L1_4x8
                            (8, 4, true, true),   // 8: B_Bi_8x4
                            (4, 8, true, true),   // 9: B_Bi_4x8
                            (4, 4, true, false),  // 10: B_L0_4x4
                            (4, 4, false, true),  // 11: B_L1_4x4
                            (4, 4, true, true),   // 12: B_Bi_4x4
                        ];

                    let sub_mb_origins = [(0, 0), (0, 8), (8, 0), (8, 8)];
                    let mut sub_mb_types = [0u32; 4];
                    for smt in &mut sub_mb_types {
                        let v = reader.read_ue()?;
                        if v > 12 {
                            return Err(DecodeError::InvalidSyntax("B sub_mb_type out of range"));
                        }
                        *smt = v;
                    }
                    if sub_mb_types
                        .iter()
                        .any(|&smt| smt > 3 || (smt == 0 && !sp.direct_8x8_inference_flag))
                    {
                        no_sub_less_8x8_b = false;
                    }

                    // Parse ref_idx for each 8x8 sub-MB
                    let mut sub_ref_l0 = [-1i8; 4];
                    let mut sub_ref_l1 = [-1i8; 4];
                    for smb in 0..4 {
                        if sub_mb_types[smb] == 0 {
                            continue;
                        } // B_Direct_8x8: no ref_idx
                        let (_, _, pl0, _) = B_SUB_TABLE[sub_mb_types[smb] as usize];
                        if pl0 && sp.num_ref_idx_l0_active > 1 {
                            sub_ref_l0[smb] = reader.read_te(sp.num_ref_idx_l0_active - 1)? as i8;
                        } else if pl0 {
                            sub_ref_l0[smb] = 0;
                        }
                    }
                    for smb in 0..4 {
                        if sub_mb_types[smb] == 0 {
                            continue;
                        }
                        let (_, _, _, pl1) = B_SUB_TABLE[sub_mb_types[smb] as usize];
                        if pl1 && sp.num_ref_idx_l1_active > 1 {
                            sub_ref_l1[smb] = reader.read_te(sp.num_ref_idx_l1_active - 1)? as i8;
                        } else if pl1 {
                            sub_ref_l1[smb] = 0;
                        }
                    }

                    // Store ref_idx into cache for MV prediction (both lists)
                    for list in 0..2 {
                        let ref_store = if list == 0 {
                            &mut self.ref_idx_store_l0
                        } else {
                            &mut self.ref_idx_store_l1
                        };
                        let sub_ref = if list == 0 { &sub_ref_l0 } else { &sub_ref_l1 };
                        for smb in 0..4 {
                            let (sy, sx) = sub_mb_origins[smb];
                            for r in (0..8).step_by(4) {
                                for c in (0..8).step_by(4) {
                                    let lr = (sy + r) / 4;
                                    let lc = (sx + c) / 4;
                                    let blk = OFFSET_TO_BLOCK[lr][lc];
                                    ref_store[mb_idx * 16 + blk] = sub_ref[smb];
                                }
                            }
                        }
                    }

                    // Collect sub-partition layouts for each sub-MB
                    struct SubLayout {
                        smb: usize,
                        sx: usize,
                        sy: usize,
                        sub_w: usize,
                        sub_h: usize,
                        pl0: bool,
                        pl1: bool,
                        offsets: Vec<(usize, usize)>, // (dx, dy) within 8x8
                    }
                    let mut layouts: Vec<SubLayout> = Vec::new();
                    for smb in 0..4 {
                        let (sy, sx) = sub_mb_origins[smb];
                        let smt = sub_mb_types[smb] as usize;
                        if smt == 0 {
                            // B_Direct_8x8: handled separately below
                            layouts.push(SubLayout {
                                smb,
                                sx,
                                sy,
                                sub_w: 8,
                                sub_h: 8,
                                pl0: false,
                                pl1: false, // direct mode flag
                                offsets: vec![(0, 0)],
                            });
                            continue;
                        }
                        let (sub_w, sub_h, pl0, pl1) = B_SUB_TABLE[smt];
                        let offsets = match (sub_w, sub_h) {
                            (8, 8) => vec![(0, 0)],
                            (8, 4) => vec![(0, 0), (0, 4)],
                            (4, 8) => vec![(0, 0), (4, 0)],
                            (4, 4) => vec![(0, 0), (4, 0), (0, 4), (4, 4)],
                            _ => unreachable!(),
                        };
                        layouts.push(SubLayout {
                            smb,
                            sx,
                            sy,
                            sub_w,
                            sub_h,
                            pl0,
                            pl1,
                            offsets,
                        });
                    }

                    // Derive B_Direct_8x8 MVs per 4x4 block BEFORE MVD parsing
                    for layout in &layouts {
                        if sub_mb_types[layout.smb] == 0 {
                            self.derive_direct_mvs(mb_idx, layout.smb * 4, 4, sp);
                        }
                    }

                    // Parse MVDs: for each list, for each sub-MB, for each sub-partition
                    // (spec 7.3.5.1 parsing order)
                    struct SubMv {
                        mv_l0: [i16; 2],
                        mv_l1: [i16; 2],
                    }
                    let mut sub_mvs: Vec<SubMv> = Vec::new();
                    // Initialize with zeros
                    for layout in &layouts {
                        for &(_dx, _dy) in &layout.offsets {
                            sub_mvs.push(SubMv {
                                mv_l0: [0; 2],
                                mv_l1: [0; 2],
                            });
                        }
                    }

                    // L0 MVDs first
                    let mut idx = 0;
                    for layout in &layouts {
                        if sub_mb_types[layout.smb] == 0 {
                            idx += 1;
                            continue;
                        } // direct
                        for &(dx, dy) in &layout.offsets {
                            if layout.pl0 {
                                let px = layout.sx + dx;
                                let py = layout.sy + dy;
                                let mvd_x = reader.read_se()? as i16;
                                let mvd_y = reader.read_se()? as i16;
                                let (mvp_x, mvp_y) = predict_mv_sub(
                                    self.mv_store_l0,
                                    self.ref_idx_store_l0,
                                    mb_idx,
                                    self.mb_width as usize,
                                    px,
                                    py,
                                    layout.sub_w,
                                    layout.sub_h,
                                    sub_ref_l0[layout.smb],
                                    self.mb_slice_id,
                                    self.this_slice_id,
                                    MbaffCtx {
                                        mbaff: self.mbaff,
                                        mb_field_decoding: self.mb_field_decoding,
                                    },
                                );
                                let mv = [mvp_x + mvd_x, mvp_y + mvd_y];
                                sub_mvs[idx].mv_l0 = mv;
                                // Store immediately for subsequent sub-partition prediction
                                for r in (0..layout.sub_h).step_by(4) {
                                    for c in (0..layout.sub_w).step_by(4) {
                                        let lr = (py + r) / 4;
                                        let lc = (px + c) / 4;
                                        let blk = OFFSET_TO_BLOCK[lr][lc];
                                        self.mv_store_l0[mb_idx * 16 + blk] = mv;
                                    }
                                }
                            }
                            idx += 1;
                        }
                    }

                    // L1 MVDs second
                    idx = 0;
                    for layout in &layouts {
                        if sub_mb_types[layout.smb] == 0 {
                            idx += 1;
                            continue;
                        }
                        for &(dx, dy) in &layout.offsets {
                            if layout.pl1 {
                                let px = layout.sx + dx;
                                let py = layout.sy + dy;
                                let mvd_x = reader.read_se()? as i16;
                                let mvd_y = reader.read_se()? as i16;
                                let (mvp_x, mvp_y) = predict_mv_sub(
                                    self.mv_store_l1,
                                    self.ref_idx_store_l1,
                                    mb_idx,
                                    self.mb_width as usize,
                                    px,
                                    py,
                                    layout.sub_w,
                                    layout.sub_h,
                                    sub_ref_l1[layout.smb],
                                    self.mb_slice_id,
                                    self.this_slice_id,
                                    MbaffCtx {
                                        mbaff: self.mbaff,
                                        mb_field_decoding: self.mb_field_decoding,
                                    },
                                );
                                let mv = [mvp_x + mvd_x, mvp_y + mvd_y];
                                sub_mvs[idx].mv_l1 = mv;
                                for r in (0..layout.sub_h).step_by(4) {
                                    for c in (0..layout.sub_w).step_by(4) {
                                        let lr = (py + r) / 4;
                                        let lc = (px + c) / 4;
                                        let blk = OFFSET_TO_BLOCK[lr][lc];
                                        self.mv_store_l1[mb_idx * 16 + blk] = mv;
                                    }
                                }
                            }
                            idx += 1;
                        }
                    }

                    // Build sub_parts from stored MVs
                    idx = 0;
                    for layout in &layouts {
                        if sub_mb_types[layout.smb] == 0 {
                            // B_Direct_8x8: coalesce to 8x8 if all 4 blocks share MV/ref
                            let base = mb_idx * 16;
                            let blk0 = OFFSET_TO_BLOCK[layout.sy / 4][layout.sx / 4];
                            let mv0 = self.mv_store_l0[base + blk0];
                            let mv1 = self.mv_store_l1[base + blk0];
                            let r0 = self.ref_idx_store_l0[base + blk0];
                            let r1 = self.ref_idx_store_l1[base + blk0];
                            let uniform = (1..4).all(|sub| {
                                let b = blk0 + sub;
                                self.mv_store_l0[base + b] == mv0
                                    && self.mv_store_l1[base + b] == mv1
                                    && self.ref_idx_store_l0[base + b] == r0
                                    && self.ref_idx_store_l1[base + b] == r1
                            });
                            if uniform {
                                sub_parts.push(SubPart {
                                    x: layout.sx,
                                    y: layout.sy,
                                    w: 8,
                                    h: 8,
                                    ref_idx_l0: r0,
                                    ref_idx_l1: r1,
                                    mv_l0: mv0,
                                    mv_l1: mv1,
                                    pred_l0: r0 >= 0,
                                    pred_l1: r1 >= 0,
                                });
                            } else {
                                for dr in (0..8).step_by(4) {
                                    for dc in (0..8).step_by(4) {
                                        let lr = (layout.sy + dr) / 4;
                                        let lc = (layout.sx + dc) / 4;
                                        let blk = OFFSET_TO_BLOCK[lr][lc];
                                        let mv_l0 = self.mv_store_l0[base + blk];
                                        let mv_l1 = self.mv_store_l1[base + blk];
                                        let ri_l0 = self.ref_idx_store_l0[base + blk];
                                        let ri_l1 = self.ref_idx_store_l1[base + blk];
                                        sub_parts.push(SubPart {
                                            x: layout.sx + dc,
                                            y: layout.sy + dr,
                                            w: 4,
                                            h: 4,
                                            ref_idx_l0: ri_l0,
                                            ref_idx_l1: ri_l1,
                                            mv_l0,
                                            mv_l1,
                                            pred_l0: ri_l0 >= 0,
                                            pred_l1: ri_l1 >= 0,
                                        });
                                    }
                                }
                            }
                            idx += 1;
                        } else {
                            let smt = sub_mb_types[layout.smb] as usize;
                            let (_, _, pl0, pl1) = B_SUB_TABLE[smt];
                            for &(dx, dy) in &layout.offsets {
                                sub_parts.push(SubPart {
                                    x: layout.sx + dx,
                                    y: layout.sy + dy,
                                    w: layout.sub_w,
                                    h: layout.sub_h,
                                    ref_idx_l0: sub_ref_l0[layout.smb],
                                    ref_idx_l1: sub_ref_l1[layout.smb],
                                    mv_l0: sub_mvs[idx].mv_l0,
                                    mv_l1: sub_mvs[idx].mv_l1,
                                    pred_l0: pl0,
                                    pred_l1: pl1,
                                });
                                idx += 1;
                            }
                        }
                    }
                }
                _ => return Err(DecodeError::InvalidSyntax("invalid B-slice mb_type")),
            }

            // Parse CBP using inter table
            let cbp_code = reader.read_ue()? as usize;
            if cbp_code >= 48 {
                return Err(DecodeError::from("invalid coded_block_pattern"));
            }
            let cbp = CBP_INTER_TABLE[cbp_code];
            let cbp_luma = cbp & 0x0F;
            let cbp_chroma = cbp >> 4;

            // 8x8 transform flag (High profile, spec 7.3.5)
            let use_8x8_dct_b = sp.transform_8x8_mode_flag
                && cbp_luma != 0
                && no_sub_less_8x8_b
                && reader.read_bit()? != 0;
            // The deblocker reads this to skip the inner 4x4 edges of an 8x8 transform (8.7).
            self.mb_is_8x8dct[mb_idx] = use_8x8_dct_b;

            let mb_qp = if cbp_luma != 0 || cbp_chroma != 0 {
                let mb_qp_delta = reader.read_se()?;
                sp.apply_mb_qp_delta(self.prev_mb_qp, mb_qp_delta)?
            } else {
                self.prev_mb_qp
            };
            self.prev_mb_qp = mb_qp;
            let qp_y = sp.luma_qp_prime(mb_qp);
            let (qp_cb, qp_cr) = sp.chroma_qp_prime(mb_qp);

            // Decode luma residual
            let mut luma_residual = [0i32; 256];
            if use_8x8_dct_b {
                for i8x8 in 0..4 {
                    if cbp_luma & (1 << i8x8) == 0 {
                        for sub in 0..4 {
                            self.nc_luma[mb_idx * 16 + i8x8 * 4 + sub] = 0;
                        }
                        continue;
                    }
                    let mut block_8x8 = [0i32; 64];
                    // CAVLC: decode 4 groups of 16 coefficients (spec 7.3.5.3.2)
                    for i4x4 in 0..4 {
                        let blk = i8x8 * 4 + i4x4;
                        let nc = compute_nc(
                            self.nc_luma,
                            mb_idx,
                            self.mb_width as usize,
                            blk,
                            16,
                            self.mb_slice_id,
                            self.this_slice_id,
                            self.mbaff,
                            self.mb_field_decoding,
                        );
                        let mut quad_coeffs = [0i32; 16];
                        let tc = parse_residual_block_cavlc(reader, &mut quad_coeffs, 16, nc)?;
                        self.nc_luma[mb_idx * 16 + blk] = tc;
                        let scan_base = i4x4 * 16;
                        for k in 0..16 {
                            if quad_coeffs[k] != 0 {
                                block_8x8[zigzag_8x8_cavlc[scan_base + k]] = quad_coeffs[k];
                            }
                        }
                    }
                    dequant_8x8(&mut block_8x8, qp_y, &sp.scaling_list_8x8[1]);
                    inverse_dct_8x8(&mut block_8x8);
                    let row_off = (i8x8 / 2) * 8;
                    let col_off = (i8x8 % 2) * 8;
                    for r in 0..8 {
                        for c in 0..8 {
                            luma_residual[(row_off + r) * 16 + col_off + c] = block_8x8[r * 8 + c];
                        }
                    }
                }
            } else {
                for blk in 0..16 {
                    if cbp_luma & (1 << (blk / 4)) != 0 {
                        let nc = compute_nc(
                            self.nc_luma,
                            mb_idx,
                            self.mb_width as usize,
                            blk,
                            16,
                            self.mb_slice_id,
                            self.this_slice_id,
                            self.mbaff,
                            self.mb_field_decoding,
                        );
                        let mut block_coeffs = [0i32; 16];
                        let tc = parse_residual_block_cavlc(reader, &mut block_coeffs, 16, nc)?;
                        self.nc_luma[mb_idx * 16 + blk] = tc;

                        let mut raster = [0i32; 16];
                        for i in 0..16 {
                            let (r, c) = zigzag_4x4[i];
                            raster[r * 4 + c] = block_coeffs[i];
                        }
                        // Use inter scaling list (index 3) for luma
                        dequant_4x4_full(&mut raster, qp_y, &sp.scaling_list_4x4[3]);
                        inverse_dct_4x4(&mut raster);

                        let (blk_row, blk_col) = BLOCK_INDEX_TO_OFFSET[blk];
                        for r in 0..4 {
                            for c in 0..4 {
                                luma_residual[(blk_row + r) * 16 + blk_col + c] = raster[r * 4 + c];
                            }
                        }
                    }
                }
            }

            // Luma MC + residual for each sub-partition
            for sub_part in &sub_parts {
                let mut luma_pred = [0u16; 256];

                if sub_part.pred_l0 && sub_part.pred_l1 {
                    // Bi-prediction: average L0 and L1
                    let mut pred_l0 = vec![0u16; sub_part.w * sub_part.h];
                    let mut pred_l1 = vec![0u16; sub_part.w * sub_part.h];
                    let ref_l0 = ref_pic_safe(sp.ref_pic_list_l0, sub_part.ref_idx_l0)
                        .ok_or(DecodeError::InvalidSyntax("empty ref list"))?;
                    let (
                        mc_y_l0,
                        ref_stride_l0,
                        ref_y_off_l0,
                        _mc_cy_l0,
                        _c_ref_stride_l0,
                        _c_ref_off_l0,
                    ) = self.mc_params(mb_idx, mb_y, ref_l0.width as usize, sub_part.ref_idx_l0);
                    inter_pred::luma_mc_stride(
                        ref_l0,
                        (mb_x + sub_part.x) as i32,
                        mc_y_l0 + sub_part.y as i32,
                        sub_part.mv_l0[0] as i32,
                        sub_part.mv_l0[1] as i32,
                        sub_part.w,
                        sub_part.h,
                        &mut pred_l0,
                        ref_stride_l0,
                        ref_y_off_l0,
                    );
                    let ref_l1 = ref_pic_safe(sp.ref_pic_list_l1, sub_part.ref_idx_l1)
                        .ok_or(DecodeError::InvalidSyntax("empty ref list"))?;
                    let (
                        mc_y_l1,
                        ref_stride_l1,
                        ref_y_off_l1,
                        _mc_cy_l1,
                        _c_ref_stride_l1,
                        _c_ref_off_l1,
                    ) = self.mc_params(mb_idx, mb_y, ref_l1.width as usize, sub_part.ref_idx_l1);
                    inter_pred::luma_mc_stride(
                        ref_l1,
                        (mb_x + sub_part.x) as i32,
                        mc_y_l1 + sub_part.y as i32,
                        sub_part.mv_l1[0] as i32,
                        sub_part.mv_l1[1] as i32,
                        sub_part.w,
                        sub_part.h,
                        &mut pred_l1,
                        ref_stride_l1,
                        ref_y_off_l1,
                    );
                    sp.wctx.apply_bi(
                        &pred_l0,
                        &pred_l1,
                        &mut luma_pred,
                        sub_part.ref_idx_l0 as usize,
                        sub_part.ref_idx_l1 as usize,
                        false,
                        0,
                    );
                } else if sub_part.pred_l0 {
                    let ref_pic = ref_pic_safe(sp.ref_pic_list_l0, sub_part.ref_idx_l0)
                        .ok_or(DecodeError::InvalidSyntax("empty ref list"))?;
                    let (mc_y, ref_stride, ref_y_off, _mc_cy, _c_ref_stride, _c_ref_off) =
                        self.mc_params(mb_idx, mb_y, ref_pic.width as usize, sub_part.ref_idx_l0);
                    inter_pred::luma_mc_stride(
                        ref_pic,
                        (mb_x + sub_part.x) as i32,
                        mc_y + sub_part.y as i32,
                        sub_part.mv_l0[0] as i32,
                        sub_part.mv_l0[1] as i32,
                        sub_part.w,
                        sub_part.h,
                        &mut luma_pred,
                        ref_stride,
                        ref_y_off,
                    );
                    if sp.use_weight == 1 {
                        sp.wctx.apply_uni(
                            &mut luma_pred,
                            0,
                            sub_part.ref_idx_l0 as usize,
                            false,
                            0,
                        );
                    }
                } else if sub_part.pred_l1 {
                    let ref_pic = ref_pic_safe(sp.ref_pic_list_l1, sub_part.ref_idx_l1)
                        .ok_or(DecodeError::InvalidSyntax("empty ref list"))?;
                    let (mc_y, ref_stride, ref_y_off, _mc_cy, _c_ref_stride, _c_ref_off) =
                        self.mc_params(mb_idx, mb_y, ref_pic.width as usize, sub_part.ref_idx_l1);
                    inter_pred::luma_mc_stride(
                        ref_pic,
                        (mb_x + sub_part.x) as i32,
                        mc_y + sub_part.y as i32,
                        sub_part.mv_l1[0] as i32,
                        sub_part.mv_l1[1] as i32,
                        sub_part.w,
                        sub_part.h,
                        &mut luma_pred,
                        ref_stride,
                        ref_y_off,
                    );
                    if sp.use_weight == 1 {
                        sp.wctx.apply_uni(
                            &mut luma_pred,
                            1,
                            sub_part.ref_idx_l1 as usize,
                            false,
                            0,
                        );
                    }
                }

                for r in 0..sub_part.h {
                    for c in 0..sub_part.w {
                        let val = (luma_pred[r * sub_part.w + c] as i32
                            + luma_residual[(sub_part.y + r) * 16 + sub_part.x + c])
                            .clamp(0, self.pixel_max_y) as u16;
                        self.frame.y[self.ly_offset
                            + (sub_part.y + r) * self.ly_stride
                            + mb_x
                            + sub_part.x
                            + c] = val;
                    }
                }
            }

            // Chroma
            let mut chroma_dc_cb = [0i32; 4];
            let mut chroma_dc_cr = [0i32; 4];
            if cbp_chroma >= 1 {
                parse_residual_block_cavlc(reader, &mut chroma_dc_cb, 4, -1)?;
                parse_residual_block_cavlc(reader, &mut chroma_dc_cr, 4, -1)?;
            }
            let mut chroma_ac_scan_cb = [[0i32; 15]; 4];
            let mut chroma_ac_scan_cr = [[0i32; 15]; 4];
            if cbp_chroma >= 2 {
                for blk in 0..4 {
                    let nc = compute_nc(
                        self.nc_cb,
                        mb_idx,
                        self.mb_width as usize,
                        blk,
                        4,
                        self.mb_slice_id,
                        self.this_slice_id,
                        self.mbaff,
                        self.mb_field_decoding,
                    );
                    let tc =
                        parse_residual_block_cavlc(reader, &mut chroma_ac_scan_cb[blk], 15, nc)?;
                    self.nc_cb[mb_idx * 4 + blk] = tc;
                }
                for blk in 0..4 {
                    let nc = compute_nc(
                        self.nc_cr,
                        mb_idx,
                        self.mb_width as usize,
                        blk,
                        4,
                        self.mb_slice_id,
                        self.this_slice_id,
                        self.mbaff,
                        self.mb_field_decoding,
                    );
                    let tc =
                        parse_residual_block_cavlc(reader, &mut chroma_ac_scan_cr[blk], 15, nc)?;
                    self.nc_cr[mb_idx * 4 + blk] = tc;
                }
            }

            let chroma_mb_x = mb_x / 2;
            // Precompute mc_params per sub_part before entering the plane loop
            // (avoids borrow conflict with &mut self.frame inside the loop).
            let mut sub_mc: Vec<(i32, usize, usize, usize, usize, usize)> =
                Vec::with_capacity(sub_parts.len());
            for sub_part in sub_parts.iter() {
                if sub_part.pred_l0 && sub_part.pred_l1 {
                    // We'll handle L0/L1 separately inside; store L0 params as placeholder
                    let ref_l0 = ref_pic_safe(sp.ref_pic_list_l0, sub_part.ref_idx_l0);
                    let w = ref_l0
                        .map(|r| r.width as usize)
                        .unwrap_or(self.width as usize);
                    sub_mc.push(self.mc_params(mb_idx, mb_y, w, sub_part.ref_idx_l0));
                } else if sub_part.pred_l0 {
                    let ref_pic = ref_pic_safe(sp.ref_pic_list_l0, sub_part.ref_idx_l0);
                    let w = ref_pic
                        .map(|r| r.width as usize)
                        .unwrap_or(self.width as usize);
                    sub_mc.push(self.mc_params(mb_idx, mb_y, w, sub_part.ref_idx_l0));
                } else {
                    let ref_pic = ref_pic_safe(sp.ref_pic_list_l1, sub_part.ref_idx_l1);
                    let w = ref_pic
                        .map(|r| r.width as usize)
                        .unwrap_or(self.width as usize);
                    sub_mc.push(self.mc_params(mb_idx, mb_y, w, sub_part.ref_idx_l1));
                }
            }
            // Also precompute L1 mc_params for bi-pred sub_parts
            let mut sub_mc_l1: Vec<(i32, usize, usize, usize, usize, usize)> =
                Vec::with_capacity(sub_parts.len());
            for sub_part in sub_parts.iter() {
                if sub_part.pred_l0 && sub_part.pred_l1 {
                    let ref_l1 = ref_pic_safe(sp.ref_pic_list_l1, sub_part.ref_idx_l1);
                    let w = ref_l1
                        .map(|r| r.width as usize)
                        .unwrap_or(self.width as usize);
                    sub_mc_l1.push(self.mc_params(mb_idx, mb_y, w, sub_part.ref_idx_l1));
                } else {
                    sub_mc_l1.push((0, 0, 0, 0, 0, 0)); // unused
                }
            }

            for (plane_dc, plane_ac, frame_plane, scale_idx) in [
                (
                    &mut chroma_dc_cb,
                    &chroma_ac_scan_cb,
                    &mut self.frame.u,
                    4usize,
                ),
                (
                    &mut chroma_dc_cr,
                    &chroma_ac_scan_cr,
                    &mut self.frame.v,
                    5usize,
                ),
            ] {
                let chroma_scale = &sp.scaling_list_4x4[scale_idx];
                // Cb uses scaling list 1 (intra) / 4 (inter), Cr 2 / 5.
                let qp_c = if matches!(scale_idx, 1 | 4) {
                    qp_cb
                } else {
                    qp_cr
                };
                if cbp_chroma >= 1 {
                    inverse_hadamard_2x2(plane_dc);
                    dequant_chroma_dc(plane_dc, qp_c, chroma_scale[0]);
                }

                let mut chroma_residual = [0i32; 64];
                for blk in 0..4 {
                    let blk_row = (blk / 2) * 4;
                    let blk_col = (blk % 2) * 4;
                    let mut block_raster = [0i32; 16];
                    block_raster[0] = plane_dc[blk];
                    if cbp_chroma >= 2 {
                        for scan_idx in 0..15 {
                            let (r, c) = zigzag_4x4[scan_idx + 1];
                            block_raster[r * 4 + c] = plane_ac[blk][scan_idx];
                        }
                        dequant_4x4_ac_raster(&mut block_raster, qp_c, chroma_scale);
                    }
                    inverse_dct_4x4(&mut block_raster);
                    for r in 0..4 {
                        for c in 0..4 {
                            chroma_residual[(blk_row + r) * 8 + blk_col + c] =
                                block_raster[r * 4 + c];
                        }
                    }
                }

                // Chroma MC for each sub-partition
                let mut chroma_pred = [0u16; 64];
                for (sp_i, sub_part) in sub_parts.iter().enumerate() {
                    let cx_off = sub_part.x / 2;
                    let cy_off = sub_part.y / 2;
                    let cw = sub_part.w.max(2) / 2;
                    let ch = sub_part.h.max(2) / 2;
                    if cw == 0 || ch == 0 {
                        continue;
                    }

                    let chroma_h = (self.height / 2) as usize;

                    let mut part_pred = [0u16; 64];
                    if sub_part.pred_l0 && sub_part.pred_l1 {
                        let ref_l0 = ref_pic_safe(sp.ref_pic_list_l0, sub_part.ref_idx_l0)
                            .ok_or(DecodeError::InvalidSyntax("empty ref list"))?;
                        let ref_l1 = ref_pic_safe(sp.ref_pic_list_l1, sub_part.ref_idx_l1)
                            .ok_or(DecodeError::InvalidSyntax("empty ref list"))?;
                        let (
                            _mc_y_l0,
                            _rs_l0,
                            _ref_y_off_l0,
                            mc_cy_l0,
                            c_ref_stride_l0,
                            c_ref_off_l0,
                        ) = sub_mc[sp_i];
                        let (
                            _mc_y_l1,
                            _rs_l1,
                            _ref_y_off_l1,
                            mc_cy_l1,
                            c_ref_stride_l1,
                            c_ref_off_l1,
                        ) = sub_mc_l1[sp_i];
                        let cr_l0 = if scale_idx == 4 {
                            &ref_l0.u[c_ref_off_l0..]
                        } else {
                            &ref_l0.v[c_ref_off_l0..]
                        };
                        let cr_l1 = if scale_idx == 4 {
                            &ref_l1.u[c_ref_off_l1..]
                        } else {
                            &ref_l1.v[c_ref_off_l1..]
                        };
                        let mut c_l0 = vec![0u16; cw * ch];
                        let mut c_l1 = vec![0u16; cw * ch];
                        let cmv_y_off_l0 = crate::slice_context::chroma_field_mv_offset_impl(
                            self.field_pic_flag,
                            self.bottom_field_flag,
                            ref_l0,
                        );
                        let cmv_y_off_l1 = crate::slice_context::chroma_field_mv_offset_impl(
                            self.field_pic_flag,
                            self.bottom_field_flag,
                            ref_l1,
                        );
                        inter_pred::chroma_mc(
                            cr_l0,
                            c_ref_stride_l0,
                            chroma_h,
                            (chroma_mb_x + cx_off) as i32,
                            (mc_cy_l0 + cy_off) as i32,
                            sub_part.mv_l0[0] as i32,
                            sub_part.mv_l0[1] as i32 + cmv_y_off_l0,
                            cw,
                            ch,
                            &mut c_l0,
                        );
                        inter_pred::chroma_mc(
                            cr_l1,
                            c_ref_stride_l1,
                            chroma_h,
                            (chroma_mb_x + cx_off) as i32,
                            (mc_cy_l1 + cy_off) as i32,
                            sub_part.mv_l1[0] as i32,
                            sub_part.mv_l1[1] as i32 + cmv_y_off_l1,
                            cw,
                            ch,
                            &mut c_l1,
                        );
                        let chroma_comp = if scale_idx == 4 { 0 } else { 1 };
                        sp.wctx.apply_bi(
                            &c_l0,
                            &c_l1,
                            &mut part_pred,
                            sub_part.ref_idx_l0 as usize,
                            sub_part.ref_idx_l1 as usize,
                            true,
                            chroma_comp,
                        );
                    } else {
                        let (ref_list, ref_idx, mv) = if sub_part.pred_l0 {
                            (sp.ref_pic_list_l0, sub_part.ref_idx_l0, sub_part.mv_l0)
                        } else {
                            (sp.ref_pic_list_l1, sub_part.ref_idx_l1, sub_part.mv_l1)
                        };
                        let ref_pic = ref_pic_safe(ref_list, ref_idx)
                            .ok_or(DecodeError::InvalidSyntax("empty ref list"))?;
                        let (_mc_y, _rs, _ref_y_off, mc_cy, c_ref_stride, c_ref_off) = sub_mc[sp_i];
                        let cr = if scale_idx == 4 {
                            &ref_pic.u[c_ref_off..]
                        } else {
                            &ref_pic.v[c_ref_off..]
                        };
                        let cmv_y_off = crate::slice_context::chroma_field_mv_offset_impl(
                            self.field_pic_flag,
                            self.bottom_field_flag,
                            ref_pic,
                        );
                        inter_pred::chroma_mc(
                            cr,
                            c_ref_stride,
                            chroma_h,
                            (chroma_mb_x + cx_off) as i32,
                            (mc_cy + cy_off) as i32,
                            mv[0] as i32,
                            mv[1] as i32 + cmv_y_off,
                            cw,
                            ch,
                            &mut part_pred,
                        );
                        if sp.use_weight == 1 {
                            let chroma_comp = if scale_idx == 4 { 0 } else { 1 };
                            let (list, ri_val) = if sub_part.pred_l0 {
                                (0, sub_part.ref_idx_l0 as usize)
                            } else {
                                (1, sub_part.ref_idx_l1 as usize)
                            };
                            sp.wctx
                                .apply_uni(&mut part_pred, list, ri_val, true, chroma_comp);
                        }
                    }
                    for r in 0..ch {
                        for c in 0..cw {
                            chroma_pred[(cy_off + r) * 8 + cx_off + c] = part_pred[r * cw + c];
                        }
                    }
                }

                for y in 0..8 {
                    for x in 0..8 {
                        let val = (chroma_pred[y * 8 + x] as i32 + chroma_residual[y * 8 + x])
                            .clamp(0, self.pixel_max_c) as u16;
                        frame_plane[self.lc_offset + y * self.lc_stride + chroma_mb_x + x] = val;
                    }
                }
            }

            self.mb_info[mb_idx] = MbInfo {
                mb_type: MbType::Inter,
                qp_y: mb_qp,
                ..Default::default()
            };
            // mb_idx handled by caller
            return Ok(());
        } else if is_inter {
            // === Inter (P) macroblock ===
            let is_p8x8 = mb_type == 3 || mb_type == 4;

            // For P_8x8/P_8x8ref0: parse sub_mb_type for each 8x8 sub-MB
            // sub_mb_type: 0=8x8, 1=8x4, 2=4x8, 3=4x4
            let mut sub_mb_types = [0u32; 4];
            if is_p8x8 {
                for smt in &mut sub_mb_types {
                    *smt = reader.read_ue()?;
                }
            }

            // Collect all sub-partition info: (x_off, y_off, self.width, self.height, ref_idx)
            // For non-P_8x8: 1-2 partitions as before
            // For P_8x8: up to 16 sub-partitions across 4 sub-MBs
            struct SubPart {
                x: usize,
                y: usize,
                w: usize,
                h: usize,
                ref_idx: i8,
                mv: [i16; 2],
            }
            let mut sub_parts: Vec<SubPart> = Vec::new();

            if is_p8x8 {
                // 8x8 sub-MB origins within the macroblock
                let sub_mb_origins = [(0, 0), (0, 8), (8, 0), (8, 8)];

                // Parse ref_idx for each 8x8 sub-MB
                // Field-coded MBs double the effective ref count (each frame ref → 2 field refs)
                let eff_num_ref_l0 = self.effective_num_ref(mb_idx, sp.num_ref_idx_l0_active);
                let mut sub_ref = [0i8; 4];
                if mb_type == 3 {
                    // P_8x8: parse ref_idx per sub-MB
                    for sr in &mut sub_ref {
                        if eff_num_ref_l0 > 1 {
                            *sr = reader.read_te(eff_num_ref_l0 - 1)? as i8;
                        }
                    }
                }
                // P_8x8ref0 (mb_type=4): all ref_idx = 0 (already initialized)
                // Parse MVD for each sub-partition and store MVs
                for smb in 0..4 {
                    let (sy, sx) = sub_mb_origins[smb];
                    let ref_idx = sub_ref[smb];

                    // Sub-partition layout within this 8x8
                    let sub_parts_layout: Vec<(usize, usize, usize, usize)> =
                        match sub_mb_types[smb] {
                            0 => vec![(0, 0, 8, 8)],                                           // 8x8
                            1 => vec![(0, 0, 8, 4), (0, 4, 8, 4)], // 8x4
                            2 => vec![(0, 0, 4, 8), (4, 0, 4, 8)], // 4x8
                            3 => vec![(0, 0, 4, 4), (4, 0, 4, 4), (0, 4, 4, 4), (4, 4, 4, 4)], // 4x4
                            _ => return Err(DecodeError::from("invalid sub_mb_type")),
                        };

                    for &(dx, dy, spw, sph) in &sub_parts_layout {
                        let px = sx + dx;
                        let py = sy + dy;
                        let mvd_x = reader.read_se()? as i16;
                        let mvd_y = reader.read_se()? as i16;
                        let (mvp_x, mvp_y) = predict_mv_sub(
                            self.mv_store_l0,
                            self.ref_idx_store_l0,
                            mb_idx,
                            self.mb_width as usize,
                            px,
                            py,
                            spw,
                            sph,
                            ref_idx,
                            self.mb_slice_id,
                            self.this_slice_id,
                            MbaffCtx {
                                mbaff: self.mbaff,
                                mb_field_decoding: self.mb_field_decoding,
                            },
                        );
                        let mv = [mvp_x + mvd_x, mvp_y + mvd_y];
                        // Store MV for all 4x4 blocks in this sub-partition
                        for r in (0..sph).step_by(4) {
                            for c in (0..spw).step_by(4) {
                                let lr = (py + r) / 4;
                                let lc = (px + c) / 4;
                                let blk = OFFSET_TO_BLOCK[lr][lc];
                                self.mv_store_l0[mb_idx * 16 + blk] = mv;
                                self.ref_idx_store_l0[mb_idx * 16 + blk] = ref_idx;
                            }
                        }

                        sub_parts.push(SubPart {
                            x: px,
                            y: py,
                            w: spw,
                            h: sph,
                            ref_idx,
                            mv,
                        });
                    }
                }
            } else {
                // P_L0_16x16, P16x8, P8x16
                let (part_w, part_h, num_parts) = match mb_type {
                    0 => (16usize, 16usize, 1usize),
                    1 => (16, 8, 2),
                    2 => (8, 16, 2),
                    _ => unreachable!(),
                };

                let eff_num_ref_l0 = self.effective_num_ref(mb_idx, sp.num_ref_idx_l0_active);
                let mut part_ref = [0i8; 2];
                for ref_entry in part_ref.iter_mut().take(num_parts) {
                    if eff_num_ref_l0 > 1 {
                        *ref_entry = reader.read_te(eff_num_ref_l0 - 1)? as i8;
                    }
                }

                #[allow(clippy::needless_range_loop)]
                for p in 0..num_parts {
                    let mvd_x = reader.read_se()? as i16;
                    let mvd_y = reader.read_se()? as i16;
                    let (mvp_x, mvp_y) = predict_mv(
                        self.mv_store_l0,
                        self.ref_idx_store_l0,
                        mb_idx,
                        self.mb_width as usize,
                        p,
                        part_w,
                        part_h,
                        part_ref[p],
                        self.mb_slice_id,
                        self.this_slice_id,
                        MbaffCtx {
                            mbaff: self.mbaff,
                            mb_field_decoding: self.mb_field_decoding,
                        },
                    );
                    let mv = [mvp_x + mvd_x, mvp_y + mvd_y];
                    let (py_off, px_off) = match mb_type {
                        1 => (p * 8, 0),
                        2 => (0, p * 8),
                        _ => (0, 0),
                    };

                    // Store MV/ref immediately
                    for r in (0..part_h).step_by(4) {
                        for c in (0..part_w).step_by(4) {
                            let lr = (py_off + r) / 4;
                            let lc = (px_off + c) / 4;
                            let blk = OFFSET_TO_BLOCK[lr][lc];
                            self.mv_store_l0[mb_idx * 16 + blk] = mv;
                            self.ref_idx_store_l0[mb_idx * 16 + blk] = part_ref[p];
                        }
                    }

                    sub_parts.push(SubPart {
                        x: px_off,
                        y: py_off,
                        w: part_w,
                        h: part_h,
                        ref_idx: part_ref[p],
                        mv,
                    });
                }
            }

            // Parse CBP using inter table
            let cbp_code = reader.read_ue()? as usize;
            if cbp_code >= 48 {
                return Err(DecodeError::from("invalid coded_block_pattern"));
            }
            let cbp = CBP_INTER_TABLE[cbp_code];
            let cbp_luma = cbp & 0x0F;
            let cbp_chroma = cbp >> 4;
            // 8x8 transform flag (High profile inter MBs, spec 7.3.5).
            // Only present when noSubMbPartSizeLessThan8x8Flag is true.
            let no_sub_less_8x8 = if is_p8x8 {
                sub_mb_types.iter().all(|&smt| smt == 0) // P_8x8: smt=0 means 8x8 sub-partition
            } else {
                true // P_16x16, P_16x8, P_8x16 always >= 8x8
            };
            let use_8x8_dct = sp.transform_8x8_mode_flag
                && cbp_luma != 0
                && no_sub_less_8x8
                && reader.read_bit()? != 0;
            // The deblocker reads this to skip the inner 4x4 edges of an 8x8 transform (8.7).
            self.mb_is_8x8dct[mb_idx] = use_8x8_dct;

            let mb_qp = if cbp_luma != 0 || cbp_chroma != 0 {
                let mb_qp_delta = reader.read_se()?;
                sp.apply_mb_qp_delta(self.prev_mb_qp, mb_qp_delta)?
            } else {
                self.prev_mb_qp
            };
            self.prev_mb_qp = mb_qp;
            let qp_y = sp.luma_qp_prime(mb_qp);
            let (qp_cb, qp_cr) = sp.chroma_qp_prime(mb_qp);
            // Decode luma residual
            let mut luma_residual = [0i32; 256];
            if use_8x8_dct {
                // 8x8 transform: 4 blocks of 64 coefficients each
                let scale_8x8 = if is_inter {
                    &sp.scaling_list_8x8[1]
                } else {
                    &sp.scaling_list_8x8[0]
                };
                for i8x8 in 0..4 {
                    if cbp_luma & (1 << i8x8) == 0 {
                        continue;
                    }
                    let mut block_8x8 = [0i32; 64];
                    // Decode 4 groups of 16 coefficients via CAVLC
                    for i4x4 in 0..4 {
                        let blk = i8x8 * 4 + i4x4;
                        let nc = compute_nc(
                            self.nc_luma,
                            mb_idx,
                            self.mb_width as usize,
                            blk,
                            16,
                            self.mb_slice_id,
                            self.this_slice_id,
                            self.mbaff,
                            self.mb_field_decoding,
                        );
                        let mut quad_coeffs = [0i32; 16];
                        let tc = parse_residual_block_cavlc(reader, &mut quad_coeffs, 16, nc)?;
                        self.nc_luma[mb_idx * 16 + blk] = tc;
                        // Place into 8x8 block using scan table
                        let scan_base = i4x4 * 16;
                        for k in 0..16 {
                            if quad_coeffs[k] != 0 {
                                block_8x8[zigzag_8x8_cavlc[scan_base + k]] = quad_coeffs[k];
                            }
                        }
                    }
                    dequant_8x8(&mut block_8x8, qp_y, scale_8x8);
                    inverse_dct_8x8(&mut block_8x8);
                    // Copy 8x8 residual into 16x16 luma residual
                    let row_off = (i8x8 / 2) * 8;
                    let col_off = (i8x8 % 2) * 8;
                    for r in 0..8 {
                        for c in 0..8 {
                            luma_residual[(row_off + r) * 16 + col_off + c] = block_8x8[r * 8 + c];
                        }
                    }
                }
            } else {
                // 4x4 transform (existing path)
                for blk in 0..16 {
                    if cbp_luma & (1 << (blk / 4)) != 0 {
                        let nc = compute_nc(
                            self.nc_luma,
                            mb_idx,
                            self.mb_width as usize,
                            blk,
                            16,
                            self.mb_slice_id,
                            self.this_slice_id,
                            self.mbaff,
                            self.mb_field_decoding,
                        );
                        let mut block_coeffs = [0i32; 16];
                        let tc = parse_residual_block_cavlc(reader, &mut block_coeffs, 16, nc)?;
                        self.nc_luma[mb_idx * 16 + blk] = tc;

                        let mut raster = [0i32; 16];
                        for i in 0..16 {
                            let (r, c) = zigzag_4x4[i];
                            raster[r * 4 + c] = block_coeffs[i];
                        }
                        dequant_4x4_full(&mut raster, qp_y, &sp.scaling_list_4x4[3]);
                        inverse_dct_4x4(&mut raster);

                        let (blk_row, blk_col) = BLOCK_INDEX_TO_OFFSET[blk];
                        for r in 0..4 {
                            for c in 0..4 {
                                luma_residual[(blk_row + r) * 16 + blk_col + c] = raster[r * 4 + c];
                            }
                        }
                    }
                }
            } // close if use_8x8_dct else

            // Motion compensate and add residual for each sub-partition
            for sub_part in &sub_parts {
                let fri = self.frame_ref_idx(mb_idx, sub_part.ref_idx);
                let ref_pic = sp
                    .ref_pic_list
                    .get(fri as usize)
                    .or_else(|| sp.ref_pic_list.last())
                    .ok_or(DecodeError::InvalidSyntax(
                        "P sub-partition references empty ref_pic_list",
                    ))?;
                let (mc_y, ref_stride, ref_y_off, _mc_cy, _c_ref_stride, _c_ref_off) =
                    self.mc_params(mb_idx, mb_y, ref_pic.width as usize, sub_part.ref_idx);
                let mut luma_pred = [0u16; 256];
                inter_pred::luma_mc_stride(
                    ref_pic,
                    (mb_x + sub_part.x) as i32,
                    mc_y + sub_part.y as i32,
                    sub_part.mv[0] as i32,
                    sub_part.mv[1] as i32,
                    sub_part.w,
                    sub_part.h,
                    &mut luma_pred,
                    ref_stride,
                    ref_y_off,
                );
                if sp.use_weight == 1 {
                    sp.wctx
                        .apply_uni(&mut luma_pred, 0, sub_part.ref_idx as usize, false, 0);
                }
                for r in 0..sub_part.h {
                    for c in 0..sub_part.w {
                        let val = (luma_pred[r * sub_part.w + c] as i32
                            + luma_residual[(sub_part.y + r) * 16 + sub_part.x + c])
                            .clamp(0, self.pixel_max_y) as u16;
                        self.frame.y[self.ly_offset
                            + (sub_part.y + r) * self.ly_stride
                            + mb_x
                            + sub_part.x
                            + c] = val;
                    }
                }
            }

            // Chroma (P-slice inter)
            let mut chroma_dc_cb = [0i32; 4];
            let mut chroma_dc_cr = [0i32; 4];
            if cbp_chroma >= 1 {
                parse_residual_block_cavlc(reader, &mut chroma_dc_cb, 4, -1)?;
                parse_residual_block_cavlc(reader, &mut chroma_dc_cr, 4, -1)?;
            }
            let mut chroma_ac_scan_cb = [[0i32; 15]; 4];
            let mut chroma_ac_scan_cr = [[0i32; 15]; 4];
            if cbp_chroma >= 2 {
                for blk in 0..4 {
                    let nc = compute_nc(
                        self.nc_cb,
                        mb_idx,
                        self.mb_width as usize,
                        blk,
                        4,
                        self.mb_slice_id,
                        self.this_slice_id,
                        self.mbaff,
                        self.mb_field_decoding,
                    );
                    let tc =
                        parse_residual_block_cavlc(reader, &mut chroma_ac_scan_cb[blk], 15, nc)?;
                    self.nc_cb[mb_idx * 4 + blk] = tc;
                }
                for blk in 0..4 {
                    let nc = compute_nc(
                        self.nc_cr,
                        mb_idx,
                        self.mb_width as usize,
                        blk,
                        4,
                        self.mb_slice_id,
                        self.this_slice_id,
                        self.mbaff,
                        self.mb_field_decoding,
                    );
                    let tc =
                        parse_residual_block_cavlc(reader, &mut chroma_ac_scan_cr[blk], 15, nc)?;
                    self.nc_cr[mb_idx * 4 + blk] = tc;
                }
            }

            let chroma_mb_x = mb_x / 2;

            // Precompute mc_params per sub_part before entering the plane loop
            // (avoids borrow conflict with &mut self.frame inside the loop).
            let p_sub_mc: Vec<(i32, usize, usize, usize, usize, usize)> = sub_parts
                .iter()
                .map(|sp_part| {
                    let fri = self.frame_ref_idx(mb_idx, sp_part.ref_idx);
                    let ref_pic = sp
                        .ref_pic_list
                        .get(fri as usize)
                        .or_else(|| sp.ref_pic_list.last());
                    let w = ref_pic
                        .map(|r| r.width as usize)
                        .unwrap_or(self.width as usize);
                    self.mc_params(mb_idx, mb_y, w, sp_part.ref_idx)
                })
                .collect();

            // Chroma MC + residual for each chroma plane
            // Each partition gets its own chroma MC with its own MV
            {
                for (plane_dc, plane_ac, frame_plane, scale_idx) in [
                    (
                        &mut chroma_dc_cb,
                        &chroma_ac_scan_cb,
                        &mut self.frame.u,
                        4usize,
                    ),
                    (
                        &mut chroma_dc_cr,
                        &chroma_ac_scan_cr,
                        &mut self.frame.v,
                        5usize,
                    ),
                ] {
                    let chroma_scale = &sp.scaling_list_4x4[scale_idx];
                    // Cb uses scaling list 1 (intra) / 4 (inter), Cr 2 / 5.
                    let qp_c = if matches!(scale_idx, 1 | 4) {
                        qp_cb
                    } else {
                        qp_cr
                    };
                    if cbp_chroma >= 1 {
                        inverse_hadamard_2x2(plane_dc);
                        dequant_chroma_dc(plane_dc, qp_c, chroma_scale[0]);
                    }

                    let mut chroma_residual = [0i32; 64];
                    for blk in 0..4 {
                        let blk_row = (blk / 2) * 4;
                        let blk_col = (blk % 2) * 4;
                        let mut block_raster = [0i32; 16];
                        block_raster[0] = plane_dc[blk];
                        if cbp_chroma >= 2 {
                            for scan_idx in 0..15 {
                                let (r, c) = zigzag_4x4[scan_idx + 1];
                                block_raster[r * 4 + c] = plane_ac[blk][scan_idx];
                            }
                            dequant_4x4_ac_raster(&mut block_raster, qp_c, chroma_scale);
                        }
                        inverse_dct_4x4(&mut block_raster);
                        for r in 0..4 {
                            for c in 0..4 {
                                chroma_residual[(blk_row + r) * 8 + blk_col + c] =
                                    block_raster[r * 4 + c];
                            }
                        }
                    }

                    // MC each sub-partition's chroma region
                    let mut chroma_pred = [0u16; 64];
                    for (sp_i, sub_part) in sub_parts.iter().enumerate() {
                        // Chroma coordinates are half of luma
                        let cx_off = sub_part.x / 2;
                        let cy_off = sub_part.y / 2;
                        let cw = sub_part.w.max(2) / 2; // min chroma block = 1, but MC needs >= 1
                        let ch = sub_part.h.max(2) / 2;
                        if cw == 0 || ch == 0 {
                            continue;
                        }
                        let fri_val = p_sub_mc[sp_i]; // mc_params already computed
                        let _ = fri_val; // ref_idx mapping is embedded in mc_params
                        let fri = if self.mbaff && self.mb_field_decoding[mb_idx / 2] {
                            sub_part.ref_idx / 2
                        } else {
                            sub_part.ref_idx
                        };
                        let part_ref_pic = sp
                            .ref_pic_list
                            .get(fri as usize)
                            .or_else(|| sp.ref_pic_list.last())
                            .ok_or(DecodeError::InvalidSyntax(
                                "P sub-partition chroma references empty ref_pic_list",
                            ))?;
                        let (_mc_y, _rs, _ref_y_off, mc_cy, c_ref_stride, c_ref_off) =
                            p_sub_mc[sp_i];
                        let chroma_ref = if scale_idx == 4 {
                            &part_ref_pic.u[c_ref_off..]
                        } else {
                            &part_ref_pic.v[c_ref_off..]
                        };
                        let mut part_pred = [0u16; 64];
                        let cmv_y_off = crate::slice_context::chroma_field_mv_offset_impl(
                            self.field_pic_flag,
                            self.bottom_field_flag,
                            part_ref_pic,
                        );
                        inter_pred::chroma_mc(
                            chroma_ref,
                            c_ref_stride,
                            (self.height / 2) as usize,
                            (chroma_mb_x + cx_off) as i32,
                            (mc_cy + cy_off) as i32,
                            sub_part.mv[0] as i32,
                            sub_part.mv[1] as i32 + cmv_y_off,
                            cw,
                            ch,
                            &mut part_pred,
                        );
                        if sp.use_weight == 1 {
                            let chroma_comp = if scale_idx == 4 { 0 } else { 1 };
                            sp.wctx.apply_uni(
                                &mut part_pred,
                                0,
                                sub_part.ref_idx as usize,
                                true,
                                chroma_comp,
                            );
                        }
                        for r in 0..ch {
                            for c in 0..cw {
                                chroma_pred[(cy_off + r) * 8 + cx_off + c] = part_pred[r * cw + c];
                            }
                        }
                    }

                    for y in 0..8 {
                        for x in 0..8 {
                            let val = (chroma_pred[y * 8 + x] as i32 + chroma_residual[y * 8 + x])
                                .clamp(0, self.pixel_max_c)
                                as u16;
                            frame_plane[self.lc_offset + y * self.lc_stride + chroma_mb_x + x] =
                                val;
                        }
                    }
                }
            }

            self.mb_info[mb_idx] = MbInfo {
                mb_type: MbType::Inter,
                qp_y: mb_qp,
                ..Default::default()
            };
            // mb_idx handled by caller
            return Ok(());
        }

        // === Intra macroblock (I4x4, I16x16, I_PCM) ===
        // Cross-slice intra prediction: neighbors from other slices unavailable (spec 6.4.1)
        // constrained_intra_pred_flag: inter-predicted neighbors also unavailable
        let above_mb_avail = self
            .above_mb(mb_idx)
            .is_some_and(|above| self.is_intra_neighbor_avail(above, sp));
        let left_mb_avail = self
            .left_mb(mb_idx)
            .is_some_and(|left| self.is_intra_neighbor_avail(left, sp));
        let above_left_mb_avail = self
            .above_mb(mb_idx)
            .and_then(|above| self.left_mb(above))
            .is_some_and(|al| self.is_intra_neighbor_avail(al, sp));
        let above_right_mb_avail = if !self.mbaff {
            mb_idx >= self.mb_width as usize
                && (mb_idx % self.mb_width as usize) + 1 < self.mb_width as usize
                && self.mb_slice_id[mb_idx - self.mb_width as usize + 1] == self.this_slice_id
                && self.is_intra_neighbor_avail(mb_idx - self.mb_width as usize + 1, sp)
        } else if !mb_idx.is_multiple_of(2) {
            // MBAFF bottom MBs: above-right is in the next pair (not yet decoded)
            false
        } else {
            self.above_mb(mb_idx)
                .and_then(|above| {
                    let above_pair = above / 2;
                    let above_col = above_pair % self.mb_width as usize;
                    if above_col + 1 >= self.mb_width as usize {
                        return None;
                    }
                    let ar_mb = (above_pair + 1) * 2 + (above % 2);
                    if self.mb_slice_id.get(ar_mb).copied() != Some(self.this_slice_id) {
                        return None;
                    }
                    if !self.is_intra_neighbor_avail(ar_mb, sp) {
                        return None;
                    }
                    Some(ar_mb)
                })
                .is_some()
        };

        // Variables shared between I4x4/I16x16 for chroma reconstruction
        let intra_chroma_pred_mode;
        let cbp_chroma: u8;
        let mb_qp;
        let qp_y;
        let qp_cb;
        let qp_cr;

        if mb_type == 0 {
            // === I_NxN (I4x4 or I8x8) macroblock ===

            // Check 8x8 transform flag
            let use_8x8_intra = sp.transform_8x8_mode_flag && reader.read_bit()? != 0;
            // The deblocker reads this to skip the inner 4x4 edges of an 8x8 transform (8.7).
            self.mb_is_8x8dct[mb_idx] = use_8x8_intra;

            let intra_avail = self.intra_avail_map(sp);

            // Parse prediction modes: 4 for I8x8, 16 for I4x4
            let num_modes = if use_8x8_intra { 4 } else { 16 };
            let mut pred_modes = [2u8; 16];
            for blk_idx in 0..num_modes {
                let blk = if use_8x8_intra { blk_idx * 4 } else { blk_idx };
                let prev_flag = reader.read_bit()?;
                let predicted = predict_i4x4_mode(
                    self.i4x4_modes,
                    mb_idx,
                    self.mb_width as usize,
                    blk,
                    self.mb_slice_id,
                    self.this_slice_id,
                    &intra_avail,
                    self.mbaff,
                    self.mb_field_decoding,
                );
                let mode = if prev_flag != 0 {
                    predicted
                } else {
                    let rem = reader.read_bits(3)? as u8;
                    if rem < predicted {
                        rem
                    } else {
                        rem + 1
                    }
                };
                if use_8x8_intra {
                    // Store same mode for all 4 sub-blocks
                    for sub in 0..4 {
                        pred_modes[blk_idx * 4 + sub] = mode;
                        self.i4x4_modes[mb_idx * 16 + blk_idx * 4 + sub] = mode;
                    }
                } else {
                    pred_modes[blk] = mode;
                    self.i4x4_modes[mb_idx * 16 + blk] = mode;
                }
            }
            intra_chroma_pred_mode = reader.read_ue()? as u8;

            let cbp_code = reader.read_ue()? as usize;
            if cbp_code >= 48 {
                return Err(DecodeError::from("invalid coded_block_pattern"));
            }
            let cbp = CBP_INTRA_TABLE[cbp_code];
            let cbp_luma = cbp & 0x0F;
            cbp_chroma = cbp >> 4;

            if cbp_luma != 0 || cbp_chroma != 0 {
                let mb_qp_delta = reader.read_se()?;
                mb_qp = sp.apply_mb_qp_delta(self.prev_mb_qp, mb_qp_delta)?;
            } else {
                mb_qp = self.prev_mb_qp;
            }
            self.prev_mb_qp = mb_qp;
            qp_y = sp.luma_qp_prime(mb_qp);
            (qp_cb, qp_cr) = sp.chroma_qp_prime(mb_qp);

            // Parse luma residual and reconstruct
            if use_8x8_intra {
                // I8x8: decode 4 8x8 blocks with 8x8 transform
                let mut luma_residual = [0i32; 256];
                for i8x8 in 0..4 {
                    if cbp_luma & (1 << i8x8) == 0 {
                        continue;
                    }
                    let mut block_8x8 = [0i32; 64];
                    for i4x4 in 0..4 {
                        let blk = i8x8 * 4 + i4x4;
                        let nc = compute_nc(
                            self.nc_luma,
                            mb_idx,
                            self.mb_width as usize,
                            blk,
                            16,
                            self.mb_slice_id,
                            self.this_slice_id,
                            self.mbaff,
                            self.mb_field_decoding,
                        );
                        let mut quad_coeffs = [0i32; 16];
                        let tc = parse_residual_block_cavlc(reader, &mut quad_coeffs, 16, nc)?;
                        self.nc_luma[mb_idx * 16 + blk] = tc;
                        let scan_base = i4x4 * 16;
                        for k in 0..16 {
                            if quad_coeffs[k] != 0 {
                                block_8x8[zigzag_8x8_cavlc[scan_base + k]] = quad_coeffs[k];
                            }
                        }
                    }
                    dequant_8x8(&mut block_8x8, qp_y, &sp.scaling_list_8x8[0]);
                    inverse_dct_8x8(&mut block_8x8);

                    let row_off = (i8x8 / 2) * 8;
                    let col_off = (i8x8 % 2) * 8;
                    for r in 0..8 {
                        for c in 0..8 {
                            luma_residual[(row_off + r) * 16 + col_off + c] = block_8x8[r * 8 + c];
                        }
                    }
                }
                // I8x8 prediction + residual reconstruction
                for i8x8 in 0..4 {
                    self.reconstruct_luma_8x8_block(
                        i8x8,
                        mb_x,
                        pred_modes[i8x8 * 4],
                        &luma_residual,
                        above_mb_avail,
                        left_mb_avail,
                        above_left_mb_avail,
                        above_right_mb_avail,
                    );
                }
            } else {
                // I4x4: existing 4x4 path
                for blk in 0..16 {
                    let (blk_row, blk_col) = BLOCK_INDEX_TO_OFFSET[blk];
                    let px = mb_x + blk_col;
                    let py = mb_y + blk_row;

                    // Parse residual
                    let mut block_coeffs = [0i32; 16];
                    if cbp_luma & (1 << (blk / 4)) != 0 {
                        let nc = compute_nc(
                            self.nc_luma,
                            mb_idx,
                            self.mb_width as usize,
                            blk,
                            16,
                            self.mb_slice_id,
                            self.this_slice_id,
                            self.mbaff,
                            self.mb_field_decoding,
                        );
                        let tc = parse_residual_block_cavlc(reader, &mut block_coeffs, 16, nc)?;
                        self.nc_luma[mb_idx * 16 + blk] = tc;

                        // Unzigzag: convert from zigzag scan order to raster order
                        let mut raster = [0i32; 16];
                        for i in 0..16 {
                            let (r, c) = zigzag_4x4[i];
                            raster[r * 4 + c] = block_coeffs[i];
                        }
                        block_coeffs = raster;

                        dequant_4x4_full(&mut block_coeffs, qp_y, &sp.scaling_list_4x4[0]);
                    }
                    inverse_dct_4x4(&mut block_coeffs);
                    // I4x4 prediction + residual
                    self.reconstruct_luma_4x4_block(
                        px,
                        py,
                        mb_x,
                        mb_y,
                        blk,
                        pred_modes[blk],
                        &block_coeffs,
                        above_mb_avail,
                        left_mb_avail,
                        above_left_mb_avail,
                        above_right_mb_avail,
                    );
                }
            } // close if use_8x8_intra else
        } else if mb_type <= 24 {
            // === I16x16 macroblock ===
            let mt = mb_type - 1;
            let intra16x16_pred_mode = (mt % 4) as u8;
            cbp_chroma = ((mt / 4) % 3) as u8;
            let cbp_luma = if mt >= 12 { 15u8 } else { 0u8 };

            intra_chroma_pred_mode = reader.read_ue()? as u8;

            let mb_qp_delta = reader.read_se()?;
            mb_qp = sp.apply_mb_qp_delta(self.prev_mb_qp, mb_qp_delta)?;
            self.prev_mb_qp = mb_qp;
            qp_y = sp.luma_qp_prime(mb_qp);
            (qp_cb, qp_cr) = sp.chroma_qp_prime(mb_qp);

            // Parse luma DC
            let mut luma_dc = [0i32; 16];
            let nc_dc = compute_nc(
                self.nc_luma,
                mb_idx,
                self.mb_width as usize,
                0,
                16,
                self.mb_slice_id,
                self.this_slice_id,
                self.mbaff,
                self.mb_field_decoding,
            );
            parse_residual_block_cavlc(reader, &mut luma_dc, 16, nc_dc)?;

            // Parse luma AC
            let mut luma_ac_scan = [[0i32; 15]; 16];
            if cbp_luma != 0 {
                for blk in 0..16 {
                    let nc = compute_nc(
                        self.nc_luma,
                        mb_idx,
                        self.mb_width as usize,
                        blk,
                        16,
                        self.mb_slice_id,
                        self.this_slice_id,
                        self.mbaff,
                        self.mb_field_decoding,
                    );
                    let tc = parse_residual_block_cavlc(reader, &mut luma_ac_scan[blk], 15, nc)?;
                    self.nc_luma[mb_idx * 16 + blk] = tc;
                }
            }

            // Unzigzag DC, Hadamard, dequant
            let mut luma_dc_raster = [0i32; 16];
            for i in 0..16 {
                let (r, c) = zigzag_4x4[i];
                luma_dc_raster[r * 4 + c] = luma_dc[i];
            }
            inverse_hadamard_4x4(&mut luma_dc_raster);
            dequant_luma_dc_i16x16(&mut luma_dc_raster, qp_y, sp.scaling_list_4x4[0][0]);

            const DC_RASTER_TO_BLOCK: [usize; 16] =
                [0, 1, 4, 5, 2, 3, 6, 7, 8, 9, 12, 13, 10, 11, 14, 15];

            let mut luma_residual = [0i32; 256];
            for blk in 0..16 {
                let (blk_row, blk_col) = BLOCK_INDEX_TO_OFFSET[blk];
                let mut block_raster = [0i32; 16];
                let dc_idx = DC_RASTER_TO_BLOCK.iter().position(|&b| b == blk).unwrap();
                block_raster[0] = luma_dc_raster[dc_idx];

                if cbp_luma != 0 {
                    for scan_idx in 0..15 {
                        let (r, c) = zigzag_4x4[scan_idx + 1];
                        block_raster[r * 4 + c] = luma_ac_scan[blk][scan_idx];
                    }
                    dequant_4x4_ac_raster(&mut block_raster, qp_y, &sp.scaling_list_4x4[0]);
                }

                inverse_dct_4x4(&mut block_raster);

                for r in 0..4 {
                    for c in 0..4 {
                        luma_residual[(blk_row + r) * 16 + blk_col + c] = block_raster[r * 4 + c];
                    }
                }
            }

            // I16x16 prediction + residual
            self.reconstruct_luma_16x16(
                mb_x,
                intra16x16_pred_mode,
                &luma_residual,
                above_mb_avail,
                left_mb_avail,
                above_left_mb_avail,
            );
        } else if mb_type == 25 {
            // === I_PCM macroblock ===
            reader.align_to_byte();
            for r in 0..16 {
                for c in 0..16 {
                    // pcm_sample_luma is u(v) with v = BitDepthY (spec 7.3.5)
                    let val = reader.read_bits(self.bit_depth_y as u8)? as u16;
                    let idx = self.ly_offset + r * self.ly_stride + mb_x + c;
                    if idx < self.frame.y.len() {
                        self.frame.y[idx] = val;
                    }
                }
            }
            let cx = mb_x / 2;
            for r in 0..8 {
                for c in 0..8 {
                    // pcm_sample_chroma is u(v) with v = BitDepthC
                    let val = reader.read_bits(self.bit_depth_c as u8)? as u16;
                    let idx = self.lc_offset + r * self.lc_stride + cx + c;
                    if idx < self.frame.u.len() {
                        self.frame.u[idx] = val;
                    }
                }
            }
            for r in 0..8 {
                for c in 0..8 {
                    // pcm_sample_chroma is u(v) with v = BitDepthC
                    let val = reader.read_bits(self.bit_depth_c as u8)? as u16;
                    let idx = self.lc_offset + r * self.lc_stride + cx + c;
                    if idx < self.frame.v.len() {
                        self.frame.v[idx] = val;
                    }
                }
            }
            for blk in 0..16 {
                self.nc_luma[mb_idx * 16 + blk] = 16;
            }
            for blk in 0..4 {
                self.nc_cb[mb_idx * 4 + blk] = 16;
                self.nc_cr[mb_idx * 4 + blk] = 16;
            }
            self.mb_info[mb_idx] = MbInfo {
                mb_type: MbType::Ipcm,
                qp_y: 0,
                ..Default::default()
            };
            self.prev_mb_qp = 0;
            return Ok(());
        } else {
            return Err(DecodeError::from("unsupported mb_type for I slice"));
        }

        self.mb_info[mb_idx] = MbInfo {
            mb_type: MbType::Intra,
            qp_y: mb_qp,
            ..Default::default()
        };

        // Intra MBs keep ref_idx=-1 (default) and mv=(0,0) (default).
        // The -1 ref_idx ensures predict_mv's match_count logic correctly
        // excludes intra neighbors from directional prediction.

        // === Reconstruct chroma (shared by I4x4 and I16x16) ===
        let mut chroma_dc_cb = [0i32; 4];
        let mut chroma_dc_cr = [0i32; 4];
        if cbp_chroma >= 1 {
            parse_residual_block_cavlc(reader, &mut chroma_dc_cb, 4, -1)?;
            parse_residual_block_cavlc(reader, &mut chroma_dc_cr, 4, -1)?;
        }

        let mut chroma_ac_scan_cb = [[0i32; 15]; 4];
        let mut chroma_ac_scan_cr = [[0i32; 15]; 4];
        if cbp_chroma >= 2 {
            for blk in 0..4 {
                let nc = compute_nc(
                    self.nc_cb,
                    mb_idx,
                    self.mb_width as usize,
                    blk,
                    4,
                    self.mb_slice_id,
                    self.this_slice_id,
                    self.mbaff,
                    self.mb_field_decoding,
                );
                let tc = parse_residual_block_cavlc(reader, &mut chroma_ac_scan_cb[blk], 15, nc)?;
                self.nc_cb[mb_idx * 4 + blk] = tc;
            }
            for blk in 0..4 {
                let nc = compute_nc(
                    self.nc_cr,
                    mb_idx,
                    self.mb_width as usize,
                    blk,
                    4,
                    self.mb_slice_id,
                    self.this_slice_id,
                    self.mbaff,
                    self.mb_field_decoding,
                );
                let tc = parse_residual_block_cavlc(reader, &mut chroma_ac_scan_cr[blk], 15, nc)?;
                self.nc_cr[mb_idx * 4 + blk] = tc;
            }
        }

        let chroma_mb_x = mb_x / 2;

        // Scaling list indices: 1=Intra Cb, 2=Intra Cr
        for (plane_dc, plane_ac_scan, plane_buf, scale_idx) in [
            (
                &mut chroma_dc_cb,
                &chroma_ac_scan_cb,
                &mut self.frame.u,
                1usize,
            ),
            (
                &mut chroma_dc_cr,
                &chroma_ac_scan_cr,
                &mut self.frame.v,
                2usize,
            ),
        ] {
            let chroma_scale = &sp.scaling_list_4x4[scale_idx];
            // Cb uses scaling list 1 (intra) / 4 (inter), Cr 2 / 5.
            let qp_c = if matches!(scale_idx, 1 | 4) {
                qp_cb
            } else {
                qp_cr
            };
            if cbp_chroma >= 1 {
                inverse_hadamard_2x2(plane_dc);
                dequant_chroma_dc(plane_dc, qp_c, chroma_scale[0]);
            }

            let mut chroma_residual = [0i32; 64];
            for blk in 0..4 {
                let blk_row = (blk / 2) * 4;
                let blk_col = (blk % 2) * 4;
                let mut block_raster = [0i32; 16];
                block_raster[0] = plane_dc[blk];

                if cbp_chroma >= 2 {
                    for scan_idx in 0..15 {
                        let (r, c) = zigzag_4x4[scan_idx + 1];
                        block_raster[r * 4 + c] = plane_ac_scan[blk][scan_idx];
                    }
                    dequant_4x4_ac_raster(&mut block_raster, qp_c, chroma_scale);
                }

                inverse_dct_4x4(&mut block_raster);

                for r in 0..4 {
                    for c in 0..4 {
                        chroma_residual[(blk_row + r) * 8 + blk_col + c] = block_raster[r * 4 + c];
                    }
                }
            }

            let mut chroma_pred = [0u16; 64];
            let cbase = self.lc_offset;
            let cstride = self.lc_stride;
            let above_c: Option<Vec<u16>> = if cbase >= cstride && above_mb_avail {
                Some(
                    (0..8)
                        .map(|x| plane_buf[cbase - cstride + chroma_mb_x + x])
                        .collect(),
                )
            } else {
                None
            };
            let left_c: Option<Vec<u16>> = if chroma_mb_x > 0 && left_mb_avail {
                Some(
                    (0..8)
                        .map(|y| plane_buf[cbase + y * cstride + chroma_mb_x - 1])
                        .collect(),
                )
            } else {
                None
            };
            let above_left_c = if chroma_mb_x > 0 && cbase >= cstride && above_left_mb_avail {
                Some(plane_buf[cbase - cstride + chroma_mb_x - 1])
            } else {
                None
            };

            predict_chroma_8x8(
                intra_chroma_pred_mode,
                above_c.as_deref(),
                left_c.as_deref(),
                above_left_c,
                &mut chroma_pred,
                self.bit_depth_c,
            );

            for y in 0..8 {
                for x in 0..8 {
                    let val = (chroma_pred[y * 8 + x] as i32 + chroma_residual[y * 8 + x])
                        .clamp(0, self.pixel_max_c) as u16;
                    plane_buf[cbase + y * cstride + chroma_mb_x + x] = val;
                }
            }
        }
        Ok(())
    }
}
