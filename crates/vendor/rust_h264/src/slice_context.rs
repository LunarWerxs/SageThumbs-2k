//! Per-slice mutable decode state shared across all MB decode paths.
//!
//! `SliceContext` bundles the ~25 mutable arrays and geometry values that
//! every MB decoder (CABAC/CAVLC, I/P/B) reads and writes. Extracting it
//! from `decode_slice` enables splitting MB decode logic into methods.

use std::rc::Rc;

use crate::decoder::Frame;

#[allow(dead_code)] // Fields progressively used as methods are migrated
/// Read-only per-slice parameters used by MB decode methods.
///
/// Bundles the immutable slice-level data (header fields, PPS/SPS flags,
/// reference lists) to avoid long parameter lists.
pub(crate) struct SliceParams<'a> {
    pub is_p_slice: bool,
    pub is_b_slice: bool,
    pub use_weight: u8,
    pub current_poc: i32,
    pub direct_spatial_mv_pred_flag: bool,
    pub direct_8x8_inference_flag: bool,
    pub transform_8x8_mode_flag: bool,
    pub scaling_list_4x4: &'a [[u8; 16]; 6],
    pub scaling_list_8x8: &'a [[u8; 64]; 2],
    pub constrained_intra_pred_flag: bool,
    /// Cb chroma QP offset (PPS chroma_qp_index_offset).
    pub chroma_qp_index_offset: i32,
    /// Cr chroma QP offset (PPS second_chroma_qp_index_offset).
    pub second_chroma_qp_index_offset: i32,
    /// BitDepthY = 8 + bit_depth_luma_minus8 (8..=14).
    pub bit_depth_y: u32,
    /// BitDepthC = 8 + bit_depth_chroma_minus8 (8..=14).
    pub bit_depth_c: u32,
    /// QpBdOffsetY = 6 * bit_depth_luma_minus8.
    pub qp_bd_offset_y: i32,
    /// QpBdOffsetC = 6 * bit_depth_chroma_minus8.
    pub qp_bd_offset_c: i32,
    pub ref_pic_list: &'a [Rc<DecodedPicture>],
    pub ref_pic_list_l0: &'a [Rc<DecodedPicture>],
    pub ref_pic_list_l1: &'a [Rc<DecodedPicture>],
    pub num_ref_idx_l0_active: u32,
    pub num_ref_idx_l1_active: u32,
    pub wctx: &'a WeightContext<'a>,
    pub first_mb_in_slice: u32,
    pub slice_qp: i32,
    pub is_i_slice: bool,
    pub cabac_init_idc: u32,
}
use crate::dpb::{DecodedPicture, PictureStructure};
use crate::error::DecodeError;
use crate::inter_pred;
use crate::intra_pred::{
    predict_chroma_8x8, predict_intra_16x16, predict_intra_4x4, predict_intra_8x8,
};
use crate::mv_pred::{
    derive_spatial_direct_blk, derive_temporal_direct_blk, predict_mv_skip, ref_pic_safe, MbaffCtx,
    WeightContext,
};
use crate::neighbor::dequant_4x4_ac_raster;
use crate::residual::BLOCK_INDEX_TO_OFFSET;
use crate::residual::{
    chroma_qp, dequant_chroma_dc, inverse_dct_4x4, inverse_hadamard_2x2, ZIGZAG_4X4,
    ZIGZAG_4X4_FIELD,
};

impl SliceParams<'_> {
    /// Derive QPY from QPY,PRED and mb_qp_delta (spec 7.4.5):
    /// `((QPY,PRED + mb_qp_delta + 52 + 2 * QpBdOffsetY) % (52 + QpBdOffsetY)) - QpBdOffsetY`.
    /// mb_qp_delta must lie in `-(26 + QpBdOffsetY / 2)..=(25 + QpBdOffsetY / 2)`.
    /// The result lies in `-QpBdOffsetY..=51` and may be negative.
    pub(crate) fn apply_mb_qp_delta(
        &self,
        qp_pred: i32,
        mb_qp_delta: i32,
    ) -> Result<i32, DecodeError> {
        let half = self.qp_bd_offset_y / 2;
        if mb_qp_delta < -(26 + half) || mb_qp_delta > 25 + half {
            return Err(DecodeError::InvalidSyntax("mb_qp_delta out of range"));
        }
        let qp_pred = qp_pred.clamp(-self.qp_bd_offset_y, 51);
        let range = 52 + self.qp_bd_offset_y;
        Ok(
            (qp_pred + mb_qp_delta + 52 + 2 * self.qp_bd_offset_y).rem_euclid(range)
                - self.qp_bd_offset_y,
        )
    }

    /// QP'Y = QPY + QpBdOffsetY, the luma quantiser used by dequantisation.
    #[inline]
    pub(crate) fn luma_qp_prime(&self, qp_y: i32) -> i32 {
        qp_y + self.qp_bd_offset_y
    }

    /// (QP'Cb, QP'Cr) = QPC + QpBdOffsetC for each chroma plane (spec 8.5.8),
    /// with Cb using chroma_qp_index_offset and Cr second_chroma_qp_index_offset.
    #[inline]
    pub(crate) fn chroma_qp_prime(&self, qp_y: i32) -> (i32, i32) {
        let off_c = self.qp_bd_offset_c;
        (
            chroma_qp(qp_y, self.chroma_qp_index_offset, off_c) + off_c,
            chroma_qp(qp_y, self.second_chroma_qp_index_offset, off_c) + off_c,
        )
    }
}

/// Mutable per-slice state passed to MB decode routines.
///
/// All mutable MB-level arrays live here so that individual decode
/// functions can be extracted as methods on `&mut SliceContext`.
#[allow(dead_code)] // Fields progressively used as methods are extracted from decode_slice
pub(crate) struct SliceContext<'a> {
    // Pixel output
    pub frame: &'a mut Frame,
    pub stride: usize,
    pub width: u32,
    pub height: u32,

    // Geometry
    pub mb_width: u32,

    // MBAFF state
    pub mbaff: bool,
    pub mb_field_decoding: &'a mut [bool],

    // Per-4x4-block coefficient counts (for CABAC CBF / CAVLC nC)
    pub nc_luma: &'a mut [u8],
    pub nc_cb: &'a mut [u8],
    pub nc_cr: &'a mut [u8],

    // Per-4x4-block motion vectors and reference indices
    pub mv_store_l0: &'a mut [[i16; 2]],
    pub mv_store_l1: &'a mut [[i16; 2]],
    pub ref_idx_store_l0: &'a mut [i8],
    pub ref_poc_store_l0: &'a mut [i32],
    pub ref_idx_store_l1: &'a mut [i8],

    // Per-4x4-block MVD (for CABAC amvd context)
    pub mvd_store: &'a mut [[i16; 2]],
    pub mvd_store_l1: &'a mut [[i16; 2]],

    // Per-MB deblocking / neighbor info
    pub mb_info: &'a mut [crate::deblock::MbInfo],

    // Per-4x4-block intra prediction modes
    pub i4x4_modes: &'a mut [u8],

    // Per-MB CABAC neighbor context state
    pub mb_cbp: &'a mut [u16],
    pub mb_chroma_pred: &'a mut [u8],
    pub mb_is_8x8dct: &'a mut [bool],
    pub mb_skip: &'a mut [bool],
    pub mb_is_direct: &'a mut [bool],
    pub blk_is_direct: &'a mut [bool],
    pub is_i16x16: &'a mut [bool],

    // Multi-slice boundary tracking
    pub mb_slice_id: &'a mut [u16],
    pub this_slice_id: u16,

    // QP state (carried across MBs within a slice)
    pub prev_mb_qp: i32,
    pub last_qp_delta_nonzero: bool,

    // Per-MB pixel layout (set by `set_mb_layout` at the start of each MB decode).
    // For frame-coded MBs: ly_stride = width, ly_offset = mb_y * width.
    // For field-coded MBs: ly_stride = width * 2, ly_offset = field_y_base.
    pub ly_stride: usize,
    pub ly_offset: usize,
    pub lc_stride: usize,
    pub lc_offset: usize,

    /// True if this is a field picture (field_pic_flag=1). Used to select
    /// field coefficient scan order (spec Table 8-13).
    pub field_pic_flag: bool,
    /// True if this is the bottom field (only meaningful when field_pic_flag=true).
    /// Used for chroma MV offset when referencing opposite-parity fields.
    pub bottom_field_flag: bool,

    // Sample bit depths and Clip1 maxima, computed once per slice.
    /// BitDepthY (8..=14).
    pub bit_depth_y: u32,
    /// BitDepthC (8..=14).
    pub bit_depth_c: u32,
    /// Clip1Y maximum, `(1 << BitDepthY) - 1`.
    pub pixel_max_y: i32,
    /// Clip1C maximum, `(1 << BitDepthC) - 1`.
    pub pixel_max_c: i32,
}

/// Standalone chroma field MV offset computation (avoids borrow issues).
pub(crate) fn chroma_field_mv_offset_impl(
    field_pic_flag: bool,
    bottom_field_flag: bool,
    ref_pic: &DecodedPicture,
) -> i32 {
    if !field_pic_flag {
        return 0;
    }
    let ref_is_bottom = ref_pic.structure == PictureStructure::BottomField;
    if bottom_field_flag == ref_is_bottom {
        0 // same parity
    } else if bottom_field_flag {
        2 // bottom referencing top: +2
    } else {
        -2 // top referencing bottom: -2
    }
}

use crate::deblock::MbType;

impl SliceContext<'_> {
    /// Returns true if the current MB uses field coefficient scan order.
    /// True for field pictures and for field-coded MBs in MBAFF.
    pub(crate) fn is_field_scan(&self, mb_idx: usize) -> bool {
        self.field_pic_flag || (self.mbaff && self.mb_field_decoding[mb_idx / 2])
    }

    /// Compute the chroma MV vertical offset for field pictures.
    /// When a field references a field of opposite parity, the chroma MV needs
    /// a ±2 eighth-pel offset to compensate for the interlaced chroma sampling
    /// grid difference (spec 8.4.2.2, FFmpeg h264_mb.c lines 289-293).
    /// Returns the offset in eighth-pel units (added to the luma MV for chroma MC).
    pub(crate) fn chroma_field_mv_offset(&self, ref_pic: &DecodedPicture) -> i32 {
        chroma_field_mv_offset_impl(self.field_pic_flag, self.bottom_field_flag, ref_pic)
    }

    /// Set per-MB pixel layout (stride and y-offset) based on field/frame coding.
    /// Must be called at the start of each MB decode.
    pub(crate) fn set_mb_layout(&mut self, mb_idx: usize, mb_x: usize, mb_y: usize) {
        let w = self.stride;
        let cw = (self.width / 2) as usize;
        if self.mbaff && self.mb_field_decoding[mb_idx / 2] {
            // Field-coded MB pair: doubled stride, field-line offset
            let pair_row = (mb_idx / 2) / self.mb_width as usize;
            let pair_y = pair_row * 32;
            let is_bottom = !mb_idx.is_multiple_of(2);
            self.ly_stride = w * 2;
            self.ly_offset = (pair_y + if is_bottom { 1 } else { 0 }) * w;
            self.lc_stride = cw * 2;
            let pair_cy = pair_row * 16; // chroma pair y
            self.lc_offset = (pair_cy + if is_bottom { 1 } else { 0 }) * cw;
        } else {
            // Frame-coded (progressive or MBAFF frame-coded pair)
            self.ly_stride = w;
            self.ly_offset = mb_y * w;
            self.lc_stride = cw;
            self.lc_offset = (mb_y / 2) * cw;
        }
        let _ = mb_x; // mb_x not needed for layout, only mb_y
    }

    /// Returns MC parameters for the current field-coded or frame-coded MB.
    /// `ref_idx` is the decoded reference index for the partition being MC'd.
    /// Returns `(mc_y, ref_stride, ref_y_offset, mc_cy, c_ref_stride, c_ref_offset)`.
    ///
    /// For field-coded MBs, the reference index is in field units:
    /// - Even ref_idx → same-parity field (top→top, bottom→bottom)
    /// - Odd ref_idx → opposite-parity field (top→bottom, bottom→top)
    ///
    /// The actual frame reference is at `ref_list[ref_idx / 2]`.
    #[inline]
    pub(crate) fn mc_params(
        &self,
        mb_idx: usize,
        mb_y: usize,
        ref_width: usize,
        ref_idx: i8,
    ) -> (i32, usize, usize, usize, usize, usize) {
        if self.mbaff && self.mb_field_decoding[mb_idx / 2] {
            let pair_row = (mb_idx / 2) / self.mb_width as usize;
            let is_bottom = !mb_idx.is_multiple_of(2);
            let mc_y = (pair_row * 16) as i32;
            let ref_stride = ref_width * 2;
            // Determine which field of the reference to read:
            // Even ref_idx → same parity, odd ref_idx → opposite parity
            let ref_is_bottom = is_bottom ^ (ref_idx % 2 != 0);
            let ref_y_off = if ref_is_bottom { ref_width } else { 0 };
            let mc_cy = pair_row * 8;
            let cw = ref_width / 2;
            let c_ref_stride = cw * 2;
            let c_ref_off = if ref_is_bottom { cw } else { 0 };
            (mc_y, ref_stride, ref_y_off, mc_cy, c_ref_stride, c_ref_off)
        } else {
            (mb_y as i32, ref_width, 0, mb_y / 2, ref_width / 2, 0)
        }
    }

    /// Map a field-coded ref_idx to the frame-level reference list index.
    /// For field-coded MBs, ref_idx is doubled (2 fields per frame ref).
    /// For frame-coded, returns ref_idx unchanged.
    #[inline]
    pub(crate) fn frame_ref_idx(&self, mb_idx: usize, ref_idx: i8) -> i8 {
        if self.mbaff && self.mb_field_decoding[mb_idx / 2] {
            ref_idx / 2
        } else {
            ref_idx
        }
    }

    /// Returns true if the current MB is field-coded (for CABAC context selection).
    /// True for field pictures and for field-coded MBs in MBAFF.
    #[inline]
    pub(crate) fn is_field_coded(&self, mb_idx: usize) -> bool {
        self.field_pic_flag || (self.mbaff && self.mb_field_decoding[mb_idx / 2])
    }

    /// Field-coded MBs double the count (each frame ref → 2 field refs).
    #[inline]
    pub(crate) fn effective_num_ref(&self, mb_idx: usize, num_ref: u32) -> u32 {
        if self.mbaff && self.mb_field_decoding[mb_idx / 2] {
            (num_ref * 2).min(32)
        } else {
            num_ref
        }
    }

    /// Get the left MB index for context lookups.
    /// Returns `None` if no left MB exists or it's in a different slice.
    #[inline]
    pub(crate) fn left_mb(&self, mb_idx: usize) -> Option<usize> {
        if !self.mbaff {
            let mb_col = mb_idx % self.mb_width as usize;
            if mb_col == 0 {
                return None;
            }
            let left = mb_idx - 1;
            if self.mb_slice_id[left] != self.this_slice_id {
                return None;
            }
            Some(left)
        } else {
            let pair_addr = mb_idx / 2;
            let pair_col = pair_addr % self.mb_width as usize;
            if pair_col == 0 {
                return None;
            }
            let left_pair = pair_addr - 1;
            let cur_field = self.mb_field_decoding[pair_addr];
            let left_field = self.mb_field_decoding[left_pair];
            // For per-MB context (skip, mb_type, cbp, etc.), use same position in left pair
            let left_mb = if cur_field == left_field {
                left_pair * 2 + (mb_idx % 2)
            } else if !cur_field && left_field {
                // Current frame, left field: use top field for top MB, bottom for bottom
                left_pair * 2 + (mb_idx % 2)
            } else {
                // Current field, left frame: use same-parity MB
                left_pair * 2 + (mb_idx % 2)
            };
            if self.mb_slice_id[left_mb] != self.this_slice_id {
                return None;
            }
            Some(left_mb)
        }
    }

    /// Get the above MB index for context lookups.
    /// Returns `None` if no above MB exists or it's in a different slice.
    #[inline]
    pub(crate) fn above_mb(&self, mb_idx: usize) -> Option<usize> {
        if !self.mbaff {
            if mb_idx < self.mb_width as usize {
                return None;
            }
            let above = mb_idx - self.mb_width as usize;
            if self.mb_slice_id[above] != self.this_slice_id {
                return None;
            }
            Some(above)
        } else {
            let is_top = mb_idx.is_multiple_of(2);
            let pair_addr = mb_idx / 2;
            let is_field = self.mb_field_decoding[pair_addr];
            if !is_top && !is_field {
                // Frame-coded bottom MB: above is top MB of same pair
                let above = mb_idx - 1;
                if self.mb_slice_id[above] != self.this_slice_id {
                    return None;
                }
                Some(above)
            } else if !is_top && is_field {
                // Field-coded bottom MB: above is bottom of above pair (same field)
                let pair_row = pair_addr / self.mb_width as usize;
                if pair_row == 0 {
                    return None;
                }
                let above_pair = pair_addr - self.mb_width as usize;
                let above_mb = above_pair * 2 + 1; // bottom of above pair (same field)
                if self.mb_slice_id[above_mb] != self.this_slice_id {
                    return None;
                }
                Some(above_mb)
            } else {
                // Top MB: above is bottom of above pair
                let pair_row = pair_addr / self.mb_width as usize;
                if pair_row == 0 {
                    return None;
                }
                let above_pair = pair_addr - self.mb_width as usize;
                let above_mb = above_pair * 2 + 1; // bottom of above pair
                if self.mb_slice_id[above_mb] != self.this_slice_id {
                    return None;
                }
                Some(above_mb)
            }
        }
    }

    /// Check if a neighbor MB is available for intra prediction samples.
    /// When `constrained_intra_pred_flag` is set in a P/B slice, inter-predicted
    /// neighbors are treated as unavailable (spec 6.4.1).
    pub(crate) fn is_intra_neighbor_avail(&self, neighbor_idx: usize, sp: &SliceParams) -> bool {
        if !sp.constrained_intra_pred_flag || !(sp.is_p_slice || sp.is_b_slice) {
            return true; // no constraint, neighbor is available
        }
        let mt = self.mb_info[neighbor_idx].mb_type;
        mt == MbType::Intra || mt == MbType::Ipcm
    }

    /// Build per-MB intra availability array for predict_i4x4_mode.
    /// When constrained_intra_pred_flag is off or in I-slices, all MBs are available.
    pub(crate) fn intra_avail_map(&self, sp: &SliceParams) -> Vec<bool> {
        if !sp.constrained_intra_pred_flag || !(sp.is_p_slice || sp.is_b_slice) {
            vec![true; self.mb_info.len()]
        } else {
            self.mb_info
                .iter()
                .map(|info| info.mb_type == MbType::Intra || info.mb_type == MbType::Ipcm)
                .collect()
        }
    }

    /// Decode a P-slice skip macroblock: median MV prediction, MC, no residual.
    pub(crate) fn decode_p_skip_mb(
        &mut self,
        mb_idx: usize,
        mb_x: usize,
        mb_y: usize,
        sp: &SliceParams,
    ) {
        let ref_pic_list = sp.ref_pic_list;
        let wctx = sp.wctx;
        let use_weight = sp.use_weight;
        let (mvp_x, mvp_y) = predict_mv_skip(
            self.mv_store_l0,
            self.ref_idx_store_l0,
            mb_idx,
            self.mb_width as usize,
            self.mb_slice_id,
            self.this_slice_id,
            MbaffCtx {
                mbaff: self.mbaff,
                mb_field_decoding: self.mb_field_decoding,
            },
        );
        if let Some(ref_pic) = ref_pic_list.first() {
            let (mc_y, ref_stride, ref_y_off, mc_cy, c_ref_stride, c_ref_off) =
                self.mc_params(mb_idx, mb_y, ref_pic.width as usize, 0i8);
            // Luma MC
            let mut luma_pred = [0u16; 256];
            inter_pred::luma_mc_stride(
                ref_pic,
                mb_x as i32,
                mc_y,
                mvp_x as i32,
                mvp_y as i32,
                16,
                16,
                &mut luma_pred,
                ref_stride,
                ref_y_off,
            );
            if use_weight == 1 {
                wctx.apply_uni(&mut luma_pred, 0, 0, false, 0);
            }
            for r in 0..16 {
                for c in 0..16 {
                    self.frame.y[self.ly_offset + r * self.ly_stride + mb_x + c] =
                        luma_pred[r * 16 + c];
                }
            }
            // Chroma MC
            let cw = c_ref_stride;
            let cx = mb_x / 2;
            let cy = mc_cy;
            let cstride = self.lc_stride;
            let cbase = self.lc_offset;
            // Field picture chroma MV offset for opposite-parity reference
            let chroma_mv_y_offset = self.chroma_field_mv_offset(ref_pic);
            let cmv_y = mvp_y as i32 + chroma_mv_y_offset;
            let mut cb_pred = [0u16; 64];
            let mut cr_pred = [0u16; 64];
            inter_pred::chroma_mc(
                &ref_pic.u[c_ref_off..],
                cw,
                (self.height / 2) as usize,
                cx as i32,
                cy as i32,
                mvp_x as i32,
                cmv_y,
                8,
                8,
                &mut cb_pred,
            );
            inter_pred::chroma_mc(
                &ref_pic.v[c_ref_off..],
                cw,
                (self.height / 2) as usize,
                cx as i32,
                cy as i32,
                mvp_x as i32,
                cmv_y,
                8,
                8,
                &mut cr_pred,
            );
            if use_weight == 1 {
                wctx.apply_uni(&mut cb_pred, 0, 0, true, 0);
                wctx.apply_uni(&mut cr_pred, 0, 0, true, 1);
            }
            for r in 0..8 {
                for c in 0..8 {
                    self.frame.u[cbase + r * cstride + cx + c] = cb_pred[r * 8 + c];
                    self.frame.v[cbase + r * cstride + cx + c] = cr_pred[r * 8 + c];
                }
            }
        }
        // Store MVs and ref indices
        for blk in 0..16 {
            self.mv_store_l0[mb_idx * 16 + blk] = [mvp_x, mvp_y];
            self.ref_idx_store_l0[mb_idx * 16 + blk] = 0;
        }
    }

    /// Derive direct-mode MVs for all 16 4x4 blocks of a macroblock.
    ///
    /// Used by B_Skip, B_Direct_16x16, and B_Direct_8x8 sub-partitions.
    pub(crate) fn derive_direct_mvs(
        &mut self,
        mb_idx: usize,
        blk_start: usize,
        blk_count: usize,
        sp: &SliceParams,
    ) {
        let direct_spatial = sp.direct_spatial_mv_pred_flag;
        let direct_8x8_inference_flag = sp.direct_8x8_inference_flag;
        let current_poc = sp.current_poc;
        let ref_pic_list_l0 = sp.ref_pic_list_l0;
        let ref_pic_list_l1 = sp.ref_pic_list_l1;
        // When direct_8x8_inference_flag is set, all 4 blocks within each 8x8
        // group get the same result: the neighbor-derived MV/ref uses MB-level
        // position (0,0), and the co-located lookup maps to the same representative
        // block per 8x8 group. Derive once per group and fill all 4 sub-blocks.
        if direct_spatial {
            if direct_8x8_inference_flag && blk_count == 4 {
                // Single 8x8 group: derive once, fill 4 blocks
                let first_blk = blk_start;
                let (mv_l0, mv_l1, ri_l0, ri_l1, _, _) = derive_spatial_direct_blk(
                    self.mv_store_l0,
                    self.ref_idx_store_l0,
                    self.mv_store_l1,
                    self.ref_idx_store_l1,
                    mb_idx,
                    self.mb_width as usize,
                    ref_pic_list_l1.first().map(|p| p.as_ref()),
                    first_blk,
                    self.mb_slice_id,
                    self.this_slice_id,
                    direct_8x8_inference_flag,
                    MbaffCtx {
                        mbaff: self.mbaff,
                        mb_field_decoding: self.mb_field_decoding,
                    },
                );
                let base = mb_idx * 16;
                for blk in blk_start..blk_start + 4 {
                    self.mv_store_l0[base + blk] = mv_l0;
                    self.ref_idx_store_l0[base + blk] = ri_l0;
                    self.mv_store_l1[base + blk] = mv_l1;
                    self.ref_idx_store_l1[base + blk] = ri_l1;
                }
            } else if direct_8x8_inference_flag && blk_count == 16 {
                // Full MB: derive once per 8x8 group (4 calls instead of 16)
                let base = mb_idx * 16;
                for group in 0..4 {
                    let first_blk = group * 4;
                    let (mv_l0, mv_l1, ri_l0, ri_l1, _, _) = derive_spatial_direct_blk(
                        self.mv_store_l0,
                        self.ref_idx_store_l0,
                        self.mv_store_l1,
                        self.ref_idx_store_l1,
                        mb_idx,
                        self.mb_width as usize,
                        ref_pic_list_l1.first().map(|p| p.as_ref()),
                        first_blk,
                        self.mb_slice_id,
                        self.this_slice_id,
                        direct_8x8_inference_flag,
                        MbaffCtx {
                            mbaff: self.mbaff,
                            mb_field_decoding: self.mb_field_decoding,
                        },
                    );
                    for blk in first_blk..first_blk + 4 {
                        self.mv_store_l0[base + blk] = mv_l0;
                        self.ref_idx_store_l0[base + blk] = ri_l0;
                        self.mv_store_l1[base + blk] = mv_l1;
                        self.ref_idx_store_l1[base + blk] = ri_l1;
                    }
                }
            } else {
                // No inference flag: per-block derivation
                for blk in blk_start..blk_start + blk_count {
                    let (mv_l0, mv_l1, ri_l0, ri_l1, _, _) = derive_spatial_direct_blk(
                        self.mv_store_l0,
                        self.ref_idx_store_l0,
                        self.mv_store_l1,
                        self.ref_idx_store_l1,
                        mb_idx,
                        self.mb_width as usize,
                        ref_pic_list_l1.first().map(|p| p.as_ref()),
                        blk,
                        self.mb_slice_id,
                        self.this_slice_id,
                        direct_8x8_inference_flag,
                        MbaffCtx {
                            mbaff: self.mbaff,
                            mb_field_decoding: self.mb_field_decoding,
                        },
                    );
                    self.mv_store_l0[mb_idx * 16 + blk] = mv_l0;
                    self.ref_idx_store_l0[mb_idx * 16 + blk] = ri_l0;
                    self.mv_store_l1[mb_idx * 16 + blk] = mv_l1;
                    self.ref_idx_store_l1[mb_idx * 16 + blk] = ri_l1;
                }
            }
        } else {
            let Some(col_pic) = ref_pic_list_l1.first() else {
                return; // malformed: temporal direct requires L1 ref list
            };
            if direct_8x8_inference_flag && blk_count == 4 {
                let first_blk = blk_start;
                let (mv_l0, mv_l1, ri_l0, ri_l1, _, _) = derive_temporal_direct_blk(
                    col_pic,
                    ref_pic_list_l0,
                    current_poc,
                    col_pic.pic_order_cnt,
                    mb_idx,
                    first_blk,
                    direct_8x8_inference_flag,
                    MbaffCtx {
                        mbaff: self.mbaff,
                        mb_field_decoding: self.mb_field_decoding,
                    },
                );
                let base = mb_idx * 16;
                for blk in blk_start..blk_start + 4 {
                    self.mv_store_l0[base + blk] = mv_l0;
                    self.ref_idx_store_l0[base + blk] = ri_l0;
                    self.mv_store_l1[base + blk] = mv_l1;
                    self.ref_idx_store_l1[base + blk] = ri_l1;
                }
            } else if direct_8x8_inference_flag && blk_count == 16 {
                let base = mb_idx * 16;
                for group in 0..4 {
                    let first_blk = group * 4;
                    let (mv_l0, mv_l1, ri_l0, ri_l1, _, _) = derive_temporal_direct_blk(
                        col_pic,
                        ref_pic_list_l0,
                        current_poc,
                        col_pic.pic_order_cnt,
                        mb_idx,
                        first_blk,
                        direct_8x8_inference_flag,
                        MbaffCtx {
                            mbaff: self.mbaff,
                            mb_field_decoding: self.mb_field_decoding,
                        },
                    );
                    for blk in first_blk..first_blk + 4 {
                        self.mv_store_l0[base + blk] = mv_l0;
                        self.ref_idx_store_l0[base + blk] = ri_l0;
                        self.mv_store_l1[base + blk] = mv_l1;
                        self.ref_idx_store_l1[base + blk] = ri_l1;
                    }
                }
            } else {
                for blk in blk_start..blk_start + blk_count {
                    let (mv_l0, mv_l1, ri_l0, ri_l1, _, _) = derive_temporal_direct_blk(
                        col_pic,
                        ref_pic_list_l0,
                        current_poc,
                        col_pic.pic_order_cnt,
                        mb_idx,
                        blk,
                        direct_8x8_inference_flag,
                        MbaffCtx {
                            mbaff: self.mbaff,
                            mb_field_decoding: self.mb_field_decoding,
                        },
                    );
                    self.mv_store_l0[mb_idx * 16 + blk] = mv_l0;
                    self.ref_idx_store_l0[mb_idx * 16 + blk] = ri_l0;
                    self.mv_store_l1[mb_idx * 16 + blk] = mv_l1;
                    self.ref_idx_store_l1[mb_idx * 16 + blk] = ri_l1;
                }
            }
        }
    }

    /// Decode a B-slice skip macroblock: spatial/temporal direct MV derivation,
    /// per-4x4-block MC (luma + chroma), no residual.
    /// Decode a B-slice skip macroblock: spatial/temporal direct MV derivation,
    /// per-4x4-block MC (luma + chroma), no residual.
    pub(crate) fn decode_b_skip_mb(
        &mut self,
        mb_idx: usize,
        mb_x: usize,
        mb_y: usize,
        sp: &SliceParams,
    ) {
        let ref_pic_list_l0 = sp.ref_pic_list_l0;
        let ref_pic_list_l1 = sp.ref_pic_list_l1;
        let wctx = sp.wctx;
        let use_weight = sp.use_weight;
        // Derive MVs per 4x4 block via spatial or temporal direct mode
        self.derive_direct_mvs(mb_idx, 0, 16, sp);

        // Luma MC: per-4x4-block
        let mut luma_pred = [0u16; 256];
        for (blk, &(blk_row, blk_col)) in BLOCK_INDEX_TO_OFFSET.iter().enumerate() {
            let bx = mb_x + blk_col;
            let mv0 = self.mv_store_l0[mb_idx * 16 + blk];
            let mv1 = self.mv_store_l1[mb_idx * 16 + blk];
            let r0 = self.ref_idx_store_l0[mb_idx * 16 + blk];
            let r1 = self.ref_idx_store_l1[mb_idx * 16 + blk];
            let bp0 = r0 >= 0;
            let bp1 = r1 >= 0;
            let mut blk_pred = [0u16; 16];
            if bp0 && bp1 {
                let mut p0 = [0u16; 16];
                let mut p1 = [0u16; 16];
                let Some(ref_l0) = ref_pic_safe(ref_pic_list_l0, r0) else {
                    return;
                };
                let Some(ref_l1) = ref_pic_safe(ref_pic_list_l1, r1) else {
                    return;
                };
                let (
                    mc_y_l0,
                    ref_stride_l0,
                    ref_y_off_l0,
                    _mc_cy_l0,
                    _c_ref_stride_l0,
                    _c_ref_off_l0,
                ) = self.mc_params(mb_idx, mb_y, ref_l0.width as usize, r0);
                inter_pred::luma_mc_stride(
                    ref_l0,
                    bx as i32,
                    mc_y_l0 + blk_row as i32,
                    mv0[0] as i32,
                    mv0[1] as i32,
                    4,
                    4,
                    &mut p0,
                    ref_stride_l0,
                    ref_y_off_l0,
                );
                let (
                    mc_y_l1,
                    ref_stride_l1,
                    ref_y_off_l1,
                    _mc_cy_l1,
                    _c_ref_stride_l1,
                    _c_ref_off_l1,
                ) = self.mc_params(mb_idx, mb_y, ref_l1.width as usize, r1);
                inter_pred::luma_mc_stride(
                    ref_l1,
                    bx as i32,
                    mc_y_l1 + blk_row as i32,
                    mv1[0] as i32,
                    mv1[1] as i32,
                    4,
                    4,
                    &mut p1,
                    ref_stride_l1,
                    ref_y_off_l1,
                );
                wctx.apply_bi(&p0, &p1, &mut blk_pred, r0 as usize, r1 as usize, false, 0);
            } else if bp0 {
                let Some(ref_pic) = ref_pic_safe(ref_pic_list_l0, r0) else {
                    return;
                };
                let (mc_y, ref_stride, ref_y_off, _mc_cy, _c_ref_stride, _c_ref_off) =
                    self.mc_params(mb_idx, mb_y, ref_pic.width as usize, r0);
                inter_pred::luma_mc_stride(
                    ref_pic,
                    bx as i32,
                    mc_y + blk_row as i32,
                    mv0[0] as i32,
                    mv0[1] as i32,
                    4,
                    4,
                    &mut blk_pred,
                    ref_stride,
                    ref_y_off,
                );
                if use_weight == 1 {
                    wctx.apply_uni(&mut blk_pred, 0, r0 as usize, false, 0);
                }
            } else if bp1 {
                let Some(ref_pic) = ref_pic_safe(ref_pic_list_l1, r1) else {
                    return;
                };
                let (mc_y, ref_stride, ref_y_off, _mc_cy, _c_ref_stride, _c_ref_off) =
                    self.mc_params(mb_idx, mb_y, ref_pic.width as usize, r1);
                inter_pred::luma_mc_stride(
                    ref_pic,
                    bx as i32,
                    mc_y + blk_row as i32,
                    mv1[0] as i32,
                    mv1[1] as i32,
                    4,
                    4,
                    &mut blk_pred,
                    ref_stride,
                    ref_y_off,
                );
                if use_weight == 1 {
                    wctx.apply_uni(&mut blk_pred, 1, r1 as usize, false, 0);
                }
            }
            for r in 0..4 {
                for c in 0..4 {
                    luma_pred[(blk_row + r) * 16 + blk_col + c] = blk_pred[r * 4 + c];
                }
            }
        }
        for r in 0..16 {
            for c in 0..16 {
                self.frame.y[self.ly_offset + r * self.ly_stride + mb_x + c] =
                    luma_pred[r * 16 + c];
            }
        }

        // Chroma MC: per-4x4-block
        let cx = mb_x / 2;
        let cstride = self.lc_stride;
        let cbase = self.lc_offset;
        let chroma_h = (self.height / 2) as usize;
        for plane_idx in 0..2 {
            let mut chroma_pred = [0u16; 64];
            for cblk in 0..4 {
                let cblk_row = (cblk / 2) * 4;
                let cblk_col = (cblk % 2) * 4;
                let luma_blk = cblk * 4;
                let mv0 = self.mv_store_l0[mb_idx * 16 + luma_blk];
                let mv1 = self.mv_store_l1[mb_idx * 16 + luma_blk];
                let r0 = self.ref_idx_store_l0[mb_idx * 16 + luma_blk];
                let r1 = self.ref_idx_store_l1[mb_idx * 16 + luma_blk];
                let bp0 = r0 >= 0;
                let bp1 = r1 >= 0;
                let mut cblk_pred = [0u16; 16];
                if bp0 && bp1 {
                    let mut c0 = [0u16; 16];
                    let mut c1 = [0u16; 16];
                    let Some(rl0) = ref_pic_safe(ref_pic_list_l0, r0) else {
                        return;
                    };
                    let Some(rl1) = ref_pic_safe(ref_pic_list_l1, r1) else {
                        return;
                    };
                    let (_mc_y_l0, _rs_l0, _ref_y_off_l0, mc_cy_l0, c_ref_stride_l0, c_ref_off_l0) =
                        self.mc_params(mb_idx, mb_y, rl0.width as usize, r0);
                    let (_mc_y_l1, _rs_l1, _ref_y_off_l1, mc_cy_l1, c_ref_stride_l1, c_ref_off_l1) =
                        self.mc_params(mb_idx, mb_y, rl1.width as usize, r1);
                    let cr0 = if plane_idx == 0 {
                        &rl0.u[c_ref_off_l0..]
                    } else {
                        &rl0.v[c_ref_off_l0..]
                    };
                    let cr1 = if plane_idx == 0 {
                        &rl1.u[c_ref_off_l1..]
                    } else {
                        &rl1.v[c_ref_off_l1..]
                    };
                    let cmv_y0 = mv0[1] as i32 + self.chroma_field_mv_offset(rl0);
                    let cmv_y1 = mv1[1] as i32 + self.chroma_field_mv_offset(rl1);
                    inter_pred::chroma_mc(
                        cr0,
                        c_ref_stride_l0,
                        chroma_h,
                        (cx + cblk_col) as i32,
                        (mc_cy_l0 + cblk_row) as i32,
                        mv0[0] as i32,
                        cmv_y0,
                        4,
                        4,
                        &mut c0,
                    );
                    inter_pred::chroma_mc(
                        cr1,
                        c_ref_stride_l1,
                        chroma_h,
                        (cx + cblk_col) as i32,
                        (mc_cy_l1 + cblk_row) as i32,
                        mv1[0] as i32,
                        cmv_y1,
                        4,
                        4,
                        &mut c1,
                    );
                    wctx.apply_bi(
                        &c0,
                        &c1,
                        &mut cblk_pred,
                        r0 as usize,
                        r1 as usize,
                        true,
                        plane_idx,
                    );
                } else if bp0 {
                    let Some(ref_pic) = ref_pic_safe(ref_pic_list_l0, r0) else {
                        return;
                    };
                    let (_mc_y, _rs, _ref_y_off, mc_cy, c_ref_stride, c_ref_off) =
                        self.mc_params(mb_idx, mb_y, ref_pic.width as usize, r0);
                    let cr = if plane_idx == 0 {
                        &ref_pic.u[c_ref_off..]
                    } else {
                        &ref_pic.v[c_ref_off..]
                    };
                    let cmv_y = mv0[1] as i32 + self.chroma_field_mv_offset(ref_pic);
                    inter_pred::chroma_mc(
                        cr,
                        c_ref_stride,
                        chroma_h,
                        (cx + cblk_col) as i32,
                        (mc_cy + cblk_row) as i32,
                        mv0[0] as i32,
                        cmv_y,
                        4,
                        4,
                        &mut cblk_pred,
                    );
                    if use_weight == 1 {
                        wctx.apply_uni(&mut cblk_pred, 0, r0 as usize, true, plane_idx);
                    }
                } else if bp1 {
                    let Some(ref_pic) = ref_pic_safe(ref_pic_list_l1, r1) else {
                        return;
                    };
                    let (_mc_y, _rs, _ref_y_off, mc_cy, c_ref_stride, c_ref_off) =
                        self.mc_params(mb_idx, mb_y, ref_pic.width as usize, r1);
                    let cr = if plane_idx == 0 {
                        &ref_pic.u[c_ref_off..]
                    } else {
                        &ref_pic.v[c_ref_off..]
                    };
                    let cmv_y = mv1[1] as i32 + self.chroma_field_mv_offset(ref_pic);
                    inter_pred::chroma_mc(
                        cr,
                        c_ref_stride,
                        chroma_h,
                        (cx + cblk_col) as i32,
                        (mc_cy + cblk_row) as i32,
                        mv1[0] as i32,
                        cmv_y,
                        4,
                        4,
                        &mut cblk_pred,
                    );
                    if use_weight == 1 {
                        wctx.apply_uni(&mut cblk_pred, 1, r1 as usize, true, plane_idx);
                    }
                }
                for r in 0..4 {
                    for c in 0..4 {
                        chroma_pred[(cblk_row + r) * 8 + cblk_col + c] = cblk_pred[r * 4 + c];
                    }
                }
            }
            let fp = if plane_idx == 0 {
                &mut self.frame.u
            } else {
                &mut self.frame.v
            };
            for r in 0..8 {
                for c in 0..8 {
                    fp[cbase + r * cstride + cx + c] = chroma_pred[r * 8 + c];
                }
            }
        }
        self.mb_is_direct[mb_idx] = true;
        for blk in 0..16 {
            self.blk_is_direct[mb_idx * 16 + blk] = true;
        }
    }

    /// Gather CABAC CBP neighbor context for a macroblock.
    ///
    /// Returns `(left_cbp, top_cbp, left_cbp_chroma, top_cbp_chroma)` for use
    /// with `decode_cbp_luma` and `decode_cbp_chroma`.
    pub(crate) fn cabac_cbp_context(&self, mb_idx: usize, is_intra: bool) -> (u8, u8, u8, u8) {
        let unavail_cbp: u16 = if is_intra { 0x7CF } else { 0x00F };
        let left_cbp_raw = if let Some(left) = self.left_mb(mb_idx) {
            self.mb_cbp[left]
        } else {
            unavail_cbp
        };
        let top_cbp_raw = if let Some(above) = self.above_mb(mb_idx) {
            self.mb_cbp[above]
        } else {
            unavail_cbp
        };
        let left_cbp =
            ((left_cbp_raw & 0x7F0) | (left_cbp_raw & 2) | (((left_cbp_raw >> 2) & 2) << 2)) as u8;
        let top_cbp = top_cbp_raw as u8;
        let left_cbp_c = ((left_cbp_raw >> 4) & 3) as u8;
        let top_cbp_c = ((top_cbp_raw >> 4) & 3) as u8;
        (left_cbp, top_cbp, left_cbp_c, top_cbp_c)
    }

    /// Fill per-4x4-block MV/ref/nnz data into MbInfo for deblocking bS derivation,
    /// and build the per-block ref POC table for temporal direct mode.
    /// Reconstruct one chroma plane: Hadamard + dequant DC, unzigzag + dequant AC,
    /// IDCT, add prediction, clamp, and write to frame.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn reconstruct_chroma_plane(
        &mut self,
        plane_dc: &mut [i32; 4],
        plane_ac: &[[i32; 15]; 4],
        pred: &[u16; 64],
        is_u: bool,
        cbp_chroma: u8,
        qp_c: i32,
        chroma_scale: &[u8; 16],
        mb_x: usize,
        mb_idx: usize,
    ) {
        let chroma_mb_x = mb_x / 2;

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
                let zigzag = if self.is_field_scan(mb_idx) {
                    &ZIGZAG_4X4_FIELD
                } else {
                    &ZIGZAG_4X4
                };
                for scan_idx in 0..15 {
                    let (r, c) = zigzag[scan_idx + 1];
                    block_raster[r * 4 + c] = plane_ac[blk][scan_idx];
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
        let fp = if is_u {
            &mut self.frame.u
        } else {
            &mut self.frame.v
        };
        for y in 0..8 {
            for x in 0..8 {
                let val = (pred[y * 8 + x] as i32 + chroma_residual[y * 8 + x])
                    .clamp(0, self.pixel_max_c) as u16;
                fp[self.lc_offset + y * self.lc_stride + chroma_mb_x + x] = val;
            }
        }
    }

    /// Compute intra chroma prediction for both U and V planes.
    ///
    /// Gathers neighbor samples (respecting slice boundaries), calls `predict_chroma_8x8`
    /// for each plane, and returns the two 64-byte prediction buffers.
    /// Compute I16x16 luma prediction and add residual to the frame.
    ///
    /// Gathers neighbor samples (respecting slice boundaries), calls `predict_intra_16x16`,
    /// adds the 16x16 residual, clamps, and writes to `frame.y`.
    /// Predict and reconstruct a single I4x4 luma block.
    ///
    /// Gathers above/left/above-left neighbor samples (respecting slice boundaries
    /// and above-right availability per spec 6.4.12), calls `predict_intra_4x4`,
    /// adds residual, clamps, and writes to `frame.y`.
    #[allow(clippy::too_many_arguments)]
    /// Predict and reconstruct a single I8x8 luma block (8x8 transform).
    ///
    /// Similar to I4x4 but operates on 8x8 blocks with 16-sample above buffer
    /// and 8-sample left buffer.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn reconstruct_luma_8x8_block(
        &mut self,
        i8x8: usize,
        mb_x: usize,
        pred_mode: u8,
        luma_residual: &[i32; 256],
        above_mb_avail: bool,
        left_mb_avail: bool,
        above_left_mb_avail: bool,
        above_right_mb_avail: bool,
    ) {
        let row_off = (i8x8 / 2) * 8;
        let col_off = (i8x8 % 2) * 8;
        let px = mb_x + col_off;

        // Above samples (16: 8 above + 8 above-right)
        let above_base = self.ly_offset + row_off * self.ly_stride;
        let above_avail = above_base >= self.ly_stride && (row_off > 0 || above_mb_avail);
        let above_buf: Option<[u16; 16]> = if above_avail {
            let mut buf = [0u16; 16];
            let above_row = above_base - self.ly_stride;
            for (i, b) in buf.iter_mut().enumerate().take(8) {
                *b = self.frame.y[above_row + px + i];
            }
            let has_tr = if row_off == 0 {
                if px + 8 < (mb_x + 16).min(self.stride) {
                    true
                } else if px + 8 < self.stride {
                    above_right_mb_avail
                } else {
                    false
                }
            } else {
                col_off == 0
            };
            if has_tr {
                for i in 0..8 {
                    let col = (px + 8 + i).min(self.stride - 1);
                    buf[8 + i] = self.frame.y[above_row + col];
                }
            } else {
                let last = buf[7];
                buf[8..].fill(last);
            }
            Some(buf)
        } else {
            None
        };

        // Left samples
        let left_avail = px > 0 && (col_off > 0 || left_mb_avail);
        let left_buf: Option<[u16; 8]> = if left_avail {
            let mut buf = [0u16; 8];
            for (i, b) in buf.iter_mut().enumerate() {
                *b = self.frame.y[self.ly_offset + (row_off + i) * self.ly_stride + px - 1];
            }
            Some(buf)
        } else {
            None
        };

        // Above-left
        let al_avail = px > 0
            && above_base >= self.ly_stride
            && ((row_off > 0 && col_off > 0)
                || (row_off > 0 && col_off == 0 && left_mb_avail)
                || (row_off == 0 && col_off > 0 && above_mb_avail)
                || (row_off == 0 && col_off == 0 && above_left_mb_avail));
        let above_left_val = if al_avail {
            Some(self.frame.y[above_base - self.ly_stride + px - 1])
        } else {
            None
        };

        let has_topright = above_buf.is_some()
            && ((row_off == 0 && px + 8 < self.stride) || (row_off > 0 && col_off == 0));

        let mut pred = [0u16; 64];
        predict_intra_8x8(
            pred_mode,
            above_buf.as_ref().map(|b| &b[..]),
            left_buf.as_ref().map(|b| &b[..]),
            above_left_val,
            has_topright,
            &mut pred,
            self.bit_depth_y,
        );
        for r in 0..8 {
            for c in 0..8 {
                let val = (pred[r * 8 + c] as i32 + luma_residual[(row_off + r) * 16 + col_off + c])
                    .clamp(0, self.pixel_max_y) as u16;
                self.frame.y[self.ly_offset + (row_off + r) * self.ly_stride + px + c] = val;
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn reconstruct_luma_4x4_block(
        &mut self,
        px: usize,
        py: usize,
        mb_x: usize,
        mb_y: usize,
        blk: usize,
        pred_mode: u8,
        block_coeffs: &[i32; 16],
        above_mb_avail: bool,
        left_mb_avail: bool,
        above_left_mb_avail: bool,
        above_right_mb_avail: bool,
    ) {
        let local_row = py - mb_y;
        let local_col = px - mb_x;

        // Above samples
        let above_base = self.ly_offset + local_row * self.ly_stride;
        let above_avail = above_base >= self.ly_stride && (local_row > 0 || above_mb_avail);
        let above_buf: Option<[u16; 8]> = if above_avail {
            let mut buf = [0u16; 8];
            let above_row = above_base - self.ly_stride;
            for (i, b) in buf.iter_mut().enumerate().take(4) {
                *b = self.frame.y[above_row + px + i];
            }
            let topright_avail = if local_row == 0 {
                if px + 4 < (mb_x + 16).min(self.stride) {
                    true
                } else if px + 4 < self.stride {
                    above_right_mb_avail
                } else {
                    false
                }
            } else {
                !matches!(blk, 3 | 7 | 11 | 13 | 15)
            };
            if topright_avail {
                for (i, b) in buf.iter_mut().enumerate().skip(4) {
                    let col = (px + i).min(self.stride - 1);
                    *b = self.frame.y[above_row + col];
                }
            } else {
                let last = buf[3];
                buf[4..8].fill(last);
            }
            Some(buf)
        } else {
            None
        };

        // Left samples
        let left_avail = px > 0 && (local_col > 0 || left_mb_avail);
        let left_buf: Option<[u16; 4]> = if left_avail {
            let mut buf = [0u16; 4];
            for (i, b) in buf.iter_mut().enumerate() {
                *b = self.frame.y[self.ly_offset + (local_row + i) * self.ly_stride + px - 1];
            }
            Some(buf)
        } else {
            None
        };

        // Above-left sample
        let al_avail = px > 0
            && above_base >= self.ly_stride
            && ((local_row > 0 && local_col > 0)
                || (local_row > 0 && local_col == 0 && left_mb_avail)
                || (local_row == 0 && local_col > 0 && above_mb_avail)
                || (local_row == 0 && local_col == 0 && above_left_mb_avail));
        let above_left_val = if al_avail {
            Some(self.frame.y[above_base - self.ly_stride + px - 1])
        } else {
            None
        };

        // Predict + add residual + write
        let mut pred = [0u16; 16];
        predict_intra_4x4(
            pred_mode,
            above_buf.as_ref().map(|b| &b[..]),
            left_buf.as_ref().map(|b| &b[..]),
            above_left_val,
            &mut pred,
            self.bit_depth_y,
        );
        for r in 0..4 {
            for c in 0..4 {
                let val = (pred[r * 4 + c] as i32 + block_coeffs[r * 4 + c])
                    .clamp(0, self.pixel_max_y) as u16;
                self.frame.y[self.ly_offset + (local_row + r) * self.ly_stride + px + c] = val;
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn reconstruct_luma_16x16(
        &mut self,
        mb_x: usize,
        pred_mode: u8,
        luma_residual: &[i32; 256],
        above_avail: bool,
        left_avail: bool,
        above_left_avail: bool,
    ) {
        let above: Option<Vec<u16>> = if self.ly_offset >= self.ly_stride && above_avail {
            Some(
                (0..16)
                    .map(|x| self.frame.y[self.ly_offset - self.ly_stride + mb_x + x])
                    .collect(),
            )
        } else {
            None
        };
        let left: Option<Vec<u16>> = if mb_x > 0 && left_avail {
            Some(
                (0..16)
                    .map(|y| self.frame.y[self.ly_offset + y * self.ly_stride + mb_x - 1])
                    .collect(),
            )
        } else {
            None
        };
        let above_left = if mb_x > 0 && self.ly_offset >= self.ly_stride && above_left_avail {
            Some(self.frame.y[self.ly_offset - self.ly_stride + mb_x - 1])
        } else {
            None
        };
        let mut luma_pred = [0u16; 256];
        predict_intra_16x16(
            pred_mode,
            above.as_deref(),
            left.as_deref(),
            above_left,
            &mut luma_pred,
            self.bit_depth_y,
        );
        for r in 0..16 {
            for c in 0..16 {
                let val = (luma_pred[r * 16 + c] as i32 + luma_residual[r * 16 + c])
                    .clamp(0, self.pixel_max_y) as u16;
                self.frame.y[self.ly_offset + r * self.ly_stride + mb_x + c] = val;
            }
        }
    }

    /// Compute intra chroma prediction for both U and V planes.
    pub(crate) fn predict_chroma_intra(
        &self,
        mb_x: usize,
        intra_chroma_pred_mode: u8,
        above_avail: bool,
        left_avail: bool,
        above_left_avail: bool,
    ) -> ([u16; 64], [u16; 64]) {
        let chroma_mb_x = mb_x / 2;

        let mut pred_u = [0u16; 64];
        let mut pred_v = [0u16; 64];

        let cbase = self.lc_offset;
        let cstride = self.lc_stride;
        for (plane_buf, pred) in [(&self.frame.u, &mut pred_u), (&self.frame.v, &mut pred_v)] {
            let above = if cbase >= cstride && above_avail {
                let mut buf = [0u16; 8];
                buf.copy_from_slice(
                    &plane_buf[cbase - cstride + chroma_mb_x..cbase - cstride + chroma_mb_x + 8],
                );
                Some(buf)
            } else {
                None
            };
            let left = if mb_x > 0 && left_avail {
                let mut buf = [0u16; 8];
                for (i, b) in buf.iter_mut().enumerate() {
                    *b = plane_buf[cbase + i * cstride + chroma_mb_x - 1];
                }
                Some(buf)
            } else {
                None
            };
            let above_left = if mb_x > 0 && cbase >= cstride && above_left_avail {
                Some(plane_buf[cbase - cstride + chroma_mb_x - 1])
            } else {
                None
            };
            predict_chroma_8x8(
                intra_chroma_pred_mode,
                above.as_ref().map(|b| &b[..]),
                left.as_ref().map(|b| &b[..]),
                above_left,
                pred,
                self.bit_depth_c,
            );
        }

        (pred_u, pred_v)
    }

    /// Write I_PCM samples (spec 7.3.5) for the MB at `mb_x` into the frame.
    ///
    /// `data` starts at the byte-aligned first pcm_sample_luma. Each luma
    /// sample is u(v) with v = BitDepthY and each chroma sample u(v) with
    /// v = BitDepthC, so 256 luma + 2 * 64 chroma samples always end on a
    /// byte boundary. Returns the number of bytes consumed, or `None` if
    /// `data` is too short.
    pub(crate) fn write_pcm_samples(&mut self, data: &[u8], mb_x: usize) -> Option<usize> {
        let bits_y = self.bit_depth_y as usize;
        let bits_c = self.bit_depth_c as usize;
        let total_bits = 256 * bits_y + 128 * bits_c;
        let total_bytes = total_bits / 8;
        if data.len() < total_bytes {
            return None;
        }
        // MSB-first reader over at most 14-bit fields.
        let mut bit_pos = 0usize;
        let mut read = |bits: usize| -> u16 {
            let mut v = 0u32;
            for _ in 0..bits {
                let byte = data[bit_pos >> 3];
                v = (v << 1) | ((byte >> (7 - (bit_pos & 7))) & 1) as u32;
                bit_pos += 1;
            }
            v as u16
        };
        for r in 0..16 {
            for c in 0..16 {
                let val = read(bits_y);
                let idx = self.ly_offset + r * self.ly_stride + mb_x + c;
                if idx < self.frame.y.len() {
                    self.frame.y[idx] = val;
                }
            }
        }
        let cx = mb_x / 2;
        for plane in 0..2 {
            for r in 0..8 {
                for c in 0..8 {
                    let val = read(bits_c);
                    let idx = self.lc_offset + r * self.lc_stride + cx + c;
                    let buf = if plane == 0 {
                        &mut self.frame.u
                    } else {
                        &mut self.frame.v
                    };
                    if idx < buf.len() {
                        buf[idx] = val;
                    }
                }
            }
        }
        Some(total_bytes)
    }

    /// Fill per-4x4-block MV/ref/nnz data into MbInfo for deblocking bS derivation,
    /// and build the per-block ref POC table for temporal direct mode.
    ///
    /// Called once after the MB loop completes, covering MBs `first_mb..last_mb`.
    pub(crate) fn finalize_mb_info(&mut self, first_mb: usize, last_mb: usize, sp: &SliceParams) {
        let list_count = if sp.is_b_slice {
            2u8
        } else if sp.is_p_slice {
            1
        } else {
            0
        };
        let l0_list = if sp.is_p_slice {
            sp.ref_pic_list
        } else {
            sp.ref_pic_list_l0
        };
        let ref_pic_list_l1 = sp.ref_pic_list_l1;

        #[allow(clippy::needless_range_loop)]
        for mi in first_mb..last_mb {
            let info = &mut self.mb_info[mi];
            let base = mi * 16;
            info.list_count = list_count;
            info.is_8x8dct = self.mb_is_8x8dct[mi];
            for blk in 0..16 {
                info.mv_l0[blk] = self.mv_store_l0[base + blk];
                info.ref_idx_l0[blk] = self.ref_idx_store_l0[base + blk];
                info.mv_l1[blk] = self.mv_store_l1[base + blk];
                info.ref_idx_l1[blk] = self.ref_idx_store_l1[base + blk];
                // With the 8x8 transform the bS=2 test is per 8x8 block (8.7.2.1), and CAVLC
                // spreads an 8x8 block's coefficients over its four 4x4 counts, so one quarter
                // can read 0 while its block is coded. Blocks are in luma4x4BlkIdx order: an
                // 8x8 block is the four entries starting at `blk & !3`.
                info.nnz[blk] = if info.is_8x8dct {
                    let first = base + (blk & !3);
                    self.nc_luma[first..first + 4].iter().any(|&n| n > 0)
                } else {
                    self.nc_luma[base + blk] > 0
                };
                let ri_l0 = self.ref_idx_store_l0[base + blk];
                info.ref_poc_l0[blk] = if ri_l0 >= 0 {
                    l0_list
                        .get(ri_l0 as usize)
                        .map(|p| p.pic_order_cnt)
                        .unwrap_or(-1)
                } else {
                    -1
                };
                let ri_l1 = self.ref_idx_store_l1[base + blk];
                info.ref_poc_l1[blk] = if ri_l1 >= 0 {
                    ref_pic_list_l1
                        .get(ri_l1 as usize)
                        .map(|p| p.pic_order_cnt)
                        .unwrap_or(-1)
                } else {
                    -1
                };
            }
        }

        // Build per-block ref POC table for temporal direct mode (spec 8.4.1.2.3).
        for i in 0..self.ref_poc_store_l0.len() {
            let ri = self.ref_idx_store_l0[i];
            if ri >= 0 && (ri as usize) < l0_list.len() {
                self.ref_poc_store_l0[i] = l0_list[ri as usize].pic_order_cnt;
            }
        }
    }
}
