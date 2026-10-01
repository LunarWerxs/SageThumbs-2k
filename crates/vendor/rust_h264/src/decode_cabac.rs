//! CABAC macroblock decode — skip detection, mb_type dispatch, residual decode.
#![allow(clippy::needless_range_loop)]

use crate::cabac::CabacReader;
use crate::deblock::{MbInfo, MbType};
use crate::error::DecodeError;
use crate::inter_pred;
use crate::mv_pred::{predict_mv, predict_mv_sub, ref_pic_safe, MbaffCtx};
use crate::neighbor::{
    cabac_amvd, cabac_neighbor_nz_chroma, cabac_neighbor_nz_luma, cabac_neighbor_ref,
    dequant_4x4_ac_raster, predict_i4x4_mode,
};
use crate::residual::{
    dequant_4x4_full, dequant_8x8, dequant_chroma_dc, dequant_luma_dc_i16x16, inverse_dct_4x4,
    inverse_dct_8x8, inverse_hadamard_2x2, inverse_hadamard_4x4, BLOCK_INDEX_TO_OFFSET,
    OFFSET_TO_BLOCK, ZIGZAG_4X4, ZIGZAG_4X4_FIELD, ZIGZAG_8X8_CABAC, ZIGZAG_8X8_CABAC_FIELD,
};
use crate::slice_context::{SliceContext, SliceParams};

/// Result of decoding a single CABAC macroblock.
pub(crate) enum CabacMbResult {
    /// MB decoded successfully; caller should increment mb_idx and continue.
    Decoded,
    /// End-of-slice detected via CABAC terminate; caller should break out of MB loop.
    EndOfSlice,
}

impl SliceContext<'_> {
    /// Decode a single CABAC macroblock (skip, P/B inter, or intra).
    ///
    /// Returns `CabacMbResult::Decoded` on success or `CabacMbResult::EndOfSlice`
    /// when the CABAC terminate bin signals end of slice.
    #[allow(clippy::needless_range_loop)]
    #[allow(clippy::too_many_arguments)]
    /// Decode MBAFF mb_field_decoding_flag using CABAC contexts 70-72.
    fn decode_mbaff_field_flag(
        &mut self,
        cr: &mut CabacReader,
        st: &mut [u8; 1024],
        mb_idx: usize,
    ) {
        let pair_addr = mb_idx / 2;
        let pair_col = pair_addr % self.mb_width as usize;
        let pair_row = pair_addr / self.mb_width as usize;
        let cond_a = if pair_col > 0 {
            self.mb_field_decoding[pair_addr - 1] as u16
        } else {
            0
        };
        let cond_b = if pair_row > 0 {
            self.mb_field_decoding[pair_addr - self.mb_width as usize] as u16
        } else {
            0
        };
        let ctx_idx = 70 + cond_a + cond_b;
        self.mb_field_decoding[pair_addr] = cr.get_cabac(&mut st[ctx_idx as usize]) != 0;
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn decode_cabac_mb(
        &mut self,
        cr: &mut CabacReader,
        st: &mut [u8; 1024],
        rbsp: &[u8],
        mb_idx: usize,
        mb_x: usize,
        mb_y: usize,
        sp: &SliceParams,
    ) -> Result<CabacMbResult, DecodeError> {
        // End-of-slice terminate: for non-MBAFF, decoded for every non-first MB.
        // For MBAFF, terminate is ONLY decoded at the END of bottom MBs (by the caller loop).
        // The START terminate here is skipped entirely for MBAFF.
        let first_mb_addr = if self.mbaff {
            (sp.first_mb_in_slice as usize) * 2
        } else {
            sp.first_mb_in_slice as usize
        };
        let should_check_terminate = if self.mbaff {
            false // MBAFF terminate handled by caller (END of bottom MBs)
        } else {
            mb_idx > first_mb_addr
        };
        if should_check_terminate && cr.get_cabac_terminate() != 0 {
            return Ok(CabacMbResult::EndOfSlice);
        }

        // Select field or frame coefficient scan tables
        let field_scan = self.is_field_scan(mb_idx);
        let zigzag_4x4 = if field_scan {
            &ZIGZAG_4X4_FIELD
        } else {
            &ZIGZAG_4X4
        };
        let zigzag_8x8_cabac = if field_scan {
            &ZIGZAG_8X8_CABAC_FIELD
        } else {
            &ZIGZAG_8X8_CABAC
        };

        // MBAFF: decode mb_field_decoding_flag (spec 7.3.4, contexts 70-72)
        // For I-slices: decoded for all top MBs.
        // For P/B-slices: the ordering is skip-first, then field_flag if NOT skipped.
        //   - Top MB: decode skip first. If skipped, immediately decode bottom skip.
        //     Field_flag is decoded only when a non-skipped MB is found.
        //   - Bottom MB (when top was skipped): skip first, then field_flag if not skipped.
        // This matches FFmpeg/x264's CABAC encoding order.
        if self.mbaff && !(sp.is_p_slice || sp.is_b_slice) {
            // I-slice: field_flag for all top MBs (no skip flags exist)
            if mb_idx.is_multiple_of(2) {
                self.decode_mbaff_field_flag(cr, st, mb_idx);
            }
        }
        // For non-P/B, set layout now (field_flag already read).
        // For P/B, layout is set after skip/field_flag handling below.
        if !(sp.is_p_slice || sp.is_b_slice) {
            self.set_mb_layout(mb_idx, mb_x, mb_y);
        }

        // P/B-slice CABAC path
        if sp.is_p_slice || sp.is_b_slice {
            // Decode skip flag
            // Unavailable neighbors are treated as skipped (ctx not incremented)
            let left_skip = if let Some(left) = self.left_mb(mb_idx) {
                self.mb_skip[left]
            } else {
                true
            };
            let top_skip = if let Some(above) = self.above_mb(mb_idx) {
                self.mb_skip[above]
            } else {
                true
            };
            let is_skip = cr.decode_mb_skip(st, left_skip, top_skip, sp.is_b_slice);

            // MBAFF: after skip flag, handle field_flag based on skip result
            if self.mbaff {
                let is_top = mb_idx.is_multiple_of(2);
                if is_top && is_skip {
                    // Top MB is skipped. Check bottom skip immediately (FFmpeg pattern).
                    // The bottom's skip_flag is decoded WITHOUT field_flag first.
                    // Field_flag is only decoded if the bottom is NOT skipped.
                    // Store this skip result for the current top MB.
                    // The bottom will be handled in the next iteration.
                    // NO field_flag decoded for fully-skipped pair.
                } else if is_top && !is_skip {
                    // Top MB is NOT skipped. Decode field_flag now before macroblock_layer.
                    self.decode_mbaff_field_flag(cr, st, mb_idx);
                } else if !is_top && self.mb_skip[mb_idx - 1] && !is_skip {
                    // Bottom MB NOT skipped, but top was skipped.
                    // Decode field_flag now.
                    self.decode_mbaff_field_flag(cr, st, mb_idx);
                }
                // Bottom MB skipped (regardless of top): no field_flag needed.
            }
            self.set_mb_layout(mb_idx, mb_x, mb_y);

            if is_skip {
                self.mb_skip[mb_idx] = true;
                if sp.is_p_slice {
                    // P_Skip: median MV, ref=0, no residual
                    self.decode_p_skip_mb(mb_idx, mb_x, mb_y, sp);
                } else {
                    // B_Skip: spatial/temporal direct MV + MC, no residual
                    self.decode_b_skip_mb(mb_idx, mb_x, mb_y, sp);
                }
                // Skip MBs have zero MVD — clear for CABAC neighbor context
                for blk in 0..16 {
                    self.mvd_store[mb_idx * 16 + blk] = [0, 0];
                    self.mvd_store_l1[mb_idx * 16 + blk] = [0, 0];
                }
                self.mb_info[mb_idx] = MbInfo {
                    mb_type: MbType::Inter,
                    qp_y: self.prev_mb_qp,
                    ..Default::default()
                };
                self.last_qp_delta_nonzero = false;
                return Ok(CabacMbResult::Decoded);
            }

            // Non-skip: decode mb_type
            let raw_mb_type = if sp.is_p_slice {
                cr.decode_p_mb_type(st)
            } else {
                let left_not_direct = if let Some(left) = self.left_mb(mb_idx) {
                    !self.mb_is_direct[left]
                } else {
                    false
                };
                let top_not_direct = if let Some(above) = self.above_mb(mb_idx) {
                    !self.mb_is_direct[above]
                } else {
                    false
                };
                cr.decode_b_mb_type(st, left_not_direct, top_not_direct)
            };

            // Check if it's intra-in-P/B
            let intra_limit = if sp.is_p_slice { 5 } else { 23 };
            if raw_mb_type >= intra_limit {
                // Intra in P/B slice — decode as I-slice mb_type
                // The mb_type already includes the intra type from decode_intra_mb_type
                let i_mb_type = raw_mb_type - intra_limit;

                // Set ref_idx to -1 for all blocks (intra MB)
                for blk in 0..16 {
                    self.ref_idx_store_l0[mb_idx * 16 + blk] = -1;
                    self.ref_idx_store_l1[mb_idx * 16 + blk] = -1;
                }

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
                        && self.mb_slice_id[mb_idx - self.mb_width as usize + 1]
                            == self.this_slice_id
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

                let intra_avail = self.intra_avail_map(sp);

                if i_mb_type == 0 {
                    // I4x4/I8x8 in P/B
                    let nts = {
                        let left = if let Some(left) = self.left_mb(mb_idx) {
                            self.mb_is_8x8dct[left] as usize
                        } else {
                            0
                        };
                        let top = if let Some(above) = self.above_mb(mb_idx) {
                            self.mb_is_8x8dct[above] as usize
                        } else {
                            0
                        };
                        left + top
                    };
                    let use_8x8_intra_pb =
                        sp.transform_8x8_mode_flag && cr.get_cabac(&mut st[399 + nts]) != 0;
                    self.mb_is_8x8dct[mb_idx] = use_8x8_intra_pb;

                    let num_modes = if use_8x8_intra_pb { 4 } else { 16 };
                    let mut pred_modes = [2u8; 16];
                    for blk_idx in 0..num_modes {
                        let blk = if use_8x8_intra_pb {
                            blk_idx * 4
                        } else {
                            blk_idx
                        };
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
                        let mode = cr.decode_intra4x4_pred_mode(st, predicted);
                        if use_8x8_intra_pb {
                            for sub in 0..4 {
                                pred_modes[blk_idx * 4 + sub] = mode;
                                self.i4x4_modes[mb_idx * 16 + blk_idx * 4 + sub] = mode;
                            }
                        } else {
                            pred_modes[blk] = mode;
                            self.i4x4_modes[mb_idx * 16 + blk] = mode;
                        }
                    }
                    let left_cm = if let Some(left) = self.left_mb(mb_idx) {
                        self.mb_chroma_pred[left]
                    } else {
                        0
                    };
                    let top_cm = if let Some(above) = self.above_mb(mb_idx) {
                        self.mb_chroma_pred[above]
                    } else {
                        0
                    };
                    let intra_chroma_pred_mode = cr.decode_chroma_pred_mode(st, left_cm, top_cm);
                    self.mb_chroma_pred[mb_idx] = intra_chroma_pred_mode;

                    let (left_cbp, top_cbp, left_cbp_c, top_cbp_c) =
                        self.cabac_cbp_context(mb_idx, false);
                    let cbp_luma = cr.decode_cbp_luma(st, left_cbp, top_cbp);
                    let cbp_chroma = cr.decode_cbp_chroma(st, left_cbp_c, top_cbp_c);
                    self.mb_cbp[mb_idx] = (cbp_luma as u16) | ((cbp_chroma as u16) << 4);

                    let mb_qp = if cbp_luma != 0 || cbp_chroma != 0 {
                        let delta = cr.decode_mb_qp_delta(st, self.last_qp_delta_nonzero);
                        self.last_qp_delta_nonzero = delta != 0;
                        sp.apply_mb_qp_delta(self.prev_mb_qp, delta)?
                    } else {
                        self.last_qp_delta_nonzero = false;
                        self.prev_mb_qp
                    };
                    self.prev_mb_qp = mb_qp;
                    let qp_y = sp.luma_qp_prime(mb_qp);
                    let (qp_cb, qp_cr) = sp.chroma_qp_prime(mb_qp);

                    // Decode luma residual + prediction
                    if use_8x8_intra_pb {
                        // I8x8 in P/B: same as I-slice I8x8 CABAC path
                        let mut luma_residual = [0i32; 256];
                        for i8x8 in 0..4 {
                            if cbp_luma & (1 << i8x8) != 0 {
                                // cat=5: no coded_block_flag, CBP bit is sufficient
                                let (coeffs, tc) = cr.decode_residual_cabac_field(
                                    st,
                                    5,
                                    64,
                                    self.is_field_coded(mb_idx),
                                );
                                let tc_per = tc.div_ceil(4);
                                for sub in 0..4 {
                                    self.nc_luma[mb_idx * 16 + i8x8 * 4 + sub] = tc_per;
                                }
                                let mut block_8x8 = [0i32; 64];
                                for (pos, val) in &coeffs {
                                    block_8x8[zigzag_8x8_cabac[*pos]] = *val;
                                }
                                dequant_8x8(&mut block_8x8, qp_y, &sp.scaling_list_8x8[0]);
                                inverse_dct_8x8(&mut block_8x8);
                                let row_off = (i8x8 / 2) * 8;
                                let col_off = (i8x8 % 2) * 8;
                                for r in 0..8 {
                                    for c in 0..8 {
                                        luma_residual[(row_off + r) * 16 + col_off + c] =
                                            block_8x8[r * 8 + c];
                                    }
                                }
                            }
                        }
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
                        // I4x4 path (existing)
                        for blk in 0..16 {
                            let (blk_row, blk_col) = BLOCK_INDEX_TO_OFFSET[blk];
                            let px = mb_x + blk_col;
                            let py = mb_y + blk_row;
                            let mut block_coeffs = [0i32; 16];
                            if cbp_luma & (1 << (blk / 4)) != 0 {
                                let left_nz = cabac_neighbor_nz_luma(
                                    self.nc_luma,
                                    mb_idx,
                                    self.mb_width as usize,
                                    blk,
                                    true,
                                    true,
                                    self.mb_slice_id,
                                    self.this_slice_id,
                                    self.mbaff,
                                    self.mb_field_decoding,
                                );
                                let top_nz = cabac_neighbor_nz_luma(
                                    self.nc_luma,
                                    mb_idx,
                                    self.mb_width as usize,
                                    blk,
                                    false,
                                    true,
                                    self.mb_slice_id,
                                    self.this_slice_id,
                                    self.mbaff,
                                    self.mb_field_decoding,
                                );
                                if cr.decode_coded_block_flag(st, 2, left_nz, top_nz) {
                                    let (coeffs, tc) = cr.decode_residual_cabac_field(
                                        st,
                                        2,
                                        16,
                                        self.is_field_coded(mb_idx),
                                    );
                                    self.nc_luma[mb_idx * 16 + blk] = tc;
                                    for (pos, val) in &coeffs {
                                        let (r, c) = zigzag_4x4[*pos];
                                        block_coeffs[r * 4 + c] = *val;
                                    }
                                    dequant_4x4_full(
                                        &mut block_coeffs,
                                        qp_y,
                                        &sp.scaling_list_4x4[0],
                                    );
                                }
                            }
                            inverse_dct_4x4(&mut block_coeffs);

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
                    } // close if use_8x8_intra_pb else

                    // Chroma prediction
                    let _chroma_width = (self.width / 2) as usize;
                    let _chroma_mb_x = mb_x / 2;
                    let _chroma_mb_y = mb_y / 2;
                    let (pred_u, pred_v) = self.predict_chroma_intra(
                        mb_x,
                        intra_chroma_pred_mode,
                        above_mb_avail,
                        left_mb_avail,
                        above_left_mb_avail,
                    );

                    // Chroma DC residual
                    let mut chroma_dc_cb = [0i32; 4];
                    let mut chroma_dc_cr = [0i32; 4];
                    if cbp_chroma >= 1 {
                        let left_dc_nz = if let Some(left) = self.left_mb(mb_idx) {
                            (self.mb_cbp[left] >> 6) & 1 != 0
                        } else {
                            true
                        };
                        let top_dc_nz = if let Some(above) = self.above_mb(mb_idx) {
                            (self.mb_cbp[above] >> 6) & 1 != 0
                        } else {
                            true
                        };
                        if cr.decode_coded_block_flag(st, 3, left_dc_nz, top_dc_nz) {
                            let (coeffs, _) = cr.decode_residual_cabac_field(
                                st,
                                3,
                                4,
                                self.is_field_coded(mb_idx),
                            );
                            for (p, v) in coeffs {
                                chroma_dc_cb[p] = v;
                            }
                            self.mb_cbp[mb_idx] |= 0x40;
                        }

                        let left_dc_nz_cr = if let Some(left) = self.left_mb(mb_idx) {
                            (self.mb_cbp[left] >> 7) & 1 != 0
                        } else {
                            true
                        };
                        let top_dc_nz_cr = if let Some(above) = self.above_mb(mb_idx) {
                            (self.mb_cbp[above] >> 7) & 1 != 0
                        } else {
                            true
                        };
                        if cr.decode_coded_block_flag(st, 3, left_dc_nz_cr, top_dc_nz_cr) {
                            let (coeffs, _) = cr.decode_residual_cabac_field(
                                st,
                                3,
                                4,
                                self.is_field_coded(mb_idx),
                            );
                            for (p, v) in coeffs {
                                chroma_dc_cr[p] = v;
                            }
                            self.mb_cbp[mb_idx] |= 0x80;
                        }
                    }

                    // Chroma AC residual
                    let mut chroma_ac_cb = [[0i32; 15]; 4];
                    let mut chroma_ac_cr = [[0i32; 15]; 4];
                    if cbp_chroma >= 2 {
                        for blk in 0..4 {
                            let left_nz = cabac_neighbor_nz_chroma(
                                self.nc_cb,
                                mb_idx,
                                self.mb_width as usize,
                                blk,
                                true,
                                true,
                                self.mb_slice_id,
                                self.this_slice_id,
                                self.mbaff,
                                self.mb_field_decoding,
                            );
                            let top_nz = cabac_neighbor_nz_chroma(
                                self.nc_cb,
                                mb_idx,
                                self.mb_width as usize,
                                blk,
                                false,
                                true,
                                self.mb_slice_id,
                                self.this_slice_id,
                                self.mbaff,
                                self.mb_field_decoding,
                            );
                            if cr.decode_coded_block_flag(st, 4, left_nz, top_nz) {
                                let (coeffs, tc) = cr.decode_residual_cabac_field(
                                    st,
                                    4,
                                    15,
                                    self.is_field_coded(mb_idx),
                                );
                                self.nc_cb[mb_idx * 4 + blk] = tc;
                                for (p, v) in coeffs {
                                    chroma_ac_cb[blk][p] = v;
                                }
                            }
                        }
                        for blk in 0..4 {
                            let left_nz = cabac_neighbor_nz_chroma(
                                self.nc_cr,
                                mb_idx,
                                self.mb_width as usize,
                                blk,
                                true,
                                true,
                                self.mb_slice_id,
                                self.this_slice_id,
                                self.mbaff,
                                self.mb_field_decoding,
                            );
                            let top_nz = cabac_neighbor_nz_chroma(
                                self.nc_cr,
                                mb_idx,
                                self.mb_width as usize,
                                blk,
                                false,
                                true,
                                self.mb_slice_id,
                                self.this_slice_id,
                                self.mbaff,
                                self.mb_field_decoding,
                            );
                            if cr.decode_coded_block_flag(st, 4, left_nz, top_nz) {
                                let (coeffs, tc) = cr.decode_residual_cabac_field(
                                    st,
                                    4,
                                    15,
                                    self.is_field_coded(mb_idx),
                                );
                                self.nc_cr[mb_idx * 4 + blk] = tc;
                                for (p, v) in coeffs {
                                    chroma_ac_cr[blk][p] = v;
                                }
                            }
                        }
                    }

                    // Chroma reconstruction
                    {
                        self.reconstruct_chroma_plane(
                            &mut chroma_dc_cb,
                            &chroma_ac_cb,
                            &pred_u,
                            true,
                            cbp_chroma,
                            qp_cb,
                            &sp.scaling_list_4x4[1],
                            mb_x,
                            mb_idx,
                        );
                        self.reconstruct_chroma_plane(
                            &mut chroma_dc_cr,
                            &chroma_ac_cr,
                            &pred_v,
                            false,
                            cbp_chroma,
                            qp_cr,
                            &sp.scaling_list_4x4[2],
                            mb_x,
                            mb_idx,
                        );
                    }
                    self.mb_info[mb_idx] = MbInfo {
                        mb_type: MbType::Intra,
                        qp_y: mb_qp,
                        ..Default::default()
                    };
                } else if i_mb_type <= 24 {
                    // I16x16 in P/B
                    self.is_i16x16[mb_idx] = true;
                    let mt = i_mb_type - 1;
                    let i16_pred = (mt % 4) as u8;
                    let cbp_chroma = ((mt / 4) % 3) as u8;
                    let cbp_luma = if mt >= 12 { 15u8 } else { 0u8 };
                    let left_cm = if let Some(left) = self.left_mb(mb_idx) {
                        self.mb_chroma_pred[left]
                    } else {
                        0
                    };
                    let top_cm = if let Some(above) = self.above_mb(mb_idx) {
                        self.mb_chroma_pred[above]
                    } else {
                        0
                    };
                    let intra_chroma_pred_mode = cr.decode_chroma_pred_mode(st, left_cm, top_cm);
                    self.mb_chroma_pred[mb_idx] = intra_chroma_pred_mode;

                    let delta = cr.decode_mb_qp_delta(st, self.last_qp_delta_nonzero);
                    self.last_qp_delta_nonzero = delta != 0;
                    let mb_qp = sp.apply_mb_qp_delta(self.prev_mb_qp, delta)?;
                    self.prev_mb_qp = mb_qp;
                    let qp_y = sp.luma_qp_prime(mb_qp);
                    let (qp_cb, qp_cr) = sp.chroma_qp_prime(mb_qp);

                    // Luma DC (cat=0, 16 coefficients)
                    let mut luma_dc = [0i32; 16];
                    let dc_left_nz = if let Some(left) = self.left_mb(mb_idx) {
                        (self.mb_cbp[left] >> 8) & 1 != 0
                    } else {
                        true
                    };
                    let dc_top_nz = if let Some(above) = self.above_mb(mb_idx) {
                        (self.mb_cbp[above] >> 8) & 1 != 0
                    } else {
                        true
                    };
                    if cr.decode_coded_block_flag(st, 0, dc_left_nz, dc_top_nz) {
                        let (coeffs, _) =
                            cr.decode_residual_cabac_field(st, 0, 16, self.is_field_coded(mb_idx));
                        for (p, v) in coeffs {
                            luma_dc[p] = v;
                        }
                        self.mb_cbp[mb_idx] |= 0x100;
                    }

                    // Luma AC (cat=1, 15 coefficients per block)
                    let mut luma_ac = [[0i32; 15]; 16];
                    if cbp_luma != 0 {
                        for blk in 0..16 {
                            let left_nz = cabac_neighbor_nz_luma(
                                self.nc_luma,
                                mb_idx,
                                self.mb_width as usize,
                                blk,
                                true,
                                true,
                                self.mb_slice_id,
                                self.this_slice_id,
                                self.mbaff,
                                self.mb_field_decoding,
                            );
                            let top_nz = cabac_neighbor_nz_luma(
                                self.nc_luma,
                                mb_idx,
                                self.mb_width as usize,
                                blk,
                                false,
                                true,
                                self.mb_slice_id,
                                self.this_slice_id,
                                self.mbaff,
                                self.mb_field_decoding,
                            );
                            if cr.decode_coded_block_flag(st, 1, left_nz, top_nz) {
                                let (coeffs, tc) = cr.decode_residual_cabac_field(
                                    st,
                                    1,
                                    15,
                                    self.is_field_coded(mb_idx),
                                );
                                self.nc_luma[mb_idx * 16 + blk] = tc;
                                for (p, v) in coeffs {
                                    luma_ac[blk][p] = v;
                                }
                            }
                        }
                    }

                    // Reconstruct luma: unzigzag DC, Hadamard, dequant
                    let mut luma_dc_raster = [0i32; 16];
                    for i in 0..16 {
                        let (r, c) = zigzag_4x4[i];
                        luma_dc_raster[r * 4 + c] = luma_dc[i];
                    }
                    inverse_hadamard_4x4(&mut luma_dc_raster);
                    dequant_luma_dc_i16x16(&mut luma_dc_raster, qp_y, sp.scaling_list_4x4[0][0]);

                    const DC_RASTER_TO_BLOCK_P: [usize; 16] =
                        [0, 1, 4, 5, 2, 3, 6, 7, 8, 9, 12, 13, 10, 11, 14, 15];
                    let mut luma_residual = [0i32; 256];
                    for blk in 0..16 {
                        let (blk_row, blk_col) = BLOCK_INDEX_TO_OFFSET[blk];
                        let mut block_raster = [0i32; 16];
                        let dc_idx = DC_RASTER_TO_BLOCK_P.iter().position(|&b| b == blk).unwrap();
                        block_raster[0] = luma_dc_raster[dc_idx];

                        if cbp_luma != 0 {
                            for scan_idx in 0..15 {
                                let (r, c) = zigzag_4x4[scan_idx + 1];
                                block_raster[r * 4 + c] = luma_ac[blk][scan_idx];
                            }
                            dequant_4x4_ac_raster(&mut block_raster, qp_y, &sp.scaling_list_4x4[0]);
                        }
                        inverse_dct_4x4(&mut block_raster);

                        for r in 0..4 {
                            for c in 0..4 {
                                luma_residual[(blk_row + r) * 16 + blk_col + c] =
                                    block_raster[r * 4 + c];
                            }
                        }
                    }

                    // I16x16 prediction + residual
                    self.reconstruct_luma_16x16(
                        mb_x,
                        i16_pred,
                        &luma_residual,
                        above_mb_avail,
                        left_mb_avail,
                        above_left_mb_avail,
                    );

                    // Chroma prediction
                    let _chroma_width = (self.width / 2) as usize;
                    let _chroma_mb_x = mb_x / 2;
                    let _chroma_mb_y = mb_y / 2;
                    let (pred_u, pred_v) = self.predict_chroma_intra(
                        mb_x,
                        intra_chroma_pred_mode,
                        above_mb_avail,
                        left_mb_avail,
                        above_left_mb_avail,
                    );

                    // Chroma DC residual
                    let mut chroma_dc_cb = [0i32; 4];
                    let mut chroma_dc_cr = [0i32; 4];
                    if cbp_chroma >= 1 {
                        let left_dc_nz = if let Some(left) = self.left_mb(mb_idx) {
                            (self.mb_cbp[left] >> 6) & 1 != 0
                        } else {
                            true
                        };
                        let top_dc_nz = if let Some(above) = self.above_mb(mb_idx) {
                            (self.mb_cbp[above] >> 6) & 1 != 0
                        } else {
                            true
                        };
                        if cr.decode_coded_block_flag(st, 3, left_dc_nz, top_dc_nz) {
                            let (coeffs, _) = cr.decode_residual_cabac_field(
                                st,
                                3,
                                4,
                                self.is_field_coded(mb_idx),
                            );
                            for (p, v) in coeffs {
                                chroma_dc_cb[p] = v;
                            }
                            self.mb_cbp[mb_idx] |= 0x40;
                        }
                        let left_dc_nz_cr = if let Some(left) = self.left_mb(mb_idx) {
                            (self.mb_cbp[left] >> 7) & 1 != 0
                        } else {
                            true
                        };
                        let top_dc_nz_cr = if let Some(above) = self.above_mb(mb_idx) {
                            (self.mb_cbp[above] >> 7) & 1 != 0
                        } else {
                            true
                        };
                        if cr.decode_coded_block_flag(st, 3, left_dc_nz_cr, top_dc_nz_cr) {
                            let (coeffs, _) = cr.decode_residual_cabac_field(
                                st,
                                3,
                                4,
                                self.is_field_coded(mb_idx),
                            );
                            for (p, v) in coeffs {
                                chroma_dc_cr[p] = v;
                            }
                            self.mb_cbp[mb_idx] |= 0x80;
                        }
                    }

                    // Chroma AC residual
                    let mut chroma_ac_cb = [[0i32; 15]; 4];
                    let mut chroma_ac_cr = [[0i32; 15]; 4];
                    if cbp_chroma >= 2 {
                        for blk in 0..4 {
                            let left_nz = cabac_neighbor_nz_chroma(
                                self.nc_cb,
                                mb_idx,
                                self.mb_width as usize,
                                blk,
                                true,
                                true,
                                self.mb_slice_id,
                                self.this_slice_id,
                                self.mbaff,
                                self.mb_field_decoding,
                            );
                            let top_nz = cabac_neighbor_nz_chroma(
                                self.nc_cb,
                                mb_idx,
                                self.mb_width as usize,
                                blk,
                                false,
                                true,
                                self.mb_slice_id,
                                self.this_slice_id,
                                self.mbaff,
                                self.mb_field_decoding,
                            );
                            if cr.decode_coded_block_flag(st, 4, left_nz, top_nz) {
                                let (coeffs, tc) = cr.decode_residual_cabac_field(
                                    st,
                                    4,
                                    15,
                                    self.is_field_coded(mb_idx),
                                );
                                self.nc_cb[mb_idx * 4 + blk] = tc;
                                for (p, v) in coeffs {
                                    chroma_ac_cb[blk][p] = v;
                                }
                            }
                        }
                        for blk in 0..4 {
                            let left_nz = cabac_neighbor_nz_chroma(
                                self.nc_cr,
                                mb_idx,
                                self.mb_width as usize,
                                blk,
                                true,
                                true,
                                self.mb_slice_id,
                                self.this_slice_id,
                                self.mbaff,
                                self.mb_field_decoding,
                            );
                            let top_nz = cabac_neighbor_nz_chroma(
                                self.nc_cr,
                                mb_idx,
                                self.mb_width as usize,
                                blk,
                                false,
                                true,
                                self.mb_slice_id,
                                self.this_slice_id,
                                self.mbaff,
                                self.mb_field_decoding,
                            );
                            if cr.decode_coded_block_flag(st, 4, left_nz, top_nz) {
                                let (coeffs, tc) = cr.decode_residual_cabac_field(
                                    st,
                                    4,
                                    15,
                                    self.is_field_coded(mb_idx),
                                );
                                self.nc_cr[mb_idx * 4 + blk] = tc;
                                for (p, v) in coeffs {
                                    chroma_ac_cr[blk][p] = v;
                                }
                            }
                        }
                    }
                    self.mb_cbp[mb_idx] |= (cbp_luma as u16) | ((cbp_chroma as u16) << 4);

                    // Chroma reconstruction
                    {
                        self.reconstruct_chroma_plane(
                            &mut chroma_dc_cb,
                            &chroma_ac_cb,
                            &pred_u,
                            true,
                            cbp_chroma,
                            qp_cb,
                            &sp.scaling_list_4x4[1],
                            mb_x,
                            mb_idx,
                        );
                        self.reconstruct_chroma_plane(
                            &mut chroma_dc_cr,
                            &chroma_ac_cr,
                            &pred_v,
                            false,
                            cbp_chroma,
                            qp_cr,
                            &sp.scaling_list_4x4[2],
                            mb_x,
                            mb_idx,
                        );
                    }

                    self.mb_info[mb_idx] = MbInfo {
                        mb_type: MbType::Intra,
                        qp_y: mb_qp,
                        ..Default::default()
                    };
                } else {
                    // I_PCM in P/B via CABAC
                    let pcm_pos = cr.pcm_byte_position();
                    // pcm_sample_luma / pcm_sample_chroma are u(BitDepthY) / u(BitDepthC) (spec 7.3.5)
                    let pcm_data = rbsp.get(pcm_pos..).ok_or(DecodeError::UnexpectedEof)?;
                    let pcm_len = self
                        .write_pcm_samples(pcm_data, mb_x)
                        .ok_or(DecodeError::UnexpectedEof)?;
                    for blk in 0..16 {
                        self.nc_luma[mb_idx * 16 + blk] = 16;
                    }
                    for blk in 0..4 {
                        self.nc_cb[mb_idx * 4 + blk] = 16;
                        self.nc_cr[mb_idx * 4 + blk] = 16;
                    }
                    cr.reinit(pcm_pos + pcm_len);
                    self.prev_mb_qp = 0;
                    self.last_qp_delta_nonzero = false;
                    self.is_i16x16[mb_idx] = true; // I_PCM treated as I16x16 for CABAC context
                    self.mb_cbp[mb_idx] = 0;
                    self.mb_info[mb_idx] = MbInfo {
                        mb_type: MbType::Ipcm,
                        qp_y: 0,
                        ..Default::default()
                    };
                }
                return Ok(CabacMbResult::Decoded);
            }

            // Inter MB: decode using CABAC syntax elements
            if sp.is_p_slice {
                // P-slice inter: types 0-4
                let is_p8x8 = raw_mb_type == 3 || raw_mb_type == 4;
                let (part_w, part_h, num_parts) = match raw_mb_type {
                    0 => (16usize, 16usize, 1usize), // P_L0_16x16
                    1 => (16, 8, 2),                 // P_L0_L0_16x8
                    2 => (8, 16, 2),                 // P_L0_L0_8x16
                    3 | 4 => (8, 8, 4),              // P_8x8 / P_8x8ref0
                    _ => unreachable!(),
                };

                let sub_mb_origins = [(0usize, 0usize), (0, 8), (8, 0), (8, 8)];
                let mut sub_mb_types = [0u32; 4];
                if is_p8x8 {
                    // Sub-MB types
                    for smt in &mut sub_mb_types {
                        *smt = cr.decode_p_sub_mb_type(st);
                    }
                    // Parse ref_idx
                    let mut sub_ref = [0i8; 4];
                    if raw_mb_type == 3 {
                        for (smb, sr) in sub_ref.iter_mut().enumerate() {
                            if self.effective_num_ref(mb_idx, sp.num_ref_idx_l0_active) > 1 {
                                let (sy, sx) = sub_mb_origins[smb];
                                let (left_ref, top_ref) = cabac_neighbor_ref(
                                    self.ref_idx_store_l0,
                                    mb_idx,
                                    self.mb_width as usize,
                                    sy,
                                    sx,
                                    self.mb_slice_id,
                                    self.this_slice_id,
                                    self.mb_is_direct,
                                    self.blk_is_direct,
                                    sp.is_b_slice,
                                    self.mbaff,
                                    self.mb_field_decoding,
                                );
                                *sr = cr.decode_ref_idx(st, left_ref, top_ref);
                            }
                            // Write ref_idx immediately for neighbor context
                            let (sy, sx) = sub_mb_origins[smb];
                            for r in (0..8).step_by(4) {
                                for c in (0..8).step_by(4) {
                                    let lr = (sy + r) / 4;
                                    let lc = (sx + c) / 4;
                                    let blk = OFFSET_TO_BLOCK[lr][lc];
                                    self.ref_idx_store_l0[mb_idx * 16 + blk] = *sr;
                                }
                            }
                        }
                    }
                    // Parse MVDs and reconstruct
                    for smb in 0..4 {
                        let (sy, sx) = sub_mb_origins[smb];
                        let ref_idx = sub_ref[smb];
                        let sub_parts_layout: &[(usize, usize, usize, usize)] =
                            match sub_mb_types[smb] {
                                0 => &[(0, 0, 8, 8)],
                                1 => &[(0, 0, 8, 4), (0, 4, 8, 4)],
                                2 => &[(0, 0, 4, 8), (4, 0, 4, 8)],
                                3 => &[(0, 0, 4, 4), (4, 0, 4, 4), (0, 4, 4, 4), (4, 4, 4, 4)],
                                _ => return Err(DecodeError::from("invalid sub_mb_type")),
                            };
                        for &(dx, dy, spw, sph) in sub_parts_layout {
                            let px = sx + dx;
                            let py = sy + dy;
                            let amvd_x = cabac_amvd(
                                self.mvd_store,
                                mb_idx,
                                self.mb_width as usize,
                                py,
                                px,
                                0,
                                self.mb_slice_id,
                                self.this_slice_id,
                                self.mbaff,
                                self.mb_field_decoding,
                            );
                            let amvd_y = cabac_amvd(
                                self.mvd_store,
                                mb_idx,
                                self.mb_width as usize,
                                py,
                                px,
                                1,
                                self.mb_slice_id,
                                self.this_slice_id,
                                self.mbaff,
                                self.mb_field_decoding,
                            );
                            let mvd_x = cr.decode_mvd_comp(st, 40, amvd_x) as i16;
                            let mvd_y = cr.decode_mvd_comp(st, 47, amvd_y) as i16;
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
                            for r in (0..sph).step_by(4) {
                                for c in (0..spw).step_by(4) {
                                    let lr = (py + r) / 4;
                                    let lc = (px + c) / 4;
                                    let blk = OFFSET_TO_BLOCK[lr][lc];
                                    self.mv_store_l0[mb_idx * 16 + blk] = mv;
                                    self.ref_idx_store_l0[mb_idx * 16 + blk] = ref_idx;
                                    self.mvd_store[mb_idx * 16 + blk] = [mvd_x, mvd_y];
                                }
                            }
                            // MC
                            let ref_pic = sp
                                .ref_pic_list
                                .get(self.frame_ref_idx(mb_idx, ref_idx) as usize)
                                .or_else(|| sp.ref_pic_list.last())
                                .ok_or(DecodeError::InvalidSyntax(
                                    "P-slice references empty ref_pic_list",
                                ))?;
                            let (mc_y, ref_stride, ref_y_off, _mc_cy, _c_ref_stride, _c_ref_off) =
                                self.mc_params(mb_idx, mb_y, ref_pic.width as usize, ref_idx);
                            let mut luma_pred = [0u16; 256]; // stack: max 16x16
                            inter_pred::luma_mc_stride(
                                ref_pic,
                                (mb_x + px) as i32,
                                mc_y + py as i32,
                                mv[0] as i32,
                                mv[1] as i32,
                                spw,
                                sph,
                                &mut luma_pred,
                                ref_stride,
                                ref_y_off,
                            );
                            if sp.use_weight == 1 {
                                sp.wctx
                                    .apply_uni(&mut luma_pred, 0, ref_idx as usize, false, 0);
                            }
                            for r in 0..sph {
                                for c in 0..spw {
                                    self.frame.y[self.ly_offset
                                        + (py + r) * self.ly_stride
                                        + mb_x
                                        + px
                                        + c] = luma_pred[r * spw + c];
                                }
                            }
                        }
                    }
                    // Chroma MC for P_8x8 (per sub-partition)
                    let cx = mb_x / 2;
                    for smb in 0..4 {
                        let (sy, sx) = sub_mb_origins[smb];
                        let ref_pic = sp
                            .ref_pic_list
                            .get(self.frame_ref_idx(mb_idx, sub_ref[smb]) as usize)
                            .or_else(|| sp.ref_pic_list.last())
                            .ok_or(DecodeError::InvalidSyntax(
                                "P_8x8 references empty ref_pic_list",
                            ))?;
                        let (_mc_y, _rs, _ref_y_off, mc_cy, c_ref_stride, c_ref_off) =
                            self.mc_params(mb_idx, mb_y, ref_pic.width as usize, sub_ref[smb]);
                        let sub_parts: &[(usize, usize, usize, usize)] = match sub_mb_types[smb] {
                            0 => &[(0, 0, 8, 8)],
                            1 => &[(0, 0, 8, 4), (0, 4, 8, 4)],
                            2 => &[(0, 0, 4, 8), (4, 0, 4, 8)],
                            3 => &[(0, 0, 4, 4), (4, 0, 4, 4), (0, 4, 4, 4), (4, 4, 4, 4)],
                            _ => &[(0, 0, 8, 8)],
                        };
                        for &(dx, dy, spw, sph) in sub_parts {
                            let px = sx + dx;
                            let py = sy + dy;
                            let blk_idx = OFFSET_TO_BLOCK[py / 4][px / 4];
                            let mv = self.mv_store_l0[mb_idx * 16 + blk_idx];
                            let ccx = px / 2;
                            let ccy = py / 2;
                            let ccw = spw.max(2) / 2;
                            let cch = sph.max(2) / 2;
                            if ccw == 0 || cch == 0 {
                                continue;
                            }
                            let mut cb_pred = [0u16; 64]; // stack: max 8x8
                            let mut cr_pred_buf = [0u16; 64]; // stack: max 8x8
                            let cmv_y = mv[1] as i32
                                + crate::slice_context::chroma_field_mv_offset_impl(
                                    self.field_pic_flag,
                                    self.bottom_field_flag,
                                    ref_pic,
                                );
                            inter_pred::chroma_mc(
                                &ref_pic.u[c_ref_off..],
                                c_ref_stride,
                                (self.height / 2) as usize,
                                (cx + ccx) as i32,
                                (mc_cy + ccy) as i32,
                                mv[0] as i32,
                                cmv_y,
                                ccw,
                                cch,
                                &mut cb_pred,
                            );
                            inter_pred::chroma_mc(
                                &ref_pic.v[c_ref_off..],
                                c_ref_stride,
                                (self.height / 2) as usize,
                                (cx + ccx) as i32,
                                (mc_cy + ccy) as i32,
                                mv[0] as i32,
                                cmv_y,
                                ccw,
                                cch,
                                &mut cr_pred_buf,
                            );
                            if sp.use_weight == 1 {
                                sp.wctx
                                    .apply_uni(&mut cb_pred, 0, sub_ref[smb] as usize, true, 0);
                                sp.wctx.apply_uni(
                                    &mut cr_pred_buf,
                                    0,
                                    sub_ref[smb] as usize,
                                    true,
                                    1,
                                );
                            }
                            for r in 0..cch {
                                for c in 0..ccw {
                                    self.frame.u[self.lc_offset
                                        + (ccy + r) * self.lc_stride
                                        + cx
                                        + ccx
                                        + c] = cb_pred[r * ccw + c];
                                    self.frame.v[self.lc_offset
                                        + (ccy + r) * self.lc_stride
                                        + cx
                                        + ccx
                                        + c] = cr_pred_buf[r * ccw + c];
                                }
                            }
                        }
                    }
                } else {
                    // P_L0_16x16, P16x8, P8x16
                    let mut part_ref = [0i8; 2];
                    for (p, ref_entry) in part_ref.iter_mut().enumerate().take(num_parts) {
                        if self.effective_num_ref(mb_idx, sp.num_ref_idx_l0_active) > 1 {
                            let (py_off, px_off) = match raw_mb_type {
                                1 => (p * 8, 0),
                                2 => (0, p * 8),
                                _ => (0, 0),
                            };
                            let (left_ref, top_ref) = cabac_neighbor_ref(
                                self.ref_idx_store_l0,
                                mb_idx,
                                self.mb_width as usize,
                                py_off,
                                px_off,
                                self.mb_slice_id,
                                self.this_slice_id,
                                self.mb_is_direct,
                                self.blk_is_direct,
                                sp.is_b_slice,
                                self.mbaff,
                                self.mb_field_decoding,
                            );
                            *ref_entry = cr.decode_ref_idx(st, left_ref, top_ref);
                        }
                        // Write ref_idx to store immediately so the next
                        // partition can see it as a neighbor.
                        let (py_off, px_off) = match raw_mb_type {
                            1 => (p * 8, 0),
                            2 => (0, p * 8),
                            _ => (0, 0),
                        };
                        for r in (0..part_h).step_by(4) {
                            for c in (0..part_w).step_by(4) {
                                let lr = (py_off + r) / 4;
                                let lc = (px_off + c) / 4;
                                let blk = OFFSET_TO_BLOCK[lr][lc];
                                self.ref_idx_store_l0[mb_idx * 16 + blk] = *ref_entry;
                            }
                        }
                    }
                    for p in 0..num_parts {
                        let (py_off, px_off) = match raw_mb_type {
                            1 => (p * 8, 0),
                            2 => (0, p * 8),
                            _ => (0, 0),
                        };
                        let amvd_x = cabac_amvd(
                            self.mvd_store,
                            mb_idx,
                            self.mb_width as usize,
                            py_off,
                            px_off,
                            0,
                            self.mb_slice_id,
                            self.this_slice_id,
                            self.mbaff,
                            self.mb_field_decoding,
                        );
                        let amvd_y = cabac_amvd(
                            self.mvd_store,
                            mb_idx,
                            self.mb_width as usize,
                            py_off,
                            px_off,
                            1,
                            self.mb_slice_id,
                            self.this_slice_id,
                            self.mbaff,
                            self.mb_field_decoding,
                        );
                        let mvd_x = cr.decode_mvd_comp(st, 40, amvd_x) as i16;
                        let mvd_y = cr.decode_mvd_comp(st, 47, amvd_y) as i16;
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
                        // Store MV, ref, and MVD
                        for r in (0..part_h).step_by(4) {
                            for c in (0..part_w).step_by(4) {
                                let lr = (py_off + r) / 4;
                                let lc = (px_off + c) / 4;
                                let blk = OFFSET_TO_BLOCK[lr][lc];
                                self.mv_store_l0[mb_idx * 16 + blk] = mv;
                                self.ref_idx_store_l0[mb_idx * 16 + blk] = part_ref[p];
                                self.mvd_store[mb_idx * 16 + blk] = [mvd_x, mvd_y];
                            }
                        }

                        // MC
                        let ref_pic = sp
                            .ref_pic_list
                            .get(self.frame_ref_idx(mb_idx, part_ref[p]) as usize)
                            .or_else(|| sp.ref_pic_list.last())
                            .ok_or(DecodeError::InvalidSyntax(
                                "P 16x8/8x16 references empty ref_pic_list",
                            ))?;
                        let (mc_y, ref_stride, ref_y_off, mc_cy, c_ref_stride, c_ref_off) =
                            self.mc_params(mb_idx, mb_y, ref_pic.width as usize, part_ref[p]);
                        let mut luma_pred = [0u16; 256]; // stack: max 16x16
                        inter_pred::luma_mc_stride(
                            ref_pic,
                            (mb_x + px_off) as i32,
                            mc_y + py_off as i32,
                            mv[0] as i32,
                            mv[1] as i32,
                            part_w,
                            part_h,
                            &mut luma_pred,
                            ref_stride,
                            ref_y_off,
                        );
                        if sp.use_weight == 1 {
                            sp.wctx
                                .apply_uni(&mut luma_pred, 0, part_ref[p] as usize, false, 0);
                        }
                        for r in 0..part_h {
                            for c in 0..part_w {
                                self.frame.y[self.ly_offset
                                    + (py_off + r) * self.ly_stride
                                    + mb_x
                                    + px_off
                                    + c] = luma_pred[r * part_w + c];
                            }
                        }

                        // Chroma
                        let cx_off = px_off / 2;
                        let cy_off = py_off / 2;
                        let chroma_mb_x = mb_x / 2;
                        let chw = part_w.max(2) / 2;
                        let chh = part_h.max(2) / 2;
                        let mut cb_pred = [0u16; 64]; // stack: max 8x8
                        let mut cr_pred_buf = [0u16; 64]; // stack: max 8x8
                        let cmv_y = mv[1] as i32
                            + crate::slice_context::chroma_field_mv_offset_impl(
                                self.field_pic_flag,
                                self.bottom_field_flag,
                                ref_pic,
                            );
                        inter_pred::chroma_mc(
                            &ref_pic.u[c_ref_off..],
                            c_ref_stride,
                            (self.height / 2) as usize,
                            (chroma_mb_x + cx_off) as i32,
                            (mc_cy + cy_off) as i32,
                            mv[0] as i32,
                            cmv_y,
                            chw,
                            chh,
                            &mut cb_pred,
                        );
                        inter_pred::chroma_mc(
                            &ref_pic.v[c_ref_off..],
                            c_ref_stride,
                            (self.height / 2) as usize,
                            (chroma_mb_x + cx_off) as i32,
                            (mc_cy + cy_off) as i32,
                            mv[0] as i32,
                            cmv_y,
                            chw,
                            chh,
                            &mut cr_pred_buf,
                        );
                        if sp.use_weight == 1 {
                            sp.wctx
                                .apply_uni(&mut cb_pred, 0, part_ref[p] as usize, true, 0);
                            sp.wctx
                                .apply_uni(&mut cr_pred_buf, 0, part_ref[p] as usize, true, 1);
                        }
                        for r in 0..chh {
                            for c in 0..chw {
                                self.frame.u[self.lc_offset
                                    + (cy_off + r) * self.lc_stride
                                    + chroma_mb_x
                                    + cx_off
                                    + c] = cb_pred[r * chw + c];
                                self.frame.v[self.lc_offset
                                    + (cy_off + r) * self.lc_stride
                                    + chroma_mb_x
                                    + cx_off
                                    + c] = cr_pred_buf[r * chw + c];
                            }
                        }
                    }
                }

                // Residual (inter uses CBP_INTER_TABLE equivalent from CABAC)
                let (left_cbp, top_cbp, left_cbp_c, top_cbp_c) =
                    self.cabac_cbp_context(mb_idx, false);
                let cbp_luma = cr.decode_cbp_luma(st, left_cbp, top_cbp);
                let cbp_chroma = cr.decode_cbp_chroma(st, left_cbp_c, top_cbp_c);
                self.mb_cbp[mb_idx] = (cbp_luma as u16) | ((cbp_chroma as u16) << 4);
                // 8x8 transform flag for inter MBs (spec 7.3.5).
                // Only present when noSubMbPartSizeLessThan8x8Flag is true.
                // For P_8x8, this requires all sub_mb_types to be 0 (8x8 sub-partitions).
                let no_sub_less_than_8x8 = if is_p8x8 {
                    sub_mb_types.iter().all(|&smt| smt == 0)
                } else {
                    true // P_16x16, P_16x8, P_8x16 always have partitions >= 8x8
                };
                let use_8x8_inter =
                    if sp.transform_8x8_mode_flag && cbp_luma != 0 && no_sub_less_than_8x8 {
                        let nts = {
                            let left = if let Some(left) = self.left_mb(mb_idx) {
                                self.mb_is_8x8dct[left] as usize
                            } else {
                                0
                            };
                            let top = if let Some(above) = self.above_mb(mb_idx) {
                                self.mb_is_8x8dct[above] as usize
                            } else {
                                0
                            };
                            left + top
                        };
                        cr.get_cabac(&mut st[399 + nts]) != 0
                    } else {
                        false
                    };
                self.mb_is_8x8dct[mb_idx] = use_8x8_inter;

                let mb_qp = if cbp_luma != 0 || cbp_chroma != 0 {
                    let delta = cr.decode_mb_qp_delta(st, self.last_qp_delta_nonzero);
                    self.last_qp_delta_nonzero = delta != 0;
                    sp.apply_mb_qp_delta(self.prev_mb_qp, delta)?
                } else {
                    self.last_qp_delta_nonzero = false;
                    self.prev_mb_qp
                };
                self.prev_mb_qp = mb_qp;
                let qp_y = sp.luma_qp_prime(mb_qp);
                // Decode and add luma residual
                if cbp_luma != 0 {
                    let mut luma_residual = [0i32; 256];
                    if use_8x8_inter {
                        // 8x8 transform: 4 blocks of 64 coefficients
                        let scale_8x8 = &sp.scaling_list_8x8[1]; // inter
                        for i8x8 in 0..4 {
                            if cbp_luma & (1 << i8x8) == 0 {
                                continue;
                            }
                            // Note: cat=5 (8x8 luma) does NOT use coded_block_flag.
                            // The CBP luma bit alone indicates coefficients are present.
                            let (coeffs, tc) = cr.decode_residual_cabac_field(
                                st,
                                5,
                                64,
                                self.is_field_coded(mb_idx),
                            );
                            let tc_per = tc.div_ceil(4);
                            for sub in 0..4 {
                                self.nc_luma[mb_idx * 16 + i8x8 * 4 + sub] = tc_per;
                            }
                            let mut block_8x8 = [0i32; 64];
                            for (pos, val) in &coeffs {
                                block_8x8[zigzag_8x8_cabac[*pos]] = *val;
                            }
                            dequant_8x8(&mut block_8x8, qp_y, scale_8x8);
                            inverse_dct_8x8(&mut block_8x8);
                            let row_off = (i8x8 / 2) * 8;
                            let col_off = (i8x8 % 2) * 8;
                            for r in 0..8 {
                                for c in 0..8 {
                                    luma_residual[(row_off + r) * 16 + col_off + c] =
                                        block_8x8[r * 8 + c];
                                }
                            }
                        }
                    } else {
                        // 4x4 transform (existing path)
                        for blk in 0..16 {
                            if cbp_luma & (1 << (blk / 4)) != 0 {
                                let left_nz = cabac_neighbor_nz_luma(
                                    self.nc_luma,
                                    mb_idx,
                                    self.mb_width as usize,
                                    blk,
                                    true,
                                    false,
                                    self.mb_slice_id,
                                    self.this_slice_id,
                                    self.mbaff,
                                    self.mb_field_decoding,
                                );
                                let top_nz = cabac_neighbor_nz_luma(
                                    self.nc_luma,
                                    mb_idx,
                                    self.mb_width as usize,
                                    blk,
                                    false,
                                    false,
                                    self.mb_slice_id,
                                    self.this_slice_id,
                                    self.mbaff,
                                    self.mb_field_decoding,
                                );
                                let cbf = cr.decode_coded_block_flag(st, 2, left_nz, top_nz);
                                if cbf {
                                    let (coeffs, tc) = cr.decode_residual_cabac_field(
                                        st,
                                        2,
                                        16,
                                        self.is_field_coded(mb_idx),
                                    );
                                    self.nc_luma[mb_idx * 16 + blk] = tc;
                                    let mut block_coeffs = [0i32; 16];
                                    for (pos, val) in &coeffs {
                                        let (r, c) = zigzag_4x4[*pos];
                                        block_coeffs[r * 4 + c] = *val;
                                    }
                                    dequant_4x4_full(
                                        &mut block_coeffs,
                                        qp_y,
                                        &sp.scaling_list_4x4[3],
                                    );
                                    inverse_dct_4x4(&mut block_coeffs);
                                    let (blk_row, blk_col) = BLOCK_INDEX_TO_OFFSET[blk];
                                    for r in 0..4 {
                                        for c in 0..4 {
                                            luma_residual[(blk_row + r) * 16 + blk_col + c] =
                                                block_coeffs[r * 4 + c];
                                        }
                                    }
                                }
                            }
                        }
                    } // close if use_8x8_inter else
                      // Add residual to prediction
                    for r in 0..16 {
                        for c in 0..16 {
                            let val = (self.frame.y[self.ly_offset + r * self.ly_stride + mb_x + c]
                                as i32
                                + luma_residual[r * 16 + c])
                                .clamp(0, self.pixel_max_y)
                                as u16;
                            self.frame.y[self.ly_offset + r * self.ly_stride + mb_x + c] = val;
                        }
                    }
                }

                // Chroma residual
                let (qp_cb, qp_cr) = sp.chroma_qp_prime(mb_qp);
                if cbp_chroma >= 1 {
                    let mut chroma_dc_cb = [0i32; 4];
                    let mut chroma_dc_cr = [0i32; 4];
                    let left_dc_nz = if let Some(left) = self.left_mb(mb_idx) {
                        (self.mb_cbp[left] >> 6) & 1 != 0
                    } else {
                        false
                    };
                    let top_dc_nz = if let Some(above) = self.above_mb(mb_idx) {
                        (self.mb_cbp[above] >> 6) & 1 != 0
                    } else {
                        false
                    };
                    if cr.decode_coded_block_flag(st, 3, left_dc_nz, top_dc_nz) {
                        let (coeffs, _) =
                            cr.decode_residual_cabac_field(st, 3, 4, self.is_field_coded(mb_idx));
                        for (pos, val) in coeffs {
                            chroma_dc_cb[pos] = val;
                        }
                        self.mb_cbp[mb_idx] |= 0x40;
                    }
                    let left_dc_cr = if let Some(left) = self.left_mb(mb_idx) {
                        (self.mb_cbp[left] >> 7) & 1 != 0
                    } else {
                        false
                    };
                    let top_dc_cr = if let Some(above) = self.above_mb(mb_idx) {
                        (self.mb_cbp[above] >> 7) & 1 != 0
                    } else {
                        false
                    };
                    if cr.decode_coded_block_flag(st, 3, left_dc_cr, top_dc_cr) {
                        let (coeffs, _) =
                            cr.decode_residual_cabac_field(st, 3, 4, self.is_field_coded(mb_idx));
                        for (pos, val) in coeffs {
                            chroma_dc_cr[pos] = val;
                        }
                        self.mb_cbp[mb_idx] |= 0x80;
                    }

                    let chroma_mb_x = mb_x / 2;
                    let lc_off = self.lc_offset;
                    let lc_str = self.lc_stride;
                    let is_field_mb = self.is_field_coded(mb_idx);
                    for (plane_dc, frame_plane, scale_idx) in [
                        (&mut chroma_dc_cb, &mut self.frame.u, 4usize),
                        (&mut chroma_dc_cr, &mut self.frame.v, 5usize),
                    ] {
                        let chroma_scale = &sp.scaling_list_4x4[scale_idx];
                        // Cb uses scaling list 1 (intra) / 4 (inter), Cr 2 / 5.
                        let qp_c = if matches!(scale_idx, 1 | 4) {
                            qp_cb
                        } else {
                            qp_cr
                        };
                        inverse_hadamard_2x2(plane_dc);
                        dequant_chroma_dc(plane_dc, qp_c, chroma_scale[0]);
                        let mut chroma_residual = [0i32; 64];
                        for blk in 0..4 {
                            let blk_row = (blk / 2) * 4;
                            let blk_col = (blk % 2) * 4;
                            let mut block_raster = [0i32; 16];
                            block_raster[0] = plane_dc[blk];
                            if cbp_chroma >= 2 {
                                let nc_arr = if scale_idx == 4 {
                                    &self.nc_cb
                                } else {
                                    &self.nc_cr
                                };
                                let left_nz = cabac_neighbor_nz_chroma(
                                    nc_arr,
                                    mb_idx,
                                    self.mb_width as usize,
                                    blk,
                                    true,
                                    false,
                                    self.mb_slice_id,
                                    self.this_slice_id,
                                    self.mbaff,
                                    self.mb_field_decoding,
                                );
                                let top_nz = cabac_neighbor_nz_chroma(
                                    nc_arr,
                                    mb_idx,
                                    self.mb_width as usize,
                                    blk,
                                    false,
                                    false,
                                    self.mb_slice_id,
                                    self.this_slice_id,
                                    self.mbaff,
                                    self.mb_field_decoding,
                                );
                                if cr.decode_coded_block_flag(st, 4, left_nz, top_nz) {
                                    let (coeffs, tc) =
                                        cr.decode_residual_cabac_field(st, 4, 15, is_field_mb);
                                    if scale_idx == 4 {
                                        self.nc_cb[mb_idx * 4 + blk] = tc;
                                    } else {
                                        self.nc_cr[mb_idx * 4 + blk] = tc;
                                    }
                                    for (pos, val) in coeffs {
                                        let (r, c) = zigzag_4x4[pos + 1];
                                        block_raster[r * 4 + c] = val;
                                    }
                                    dequant_4x4_ac_raster(&mut block_raster, qp_c, chroma_scale);
                                }
                            }
                            inverse_dct_4x4(&mut block_raster);
                            for r in 0..4 {
                                for c in 0..4 {
                                    chroma_residual[(blk_row + r) * 8 + blk_col + c] =
                                        block_raster[r * 4 + c];
                                }
                            }
                        }
                        for y in 0..8 {
                            for x in 0..8 {
                                let val = (frame_plane[lc_off + y * lc_str + chroma_mb_x + x]
                                    as i32
                                    + chroma_residual[y * 8 + x])
                                    .clamp(0, self.pixel_max_c)
                                    as u16;
                                frame_plane[lc_off + y * lc_str + chroma_mb_x + x] = val;
                            }
                        }
                    }
                }

                self.mb_info[mb_idx] = MbInfo {
                    mb_type: MbType::Inter,
                    qp_y: mb_qp,
                    ..Default::default()
                };
                return Ok(CabacMbResult::Decoded);
            } else {
                // B-slice inter with CABAC
                // raw_mb_type mapping (from decode_b_mb_type):
                // 0: B_Direct_16x16
                // 1: B_L0_16x16, 2: B_L1_16x16, 3: B_Bi_16x16
                // 4-21: B 16x8/8x16 partition variants
                // 22: B_8x8
                // Track whether all sub-partitions are >= 8x8 (for transform_size_8x8_flag)
                let mut no_sub_less_than_8x8_b = true;

                #[rustfmt::skip]
                #[allow(clippy::type_complexity)]
                const B_PART_TABLE: [(usize, usize, [(bool, bool); 2]); 18] = [
                    (16, 8, [(true,false),(true,false)]),  // 4: B_L0_L0_16x8
                    (8, 16, [(true,false),(true,false)]),  // 5: B_L0_L0_8x16
                    (16, 8, [(false,true),(false,true)]),  // 6: B_L1_L1_16x8
                    (8, 16, [(false,true),(false,true)]),  // 7: B_L1_L1_8x16
                    (16, 8, [(true,false),(false,true)]),  // 8: B_L0_L1_16x8
                    (8, 16, [(true,false),(false,true)]),  // 9: B_L0_L1_8x16
                    (16, 8, [(false,true),(true,false)]),  // 10: B_L1_L0_16x8
                    (8, 16, [(false,true),(true,false)]),  // 11: B_L1_L0_8x16
                    (16, 8, [(true,false),(true,true)]),   // 12: B_L0_Bi_16x8
                    (8, 16, [(true,false),(true,true)]),   // 13: B_L0_Bi_8x16
                    (16, 8, [(false,true),(true,true)]),   // 14: B_L1_Bi_16x8
                    (8, 16, [(false,true),(true,true)]),   // 15: B_L1_Bi_8x16
                    (16, 8, [(true,true),(true,false)]),   // 16: B_Bi_L0_16x8
                    (8, 16, [(true,true),(true,false)]),   // 17: B_Bi_L0_8x16
                    (16, 8, [(true,true),(false,true)]),   // 18: B_Bi_L1_16x8
                    (8, 16, [(true,true),(false,true)]),   // 19: B_Bi_L1_8x16
                    (16, 8, [(true,true),(true,true)]),    // 20: B_Bi_Bi_16x8
                    (8, 16, [(true,true),(true,true)]),    // 21: B_Bi_Bi_8x16
                ];

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

                #[derive(Clone, Copy, Default)]
                struct BSubPart {
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
                let mut b_sub_parts = [BSubPart::default(); 16];
                let mut b_sub_count = 0usize;

                if raw_mb_type == 0 {
                    // B_Direct_16x16: derive MVs per 4x4 block
                    self.mb_is_direct[mb_idx] = true;
                    for blk in 0..16 {
                        self.blk_is_direct[mb_idx * 16 + blk] = true;
                    }
                    if !sp.direct_8x8_inference_flag {
                        no_sub_less_than_8x8_b = false;
                    }
                    self.derive_direct_mvs(mb_idx, 0, 16, sp);
                    // B_Direct has zero MVD
                    for blk in 0..16 {
                        self.mvd_store[mb_idx * 16 + blk] = [0, 0];
                        self.mvd_store_l1[mb_idx * 16 + blk] = [0, 0];
                    }
                    // Build sub_parts, coalescing per-8x8 when MVs are uniform
                    let base = mb_idx * 16;
                    for i8x8 in 0..4 {
                        let blk0 = i8x8 * 4;
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
                        let (blk_row, blk_col) = BLOCK_INDEX_TO_OFFSET[blk0];
                        if uniform {
                            b_sub_parts[b_sub_count] = BSubPart {
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
                            };
                            b_sub_count += 1;
                        } else {
                            for sub in 0..4 {
                                let b = blk0 + sub;
                                let (br, bc) = BLOCK_INDEX_TO_OFFSET[b];
                                let rl0 = self.ref_idx_store_l0[base + b];
                                let rl1 = self.ref_idx_store_l1[base + b];
                                b_sub_parts[b_sub_count] = BSubPart {
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
                                };
                                b_sub_count += 1;
                            }
                        }
                    }
                } else if raw_mb_type <= 3 {
                    // B_L0_16x16 (1), B_L1_16x16 (2), B_Bi_16x16 (3)
                    let pred_l0 = raw_mb_type == 1 || raw_mb_type == 3;
                    let pred_l1 = raw_mb_type == 2 || raw_mb_type == 3;

                    let mut ref_l0 = 0i8;
                    let mut ref_l1 = 0i8;

                    if pred_l0 && self.effective_num_ref(mb_idx, sp.num_ref_idx_l0_active) > 1 {
                        let (left_ref, top_ref) = cabac_neighbor_ref(
                            self.ref_idx_store_l0,
                            mb_idx,
                            self.mb_width as usize,
                            0,
                            0,
                            self.mb_slice_id,
                            self.this_slice_id,
                            self.mb_is_direct,
                            self.blk_is_direct,
                            sp.is_b_slice,
                            self.mbaff,
                            self.mb_field_decoding,
                        );
                        ref_l0 = cr.decode_ref_idx(st, left_ref, top_ref);
                    }
                    if pred_l1 && self.effective_num_ref(mb_idx, sp.num_ref_idx_l1_active) > 1 {
                        let (left_ref, top_ref) = cabac_neighbor_ref(
                            self.ref_idx_store_l1,
                            mb_idx,
                            self.mb_width as usize,
                            0,
                            0,
                            self.mb_slice_id,
                            self.this_slice_id,
                            self.mb_is_direct,
                            self.blk_is_direct,
                            sp.is_b_slice,
                            self.mbaff,
                            self.mb_field_decoding,
                        );
                        ref_l1 = cr.decode_ref_idx(st, left_ref, top_ref);
                    }

                    let mut mv_l0 = [0i16; 2];
                    let mut mv_l1 = [0i16; 2];

                    if pred_l0 {
                        let amvd_x = cabac_amvd(
                            self.mvd_store,
                            mb_idx,
                            self.mb_width as usize,
                            0,
                            0,
                            0,
                            self.mb_slice_id,
                            self.this_slice_id,
                            self.mbaff,
                            self.mb_field_decoding,
                        );
                        let amvd_y = cabac_amvd(
                            self.mvd_store,
                            mb_idx,
                            self.mb_width as usize,
                            0,
                            0,
                            1,
                            self.mb_slice_id,
                            self.this_slice_id,
                            self.mbaff,
                            self.mb_field_decoding,
                        );
                        let mvd_x = cr.decode_mvd_comp(st, 40, amvd_x) as i16;
                        let mvd_y = cr.decode_mvd_comp(st, 47, amvd_y) as i16;
                        let (mvp_x, mvp_y) = predict_mv(
                            self.mv_store_l0,
                            self.ref_idx_store_l0,
                            mb_idx,
                            self.mb_width as usize,
                            0,
                            16,
                            16,
                            ref_l0,
                            self.mb_slice_id,
                            self.this_slice_id,
                            MbaffCtx {
                                mbaff: self.mbaff,
                                mb_field_decoding: self.mb_field_decoding,
                            },
                        );
                        mv_l0 = [mvp_x + mvd_x, mvp_y + mvd_y];
                        for blk in 0..16 {
                            self.mv_store_l0[mb_idx * 16 + blk] = mv_l0;
                            self.ref_idx_store_l0[mb_idx * 16 + blk] = ref_l0;
                            self.mvd_store[mb_idx * 16 + blk] = [mvd_x, mvd_y];
                        }
                    }
                    if pred_l1 {
                        let amvd_x = cabac_amvd(
                            self.mvd_store_l1,
                            mb_idx,
                            self.mb_width as usize,
                            0,
                            0,
                            0,
                            self.mb_slice_id,
                            self.this_slice_id,
                            self.mbaff,
                            self.mb_field_decoding,
                        );
                        let amvd_y = cabac_amvd(
                            self.mvd_store_l1,
                            mb_idx,
                            self.mb_width as usize,
                            0,
                            0,
                            1,
                            self.mb_slice_id,
                            self.this_slice_id,
                            self.mbaff,
                            self.mb_field_decoding,
                        );
                        let mvd_x = cr.decode_mvd_comp(st, 40, amvd_x) as i16;
                        let mvd_y = cr.decode_mvd_comp(st, 47, amvd_y) as i16;
                        let (mvp_x, mvp_y) = predict_mv(
                            self.mv_store_l1,
                            self.ref_idx_store_l1,
                            mb_idx,
                            self.mb_width as usize,
                            0,
                            16,
                            16,
                            ref_l1,
                            self.mb_slice_id,
                            self.this_slice_id,
                            MbaffCtx {
                                mbaff: self.mbaff,
                                mb_field_decoding: self.mb_field_decoding,
                            },
                        );
                        mv_l1 = [mvp_x + mvd_x, mvp_y + mvd_y];
                        for blk in 0..16 {
                            self.mv_store_l1[mb_idx * 16 + blk] = mv_l1;
                            self.ref_idx_store_l1[mb_idx * 16 + blk] = ref_l1;
                            self.mvd_store_l1[mb_idx * 16 + blk] = [mvd_x, mvd_y];
                        }
                    }

                    // Set default ref_idx for inactive list
                    if !pred_l0 {
                        for blk in 0..16 {
                            self.ref_idx_store_l0[mb_idx * 16 + blk] = -1;
                            self.mv_store_l0[mb_idx * 16 + blk] = [0, 0];
                            self.mvd_store[mb_idx * 16 + blk] = [0, 0];
                        }
                    }
                    if !pred_l1 {
                        for blk in 0..16 {
                            self.ref_idx_store_l1[mb_idx * 16 + blk] = -1;
                            self.mv_store_l1[mb_idx * 16 + blk] = [0, 0];
                            self.mvd_store_l1[mb_idx * 16 + blk] = [0, 0];
                        }
                    }

                    b_sub_parts[b_sub_count] = BSubPart {
                        x: 0,
                        y: 0,
                        w: 16,
                        h: 16,
                        ref_idx_l0: if pred_l0 { ref_l0 } else { -1 },
                        ref_idx_l1: if pred_l1 { ref_l1 } else { -1 },
                        mv_l0,
                        mv_l1,
                        pred_l0,
                        pred_l1,
                    };
                    b_sub_count += 1;
                } else if raw_mb_type <= 21 {
                    // B 16x8/8x16 partition variants (mb_type 4-21)
                    let entry = B_PART_TABLE[(raw_mb_type - 4) as usize];
                    let (part_w, part_h) = (entry.0, entry.1);
                    let pred_flags = entry.2;

                    // Parse ref_idx: L0 for all partitions, then L1 for all
                    let mut part_ref_l0 = [-1i8; 2];
                    let mut part_ref_l1 = [-1i8; 2];
                    for p in 0..2 {
                        if pred_flags[p].0 {
                            if self.effective_num_ref(mb_idx, sp.num_ref_idx_l0_active) > 1 {
                                let (py_off, px_off) =
                                    if part_h == 8 { (p * 8, 0) } else { (0, p * 8) };
                                let (left_ref, top_ref) = cabac_neighbor_ref(
                                    self.ref_idx_store_l0,
                                    mb_idx,
                                    self.mb_width as usize,
                                    py_off,
                                    px_off,
                                    self.mb_slice_id,
                                    self.this_slice_id,
                                    self.mb_is_direct,
                                    self.blk_is_direct,
                                    sp.is_b_slice,
                                    self.mbaff,
                                    self.mb_field_decoding,
                                );
                                part_ref_l0[p] = cr.decode_ref_idx(st, left_ref, top_ref);
                            } else {
                                part_ref_l0[p] = 0;
                            }
                        }
                        // Write L0 ref_idx immediately for neighbor context
                        let (py_off, px_off) = if part_h == 8 { (p * 8, 0) } else { (0, p * 8) };
                        for r in (0..part_h).step_by(4) {
                            for c in (0..part_w).step_by(4) {
                                let lr = (py_off + r) / 4;
                                let lc = (px_off + c) / 4;
                                let blk = OFFSET_TO_BLOCK[lr][lc];
                                self.ref_idx_store_l0[mb_idx * 16 + blk] = part_ref_l0[p];
                            }
                        }
                    }
                    for p in 0..2 {
                        if pred_flags[p].1 {
                            if self.effective_num_ref(mb_idx, sp.num_ref_idx_l1_active) > 1 {
                                let (py_off, px_off) =
                                    if part_h == 8 { (p * 8, 0) } else { (0, p * 8) };
                                let (left_ref, top_ref) = cabac_neighbor_ref(
                                    self.ref_idx_store_l1,
                                    mb_idx,
                                    self.mb_width as usize,
                                    py_off,
                                    px_off,
                                    self.mb_slice_id,
                                    self.this_slice_id,
                                    self.mb_is_direct,
                                    self.blk_is_direct,
                                    sp.is_b_slice,
                                    self.mbaff,
                                    self.mb_field_decoding,
                                );
                                part_ref_l1[p] = cr.decode_ref_idx(st, left_ref, top_ref);
                            } else {
                                part_ref_l1[p] = 0;
                            }
                        }
                        // Write L1 ref_idx immediately for neighbor context
                        let (py_off, px_off) = if part_h == 8 { (p * 8, 0) } else { (0, p * 8) };
                        for r in (0..part_h).step_by(4) {
                            for c in (0..part_w).step_by(4) {
                                let lr = (py_off + r) / 4;
                                let lc = (px_off + c) / 4;
                                let blk = OFFSET_TO_BLOCK[lr][lc];
                                self.ref_idx_store_l1[mb_idx * 16 + blk] = part_ref_l1[p];
                            }
                        }
                    }

                    // Parse MVDs: L0 for all partitions, then L1 for all
                    let mut mv_l0_parts = [[0i16; 2]; 2];
                    let mut mv_l1_parts = [[0i16; 2]; 2];

                    for p in 0..2 {
                        if pred_flags[p].0 {
                            let (py_off, px_off) =
                                if part_h == 8 { (p * 8, 0) } else { (0, p * 8) };
                            // Store ref_idx before MV prediction
                            for r in (0..part_h).step_by(4) {
                                for c in (0..part_w).step_by(4) {
                                    let lr = (py_off + r) / 4;
                                    let lc = (px_off + c) / 4;
                                    let blk = OFFSET_TO_BLOCK[lr][lc];
                                    self.ref_idx_store_l0[mb_idx * 16 + blk] = part_ref_l0[p];
                                }
                            }
                            let amvd_x = cabac_amvd(
                                self.mvd_store,
                                mb_idx,
                                self.mb_width as usize,
                                py_off,
                                px_off,
                                0,
                                self.mb_slice_id,
                                self.this_slice_id,
                                self.mbaff,
                                self.mb_field_decoding,
                            );
                            let amvd_y = cabac_amvd(
                                self.mvd_store,
                                mb_idx,
                                self.mb_width as usize,
                                py_off,
                                px_off,
                                1,
                                self.mb_slice_id,
                                self.this_slice_id,
                                self.mbaff,
                                self.mb_field_decoding,
                            );
                            let mvd_x = cr.decode_mvd_comp(st, 40, amvd_x) as i16;
                            let mvd_y = cr.decode_mvd_comp(st, 47, amvd_y) as i16;
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
                            // Store MV and MVD immediately for partition 1 prediction
                            for r in (0..part_h).step_by(4) {
                                for c in (0..part_w).step_by(4) {
                                    let lr = (py_off + r) / 4;
                                    let lc = (px_off + c) / 4;
                                    let blk = OFFSET_TO_BLOCK[lr][lc];
                                    self.mv_store_l0[mb_idx * 16 + blk] = mv_l0_parts[p];
                                    self.mvd_store[mb_idx * 16 + blk] = [mvd_x, mvd_y];
                                }
                            }
                        } else {
                            // Inactive L0: set ref_idx = -1, zero MV and MVD
                            let (py_off, px_off) =
                                if part_h == 8 { (p * 8, 0) } else { (0, p * 8) };
                            for r in (0..part_h).step_by(4) {
                                for c in (0..part_w).step_by(4) {
                                    let lr = (py_off + r) / 4;
                                    let lc = (px_off + c) / 4;
                                    let blk = OFFSET_TO_BLOCK[lr][lc];
                                    self.ref_idx_store_l0[mb_idx * 16 + blk] = -1;
                                    self.mv_store_l0[mb_idx * 16 + blk] = [0, 0];
                                    self.mvd_store[mb_idx * 16 + blk] = [0, 0];
                                }
                            }
                        }
                    }

                    for p in 0..2 {
                        if pred_flags[p].1 {
                            let (py_off, px_off) =
                                if part_h == 8 { (p * 8, 0) } else { (0, p * 8) };
                            // Store ref_idx before MV prediction
                            for r in (0..part_h).step_by(4) {
                                for c in (0..part_w).step_by(4) {
                                    let lr = (py_off + r) / 4;
                                    let lc = (px_off + c) / 4;
                                    let blk = OFFSET_TO_BLOCK[lr][lc];
                                    self.ref_idx_store_l1[mb_idx * 16 + blk] = part_ref_l1[p];
                                }
                            }
                            let amvd_x = cabac_amvd(
                                self.mvd_store_l1,
                                mb_idx,
                                self.mb_width as usize,
                                py_off,
                                px_off,
                                0,
                                self.mb_slice_id,
                                self.this_slice_id,
                                self.mbaff,
                                self.mb_field_decoding,
                            );
                            let amvd_y = cabac_amvd(
                                self.mvd_store_l1,
                                mb_idx,
                                self.mb_width as usize,
                                py_off,
                                px_off,
                                1,
                                self.mb_slice_id,
                                self.this_slice_id,
                                self.mbaff,
                                self.mb_field_decoding,
                            );
                            let mvd_x = cr.decode_mvd_comp(st, 40, amvd_x) as i16;
                            let mvd_y = cr.decode_mvd_comp(st, 47, amvd_y) as i16;
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
                                    self.mvd_store_l1[mb_idx * 16 + blk] = [mvd_x, mvd_y];
                                }
                            }
                        } else {
                            // Inactive L1: set ref_idx = -1, zero MV and MVD
                            let (py_off, px_off) =
                                if part_h == 8 { (p * 8, 0) } else { (0, p * 8) };
                            for r in (0..part_h).step_by(4) {
                                for c in (0..part_w).step_by(4) {
                                    let lr = (py_off + r) / 4;
                                    let lc = (px_off + c) / 4;
                                    let blk = OFFSET_TO_BLOCK[lr][lc];
                                    self.ref_idx_store_l1[mb_idx * 16 + blk] = -1;
                                    self.mv_store_l1[mb_idx * 16 + blk] = [0, 0];
                                    self.mvd_store_l1[mb_idx * 16 + blk] = [0, 0];
                                }
                            }
                        }
                    }

                    // Build sub_parts for MC
                    for p in 0..2 {
                        let (py_off, px_off) = if part_h == 8 { (p * 8, 0) } else { (0, p * 8) };
                        b_sub_parts[b_sub_count] = BSubPart {
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
                        };
                        b_sub_count += 1;
                    }
                } else if raw_mb_type == 22 {
                    // B_8x8
                    let sub_mb_origins = [(0usize, 0usize), (0, 8), (8, 0), (8, 8)];
                    let mut sub_mb_types = [0u32; 4];
                    for smt in &mut sub_mb_types {
                        *smt = cr.decode_b_sub_mb_type(st);
                    }
                    // Check if any sub-partition is smaller than 8x8
                    // (sub_mb_types 0-3 are 8x8; 4+ are sub-8x8)
                    // B_Direct_8x8 (smt==0) uses 4x4 when direct_8x8_inference_flag=0
                    if sub_mb_types
                        .iter()
                        .any(|&smt| smt > 3 || (smt == 0 && !sp.direct_8x8_inference_flag))
                    {
                        no_sub_less_than_8x8_b = false;
                    }

                    // Parse ref_idx: L0 for all sub-MBs, then L1
                    let mut sub_ref_l0 = [-1i8; 4];
                    let mut sub_ref_l1 = [-1i8; 4];
                    for smb in 0..4 {
                        if sub_mb_types[smb] == 0 {
                            // B_Direct_8x8: don't decode ref, keep -1
                        } else {
                            let (_, _, pl0, _) = B_SUB_TABLE[sub_mb_types[smb] as usize];
                            if pl0 {
                                if self.effective_num_ref(mb_idx, sp.num_ref_idx_l0_active) > 1 {
                                    let (sy, sx) = sub_mb_origins[smb];
                                    let (left_ref, top_ref) = cabac_neighbor_ref(
                                        self.ref_idx_store_l0,
                                        mb_idx,
                                        self.mb_width as usize,
                                        sy,
                                        sx,
                                        self.mb_slice_id,
                                        self.this_slice_id,
                                        self.mb_is_direct,
                                        self.blk_is_direct,
                                        sp.is_b_slice,
                                        self.mbaff,
                                        self.mb_field_decoding,
                                    );
                                    sub_ref_l0[smb] = cr.decode_ref_idx(st, left_ref, top_ref);
                                } else {
                                    sub_ref_l0[smb] = 0;
                                }
                            }
                        }
                        // Write L0 ref_idx immediately for neighbor context
                        let (sy, sx) = sub_mb_origins[smb];
                        for r in (0..8).step_by(4) {
                            for c in (0..8).step_by(4) {
                                let lr = (sy + r) / 4;
                                let lc = (sx + c) / 4;
                                let blk = OFFSET_TO_BLOCK[lr][lc];
                                self.ref_idx_store_l0[mb_idx * 16 + blk] = sub_ref_l0[smb];
                            }
                        }
                    }
                    for smb in 0..4 {
                        if sub_mb_types[smb] == 0 {
                            // B_Direct_8x8: don't decode ref
                        } else {
                            let (_, _, _, pl1) = B_SUB_TABLE[sub_mb_types[smb] as usize];
                            if pl1 {
                                if self.effective_num_ref(mb_idx, sp.num_ref_idx_l1_active) > 1 {
                                    let (sy, sx) = sub_mb_origins[smb];
                                    let (left_ref, top_ref) = cabac_neighbor_ref(
                                        self.ref_idx_store_l1,
                                        mb_idx,
                                        self.mb_width as usize,
                                        sy,
                                        sx,
                                        self.mb_slice_id,
                                        self.this_slice_id,
                                        self.mb_is_direct,
                                        self.blk_is_direct,
                                        sp.is_b_slice,
                                        self.mbaff,
                                        self.mb_field_decoding,
                                    );
                                    sub_ref_l1[smb] = cr.decode_ref_idx(st, left_ref, top_ref);
                                } else {
                                    sub_ref_l1[smb] = 0;
                                }
                            }
                        }
                        // Write L1 ref_idx immediately for neighbor context
                        let (sy, sx) = sub_mb_origins[smb];
                        for r in (0..8).step_by(4) {
                            for c in (0..8).step_by(4) {
                                let lr = (sy + r) / 4;
                                let lc = (sx + c) / 4;
                                let blk = OFFSET_TO_BLOCK[lr][lc];
                                self.ref_idx_store_l1[mb_idx * 16 + blk] = sub_ref_l1[smb];
                            }
                        }
                    }

                    // Store ref_idx into cache for MV prediction
                    for smb in 0..4 {
                        let (sy, sx) = sub_mb_origins[smb];
                        for r in (0..8).step_by(4) {
                            for c in (0..8).step_by(4) {
                                let lr = (sy + r) / 4;
                                let lc = (sx + c) / 4;
                                let blk = OFFSET_TO_BLOCK[lr][lc];
                                self.ref_idx_store_l0[mb_idx * 16 + blk] = sub_ref_l0[smb];
                                self.ref_idx_store_l1[mb_idx * 16 + blk] = sub_ref_l1[smb];
                            }
                        }
                    }

                    // Derive B_Direct_8x8 MVs per 4x4 block BEFORE MVD parsing
                    for (smb, &smt) in sub_mb_types.iter().enumerate() {
                        if smt != 0 {
                            continue;
                        }
                        self.derive_direct_mvs(mb_idx, smb * 4, 4, sp);
                        // B_Direct_8x8 has zero MVD
                        let (sy, sx) = sub_mb_origins[smb];
                        for r in (0..8).step_by(4) {
                            for c in (0..8).step_by(4) {
                                let lr = (sy + r) / 4;
                                let lc = (sx + c) / 4;
                                let blk = OFFSET_TO_BLOCK[lr][lc];
                                self.mvd_store[mb_idx * 16 + blk] = [0, 0];
                                self.mvd_store_l1[mb_idx * 16 + blk] = [0, 0];
                                self.blk_is_direct[mb_idx * 16 + blk] = true;
                            }
                        }
                    }

                    // Collect sub-partition layouts
                    #[derive(Clone, Copy)]
                    struct BSubLayout {
                        smb: usize,
                        sub_w: usize,
                        sub_h: usize,
                        pl0: bool,
                        pl1: bool,
                        offsets: [(usize, usize); 4],
                        num_offsets: usize,
                    }
                    let mut layouts = [BSubLayout {
                        smb: 0,
                        sub_w: 0,
                        sub_h: 0,
                        pl0: false,
                        pl1: false,
                        offsets: [(0, 0); 4],
                        num_offsets: 0,
                    }; 4];
                    let mut layout_count = 0usize;
                    for (smb, &smt_val) in sub_mb_types.iter().enumerate() {
                        let smt = smt_val as usize;
                        if smt == 0 {
                            layouts[layout_count] = BSubLayout {
                                smb,
                                sub_w: 8,
                                sub_h: 8,
                                pl0: false,
                                pl1: false,
                                offsets: [(0, 0), (0, 0), (0, 0), (0, 0)],
                                num_offsets: 1,
                            };
                            layout_count += 1;
                            continue;
                        }
                        let (sub_w, sub_h, pl0, pl1) = B_SUB_TABLE[smt];
                        let (offsets, num_offsets) = match (sub_w, sub_h) {
                            (8, 8) => ([(0, 0), (0, 0), (0, 0), (0, 0)], 1),
                            (8, 4) => ([(0, 0), (0, 4), (0, 0), (0, 0)], 2),
                            (4, 8) => ([(0, 0), (4, 0), (0, 0), (0, 0)], 2),
                            (4, 4) => ([(0, 0), (4, 0), (0, 4), (4, 4)], 4),
                            _ => unreachable!(),
                        };
                        layouts[layout_count] = BSubLayout {
                            smb,
                            sub_w,
                            sub_h,
                            pl0,
                            pl1,
                            offsets,
                            num_offsets,
                        };
                        layout_count += 1;
                    }

                    // Parse L0 MVDs
                    for li in 0..layout_count {
                        let layout = &layouts[li];
                        if sub_mb_types[layout.smb] == 0 {
                            continue;
                        }
                        if !layout.pl0 {
                            // Zero MV/MVD for inactive L0
                            let (sy, sx) = sub_mb_origins[layout.smb];
                            for r in (0..8).step_by(4) {
                                for c in (0..8).step_by(4) {
                                    let lr = (sy + r) / 4;
                                    let lc = (sx + c) / 4;
                                    let blk = OFFSET_TO_BLOCK[lr][lc];
                                    self.mv_store_l0[mb_idx * 16 + blk] = [0, 0];
                                    self.mvd_store[mb_idx * 16 + blk] = [0, 0];
                                }
                            }
                            continue;
                        }
                        let (sy, sx) = sub_mb_origins[layout.smb];
                        for oi in 0..layout.num_offsets {
                            let (dx, dy) = layout.offsets[oi];
                            let px = sx + dx;
                            let py = sy + dy;
                            let amvd_x = cabac_amvd(
                                self.mvd_store,
                                mb_idx,
                                self.mb_width as usize,
                                py,
                                px,
                                0,
                                self.mb_slice_id,
                                self.this_slice_id,
                                self.mbaff,
                                self.mb_field_decoding,
                            );
                            let amvd_y = cabac_amvd(
                                self.mvd_store,
                                mb_idx,
                                self.mb_width as usize,
                                py,
                                px,
                                1,
                                self.mb_slice_id,
                                self.this_slice_id,
                                self.mbaff,
                                self.mb_field_decoding,
                            );
                            let mvd_x = cr.decode_mvd_comp(st, 40, amvd_x) as i16;
                            let mvd_y = cr.decode_mvd_comp(st, 47, amvd_y) as i16;
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
                            for r in (0..layout.sub_h).step_by(4) {
                                for c in (0..layout.sub_w).step_by(4) {
                                    let lr = (py + r) / 4;
                                    let lc = (px + c) / 4;
                                    let blk = OFFSET_TO_BLOCK[lr][lc];
                                    self.mv_store_l0[mb_idx * 16 + blk] = mv;
                                    self.mvd_store[mb_idx * 16 + blk] = [mvd_x, mvd_y];
                                }
                            }
                        }
                    }

                    // Parse L1 MVDs
                    for li in 0..layout_count {
                        let layout = &layouts[li];
                        if sub_mb_types[layout.smb] == 0 {
                            continue;
                        }
                        if !layout.pl1 {
                            // Zero MV/MVD for inactive L1
                            let (sy, sx) = sub_mb_origins[layout.smb];
                            for r in (0..8).step_by(4) {
                                for c in (0..8).step_by(4) {
                                    let lr = (sy + r) / 4;
                                    let lc = (sx + c) / 4;
                                    let blk = OFFSET_TO_BLOCK[lr][lc];
                                    self.mv_store_l1[mb_idx * 16 + blk] = [0, 0];
                                    self.mvd_store_l1[mb_idx * 16 + blk] = [0, 0];
                                }
                            }
                            continue;
                        }
                        let (sy, sx) = sub_mb_origins[layout.smb];
                        for oi in 0..layout.num_offsets {
                            let (dx, dy) = layout.offsets[oi];
                            let px = sx + dx;
                            let py = sy + dy;
                            let amvd_x = cabac_amvd(
                                self.mvd_store_l1,
                                mb_idx,
                                self.mb_width as usize,
                                py,
                                px,
                                0,
                                self.mb_slice_id,
                                self.this_slice_id,
                                self.mbaff,
                                self.mb_field_decoding,
                            );
                            let amvd_y = cabac_amvd(
                                self.mvd_store_l1,
                                mb_idx,
                                self.mb_width as usize,
                                py,
                                px,
                                1,
                                self.mb_slice_id,
                                self.this_slice_id,
                                self.mbaff,
                                self.mb_field_decoding,
                            );
                            let mvd_x = cr.decode_mvd_comp(st, 40, amvd_x) as i16;
                            let mvd_y = cr.decode_mvd_comp(st, 47, amvd_y) as i16;
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
                            for r in (0..layout.sub_h).step_by(4) {
                                for c in (0..layout.sub_w).step_by(4) {
                                    let lr = (py + r) / 4;
                                    let lc = (px + c) / 4;
                                    let blk = OFFSET_TO_BLOCK[lr][lc];
                                    self.mv_store_l1[mb_idx * 16 + blk] = mv;
                                    self.mvd_store_l1[mb_idx * 16 + blk] = [mvd_x, mvd_y];
                                }
                            }
                        }
                    }

                    // Build sub_parts from stored MVs
                    for li in 0..layout_count {
                        let layout = &layouts[li];
                        let (sy, sx) = sub_mb_origins[layout.smb];
                        if sub_mb_types[layout.smb] == 0 {
                            // B_Direct_8x8: per-4x4-block sub_parts
                            let base = mb_idx * 16;
                            for r in (0..8).step_by(4) {
                                for c in (0..8).step_by(4) {
                                    let lr = (sy + r) / 4;
                                    let lc = (sx + c) / 4;
                                    let blk = OFFSET_TO_BLOCK[lr][lc];
                                    let ri_l0 = self.ref_idx_store_l0[base + blk];
                                    let ri_l1 = self.ref_idx_store_l1[base + blk];
                                    b_sub_parts[b_sub_count] = BSubPart {
                                        x: sx + c,
                                        y: sy + r,
                                        w: 4,
                                        h: 4,
                                        ref_idx_l0: ri_l0,
                                        ref_idx_l1: ri_l1,
                                        mv_l0: self.mv_store_l0[base + blk],
                                        mv_l1: self.mv_store_l1[base + blk],
                                        pred_l0: ri_l0 >= 0,
                                        pred_l1: ri_l1 >= 0,
                                    };
                                    b_sub_count += 1;
                                }
                            }
                        } else {
                            let smt = sub_mb_types[layout.smb] as usize;
                            let (_, _, pl0, pl1) = B_SUB_TABLE[smt];
                            for oi in 0..layout.num_offsets {
                                let (dx, dy) = layout.offsets[oi];
                                let px = sx + dx;
                                let py = sy + dy;
                                let blk0 = OFFSET_TO_BLOCK[py / 4][px / 4];
                                let base = mb_idx * 16;
                                b_sub_parts[b_sub_count] = BSubPart {
                                    x: px,
                                    y: py,
                                    w: layout.sub_w,
                                    h: layout.sub_h,
                                    ref_idx_l0: sub_ref_l0[layout.smb],
                                    ref_idx_l1: sub_ref_l1[layout.smb],
                                    mv_l0: self.mv_store_l0[base + blk0],
                                    mv_l1: self.mv_store_l1[base + blk0],
                                    pred_l0: pl0,
                                    pred_l1: pl1,
                                };
                                b_sub_count += 1;
                            }
                        }
                    }
                } else {
                    return Err(DecodeError::InvalidSyntax("invalid CABAC B-slice mb_type"));
                }

                // Motion compensation for all sub-parts
                for _sp_i in 0..b_sub_count {
                    let sub_part = &b_sub_parts[_sp_i];
                    let abs_x = mb_x + sub_part.x;
                    let mut luma_pred = [0u16; 256]; // stack: max 16x16

                    if sub_part.pred_l0 && sub_part.pred_l1 {
                        let mut p0 = [0u16; 256]; // stack: max 16x16
                        let mut p1 = [0u16; 256]; // stack: max 16x16
                        let frame_ri_l0 = self.frame_ref_idx(mb_idx, sub_part.ref_idx_l0);
                        let ref_l0 = ref_pic_safe(sp.ref_pic_list_l0, frame_ri_l0)
                            .ok_or(DecodeError::InvalidSyntax("empty ref list"))?;
                        let (
                            mc_y_l0,
                            ref_stride_l0,
                            ref_y_off_l0,
                            _mc_cy_l0,
                            _c_ref_stride_l0,
                            _c_ref_off_l0,
                        ) = self.mc_params(
                            mb_idx,
                            mb_y,
                            ref_l0.width as usize,
                            sub_part.ref_idx_l0,
                        );
                        inter_pred::luma_mc_stride(
                            ref_l0,
                            abs_x as i32,
                            mc_y_l0 + sub_part.y as i32,
                            sub_part.mv_l0[0] as i32,
                            sub_part.mv_l0[1] as i32,
                            sub_part.w,
                            sub_part.h,
                            &mut p0,
                            ref_stride_l0,
                            ref_y_off_l0,
                        );
                        let frame_ri_l1 = self.frame_ref_idx(mb_idx, sub_part.ref_idx_l1);
                        let ref_l1 = ref_pic_safe(sp.ref_pic_list_l1, frame_ri_l1)
                            .ok_or(DecodeError::InvalidSyntax("empty ref list"))?;
                        let (
                            mc_y_l1,
                            ref_stride_l1,
                            ref_y_off_l1,
                            _mc_cy_l1,
                            _c_ref_stride_l1,
                            _c_ref_off_l1,
                        ) = self.mc_params(
                            mb_idx,
                            mb_y,
                            ref_l1.width as usize,
                            sub_part.ref_idx_l1,
                        );
                        inter_pred::luma_mc_stride(
                            ref_l1,
                            abs_x as i32,
                            mc_y_l1 + sub_part.y as i32,
                            sub_part.mv_l1[0] as i32,
                            sub_part.mv_l1[1] as i32,
                            sub_part.w,
                            sub_part.h,
                            &mut p1,
                            ref_stride_l1,
                            ref_y_off_l1,
                        );
                        sp.wctx.apply_bi(
                            &p0,
                            &p1,
                            &mut luma_pred,
                            sub_part.ref_idx_l0 as usize,
                            sub_part.ref_idx_l1 as usize,
                            false,
                            0,
                        );
                    } else if sub_part.pred_l0 {
                        let ref_pic = ref_pic_safe(
                            sp.ref_pic_list_l0,
                            self.frame_ref_idx(mb_idx, sub_part.ref_idx_l0),
                        )
                        .ok_or(DecodeError::InvalidSyntax("empty ref list"))?;
                        let (mc_y, ref_stride, ref_y_off, _mc_cy, _c_ref_stride, _c_ref_off) = self
                            .mc_params(mb_idx, mb_y, ref_pic.width as usize, sub_part.ref_idx_l0);
                        inter_pred::luma_mc_stride(
                            ref_pic,
                            abs_x as i32,
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
                        let ref_pic = ref_pic_safe(
                            sp.ref_pic_list_l1,
                            self.frame_ref_idx(mb_idx, sub_part.ref_idx_l1),
                        )
                        .ok_or(DecodeError::InvalidSyntax("empty ref list"))?;
                        let (mc_y, ref_stride, ref_y_off, _mc_cy, _c_ref_stride, _c_ref_off) = self
                            .mc_params(mb_idx, mb_y, ref_pic.width as usize, sub_part.ref_idx_l1);
                        inter_pred::luma_mc_stride(
                            ref_pic,
                            abs_x as i32,
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
                            self.frame.y
                                [self.ly_offset + (sub_part.y + r) * self.ly_stride + abs_x + c] =
                                luma_pred[r * sub_part.w + c];
                        }
                    }
                    // Chroma MC
                    let chroma_h = (self.height / 2) as usize;
                    let cx = abs_x / 2;
                    let chw = sub_part.w.max(2) / 2;
                    let chh = sub_part.h.max(2) / 2;

                    for plane_idx in 0..2 {
                        let mut chroma_pred = [0u16; 64]; // stack: max 8x8
                        if sub_part.pred_l0 && sub_part.pred_l1 {
                            let ref_l0 = ref_pic_safe(
                                sp.ref_pic_list_l0,
                                self.frame_ref_idx(mb_idx, sub_part.ref_idx_l0),
                            )
                            .ok_or(DecodeError::InvalidSyntax("empty ref list"))?;
                            let ref_l1 = ref_pic_safe(
                                sp.ref_pic_list_l1,
                                self.frame_ref_idx(mb_idx, sub_part.ref_idx_l1),
                            )
                            .ok_or(DecodeError::InvalidSyntax("empty ref list"))?;
                            let (
                                _mc_y_l0,
                                _rs_l0,
                                _ref_y_off_l0,
                                mc_cy_l0,
                                c_ref_stride_l0,
                                c_ref_off_l0,
                            ) = self.mc_params(
                                mb_idx,
                                mb_y,
                                ref_l0.width as usize,
                                sub_part.ref_idx_l0,
                            );
                            let (
                                _mc_y_l1,
                                _rs_l1,
                                _ref_y_off_l1,
                                mc_cy_l1,
                                c_ref_stride_l1,
                                c_ref_off_l1,
                            ) = self.mc_params(
                                mb_idx,
                                mb_y,
                                ref_l1.width as usize,
                                sub_part.ref_idx_l1,
                            );
                            let cr0 = if plane_idx == 0 {
                                &ref_l0.u[c_ref_off_l0..]
                            } else {
                                &ref_l0.v[c_ref_off_l0..]
                            };
                            let cr1 = if plane_idx == 0 {
                                &ref_l1.u[c_ref_off_l1..]
                            } else {
                                &ref_l1.v[c_ref_off_l1..]
                            };
                            let mut c0 = vec![0u16; chw * chh];
                            let mut c1 = vec![0u16; chw * chh];
                            let cy_off = sub_part.y / 2;
                            let cmv_y_l0 = sub_part.mv_l0[1] as i32
                                + crate::slice_context::chroma_field_mv_offset_impl(
                                    self.field_pic_flag,
                                    self.bottom_field_flag,
                                    ref_l0,
                                );
                            let cmv_y_l1 = sub_part.mv_l1[1] as i32
                                + crate::slice_context::chroma_field_mv_offset_impl(
                                    self.field_pic_flag,
                                    self.bottom_field_flag,
                                    ref_l1,
                                );
                            inter_pred::chroma_mc(
                                cr0,
                                c_ref_stride_l0,
                                chroma_h,
                                cx as i32,
                                (mc_cy_l0 + cy_off) as i32,
                                sub_part.mv_l0[0] as i32,
                                cmv_y_l0,
                                chw,
                                chh,
                                &mut c0,
                            );
                            inter_pred::chroma_mc(
                                cr1,
                                c_ref_stride_l1,
                                chroma_h,
                                cx as i32,
                                (mc_cy_l1 + cy_off) as i32,
                                sub_part.mv_l1[0] as i32,
                                cmv_y_l1,
                                chw,
                                chh,
                                &mut c1,
                            );
                            sp.wctx.apply_bi(
                                &c0,
                                &c1,
                                &mut chroma_pred,
                                sub_part.ref_idx_l0 as usize,
                                sub_part.ref_idx_l1 as usize,
                                true,
                                plane_idx,
                            );
                        } else if sub_part.pred_l0 {
                            let ref_pic = ref_pic_safe(
                                sp.ref_pic_list_l0,
                                self.frame_ref_idx(mb_idx, sub_part.ref_idx_l0),
                            )
                            .ok_or(DecodeError::InvalidSyntax("empty ref list"))?;
                            let (_mc_y, _rs, _ref_y_off, mc_cy, c_ref_stride, c_ref_off) = self
                                .mc_params(
                                    mb_idx,
                                    mb_y,
                                    ref_pic.width as usize,
                                    sub_part.ref_idx_l0,
                                );
                            let plane = if plane_idx == 0 {
                                &ref_pic.u[c_ref_off..]
                            } else {
                                &ref_pic.v[c_ref_off..]
                            };
                            let cy_off = sub_part.y / 2;
                            let cmv_y = sub_part.mv_l0[1] as i32
                                + crate::slice_context::chroma_field_mv_offset_impl(
                                    self.field_pic_flag,
                                    self.bottom_field_flag,
                                    ref_pic,
                                );
                            inter_pred::chroma_mc(
                                plane,
                                c_ref_stride,
                                chroma_h,
                                cx as i32,
                                (mc_cy + cy_off) as i32,
                                sub_part.mv_l0[0] as i32,
                                cmv_y,
                                chw,
                                chh,
                                &mut chroma_pred,
                            );
                            if sp.use_weight == 1 {
                                sp.wctx.apply_uni(
                                    &mut chroma_pred,
                                    0,
                                    sub_part.ref_idx_l0 as usize,
                                    true,
                                    plane_idx,
                                );
                            }
                        } else if sub_part.pred_l1 {
                            let ref_pic = ref_pic_safe(
                                sp.ref_pic_list_l1,
                                self.frame_ref_idx(mb_idx, sub_part.ref_idx_l1),
                            )
                            .ok_or(DecodeError::InvalidSyntax("empty ref list"))?;
                            let (_mc_y, _rs, _ref_y_off, mc_cy, c_ref_stride, c_ref_off) = self
                                .mc_params(
                                    mb_idx,
                                    mb_y,
                                    ref_pic.width as usize,
                                    sub_part.ref_idx_l1,
                                );
                            let plane = if plane_idx == 0 {
                                &ref_pic.u[c_ref_off..]
                            } else {
                                &ref_pic.v[c_ref_off..]
                            };
                            let cy_off = sub_part.y / 2;
                            let cmv_y = sub_part.mv_l1[1] as i32
                                + crate::slice_context::chroma_field_mv_offset_impl(
                                    self.field_pic_flag,
                                    self.bottom_field_flag,
                                    ref_pic,
                                );
                            inter_pred::chroma_mc(
                                plane,
                                c_ref_stride,
                                chroma_h,
                                cx as i32,
                                (mc_cy + cy_off) as i32,
                                sub_part.mv_l1[0] as i32,
                                cmv_y,
                                chw,
                                chh,
                                &mut chroma_pred,
                            );
                        }

                        let fp = if plane_idx == 0 {
                            &mut self.frame.u
                        } else {
                            &mut self.frame.v
                        };
                        let local_cy = sub_part.y / 2;
                        let lc_off = self.lc_offset;
                        let lc_str = self.lc_stride;
                        for r in 0..chh {
                            for c in 0..chw {
                                fp[lc_off + (local_cy + r) * lc_str + cx + c] =
                                    chroma_pred[r * chw + c];
                            }
                        }
                    }
                }

                // Residual: CABAC CBP + coefficients
                let (left_cbp, top_cbp, left_cbp_c, top_cbp_c) =
                    self.cabac_cbp_context(mb_idx, false);
                let cbp_luma = cr.decode_cbp_luma(st, left_cbp, top_cbp);
                let cbp_chroma = cr.decode_cbp_chroma(st, left_cbp_c, top_cbp_c);
                self.mb_cbp[mb_idx] = (cbp_luma as u16) | ((cbp_chroma as u16) << 4);

                // 8x8 transform flag for B inter MBs (spec 7.3.5).
                // Only present when noSubMbPartSizeLessThan8x8Flag is true.
                let use_8x8_b_inter =
                    if sp.transform_8x8_mode_flag && cbp_luma != 0 && no_sub_less_than_8x8_b {
                        let nts = {
                            let left = if let Some(left) = self.left_mb(mb_idx) {
                                self.mb_is_8x8dct[left] as usize
                            } else {
                                0
                            };
                            let top = if let Some(above) = self.above_mb(mb_idx) {
                                self.mb_is_8x8dct[above] as usize
                            } else {
                                0
                            };
                            left + top
                        };
                        cr.get_cabac(&mut st[399 + nts]) != 0
                    } else {
                        false
                    };
                self.mb_is_8x8dct[mb_idx] = use_8x8_b_inter;

                let mb_qp = if cbp_luma != 0 || cbp_chroma != 0 {
                    let delta = cr.decode_mb_qp_delta(st, self.last_qp_delta_nonzero);
                    self.last_qp_delta_nonzero = delta != 0;
                    sp.apply_mb_qp_delta(self.prev_mb_qp, delta)?
                } else {
                    self.last_qp_delta_nonzero = false;
                    self.prev_mb_qp
                };
                self.prev_mb_qp = mb_qp;
                let qp_y = sp.luma_qp_prime(mb_qp);

                // Luma residual
                if cbp_luma != 0 {
                    let mut luma_residual = [0i32; 256];
                    if use_8x8_b_inter {
                        let scale_8x8 = &sp.scaling_list_8x8[1];
                        for i8x8 in 0..4 {
                            if cbp_luma & (1 << i8x8) == 0 {
                                continue;
                            }
                            // cat=5: no coded_block_flag, CBP bit is sufficient
                            let (coeffs, tc) = cr.decode_residual_cabac_field(
                                st,
                                5,
                                64,
                                self.is_field_coded(mb_idx),
                            );
                            let tc_per = tc.div_ceil(4);
                            for sub in 0..4 {
                                self.nc_luma[mb_idx * 16 + i8x8 * 4 + sub] = tc_per;
                            }
                            let mut block_8x8 = [0i32; 64];
                            for (pos, val) in &coeffs {
                                block_8x8[zigzag_8x8_cabac[*pos]] = *val;
                            }
                            dequant_8x8(&mut block_8x8, qp_y, scale_8x8);
                            inverse_dct_8x8(&mut block_8x8);
                            let row_off = (i8x8 / 2) * 8;
                            let col_off = (i8x8 % 2) * 8;
                            for r in 0..8 {
                                for c in 0..8 {
                                    luma_residual[(row_off + r) * 16 + col_off + c] =
                                        block_8x8[r * 8 + c];
                                }
                            }
                        }
                    } else {
                        for blk in 0..16 {
                            if cbp_luma & (1 << (blk / 4)) != 0 {
                                let left_nz = cabac_neighbor_nz_luma(
                                    self.nc_luma,
                                    mb_idx,
                                    self.mb_width as usize,
                                    blk,
                                    true,
                                    false,
                                    self.mb_slice_id,
                                    self.this_slice_id,
                                    self.mbaff,
                                    self.mb_field_decoding,
                                );
                                let top_nz = cabac_neighbor_nz_luma(
                                    self.nc_luma,
                                    mb_idx,
                                    self.mb_width as usize,
                                    blk,
                                    false,
                                    false,
                                    self.mb_slice_id,
                                    self.this_slice_id,
                                    self.mbaff,
                                    self.mb_field_decoding,
                                );
                                if cr.decode_coded_block_flag(st, 2, left_nz, top_nz) {
                                    let (coeffs, tc) = cr.decode_residual_cabac_field(
                                        st,
                                        2,
                                        16,
                                        self.is_field_coded(mb_idx),
                                    );
                                    self.nc_luma[mb_idx * 16 + blk] = tc;
                                    let mut block_coeffs = [0i32; 16];
                                    for (pos, val) in &coeffs {
                                        let (r, c) = zigzag_4x4[*pos];
                                        block_coeffs[r * 4 + c] = *val;
                                    }
                                    dequant_4x4_full(
                                        &mut block_coeffs,
                                        qp_y,
                                        &sp.scaling_list_4x4[3],
                                    );
                                    inverse_dct_4x4(&mut block_coeffs);
                                    let (blk_row, blk_col) = BLOCK_INDEX_TO_OFFSET[blk];
                                    for r in 0..4 {
                                        for c in 0..4 {
                                            luma_residual[(blk_row + r) * 16 + blk_col + c] =
                                                block_coeffs[r * 4 + c];
                                        }
                                    }
                                }
                            }
                        }
                    } // close if use_8x8_b_inter else
                    for r in 0..16 {
                        for c in 0..16 {
                            let val = (self.frame.y[self.ly_offset + r * self.ly_stride + mb_x + c]
                                as i32
                                + luma_residual[r * 16 + c])
                                .clamp(0, self.pixel_max_y)
                                as u16;
                            self.frame.y[self.ly_offset + r * self.ly_stride + mb_x + c] = val;
                        }
                    }
                }

                // Chroma residual
                let (qp_cb, qp_cr) = sp.chroma_qp_prime(mb_qp);
                if cbp_chroma >= 1 {
                    let mut chroma_dc_cb = [0i32; 4];
                    let mut chroma_dc_cr = [0i32; 4];
                    let left_dc_nz = if let Some(left) = self.left_mb(mb_idx) {
                        (self.mb_cbp[left] >> 6) & 1 != 0
                    } else {
                        false
                    };
                    let top_dc_nz = if let Some(above) = self.above_mb(mb_idx) {
                        (self.mb_cbp[above] >> 6) & 1 != 0
                    } else {
                        false
                    };
                    if cr.decode_coded_block_flag(st, 3, left_dc_nz, top_dc_nz) {
                        let (coeffs, _) =
                            cr.decode_residual_cabac_field(st, 3, 4, self.is_field_coded(mb_idx));
                        for (pos, val) in coeffs {
                            chroma_dc_cb[pos] = val;
                        }
                        self.mb_cbp[mb_idx] |= 0x40;
                    }
                    let left_dc_cr = if let Some(left) = self.left_mb(mb_idx) {
                        (self.mb_cbp[left] >> 7) & 1 != 0
                    } else {
                        false
                    };
                    let top_dc_cr = if let Some(above) = self.above_mb(mb_idx) {
                        (self.mb_cbp[above] >> 7) & 1 != 0
                    } else {
                        false
                    };
                    if cr.decode_coded_block_flag(st, 3, left_dc_cr, top_dc_cr) {
                        let (coeffs, _) =
                            cr.decode_residual_cabac_field(st, 3, 4, self.is_field_coded(mb_idx));
                        for (pos, val) in coeffs {
                            chroma_dc_cr[pos] = val;
                        }
                        self.mb_cbp[mb_idx] |= 0x80;
                    }

                    let chroma_mb_x = mb_x / 2;
                    let lc_off = self.lc_offset;
                    let lc_str = self.lc_stride;
                    let is_field_mb = self.is_field_coded(mb_idx);
                    for (plane_dc, frame_plane, scale_idx) in [
                        (&mut chroma_dc_cb, &mut self.frame.u, 4usize),
                        (&mut chroma_dc_cr, &mut self.frame.v, 5usize),
                    ] {
                        let chroma_scale = &sp.scaling_list_4x4[scale_idx];
                        // Cb uses scaling list 1 (intra) / 4 (inter), Cr 2 / 5.
                        let qp_c = if matches!(scale_idx, 1 | 4) {
                            qp_cb
                        } else {
                            qp_cr
                        };
                        inverse_hadamard_2x2(plane_dc);
                        dequant_chroma_dc(plane_dc, qp_c, chroma_scale[0]);
                        let mut chroma_residual = [0i32; 64];
                        for blk in 0..4 {
                            let blk_row = (blk / 2) * 4;
                            let blk_col = (blk % 2) * 4;
                            let mut block_raster = [0i32; 16];
                            block_raster[0] = plane_dc[blk];
                            if cbp_chroma >= 2 {
                                let nc_arr = if scale_idx == 4 {
                                    &self.nc_cb
                                } else {
                                    &self.nc_cr
                                };
                                let left_nz = cabac_neighbor_nz_chroma(
                                    nc_arr,
                                    mb_idx,
                                    self.mb_width as usize,
                                    blk,
                                    true,
                                    false,
                                    self.mb_slice_id,
                                    self.this_slice_id,
                                    self.mbaff,
                                    self.mb_field_decoding,
                                );
                                let top_nz = cabac_neighbor_nz_chroma(
                                    nc_arr,
                                    mb_idx,
                                    self.mb_width as usize,
                                    blk,
                                    false,
                                    false,
                                    self.mb_slice_id,
                                    self.this_slice_id,
                                    self.mbaff,
                                    self.mb_field_decoding,
                                );
                                if cr.decode_coded_block_flag(st, 4, left_nz, top_nz) {
                                    let (coeffs, tc) =
                                        cr.decode_residual_cabac_field(st, 4, 15, is_field_mb);
                                    if scale_idx == 4 {
                                        self.nc_cb[mb_idx * 4 + blk] = tc;
                                    } else {
                                        self.nc_cr[mb_idx * 4 + blk] = tc;
                                    }
                                    for (pos, val) in coeffs {
                                        let (r, c) = zigzag_4x4[pos + 1];
                                        block_raster[r * 4 + c] = val;
                                    }
                                    dequant_4x4_ac_raster(&mut block_raster, qp_c, chroma_scale);
                                }
                            }
                            inverse_dct_4x4(&mut block_raster);
                            for r in 0..4 {
                                for c in 0..4 {
                                    chroma_residual[(blk_row + r) * 8 + blk_col + c] =
                                        block_raster[r * 4 + c];
                                }
                            }
                        }
                        for y in 0..8 {
                            for x in 0..8 {
                                let val = (frame_plane[lc_off + y * lc_str + chroma_mb_x + x]
                                    as i32
                                    + chroma_residual[y * 8 + x])
                                    .clamp(0, self.pixel_max_c)
                                    as u16;
                                frame_plane[lc_off + y * lc_str + chroma_mb_x + x] = val;
                            }
                        }
                    }
                }

                self.mb_info[mb_idx] = MbInfo {
                    mb_type: MbType::Inter,
                    qp_y: mb_qp,
                    ..Default::default()
                };
                return Ok(CabacMbResult::Decoded);
            }
        }

        // I-slice CABAC: decode mb_type
        // Context depends on whether neighbors are I16x16 (not I4x4)
        let left_is_i16 = if let Some(left) = self.left_mb(mb_idx) {
            self.is_i16x16[left]
        } else {
            false
        };
        let top_is_i16 = if let Some(above) = self.above_mb(mb_idx) {
            self.is_i16x16[above]
        } else {
            false
        };
        let mb_type = cr.decode_intra_mb_type(st, 3, left_is_i16, top_is_i16, true);

        // Cross-slice intra prediction: neighbors from other slices unavailable (spec 6.4.1)
        let above_mb_avail_i = self.above_mb(mb_idx).is_some();
        let left_mb_avail_i = self.left_mb(mb_idx).is_some();
        let above_left_mb_avail_i = self
            .above_mb(mb_idx)
            .and_then(|above| self.left_mb(above))
            .is_some();

        // I_PCM via CABAC
        if mb_type == 25 {
            let pcm_pos = cr.pcm_byte_position();
            // pcm_sample_luma / pcm_sample_chroma are u(BitDepthY) / u(BitDepthC) (spec 7.3.5)
            let pcm_data = rbsp.get(pcm_pos..).ok_or(DecodeError::UnexpectedEof)?;
            let pcm_len = self
                .write_pcm_samples(pcm_data, mb_x)
                .ok_or(DecodeError::UnexpectedEof)?;
            for blk in 0..16 {
                self.nc_luma[mb_idx * 16 + blk] = 16;
            }
            for blk in 0..4 {
                self.nc_cb[mb_idx * 4 + blk] = 16;
                self.nc_cr[mb_idx * 4 + blk] = 16;
            }
            cr.reinit(pcm_pos + pcm_len);
            self.prev_mb_qp = 0;
            self.last_qp_delta_nonzero = false;
            self.is_i16x16[mb_idx] = true; // I_PCM treated as I16x16 for CABAC context
            self.mb_cbp[mb_idx] = 0;
            self.mb_info[mb_idx] = MbInfo {
                mb_type: MbType::Ipcm,
                qp_y: 0,
                ..Default::default()
            };
            return Ok(CabacMbResult::Decoded);
        }

        if mb_type == 0 {
            // I4x4/I8x8 via CABAC
            let nts = {
                let left = if let Some(left) = self.left_mb(mb_idx) {
                    self.mb_is_8x8dct[left] as usize
                } else {
                    0
                };
                let top = if let Some(above) = self.above_mb(mb_idx) {
                    self.mb_is_8x8dct[above] as usize
                } else {
                    0
                };
                left + top
            };
            let use_8x8_intra = sp.transform_8x8_mode_flag && cr.get_cabac(&mut st[399 + nts]) != 0;
            self.mb_is_8x8dct[mb_idx] = use_8x8_intra;

            let intra_avail = self.intra_avail_map(sp);

            // Parse prediction modes: 4 for I8x8, 16 for I4x4
            let num_modes = if use_8x8_intra { 4 } else { 16 };
            let mut pred_modes = [2u8; 16];
            for blk_idx in 0..num_modes {
                let blk = if use_8x8_intra { blk_idx * 4 } else { blk_idx };
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
                let mode = cr.decode_intra4x4_pred_mode(st, predicted);
                if use_8x8_intra {
                    for sub in 0..4 {
                        pred_modes[blk_idx * 4 + sub] = mode;
                        self.i4x4_modes[mb_idx * 16 + blk_idx * 4 + sub] = mode;
                    }
                } else {
                    pred_modes[blk] = mode;
                    self.i4x4_modes[mb_idx * 16 + blk] = mode;
                }
            }

            let left_cm = if let Some(left) = self.left_mb(mb_idx) {
                self.mb_chroma_pred[left]
            } else {
                0
            };
            let top_cm = if let Some(above) = self.above_mb(mb_idx) {
                self.mb_chroma_pred[above]
            } else {
                0
            };
            let intra_chroma_pred_mode = cr.decode_chroma_pred_mode(st, left_cm, top_cm);
            self.mb_chroma_pred[mb_idx] = intra_chroma_pred_mode;

            // CBP with proper neighbor context
            let (left_cbp, top_cbp, left_cbp_c, top_cbp_c) = self.cabac_cbp_context(mb_idx, true);
            let cbp_luma = cr.decode_cbp_luma(st, left_cbp, top_cbp);
            let cbp_chroma = cr.decode_cbp_chroma(st, left_cbp_c, top_cbp_c);
            self.mb_cbp[mb_idx] = (cbp_luma as u16) | ((cbp_chroma as u16) << 4);
            let mb_qp = if cbp_luma != 0 || cbp_chroma != 0 {
                let delta = cr.decode_mb_qp_delta(st, self.last_qp_delta_nonzero);
                self.last_qp_delta_nonzero = delta != 0;
                sp.apply_mb_qp_delta(self.prev_mb_qp, delta)?
            } else {
                self.last_qp_delta_nonzero = false;
                self.prev_mb_qp
            };
            self.prev_mb_qp = mb_qp;
            let qp_y = sp.luma_qp_prime(mb_qp);
            let (qp_cb, qp_cr) = sp.chroma_qp_prime(mb_qp);
            let above_mb_avail = above_mb_avail_i;
            let left_mb_avail = left_mb_avail_i;
            let above_left_mb_avail = above_left_mb_avail_i;
            let above_right_mb_avail = if !self.mbaff {
                mb_idx >= self.mb_width as usize
                    && (mb_idx % self.mb_width as usize) + 1 < self.mb_width as usize
                    && self.mb_slice_id[mb_idx - self.mb_width as usize + 1] == self.this_slice_id
            } else {
                // For MBAFF bottom MBs, the above-right is in the next pair which
                // hasn't been decoded yet — so it's unavailable.
                if !mb_idx.is_multiple_of(2) {
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
                            Some(ar_mb)
                        })
                        .is_some()
                } // close MBAFF top-MB else
            };
            if use_8x8_intra {
                // I8x8 via CABAC: decode 4 blocks of 64 coefficients
                let mut luma_residual = [0i32; 256];
                for i8x8 in 0..4 {
                    if cbp_luma & (1 << i8x8) == 0 {
                        // Set nC=0 for all sub-blocks
                        for sub in 0..4 {
                            self.nc_luma[mb_idx * 16 + i8x8 * 4 + sub] = 0;
                        }
                        continue;
                    }
                    // cat=5: no coded_block_flag, CBP bit is sufficient
                    let (coeffs, tc) =
                        cr.decode_residual_cabac_field(st, 5, 64, self.is_field_coded(mb_idx));
                    // Distribute nC across sub-blocks
                    let tc_per = tc.div_ceil(4);
                    for sub in 0..4 {
                        self.nc_luma[mb_idx * 16 + i8x8 * 4 + sub] = tc_per;
                    }
                    let mut block_8x8 = [0i32; 64];
                    for (pos, val) in &coeffs {
                        block_8x8[zigzag_8x8_cabac[*pos]] = *val;
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
                // I4x4 via CABAC: existing path
                for blk in 0..16 {
                    let (blk_row, blk_col) = BLOCK_INDEX_TO_OFFSET[blk];
                    let px = mb_x + blk_col;
                    let py = mb_y + blk_row;

                    let mut block_coeffs = [0i32; 16];
                    if cbp_luma & (1 << (blk / 4)) != 0 {
                        let left_nz_blk = cabac_neighbor_nz_luma(
                            self.nc_luma,
                            mb_idx,
                            self.mb_width as usize,
                            blk,
                            true,
                            true,
                            self.mb_slice_id,
                            self.this_slice_id,
                            self.mbaff,
                            self.mb_field_decoding,
                        );
                        let top_nz_blk = cabac_neighbor_nz_luma(
                            self.nc_luma,
                            mb_idx,
                            self.mb_width as usize,
                            blk,
                            false,
                            true,
                            self.mb_slice_id,
                            self.this_slice_id,
                            self.mbaff,
                            self.mb_field_decoding,
                        );

                        let cbf = cr.decode_coded_block_flag(st, 2, left_nz_blk, top_nz_blk);
                        if cbf {
                            let (coeffs, tc) = cr.decode_residual_cabac_field(
                                st,
                                2,
                                16,
                                self.is_field_coded(mb_idx),
                            );
                            self.nc_luma[mb_idx * 16 + blk] = tc;
                            for (pos, val) in &coeffs {
                                let (r, c) = zigzag_4x4[*pos];
                                block_coeffs[r * 4 + c] = *val;
                            }
                            dequant_4x4_full(&mut block_coeffs, qp_y, &sp.scaling_list_4x4[0]);
                        }
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
            } // close if use_8x8_intra else (luma only)

            // Chroma (simplified — reuse existing chroma decode pattern)
            let _chroma_width = (self.width / 2) as usize;
            let _chroma_mb_x = mb_x / 2;
            let _chroma_mb_y = mb_y / 2;

            // Chroma prediction (slice boundary: cross-slice neighbors unavailable)
            let (pred_u, pred_v) = self.predict_chroma_intra(
                mb_x,
                intra_chroma_pred_mode,
                above_mb_avail,
                left_mb_avail,
                above_left_mb_avail,
            );

            // Chroma residual: DC + AC via CABAC
            let mut chroma_dc_cb = [0i32; 4];
            let mut chroma_dc_cr = [0i32; 4];
            if cbp_chroma >= 1 {
                // Chroma DC CBF context: uses bits 6-7 of neighbor cbp_table
                let left_dc_nz = if let Some(left) = self.left_mb(mb_idx) {
                    (self.mb_cbp[left] >> 6) & 1 != 0
                } else {
                    true
                }; // unavailable intra: 0x7CF bit 6 = 1
                let top_dc_nz = if let Some(above) = self.above_mb(mb_idx) {
                    (self.mb_cbp[above] >> 6) & 1 != 0
                } else {
                    true
                };
                if cr.decode_coded_block_flag(st, 3, left_dc_nz, top_dc_nz) {
                    let (coeffs, _tc) =
                        cr.decode_residual_cabac_field(st, 3, 4, self.is_field_coded(mb_idx));
                    for (pos, val) in coeffs {
                        chroma_dc_cb[pos] = val;
                    }
                    self.mb_cbp[mb_idx] |= 0x40; // set Cb DC coded flag
                }
                let left_dc_nz_cr = if let Some(left) = self.left_mb(mb_idx) {
                    (self.mb_cbp[left] >> 7) & 1 != 0
                } else {
                    true
                };
                let top_dc_nz_cr = if let Some(above) = self.above_mb(mb_idx) {
                    (self.mb_cbp[above] >> 7) & 1 != 0
                } else {
                    true
                };
                if cr.decode_coded_block_flag(st, 3, left_dc_nz_cr, top_dc_nz_cr) {
                    let (coeffs, _tc) =
                        cr.decode_residual_cabac_field(st, 3, 4, self.is_field_coded(mb_idx));
                    for (pos, val) in coeffs {
                        chroma_dc_cr[pos] = val;
                    }
                    self.mb_cbp[mb_idx] |= 0x80; // set Cr DC coded flag
                }
            }
            let mut chroma_ac_cb = [[0i32; 15]; 4];
            let mut chroma_ac_cr = [[0i32; 15]; 4];
            if cbp_chroma >= 2 {
                // Chroma AC: cat=4, max_coeff=15
                for blk in 0..4 {
                    let left_nz = cabac_neighbor_nz_chroma(
                        self.nc_cb,
                        mb_idx,
                        self.mb_width as usize,
                        blk,
                        true,
                        true,
                        self.mb_slice_id,
                        self.this_slice_id,
                        self.mbaff,
                        self.mb_field_decoding,
                    );
                    let top_nz = cabac_neighbor_nz_chroma(
                        self.nc_cb,
                        mb_idx,
                        self.mb_width as usize,
                        blk,
                        false,
                        true,
                        self.mb_slice_id,
                        self.this_slice_id,
                        self.mbaff,
                        self.mb_field_decoding,
                    );
                    if cr.decode_coded_block_flag(st, 4, left_nz, top_nz) {
                        let (coeffs, tc) =
                            cr.decode_residual_cabac_field(st, 4, 15, self.is_field_coded(mb_idx));
                        self.nc_cb[mb_idx * 4 + blk] = tc;
                        for (pos, val) in coeffs {
                            chroma_ac_cb[blk][pos] = val;
                        }
                    }
                }
                for blk in 0..4 {
                    let left_nz = cabac_neighbor_nz_chroma(
                        self.nc_cr,
                        mb_idx,
                        self.mb_width as usize,
                        blk,
                        true,
                        true,
                        self.mb_slice_id,
                        self.this_slice_id,
                        self.mbaff,
                        self.mb_field_decoding,
                    );
                    let top_nz = cabac_neighbor_nz_chroma(
                        self.nc_cr,
                        mb_idx,
                        self.mb_width as usize,
                        blk,
                        false,
                        true,
                        self.mb_slice_id,
                        self.this_slice_id,
                        self.mbaff,
                        self.mb_field_decoding,
                    );
                    if cr.decode_coded_block_flag(st, 4, left_nz, top_nz) {
                        let (coeffs, tc) =
                            cr.decode_residual_cabac_field(st, 4, 15, self.is_field_coded(mb_idx));
                        self.nc_cr[mb_idx * 4 + blk] = tc;
                        for (pos, val) in coeffs {
                            chroma_ac_cr[blk][pos] = val;
                        }
                    }
                }
            }

            // Reconstruct chroma
            {
                self.reconstruct_chroma_plane(
                    &mut chroma_dc_cb,
                    &chroma_ac_cb,
                    &pred_u,
                    true,
                    cbp_chroma,
                    qp_cb,
                    &sp.scaling_list_4x4[1],
                    mb_x,
                    mb_idx,
                );
                self.reconstruct_chroma_plane(
                    &mut chroma_dc_cr,
                    &chroma_ac_cr,
                    &pred_v,
                    false,
                    cbp_chroma,
                    qp_cr,
                    &sp.scaling_list_4x4[2],
                    mb_x,
                    mb_idx,
                );
            }

            self.mb_info[mb_idx] = MbInfo {
                mb_type: MbType::Intra,
                qp_y: mb_qp,
                ..Default::default()
            };
            // I4x4/I8x8 is NOT I16x16 for CABAC context
        } else if mb_type <= 24 {
            // I16x16 via CABAC
            self.is_i16x16[mb_idx] = true;
            let mt = mb_type - 1;
            let intra16x16_pred_mode = (mt % 4) as u8;
            let cbp_chroma = ((mt / 4) % 3) as u8;
            let cbp_luma = if mt >= 12 { 15u8 } else { 0u8 };

            let left_cm = if let Some(left) = self.left_mb(mb_idx) {
                self.mb_chroma_pred[left]
            } else {
                0
            };
            let top_cm = if let Some(above) = self.above_mb(mb_idx) {
                self.mb_chroma_pred[above]
            } else {
                0
            };
            let intra_chroma_pred_mode = cr.decode_chroma_pred_mode(st, left_cm, top_cm);
            self.mb_chroma_pred[mb_idx] = intra_chroma_pred_mode;
            let delta = cr.decode_mb_qp_delta(st, self.last_qp_delta_nonzero);
            self.last_qp_delta_nonzero = delta != 0;
            let mb_qp = sp.apply_mb_qp_delta(self.prev_mb_qp, delta)?;
            self.prev_mb_qp = mb_qp;
            let qp_y = sp.luma_qp_prime(mb_qp);
            let (qp_cb, qp_cr) = sp.chroma_qp_prime(mb_qp);
            // Luma DC: cat=0, 16 coefficients
            // CBF context uses bit 8 of neighbor cbp_table (luma DC coded flag)
            let mut luma_dc = [0i32; 16];
            let dc_left_nz = if let Some(left) = self.left_mb(mb_idx) {
                (self.mb_cbp[left] >> 8) & 1 != 0
            } else {
                true
            }; // unavailable intra: 0x7CF bit 8 = 1 (0x7CF = 0b0111_1100_1111)
            let dc_top_nz = if let Some(above) = self.above_mb(mb_idx) {
                (self.mb_cbp[above] >> 8) & 1 != 0
            } else {
                true
            };
            if cr.decode_coded_block_flag(st, 0, dc_left_nz, dc_top_nz) {
                let (coeffs, _tc) =
                    cr.decode_residual_cabac_field(st, 0, 16, self.is_field_coded(mb_idx));
                for (pos, val) in coeffs {
                    luma_dc[pos] = val;
                }
                self.mb_cbp[mb_idx] |= 0x100; // set luma DC coded flag
            }

            // Luma AC: cat=1, 15 coefficients per block
            let mut luma_ac_scan = [[0i32; 15]; 16];
            if cbp_luma != 0 {
                for blk in 0..16 {
                    let left_nz = cabac_neighbor_nz_luma(
                        self.nc_luma,
                        mb_idx,
                        self.mb_width as usize,
                        blk,
                        true,
                        true,
                        self.mb_slice_id,
                        self.this_slice_id,
                        self.mbaff,
                        self.mb_field_decoding,
                    );
                    let top_nz = cabac_neighbor_nz_luma(
                        self.nc_luma,
                        mb_idx,
                        self.mb_width as usize,
                        blk,
                        false,
                        true,
                        self.mb_slice_id,
                        self.this_slice_id,
                        self.mbaff,
                        self.mb_field_decoding,
                    );
                    if cr.decode_coded_block_flag(st, 1, left_nz, top_nz) {
                        let (coeffs, tc) =
                            cr.decode_residual_cabac_field(st, 1, 15, self.is_field_coded(mb_idx));
                        self.nc_luma[mb_idx * 16 + blk] = tc;
                        for (pos, val) in coeffs {
                            luma_ac_scan[blk][pos] = val;
                        }
                    }
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
                above_mb_avail_i,
                left_mb_avail_i,
                above_left_mb_avail_i,
            );

            // Chroma (same as I4x4 CABAC path)
            let _chroma_width = (self.width / 2) as usize;
            let _chroma_mb_x = mb_x / 2;
            let _chroma_mb_y = mb_y / 2;
            let (pred_u, pred_v) = self.predict_chroma_intra(
                mb_x,
                intra_chroma_pred_mode,
                above_mb_avail_i,
                left_mb_avail_i,
                above_left_mb_avail_i,
            );

            // Chroma residual
            let mut chroma_dc_cb = [0i32; 4];
            let mut chroma_dc_cr = [0i32; 4];
            if cbp_chroma >= 1 {
                let left_dc_nz = if let Some(left) = self.left_mb(mb_idx) {
                    (self.mb_cbp[left] >> 6) & 1 != 0
                } else {
                    true
                };
                let top_dc_nz = if let Some(above) = self.above_mb(mb_idx) {
                    (self.mb_cbp[above] >> 6) & 1 != 0
                } else {
                    true
                };
                if cr.decode_coded_block_flag(st, 3, left_dc_nz, top_dc_nz) {
                    let (coeffs, _tc) =
                        cr.decode_residual_cabac_field(st, 3, 4, self.is_field_coded(mb_idx));
                    for (pos, val) in coeffs {
                        chroma_dc_cb[pos] = val;
                    }
                    self.mb_cbp[mb_idx] |= 0x40;
                }
                let left_dc_nz_cr = if let Some(left) = self.left_mb(mb_idx) {
                    (self.mb_cbp[left] >> 7) & 1 != 0
                } else {
                    true
                };
                let top_dc_nz_cr = if let Some(above) = self.above_mb(mb_idx) {
                    (self.mb_cbp[above] >> 7) & 1 != 0
                } else {
                    true
                };
                if cr.decode_coded_block_flag(st, 3, left_dc_nz_cr, top_dc_nz_cr) {
                    let (coeffs, _tc) =
                        cr.decode_residual_cabac_field(st, 3, 4, self.is_field_coded(mb_idx));
                    for (pos, val) in coeffs {
                        chroma_dc_cr[pos] = val;
                    }
                    self.mb_cbp[mb_idx] |= 0x80;
                }
            }
            let mut chroma_ac_cb = [[0i32; 15]; 4];
            let mut chroma_ac_cr = [[0i32; 15]; 4];
            if cbp_chroma >= 2 {
                for blk in 0..4 {
                    let left_nz = cabac_neighbor_nz_chroma(
                        self.nc_cb,
                        mb_idx,
                        self.mb_width as usize,
                        blk,
                        true,
                        true,
                        self.mb_slice_id,
                        self.this_slice_id,
                        self.mbaff,
                        self.mb_field_decoding,
                    );
                    let top_nz = cabac_neighbor_nz_chroma(
                        self.nc_cb,
                        mb_idx,
                        self.mb_width as usize,
                        blk,
                        false,
                        true,
                        self.mb_slice_id,
                        self.this_slice_id,
                        self.mbaff,
                        self.mb_field_decoding,
                    );
                    if cr.decode_coded_block_flag(st, 4, left_nz, top_nz) {
                        let (coeffs, tc) =
                            cr.decode_residual_cabac_field(st, 4, 15, self.is_field_coded(mb_idx));
                        self.nc_cb[mb_idx * 4 + blk] = tc;
                        for (pos, val) in coeffs {
                            chroma_ac_cb[blk][pos] = val;
                        }
                    }
                }
                for blk in 0..4 {
                    let left_nz = cabac_neighbor_nz_chroma(
                        self.nc_cr,
                        mb_idx,
                        self.mb_width as usize,
                        blk,
                        true,
                        true,
                        self.mb_slice_id,
                        self.this_slice_id,
                        self.mbaff,
                        self.mb_field_decoding,
                    );
                    let top_nz = cabac_neighbor_nz_chroma(
                        self.nc_cr,
                        mb_idx,
                        self.mb_width as usize,
                        blk,
                        false,
                        true,
                        self.mb_slice_id,
                        self.this_slice_id,
                        self.mbaff,
                        self.mb_field_decoding,
                    );
                    if cr.decode_coded_block_flag(st, 4, left_nz, top_nz) {
                        let (coeffs, tc) =
                            cr.decode_residual_cabac_field(st, 4, 15, self.is_field_coded(mb_idx));
                        self.nc_cr[mb_idx * 4 + blk] = tc;
                        for (pos, val) in coeffs {
                            chroma_ac_cr[blk][pos] = val;
                        }
                    }
                }
            }

            {
                self.reconstruct_chroma_plane(
                    &mut chroma_dc_cb,
                    &chroma_ac_cb,
                    &pred_u,
                    true,
                    cbp_chroma,
                    qp_cb,
                    &sp.scaling_list_4x4[1],
                    mb_x,
                    mb_idx,
                );
                self.reconstruct_chroma_plane(
                    &mut chroma_dc_cr,
                    &chroma_ac_cr,
                    &pred_v,
                    false,
                    cbp_chroma,
                    qp_cr,
                    &sp.scaling_list_4x4[2],
                    mb_x,
                    mb_idx,
                );
            }

            self.mb_info[mb_idx] = MbInfo {
                mb_type: MbType::Intra,
                qp_y: mb_qp,
                ..Default::default()
            };
            // CBP already partially set during DC/AC decode (bits 6-7)
            self.mb_cbp[mb_idx] |= (cbp_luma as u16) | ((cbp_chroma as u16) << 4);
        } else {
            // I_PCM via CABAC (mb_type == 25)
            let pcm_pos = cr.pcm_byte_position();
            // pcm_sample_luma / pcm_sample_chroma are u(BitDepthY) / u(BitDepthC) (spec 7.3.5)
            let pcm_data = rbsp.get(pcm_pos..).ok_or(DecodeError::UnexpectedEof)?;
            let pcm_len = self
                .write_pcm_samples(pcm_data, mb_x)
                .ok_or(DecodeError::UnexpectedEof)?;
            for blk in 0..16 {
                self.nc_luma[mb_idx * 16 + blk] = 16;
            }
            for blk in 0..4 {
                self.nc_cb[mb_idx * 4 + blk] = 16;
                self.nc_cr[mb_idx * 4 + blk] = 16;
            }
            cr.reinit(pcm_pos + pcm_len);
            self.prev_mb_qp = 0;
            self.last_qp_delta_nonzero = false;
            self.is_i16x16[mb_idx] = true; // I_PCM treated as I16x16 for CABAC context
            self.mb_cbp[mb_idx] = 0;
            self.mb_info[mb_idx] = MbInfo {
                mb_type: MbType::Ipcm,
                qp_y: 0,
                ..Default::default()
            };
        }

        Ok(CabacMbResult::Decoded)
    }
}
