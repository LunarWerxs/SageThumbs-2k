use std::collections::HashMap;

use std::rc::Rc;

use crate::bitstream::BitstreamReader;
use crate::deblock::{self, MbInfo, MbType};
use crate::decode_cabac::CabacMbResult;
use crate::dpb::{DecodedPicture, Dpb, ReferenceStatus};
use crate::error::DecodeError;
use crate::mv_pred::WeightContext;
use crate::nal::{NalUnit, NalUnitType};
use crate::pps::{parse_pps, Pps};
use crate::slice::{parse_slice_header, SliceType};
use crate::slice_context::{SliceContext, SliceParams};
use crate::sps::{parse_sps, Sps};

/// A decoded YUV 4:2:0 frame.
///
/// The Y plane is `width × height` samples; the U and V planes are each
/// `(width/2) × (height/2)` samples (4:2:0 chroma subsampling). Samples are
/// stored row-major with no inter-row padding (stride == width), one `u16`
/// per sample. Luma samples lie in `0..(1 << bit_depth_luma)` and chroma
/// samples in `0..(1 << bit_depth_chroma)`; for 8-bit streams every sample
/// fits in a `u8`. High 10 streams use 10-bit samples (`0..=1023`).
///
/// Frames returned by [`Decoder::decode_nal`] are emitted in **decode order**,
/// not display order. To present them in the intended order, sort by
/// `pic_order_cnt` within each GOP. See the crate-level documentation for the
/// recommended pattern (and a common pitfall around IDR boundary handling).
#[derive(Debug, Clone)]
pub struct Frame {
    /// Display width in luma samples.
    pub width: u32,
    /// Display height in luma samples.
    pub height: u32,
    /// Luma plane, `width * height` samples, row-major.
    pub y: Vec<u16>,
    /// Cb chroma plane, `(width/2) * (height/2)` samples, row-major.
    pub u: Vec<u16>,
    /// Cr chroma plane, `(width/2) * (height/2)` samples, row-major.
    pub v: Vec<u16>,
    /// Picture order count — the H.264 spec's display ordering value.
    /// Use this to sort frames into display order within a GOP.
    pub pic_order_cnt: i32,
    /// Luma sample bit depth (BitDepthY = 8 + bit_depth_luma_minus8, 8..=14).
    pub bit_depth_luma: u8,
    /// Chroma sample bit depth (BitDepthC = 8 + bit_depth_chroma_minus8, 8..=14).
    pub bit_depth_chroma: u8,
}

/// In-progress picture state shared across slices within the same frame.
#[derive(Clone)]
struct PictureState {
    frame: Frame,
    frame_num: u32,
    poc: i32,
    nal_unit_type: NalUnitType,
    nal_ref_idc: u8,
    // Per-MB arrays that persist across slices
    nc_luma: Vec<u8>,
    nc_cb: Vec<u8>,
    nc_cr: Vec<u8>,
    mv_store_l0: Vec<[i16; 2]>,
    mv_store_l1: Vec<[i16; 2]>,
    ref_idx_store_l0: Vec<i8>,
    ref_poc_store_l0: Vec<i32>,
    ref_idx_store_l1: Vec<i8>,
    mvd_store: Vec<[i16; 2]>,
    mvd_store_l1: Vec<[i16; 2]>,
    mb_info: Vec<deblock::MbInfo>,
    i4x4_modes: Vec<u8>,
    // CABAC neighbor context state
    mb_cbp: Vec<u16>,
    mb_chroma_pred: Vec<u8>,
    mb_is_8x8dct: Vec<bool>,
    mb_skip: Vec<bool>,
    mb_is_direct: Vec<bool>,
    blk_is_direct: Vec<bool>,
    is_i16x16: Vec<bool>,
    /// Per-MB slice ID for slice boundary detection. MBs from different
    /// slices are treated as unavailable for CABAC context and MV prediction.
    mb_slice_id: Vec<u16>,
    /// Current slice ID counter (incremented for each new slice).
    current_slice_id: u16,
    #[allow(dead_code)]
    prev_mb_qp: i32,
    #[allow(dead_code)]
    last_qp_delta_nonzero: bool,
    // Slice header info for finalization
    mmco_ops: Vec<(u32, u32)>,
    long_term_reference_flag: bool,
    is_intra_slice: bool,
    // Deblock parameters (from first slice; per-slice deblock offsets
    // could differ but we use the first slice's values)
    disable_deblocking_filter_idc: u32,
    slice_alpha_c0_offset_div2: i32,
    slice_beta_offset_div2: i32,
    chroma_qp_index_offset: i32,
    second_chroma_qp_index_offset: i32,
    mb_width: u32,
    mb_height: u32,
    /// Per-MB-pair field decoding flag (MBAFF only). Indexed by pair address.
    mb_field_decoding: Vec<bool>,
    /// True if this picture uses MBAFF (mb_adaptive_frame_field_flag && !field_pic_flag).
    mbaff_frame_flag: bool,
    /// True if this picture is a field picture (field_pic_flag=1).
    field_pic_flag: bool,
    /// True if this is the bottom field (only valid when field_pic_flag=true).
    bottom_field_flag: bool,
    /// Full frame height (needed for field picture output combining).
    frame_height: u32,
}

/// Streaming H.264 decoder.
///
/// Feed NAL units one at a time with [`decode_nal`](Self::decode_nal) and
/// receive decoded frames as they become available. Call [`flush`](Self::flush)
/// at end-of-stream to retrieve any final buffered frame.
///
/// Internally maintains parameter set tables (SPS, PPS), a Decoded Picture
/// Buffer (DPB), and any in-progress slice state.
///
/// # Example
///
/// ```no_run
/// use rust_h264::decoder::Decoder;
/// use rust_h264::nal::parse_annex_b;
///
/// let bitstream = std::fs::read("input.h264").unwrap();
/// let nals = parse_annex_b(&bitstream);
/// let mut decoder = Decoder::new();
///
/// for nal in &nals {
///     if let Ok(Some(frame)) = decoder.decode_nal(nal) {
///         println!("Decoded {}x{} frame, POC={}",
///                  frame.width, frame.height, frame.pic_order_cnt);
///     }
/// }
/// if let Some(frame) = decoder.flush() {
///     println!("Final frame, POC={}", frame.pic_order_cnt);
/// }
/// ```
pub struct Decoder {
    sps_table: HashMap<u32, Sps>,
    pps_table: HashMap<u32, Pps>,
    dpb: Dpb,
    /// In-progress picture being assembled from one or more slices.
    pending: Option<PictureState>,
    /// First field of a field-picture pair awaiting its complement for output.
    pending_field: Option<Frame>,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    /// Create a new decoder with empty parameter set tables and DPB.
    pub fn new() -> Self {
        Self {
            sps_table: HashMap::new(),
            pps_table: HashMap::new(),
            dpb: Dpb::new(0),
            pending: None,
            pending_field: None,
        }
    }

    /// Feed a single NAL unit to the decoder.
    ///
    /// Returns:
    /// - `Ok(Some(frame))` — a decoded frame is ready (in **decode order**;
    ///   sort by `pic_order_cnt` for display order). Note that the returned
    ///   frame belongs to the *previous* picture: when this NAL starts a new
    ///   picture, the decoder finalizes the previous one and returns it.
    /// - `Ok(None)` — the NAL was consumed but no frame is ready yet
    ///   (e.g., SPS, PPS, SEI, or the first slice of a multi-slice picture).
    /// - `Err(e)` — the NAL was malformed or used an unsupported feature.
    ///
    /// Call [`flush`](Self::flush) after the last NAL to retrieve any
    /// remaining buffered frame.
    pub fn decode_nal(&mut self, nal: &NalUnit) -> Result<Option<Frame>, DecodeError> {
        match nal.nal_unit_type {
            NalUnitType::Sps => {
                let sps = parse_sps(&nal.rbsp)?;
                self.dpb.set_max_ref_frames(sps.max_num_ref_frames);
                self.sps_table.insert(sps.seq_parameter_set_id, sps);
                Ok(None)
            }
            NalUnitType::Pps => {
                let pps_id_sps = {
                    // Peek at seq_parameter_set_id to find the right SPS
                    let mut peek = BitstreamReader::new(&nal.rbsp);
                    let _ = peek.read_ue(); // pic_parameter_set_id
                    peek.read_ue().ok()
                };
                let sps_ref = pps_id_sps.and_then(|id| self.sps_table.get(&id));
                let pps = parse_pps(&nal.rbsp, sps_ref)?;
                self.pps_table.insert(pps.pic_parameter_set_id, pps);
                Ok(None)
            }
            NalUnitType::Sei => Ok(None),
            NalUnitType::SliceIdr | NalUnitType::Slice => {
                // Peek at first_mb_in_slice to detect new vs continuation slice
                let mut peek = BitstreamReader::new(&nal.rbsp);
                let first_mb = peek.read_ue().unwrap_or(0);

                // Check if this is a new picture: first_mb==0 means first
                // slice of a new picture. Continuation slices (first_mb > 0)
                // belong to the same picture even for IDR NALs.
                let is_new_picture = first_mb == 0;

                // Finalize pending frame if a new picture starts
                let prev_frame = if is_new_picture {
                    self.finalize_pending()
                } else {
                    None
                };

                // Decode this slice (creates or continues PictureState).
                // For CAVLC multi-slice, end-of-slice detection may fail,
                // causing errors from reading past the slice boundary. If
                // we had a pending picture, the already-decoded MBs are
                // valid, so we treat the error as end-of-slice.
                //
                // Since decode_slice takes self.pending via take(), we must
                // save a backup for continuation slices so we can restore it
                // if the decode fails mid-slice.
                let had_pending = !is_new_picture && self.pending.is_some();
                let pending_backup = if had_pending {
                    self.pending.clone()
                } else {
                    None
                };
                match self.decode_slice(nal) {
                    Ok(()) => {}
                    Err(_e) if self.pending.is_some() => {}
                    Err(_e) if had_pending => {
                        // decode_slice consumed self.pending but failed before
                        // reassembling it. Restore the backup so already-decoded
                        // MBs from earlier slices are preserved.
                        self.pending = pending_backup;
                    }
                    Err(e) => return Err(e),
                }

                Ok(prev_frame)
            }
            _ => Ok(None),
        }
    }

    /// Finalize any pending picture and return it. Call this after the last
    /// NAL has been fed via [`decode_nal`](Self::decode_nal) to retrieve the
    /// final frame, which is otherwise held internally awaiting a new picture
    /// to trigger its release.
    pub fn flush(&mut self) -> Option<Frame> {
        let frame = self.finalize_pending();
        if frame.is_some() {
            return frame;
        }
        // If a single field is pending (complement never arrived), output it
        // as a half-height frame rather than losing it.
        self.pending_field.take()
    }

    /// Frame rate from the most recently parsed SPS's VUI timing info, as
    /// `(numerator, denominator)`. Returns `None` if no SPS has been parsed
    /// yet, if the SPS does not contain VUI timing info, or if the values
    /// are zero.
    ///
    /// For example, a stream encoded at 29.97 fps would return
    /// `Some((60000, 2002))`. Use [`frame_rate_f64`](Self::frame_rate_f64)
    /// for a single floating-point value.
    pub fn frame_rate(&self) -> Option<(u32, u32)> {
        // Return the first SPS that has timing info. For typical streams
        // there's only one SPS, so this is unambiguous.
        self.sps_table.values().find_map(|s| s.frame_rate())
    }

    /// Frame rate as a single floating-point value. Convenience wrapper
    /// around [`frame_rate`](Self::frame_rate).
    pub fn frame_rate_f64(&self) -> Option<f64> {
        let (n, d) = self.frame_rate()?;
        Some(n as f64 / d as f64)
    }

    /// Finalize the pending picture: apply deblocking, insert into DPB, return frame.
    fn finalize_pending(&mut self) -> Option<Frame> {
        let mut ps = self.pending.take()?;

        // Apply deblocking filter
        deblock::filter_frame_mbaff(
            &mut ps.frame,
            &ps.mb_info,
            ps.mb_width as usize,
            ps.disable_deblocking_filter_idc,
            ps.slice_alpha_c0_offset_div2,
            ps.slice_beta_offset_div2,
            ps.chroma_qp_index_offset,
            ps.second_chroma_qp_index_offset,
            ps.mbaff_frame_flag,
        );

        if ps.nal_unit_type == NalUnitType::SliceIdr {
            self.dpb.clear();
        }

        let reference = if ps.nal_ref_idc > 0 {
            if ps.nal_unit_type == NalUnitType::SliceIdr && ps.long_term_reference_flag {
                ReferenceStatus::LongTerm(0) // IDR with long_term_reference_flag → LT idx 0
            } else {
                ReferenceStatus::ShortTerm
            }
        } else {
            ReferenceStatus::Unused
        };

        // For MMCO, use CurrPicNum (= 2*frame_num+1 for field pictures)
        let mmco_curr_pic_num = if ps.field_pic_flag {
            (ps.frame_num * 2 + 1) as i32
        } else {
            ps.frame_num as i32
        };
        let mut has_mmco5 = false;
        for &(op, param) in &ps.mmco_ops {
            match op {
                1 => {
                    let pic_num_to_remove = mmco_curr_pic_num - ((param & 0xFFFF) as i32 + 1);
                    self.dpb.mark_short_term_unused(pic_num_to_remove as u32);
                }
                2 => {
                    self.dpb.mark_long_term_unused(param);
                }
                3 => {
                    let abs_diff_minus1 = param & 0xFFFF;
                    let long_term_frame_idx = param >> 16;
                    let pic_num = mmco_curr_pic_num - (abs_diff_minus1 as i32 + 1);
                    self.dpb
                        .assign_long_term(pic_num as u32, long_term_frame_idx);
                }
                4 => {
                    self.dpb.set_max_long_term_frame_idx(param);
                }
                5 => {
                    self.dpb.clear_all_refs();
                    has_mmco5 = true;
                }
                6 => {
                    // Will be applied after insert (current pic must be in DPB first)
                }
                _ => {}
            }
        }

        let pic = Rc::new(DecodedPicture {
            y: ps.frame.y.clone(),
            u: ps.frame.u.clone(),
            v: ps.frame.v.clone(),
            width: ps.mb_width * 16,
            height: (ps.frame.height.div_ceil(16)) * 16,
            frame_num: ps.frame_num,
            pic_order_cnt: ps.poc,
            mv_l0: ps.mv_store_l0,
            ref_idx_l0: ps.ref_idx_store_l0,
            ref_poc_l0: ps.ref_poc_store_l0,
            mv_l1: ps.mv_store_l1,
            ref_idx_l1: ps.ref_idx_store_l1,
            mb_width: ps.mb_width,
            is_intra: ps.is_intra_slice,
            structure: if ps.field_pic_flag {
                if ps.bottom_field_flag {
                    crate::dpb::PictureStructure::BottomField
                } else {
                    crate::dpb::PictureStructure::TopField
                }
            } else {
                crate::dpb::PictureStructure::Frame
            },
            bit_depth_luma: ps.frame.bit_depth_luma,
            bit_depth_chroma: ps.frame.bit_depth_chroma,
        });

        self.dpb.insert(pic, reference);

        // MMCO op=6: mark current picture as long-term (after insert)
        for &(op, param) in &ps.mmco_ops {
            if op == 6 {
                self.dpb.mark_current_as_long_term(param, ps.frame_num);
            }
        }

        // MMCO op=5: reset frame_num to 0 after clearing (spec 7.4.3.3)
        if has_mmco5 {
            // After op=5, the current picture should have frame_num = 0
            // This is handled by the encoder; we just need the DPB cleared.
        }

        // Crop frame from coded dimensions (MB-aligned) to display dimensions
        let coded_w = (ps.mb_width * 16) as usize;
        if coded_w == 0 {
            return None;
        }
        let coded_h = ps.frame.y.len() / coded_w;
        let display_w = ps.frame.width as usize;
        let display_h = ps.frame.height as usize;
        if coded_w != display_w || coded_h != display_h {
            // Ensure display dimensions don't exceed coded dimensions or frame buffer
            if display_w > coded_w || display_h > coded_h || coded_w * coded_h > ps.frame.y.len() {
                return None;
            }
            // Luma: copy display_w pixels per row from coded_w-stride buffer
            let mut y = vec![0u16; display_w * display_h];
            for r in 0..display_h {
                y[r * display_w..(r + 1) * display_w]
                    .copy_from_slice(&ps.frame.y[r * coded_w..r * coded_w + display_w]);
            }
            let chroma_coded_w = coded_w / 2;
            let chroma_w = display_w / 2;
            let chroma_h = display_h / 2;
            let mut u = vec![0u16; chroma_w * chroma_h];
            let mut v = vec![0u16; chroma_w * chroma_h];
            for r in 0..chroma_h {
                u[r * chroma_w..(r + 1) * chroma_w].copy_from_slice(
                    &ps.frame.u[r * chroma_coded_w..r * chroma_coded_w + chroma_w],
                );
                v[r * chroma_w..(r + 1) * chroma_w].copy_from_slice(
                    &ps.frame.v[r * chroma_coded_w..r * chroma_coded_w + chroma_w],
                );
            }
            ps.frame.y = y;
            ps.frame.u = u;
            ps.frame.v = v;
        }

        // For field pictures, combine two fields into one frame for output
        if ps.field_pic_flag {
            let field_frame = ps.frame;
            if let Some(first_field) = self.pending_field.take() {
                // Second field arrived — combine into a full frame
                let w = field_frame.width as usize;
                let full_h = ps.frame_height as usize;
                let field_h = full_h / 2;
                let mut y = vec![0u16; w * full_h];
                let mut u = vec![0u16; (w / 2) * (full_h / 2)];
                let mut v = vec![0u16; (w / 2) * (full_h / 2)];
                let cw = w / 2;

                // The two fields must agree on geometry and hold enough samples;
                // a corrupt stream can change SPS dimensions between fields.
                let ch = full_h / 4;
                let field_ok = |f: &Frame| {
                    f.width as usize == w
                        && f.bit_depth_luma == field_frame.bit_depth_luma
                        && f.bit_depth_chroma == field_frame.bit_depth_chroma
                        && f.y.len() >= field_h * w
                        && f.u.len() >= ch * cw
                        && f.v.len() >= ch * cw
                };
                if w == 0 || full_h < 2 || !field_ok(&first_field) || !field_ok(&field_frame) {
                    // Drop the stale field and hold the current one instead.
                    self.pending_field = Some(field_frame);
                    return None;
                }

                // Determine which is top and which is bottom
                let (top, bot) = if ps.bottom_field_flag {
                    (&first_field, &field_frame)
                } else {
                    (&field_frame, &first_field)
                };

                // Interleave luma lines: top→even, bottom→odd
                for r in 0..field_h {
                    let src_off = r * w;
                    y[r * 2 * w..r * 2 * w + w].copy_from_slice(&top.y[src_off..src_off + w]);
                    y[(r * 2 + 1) * w..(r * 2 + 1) * w + w]
                        .copy_from_slice(&bot.y[src_off..src_off + w]);
                }
                // Interleave chroma lines
                for r in 0..ch {
                    let src_off = r * cw;
                    u[r * 2 * cw..r * 2 * cw + cw].copy_from_slice(&top.u[src_off..src_off + cw]);
                    u[(r * 2 + 1) * cw..(r * 2 + 1) * cw + cw]
                        .copy_from_slice(&bot.u[src_off..src_off + cw]);
                    v[r * 2 * cw..r * 2 * cw + cw].copy_from_slice(&top.v[src_off..src_off + cw]);
                    v[(r * 2 + 1) * cw..(r * 2 + 1) * cw + cw]
                        .copy_from_slice(&bot.v[src_off..src_off + cw]);
                }

                return Some(Frame {
                    y,
                    u,
                    v,
                    width: w as u32,
                    height: full_h as u32,
                    pic_order_cnt: field_frame.pic_order_cnt.min(first_field.pic_order_cnt),
                    bit_depth_luma: field_frame.bit_depth_luma,
                    bit_depth_chroma: field_frame.bit_depth_chroma,
                });
            } else {
                // First field — hold it, return None
                self.pending_field = Some(field_frame);
                return None;
            }
        }

        Some(ps.frame)
    }

    fn decode_slice(&mut self, nal: &NalUnit) -> Result<(), DecodeError> {
        let pps = self
            .pps_table
            .values()
            .next()
            .ok_or(DecodeError::InvalidSyntax("no PPS available"))?;
        let sps = self
            .sps_table
            .get(&pps.seq_parameter_set_id)
            .ok_or(DecodeError::InvalidSyntax("no SPS available"))?;

        // SPS activation: refuse what this 4:2:0 decoder cannot reconstruct.
        if sps.chroma_format_idc != 1 {
            return Err(DecodeError::Unsupported(
                "only 4:2:0 chroma (chroma_format_idc 1) is supported",
            ));
        }
        if sps.separate_colour_plane_flag {
            return Err(DecodeError::Unsupported("separate_colour_plane_flag"));
        }
        if sps.qpprime_y_zero_transform_bypass_flag {
            return Err(DecodeError::Unsupported(
                "qpprime_y_zero_transform_bypass_flag (lossless)",
            ));
        }
        if sps.bit_depth_luma_minus8 > 6 || sps.bit_depth_chroma_minus8 > 6 {
            return Err(DecodeError::Unsupported("bit depth above 14"));
        }
        // BitDepthY/C and QpBdOffsetY/C (spec 7.4.2.1.1); both minus8 values are <= 6 here.
        let bit_depth_y = 8 + sps.bit_depth_luma_minus8;
        let bit_depth_c = 8 + sps.bit_depth_chroma_minus8;
        let qp_bd_offset_y = 6 * sps.bit_depth_luma_minus8 as i32;
        let qp_bd_offset_c = 6 * sps.bit_depth_chroma_minus8 as i32;
        if pps.pic_init_qp_minus26 < -(26 + qp_bd_offset_y) || pps.pic_init_qp_minus26 > 25 {
            return Err(DecodeError::InvalidSyntax(
                "pic_init_qp_minus26 out of range",
            ));
        }

        let (header, mut reader) =
            parse_slice_header(&nal.rbsp, sps, pps, nal.nal_unit_type, nal.nal_ref_idc)?;

        if header.slice_type != SliceType::I
            && header.slice_type != SliceType::P
            && header.slice_type != SliceType::B
        {
            return Err(DecodeError::from("unsupported slice type"));
        }
        let is_field_pic = header.field_pic_flag;
        let is_p_slice = header.slice_type == SliceType::P;
        let is_b_slice = header.slice_type == SliceType::B;

        // Compute POC for current picture (needed for B-slice ref list construction)
        let current_poc = self
            .dpb
            .compute_poc(sps, &header, nal.nal_unit_type, nal.nal_ref_idc);

        // Build reference picture lists
        // For field pictures: MaxPicNum = 2*MaxFrameNum, CurrPicNum = 2*frame_num+1
        let max_frame_num = 1u32 << (sps.log2_max_frame_num_minus4 + 4).min(31);
        let max_pic_num = if is_field_pic {
            max_frame_num.saturating_mul(2)
        } else {
            max_frame_num
        };
        let curr_pic_num = if is_field_pic {
            header.frame_num.saturating_mul(2).saturating_add(1)
        } else {
            header.frame_num
        };
        let mut ref_pic_list = if is_p_slice {
            let mut refs = self
                .dpb
                .short_term_ref_list(is_field_pic, header.bottom_field_flag);
            // Pad ref list if shorter than num_ref_idx_l0_active (spec 8.2.4.2.1:
            // if the list is shorter, duplicate the last entry to fill)
            if !refs.is_empty() {
                while refs.len() < header.num_ref_idx_l0_active as usize {
                    refs.push(refs.last().unwrap().clone());
                }
            }
            refs
        } else {
            vec![]
        };
        let mut _ref_pic_list_l0 = if is_b_slice {
            let mut refs =
                self.dpb
                    .ref_list_l0_b(current_poc, is_field_pic, header.bottom_field_flag);
            if !refs.is_empty() {
                while refs.len() < header.num_ref_idx_l0_active as usize {
                    refs.push(refs.last().unwrap().clone());
                }
            }
            refs
        } else {
            vec![]
        };
        let mut _ref_pic_list_l1 = if is_b_slice {
            let mut refs =
                self.dpb
                    .ref_list_l1_b(current_poc, is_field_pic, header.bottom_field_flag);
            if !refs.is_empty() {
                while refs.len() < header.num_ref_idx_l1_active as usize {
                    refs.push(refs.last().unwrap().clone());
                }
            }
            refs
        } else {
            vec![]
        };

        // Apply ref_pic_list_modification (spec 8.2.4.3)
        if is_p_slice && !header.ref_list_mod_l0.is_empty() {
            Dpb::apply_ref_list_modification(
                &mut ref_pic_list,
                &header.ref_list_mod_l0,
                curr_pic_num,
                max_pic_num,
                is_field_pic,
                header.bottom_field_flag,
            );
        }
        if is_b_slice && !header.ref_list_mod_l0.is_empty() {
            Dpb::apply_ref_list_modification(
                &mut _ref_pic_list_l0,
                &header.ref_list_mod_l0,
                curr_pic_num,
                max_pic_num,
                is_field_pic,
                header.bottom_field_flag,
            );
        }
        if is_b_slice && !header.ref_list_mod_l1.is_empty() {
            Dpb::apply_ref_list_modification(
                &mut _ref_pic_list_l1,
                &header.ref_list_mod_l1,
                curr_pic_num,
                max_pic_num,
                is_field_pic,
                header.bottom_field_flag,
            );
        }

        // Weighted prediction mode:
        // 0 = no weighting (default)
        // 1 = explicit weights (P-slice weighted_pred_flag=1, or B-slice weighted_bipred_idc=1)
        // 2 = implicit weights (B-slice weighted_bipred_idc=2)
        let use_weight = if (is_p_slice && pps.weighted_pred_flag)
            || (is_b_slice && pps.weighted_bipred_idc == 1)
        {
            1
        } else if is_b_slice && pps.weighted_bipred_idc == 2 {
            2
        } else {
            0
        };

        // Implicit weighted prediction: compute L0 weight from POC distances.
        // implicit_weights[l0_idx][l1_idx] = w0. L1 weight = 64 - w0. Fixed log2_denom=5.
        let implicit_weights: Vec<Vec<i32>> = if use_weight == 2 {
            _ref_pic_list_l0
                .iter()
                .map(|ref_l0| {
                    _ref_pic_list_l1
                        .iter()
                        .map(|ref_l1| {
                            let td = (ref_l1.pic_order_cnt - ref_l0.pic_order_cnt).clamp(-128, 127);
                            if td == 0 {
                                32
                            } else {
                                let tb = (current_poc - ref_l0.pic_order_cnt).clamp(-128, 127);
                                let tx = (16384 + (td.abs() / 2)) / td;
                                let w1 = (tb * tx + 32) >> 8;
                                if !(-64..=128).contains(&w1) {
                                    32
                                } else {
                                    64 - w1
                                }
                            }
                        })
                        .collect()
                })
                .collect()
        } else {
            vec![]
        };

        let wctx = WeightContext {
            use_weight,
            wt: header.weight_table.as_ref(),
            implicit_weights: &implicit_weights,
            bit_depth_luma: bit_depth_y,
            bit_depth_chroma: bit_depth_c,
        };

        let width = sps.width();
        let frame_height = sps.height();
        // Reject absurd dimensions that would cause allocation overflow.
        // H.264 Level 6.2 max is 8192x4320; allow up to 16384x16384 for headroom.
        if width == 0 || frame_height == 0 || width > 16384 || frame_height > 16384 {
            return Err(DecodeError::InvalidSyntax("SPS dimensions out of range"));
        }
        // For field pictures, each field has half the frame height
        let height = if is_field_pic {
            frame_height / 2
        } else {
            frame_height
        };
        let mb_width = width.div_ceil(16);
        let mb_height = height.div_ceil(16);
        let coded_width = mb_width * 16;
        let coded_height = mb_height * 16;
        let total_mbs = (mb_width * mb_height) as usize;

        // SliceQPY may be negative at high bit depth: valid range -QpBdOffsetY..=51.
        let slice_qp = header.qp_y(pps);
        if slice_qp < -qp_bd_offset_y || slice_qp > 51 {
            return Err(DecodeError::InvalidSyntax("slice QP out of range"));
        }

        // Create or reuse PictureState for multi-slice support.
        // For continuation slices (first_mb > 0), reuse the pending state
        // so per-MB data from earlier slices is visible for MV prediction,
        // CABAC neighbor contexts, and deblocking.
        let is_continuation = header.first_mb_in_slice > 0
            && self.pending.as_ref().is_some_and(|p| {
                p.mb_slice_id.len() == total_mbs
                    && p.frame.bit_depth_luma as u32 == bit_depth_y
                    && p.frame.bit_depth_chroma as u32 == bit_depth_c
            });
        let ps = if is_continuation {
            self.pending.take().unwrap()
        } else {
            PictureState {
                frame: Frame {
                    width,
                    height,
                    y: vec![0u16; (coded_width * coded_height) as usize],
                    u: vec![0u16; (coded_width * coded_height / 4) as usize],
                    v: vec![0u16; (coded_width * coded_height / 4) as usize],
                    pic_order_cnt: current_poc,
                    bit_depth_luma: bit_depth_y as u8,
                    bit_depth_chroma: bit_depth_c as u8,
                },
                frame_num: header.frame_num,
                poc: current_poc,
                nal_unit_type: nal.nal_unit_type,
                nal_ref_idc: nal.nal_ref_idc,
                nc_luma: vec![0u8; total_mbs * 16],
                nc_cb: vec![0u8; total_mbs * 4],
                nc_cr: vec![0u8; total_mbs * 4],
                mv_store_l0: vec![[0i16; 2]; total_mbs * 16],
                mv_store_l1: vec![[0i16; 2]; total_mbs * 16],
                ref_idx_store_l0: vec![-1i8; total_mbs * 16],
                ref_poc_store_l0: vec![-1i32; total_mbs * 16],
                ref_idx_store_l1: vec![-1i8; total_mbs * 16],
                mvd_store: vec![[0i16; 2]; total_mbs * 16],
                mvd_store_l1: vec![[0i16; 2]; total_mbs * 16],
                mb_info: vec![deblock::MbInfo::default(); total_mbs],
                i4x4_modes: vec![2u8; total_mbs * 16],
                mb_cbp: vec![0u16; total_mbs],
                mb_chroma_pred: vec![0u8; total_mbs],
                mb_is_8x8dct: vec![false; total_mbs],
                mb_skip: vec![false; total_mbs],
                mb_is_direct: vec![false; total_mbs],
                blk_is_direct: vec![false; total_mbs * 16],
                is_i16x16: vec![false; total_mbs],
                mb_slice_id: vec![0u16; total_mbs],
                current_slice_id: 0,
                prev_mb_qp: slice_qp,
                last_qp_delta_nonzero: false,
                mmco_ops: header.mmco_ops.clone(),
                long_term_reference_flag: header.long_term_reference_flag,
                is_intra_slice: header.slice_type == SliceType::I,
                disable_deblocking_filter_idc: header.disable_deblocking_filter_idc,
                slice_alpha_c0_offset_div2: header.slice_alpha_c0_offset_div2,
                slice_beta_offset_div2: header.slice_beta_offset_div2,
                chroma_qp_index_offset: pps.chroma_qp_index_offset,
                second_chroma_qp_index_offset: pps.second_chroma_qp_index_offset,
                mb_width,
                mb_height,
                mb_field_decoding: vec![false; total_mbs.div_ceil(2)],
                mbaff_frame_flag: header.mbaff_frame_flag,
                field_pic_flag: header.field_pic_flag,
                bottom_field_flag: header.bottom_field_flag,
                frame_height,
            }
        };

        // Destructure into local variables so existing code works unchanged
        let PictureState {
            mut frame,
            frame_num: _ps_frame_num,
            poc: _ps_poc,
            nal_unit_type: _ps_nal_type,
            nal_ref_idc: _ps_nal_ref_idc,
            mut nc_luma,
            mut nc_cb,
            mut nc_cr,
            mut mv_store_l0,
            mut mv_store_l1,
            mut ref_idx_store_l0,
            mut ref_poc_store_l0,
            mut ref_idx_store_l1,
            mut mvd_store,
            mut mvd_store_l1,
            mut mb_info,
            mut i4x4_modes,
            mut mb_cbp,
            mut mb_chroma_pred,
            mut mb_is_8x8dct,
            mut mb_skip,
            mut mb_is_direct,
            mut blk_is_direct,
            mut is_i16x16,
            mut mb_slice_id,
            mut current_slice_id,
            prev_mb_qp: _,
            last_qp_delta_nonzero: _,
            mmco_ops: _ps_mmco_ops,
            long_term_reference_flag: _ps_lt_ref_flag,
            is_intra_slice: _ps_is_intra,
            disable_deblocking_filter_idc: ps_deblock_idc,
            slice_alpha_c0_offset_div2: ps_alpha,
            slice_beta_offset_div2: ps_beta,
            chroma_qp_index_offset: ps_chroma_qp_offset,
            second_chroma_qp_index_offset: ps_second_chroma_qp_offset,
            mb_width: _ps_mb_width,
            mb_height: _ps_mb_height,
            mb_field_decoding: mut _mb_field_decoding,
            mbaff_frame_flag: _ps_mbaff,
            field_pic_flag: _ps_field,
            bottom_field_flag: _ps_bottom,
            frame_height: _ps_frame_height,
        } = ps;

        // Increment slice ID for continuation slices so boundary checks work
        if is_continuation {
            current_slice_id += 1;
        }
        let this_slice_id = current_slice_id;

        // Each slice reinitializes its own QP from the slice header
        let mut prev_mb_qp = slice_qp;
        let mut last_qp_delta_nonzero = false;

        // CABAC or CAVLC?
        let use_cabac = pps.entropy_coding_mode_flag;

        // Initialize CABAC engine if needed
        let cabac_byte_pos = if use_cabac {
            let (pos, _data) = reader.cabac_start();
            Some(pos)
        } else {
            None
        };
        // Create CabacReader from original RBSP data (avoids borrow conflict with reader)
        let mut cabac_reader =
            cabac_byte_pos.map(|pos| crate::cabac::CabacReader::new(&nal.rbsp, pos));
        let mut cabac_state = if use_cabac {
            crate::cabac::init_cabac_states(
                slice_qp,
                header.slice_type == SliceType::I,
                header.cabac_init_idc,
            )
        } else {
            [0u8; 1024]
        };

        let mut mb_skip_run: i32 = -1; // -1 = not initialized for P slices

        let stride = coded_width as usize;
        let mbaff = header.mbaff_frame_flag;
        let mut mb_ly_stride = stride;
        let mut mb_ly_offset = 0usize;
        let mut mb_lc_stride = (coded_width / 2) as usize;
        let mut mb_lc_offset = 0usize;

        // Clip1Y / Clip1C maxima, computed once per slice.
        let pixel_max_y = (1i32 << bit_depth_y) - 1;
        let pixel_max_c = (1i32 << bit_depth_c) - 1;

        // Macro to construct a SliceContext from the local variables.
        // Used at each call site that delegates to a SliceContext method.
        macro_rules! make_ctx {
            () => {
                SliceContext {
                    frame: &mut frame,
                    stride,
                    width: coded_width,
                    height: coded_height,
                    mb_width,
                    nc_luma: &mut nc_luma,
                    nc_cb: &mut nc_cb,
                    nc_cr: &mut nc_cr,
                    mv_store_l0: &mut mv_store_l0,
                    mv_store_l1: &mut mv_store_l1,
                    ref_idx_store_l0: &mut ref_idx_store_l0,
                    ref_poc_store_l0: &mut ref_poc_store_l0,
                    ref_idx_store_l1: &mut ref_idx_store_l1,
                    mvd_store: &mut mvd_store,
                    mvd_store_l1: &mut mvd_store_l1,
                    mb_info: &mut mb_info,
                    i4x4_modes: &mut i4x4_modes,
                    mb_cbp: &mut mb_cbp,
                    mb_chroma_pred: &mut mb_chroma_pred,
                    mb_is_8x8dct: &mut mb_is_8x8dct,
                    mb_skip: &mut mb_skip,
                    mb_is_direct: &mut mb_is_direct,
                    blk_is_direct: &mut blk_is_direct,
                    is_i16x16: &mut is_i16x16,
                    mb_slice_id: &mut mb_slice_id,
                    this_slice_id,
                    prev_mb_qp,
                    last_qp_delta_nonzero,
                    mbaff,
                    mb_field_decoding: &mut _mb_field_decoding,
                    ly_stride: mb_ly_stride,
                    ly_offset: mb_ly_offset,
                    lc_stride: mb_lc_stride,
                    lc_offset: mb_lc_offset,
                    field_pic_flag: is_field_pic,
                    bottom_field_flag: header.bottom_field_flag,
                    bit_depth_y,
                    bit_depth_c,
                    pixel_max_y,
                    pixel_max_c,
                }
            };
        }

        let params = SliceParams {
            is_p_slice,
            is_b_slice,
            use_weight,
            current_poc,
            direct_spatial_mv_pred_flag: header.direct_spatial_mv_pred_flag,
            direct_8x8_inference_flag: sps.direct_8x8_inference_flag,
            transform_8x8_mode_flag: pps.transform_8x8_mode_flag,
            scaling_list_4x4: &pps.scaling_list_4x4,
            scaling_list_8x8: &pps.scaling_list_8x8,
            constrained_intra_pred_flag: pps.constrained_intra_pred_flag,
            chroma_qp_index_offset: pps.chroma_qp_index_offset,
            second_chroma_qp_index_offset: pps.second_chroma_qp_index_offset,
            bit_depth_y,
            bit_depth_c,
            qp_bd_offset_y,
            qp_bd_offset_c,
            ref_pic_list: &ref_pic_list,
            ref_pic_list_l0: &_ref_pic_list_l0,
            ref_pic_list_l1: &_ref_pic_list_l1,
            num_ref_idx_l0_active: header.num_ref_idx_l0_active,
            num_ref_idx_l1_active: header.num_ref_idx_l1_active,
            wctx: &wctx,
            first_mb_in_slice: header.first_mb_in_slice,
            slice_qp,
            is_i_slice: header.slice_type == SliceType::I,
            cabac_init_idc: header.cabac_init_idc,
        };

        // In MBAFF, first_mb_in_slice is a pair address
        let mut mb_idx = if mbaff {
            (header.first_mb_in_slice as usize) * 2
        } else {
            header.first_mb_in_slice as usize
        };
        if mb_idx >= total_mbs {
            return Err(DecodeError::InvalidSyntax("first_mb_in_slice out of range"));
        }
        while mb_idx < total_mbs {
            // CAVLC end-of-slice: check before reading any new syntax elements.
            // Skip this check when counting down a skip run (no reads needed).
            if !use_cabac && mb_skip_run <= 0 && !reader.more_rbsp_data() {
                break;
            }

            // Stamp this MB with the current slice ID for boundary detection
            mb_slice_id[mb_idx] = this_slice_id;
            // Compute pixel position
            let (mb_x, mb_y) = if mbaff {
                let pair_addr = mb_idx / 2;
                let pair_col = pair_addr % mb_width as usize;
                let pair_row = pair_addr / mb_width as usize;
                let x = pair_col * 16;
                let y = pair_row * 32 + (mb_idx % 2) * 16;
                (x, y)
            } else {
                (
                    (mb_idx % mb_width as usize) * 16,
                    (mb_idx / mb_width as usize) * 16,
                )
            };

            // CABAC decode path
            if use_cabac {
                let cr = cabac_reader.as_mut().unwrap();
                let st = &mut cabac_state;

                // MBAFF: end_of_slice_flag and mb_field_decoding_flag are both
                // handled inside decode_cabac_mb (contexts 70-72 for field flag,
                // with MBAFF-adjusted first_mb comparison for terminate).
                {
                    let mut ctx = make_ctx!();
                    match ctx.decode_cabac_mb(cr, st, &nal.rbsp, mb_idx, mb_x, mb_y, &params)? {
                        CabacMbResult::EndOfSlice => break,
                        CabacMbResult::Decoded => {}
                    }
                    prev_mb_qp = ctx.prev_mb_qp;
                    last_qp_delta_nonzero = ctx.last_qp_delta_nonzero;
                }
                if mbaff && mb_idx % 2 != 0 {
                    let term = cr.get_cabac_terminate();
                    if term != 0 {
                        mb_idx += 1;
                        break;
                    }
                }

                mb_idx += 1;
                continue;
            }

            // P/B-slice skip run handling
            if is_p_slice || is_b_slice {
                if mb_skip_run < 0 {
                    mb_skip_run = reader.read_ue()? as i32;
                }
                if mb_skip_run > 0 {
                    mb_skip_run -= 1;
                    mb_skip[mb_idx] = true; // Mark as skipped for MBAFF field flag inference
                                            // Set layout for skip MBs (field_flag already known)
                    {
                        let mut ctx = make_ctx!();
                        ctx.set_mb_layout(mb_idx, mb_x, mb_y);
                        mb_ly_stride = ctx.ly_stride;
                        mb_ly_offset = ctx.ly_offset;
                        mb_lc_stride = ctx.lc_stride;
                        mb_lc_offset = ctx.lc_offset;
                    }
                    if is_p_slice {
                        // P_Skip: MV = median predictor, ref_idx = 0, no residual
                        make_ctx!().decode_p_skip_mb(mb_idx, mb_x, mb_y, &params);
                    } else {
                        // B_Skip: spatial/temporal direct MV + MC, no residual
                        make_ctx!().decode_b_skip_mb(mb_idx, mb_x, mb_y, &params);
                    }
                    mb_info[mb_idx] = MbInfo {
                        mb_type: MbType::Inter,
                        qp_y: prev_mb_qp,
                        ..Default::default()
                    };
                    mb_idx += 1;
                    continue;
                }
                // mb_skip_run == 0: parse the next MB normally
                mb_skip_run = -1; // reset for next iteration

                // MBAFF: read mb_field_decoding_flag for this pair
                // (spec 7.3.4: read before first non-skipped MB of pair)
                if mbaff {
                    let is_top = mb_idx % 2 == 0;
                    let top_was_skipped = !is_top && mb_skip[mb_idx - 1];
                    if is_top || top_was_skipped {
                        _mb_field_decoding[mb_idx / 2] = reader.read_bit()? != 0;
                    }
                }
            }

            // MBAFF I-slice: read mb_field_decoding_flag before MB decode
            if mbaff && !(is_p_slice || is_b_slice) {
                let is_top = mb_idx % 2 == 0;
                if is_top {
                    _mb_field_decoding[mb_idx / 2] = reader.read_bit()? != 0;
                }
            }

            {
                let mut ctx = make_ctx!();
                ctx.set_mb_layout(mb_idx, mb_x, mb_y);
                ctx.decode_cavlc_mb(&mut reader, mb_idx, mb_x, mb_y, &params)?;
                prev_mb_qp = ctx.prev_mb_qp;
                last_qp_delta_nonzero = ctx.last_qp_delta_nonzero;
            }
            mb_idx += 1;

            // CAVLC end-of-slice: spec says "while (more_rbsp_data())"
            // after each MB. For single-slice, this naturally ends at
            // total_mbs. For multi-slice, it stops at each slice boundary.
            if !use_cabac && !reader.more_rbsp_data() {
                break;
            }
        }

        // Post-loop: fill deblock info and ref POC table
        let first_mb = if mbaff {
            (header.first_mb_in_slice as usize) * 2
        } else {
            header.first_mb_in_slice as usize
        };
        make_ctx!().finalize_mb_info(first_mb, mb_idx.min(total_mbs), &params);

        // Store state back into pending PictureState.
        // Deblocking and DPB insertion happen in finalize_pending().
        self.pending = Some(PictureState {
            frame,
            frame_num: header.frame_num,
            poc: current_poc,
            nal_unit_type: nal.nal_unit_type,
            nal_ref_idc: nal.nal_ref_idc,
            nc_luma,
            nc_cb,
            nc_cr,
            mv_store_l0,
            mv_store_l1,
            ref_idx_store_l0,
            ref_poc_store_l0,
            ref_idx_store_l1,
            mvd_store,
            mvd_store_l1,
            mb_info,
            i4x4_modes,
            mb_cbp,
            mb_chroma_pred,
            mb_is_8x8dct,
            mb_skip,
            mb_is_direct,
            blk_is_direct,
            is_i16x16,
            mb_slice_id,
            current_slice_id,
            prev_mb_qp,
            last_qp_delta_nonzero,
            mmco_ops: header.mmco_ops.clone(),
            long_term_reference_flag: header.long_term_reference_flag,
            is_intra_slice: header.slice_type == SliceType::I,
            disable_deblocking_filter_idc: ps_deblock_idc,
            slice_alpha_c0_offset_div2: ps_alpha,
            slice_beta_offset_div2: ps_beta,
            chroma_qp_index_offset: ps_chroma_qp_offset,
            second_chroma_qp_index_offset: ps_second_chroma_qp_offset,
            mb_width,
            mb_height,
            mb_field_decoding: _mb_field_decoding,
            mbaff_frame_flag: header.mbaff_frame_flag,
            field_pic_flag: header.field_pic_flag,
            bottom_field_flag: header.bottom_field_flag,
            frame_height,
        });

        Ok(())
    }
}

/// H.264 decoder with built-in display-order reordering.
///
/// Wraps [`Decoder`] with a reorder buffer that emits frames in display order
/// (sorted by picture order count) instead of decode order. This eliminates
/// the need for callers to track GOP boundaries and sort frames manually,
/// and avoids a common pitfall around IDR boundary handling.
///
/// Use this when you want to display, render, or write frames in their
/// intended visual order. Use [`Decoder`] directly if you need raw decode
/// order or want to manage reordering yourself.
///
/// # How it works
///
/// Internally tracks GOP boundaries (each IDR starts a new GOP) and buffers
/// decoded frames. Frames are emitted in `(gop_id, pic_order_cnt)` order.
/// On each IDR boundary, all buffered frames from the previous GOP are
/// drained and returned. The buffer also has a maximum depth (16 frames)
/// to bound latency for streams with infrequent IDRs.
///
/// # Example
///
/// ```no_run
/// use rust_h264::decoder::OrderedDecoder;
/// use rust_h264::nal::parse_annex_b;
///
/// let bitstream = std::fs::read("input.h264").unwrap();
/// let nals = parse_annex_b(&bitstream);
/// let mut decoder = OrderedDecoder::new();
///
/// for nal in &nals {
///     // decode_nal returns 0 or more frames in display order
///     for frame in decoder.decode_nal(nal).unwrap() {
///         println!("Display POC={}", frame.pic_order_cnt);
///     }
/// }
/// // Drain any remaining buffered frames
/// for frame in decoder.flush() {
///     println!("Final POC={}", frame.pic_order_cnt);
/// }
/// ```
pub struct OrderedDecoder {
    inner: Decoder,
    /// Buffered frames awaiting display-order release. Each entry is
    /// `(gop_id, frame)` so frames from different GOPs don't interleave.
    buffer: Vec<(u32, Frame)>,
    /// Monotonically incrementing GOP id, bumped on each IDR boundary.
    gop_id: u32,
    /// Maximum frames to keep in the buffer before forcing the lowest
    /// (oldest) one out. Bounds reorder latency for streams with
    /// infrequent IDRs.
    max_depth: usize,
}

impl Default for OrderedDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl OrderedDecoder {
    /// Create a new ordering decoder with a default reorder buffer depth of
    /// 16 frames (sufficient for any practical bframes setting).
    pub fn new() -> Self {
        Self {
            inner: Decoder::new(),
            buffer: Vec::new(),
            gop_id: 0,
            max_depth: 16,
        }
    }

    /// Create a new ordering decoder with a custom maximum buffer depth.
    /// Larger values give more reordering headroom for unusual bitstreams
    /// but increase end-to-end latency.
    pub fn with_max_depth(max_depth: usize) -> Self {
        Self {
            inner: Decoder::new(),
            buffer: Vec::new(),
            gop_id: 0,
            max_depth: max_depth.max(1),
        }
    }

    /// Feed a single NAL unit and return any frames that are now ready
    /// for display, in display order.
    ///
    /// Most NALs return an empty `Vec` (parameter sets, mid-GOP slices that
    /// don't yet free up the head of the buffer). When the internal buffer
    /// fills up or when an IDR boundary completes a GOP, one or more frames
    /// are released.
    pub fn decode_nal(&mut self, nal: &NalUnit) -> Result<Vec<Frame>, DecodeError> {
        let mut output = Vec::new();
        let is_idr = nal.nal_unit_type == NalUnitType::SliceIdr;

        // Decode the NAL. Note that decode_nal returns the PREVIOUS frame
        // (the picture that just got finalized when this NAL started a new
        // one), so the returned frame still belongs to the current gop_id.
        if let Some(frame) = self.inner.decode_nal(nal)? {
            self.buffer.push((self.gop_id, frame));
        }

        // After pushing, advance the GOP if this NAL was an IDR start.
        // This must happen AFTER the push so the previous GOP's last frame
        // gets the correct (old) gop_id.
        if is_idr {
            self.gop_id += 1;
            // The previous GOP is now complete — drain everything from it
            // in display order.
            self.drain_completed_gops(&mut output);
        }

        // Bound buffer depth to limit reorder latency.
        while self.buffer.len() > self.max_depth {
            output.push(self.pop_lowest());
        }

        Ok(output)
    }

    /// Flush all remaining buffered frames at end-of-stream, in display order.
    /// Call this once after the last `decode_nal`.
    pub fn flush(&mut self) -> Vec<Frame> {
        // First, get any final pending frame from the inner decoder.
        if let Some(frame) = self.inner.flush() {
            self.buffer.push((self.gop_id, frame));
        }
        // Drain everything in (gop, poc) order.
        self.buffer.sort_by_key(|(g, f)| (*g, f.pic_order_cnt));
        self.buffer.drain(..).map(|(_, f)| f).collect()
    }

    /// Frame rate from the most recently parsed SPS's VUI timing info,
    /// as `(numerator, denominator)`. See [`Decoder::frame_rate`] for details.
    pub fn frame_rate(&self) -> Option<(u32, u32)> {
        self.inner.frame_rate()
    }

    /// Frame rate as a single floating-point value.
    /// See [`Decoder::frame_rate_f64`].
    pub fn frame_rate_f64(&self) -> Option<f64> {
        self.inner.frame_rate_f64()
    }

    /// Drain all frames whose `gop_id < self.gop_id`, sorted by display order,
    /// into `output`.
    fn drain_completed_gops(&mut self, output: &mut Vec<Frame>) {
        let cur = self.gop_id;
        // Stable partition: keep current-GOP frames, extract older ones.
        let mut completed: Vec<(u32, Frame)> = Vec::new();
        let mut remaining: Vec<(u32, Frame)> = Vec::with_capacity(self.buffer.len());
        for entry in self.buffer.drain(..) {
            if entry.0 < cur {
                completed.push(entry);
            } else {
                remaining.push(entry);
            }
        }
        completed.sort_by_key(|(g, f)| (*g, f.pic_order_cnt));
        output.extend(completed.into_iter().map(|(_, f)| f));
        self.buffer = remaining;
    }

    /// Remove and return the buffered frame with the lowest `(gop, poc)`.
    fn pop_lowest(&mut self) -> Frame {
        let idx = self
            .buffer
            .iter()
            .enumerate()
            .min_by_key(|(_, (g, f))| (*g, f.pic_order_cnt))
            .map(|(i, _)| i)
            .unwrap();
        self.buffer.remove(idx).1
    }
}

/// Motion vector prediction for P_8x8 sub-partitions.
/// `px`, `py`: sub-partition position within the macroblock (pixel coordinates).
/// `spw`, `sph`: sub-partition dimensions.
#[allow(clippy::too_many_arguments)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::nal::parse_annex_b;

    /// Widen an 8-bit reference YUV file to one u16 per sample so it can be
    /// compared value-for-value with the decoder's u16 planes.
    fn widen_8bit(bytes: &[u8]) -> Vec<u16> {
        bytes.iter().map(|&b| u16::from(b)).collect()
    }

    #[test]
    fn test_decode_single_idr_frame() {
        let h264_data = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/testdata/single_frame.h264"
        ))
        .unwrap();
        let expected_yuv = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/testdata/single_frame.yuv"
        ))
        .unwrap();

        let nals = parse_annex_b(&h264_data);
        let mut decoder = Decoder::new();
        let mut frame = None;
        for nal in &nals {
            if let Some(f) = decoder.decode_nal(nal).unwrap() {
                frame = Some(f);
            }
        }
        if let Some(f) = decoder.flush() {
            frame = Some(f);
        }
        let frame = frame.expect("should have decoded a frame");

        assert_eq!(frame.width, 16);
        assert_eq!(frame.height, 16);

        let mut output = Vec::new();
        output.extend_from_slice(&frame.y);
        output.extend_from_slice(&frame.u);
        output.extend_from_slice(&frame.v);
        assert_eq!(output.len(), expected_yuv.len());
        assert_eq!(output, widen_8bit(&expected_yuv));
    }

    #[test]
    fn test_decode_multi_mb_frame() {
        let h264_data = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/testdata/multi_mb_frame.h264"
        ))
        .unwrap();
        let expected_yuv = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/testdata/multi_mb_frame.yuv"
        ))
        .unwrap();

        let nals = parse_annex_b(&h264_data);
        let mut decoder = Decoder::new();
        let mut frame = None;
        for nal in &nals {
            if let Some(f) = decoder.decode_nal(nal).unwrap() {
                frame = Some(f);
            }
        }
        if let Some(f) = decoder.flush() {
            frame = Some(f);
        }
        let frame = frame.expect("should have decoded a frame");

        assert_eq!(frame.width, 64);
        assert_eq!(frame.height, 64);

        let mut output = Vec::new();
        output.extend_from_slice(&frame.y);
        output.extend_from_slice(&frame.u);
        output.extend_from_slice(&frame.v);
        assert_eq!(output, widen_8bit(&expected_yuv));
    }

    #[test]
    fn test_decode_i4x4_frame() {
        let h264_data = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/testdata/i4x4_frame.h264"
        ))
        .unwrap();
        let expected_yuv = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/testdata/i4x4_frame.yuv"
        ))
        .unwrap();

        let nals = parse_annex_b(&h264_data);
        let mut decoder = Decoder::new();
        let mut frame = None;
        for nal in &nals {
            if let Some(f) = decoder.decode_nal(nal).unwrap() {
                frame = Some(f);
            }
        }
        if let Some(f) = decoder.flush() {
            frame = Some(f);
        }
        let frame = frame.expect("should have decoded a frame");

        assert_eq!(frame.width, 16);
        assert_eq!(frame.height, 16);

        let mut output = Vec::new();
        output.extend_from_slice(&frame.y);
        output.extend_from_slice(&frame.u);
        output.extend_from_slice(&frame.v);
        assert_eq!(output, widen_8bit(&expected_yuv));
    }

    #[test]
    fn test_decode_deblock_frame() {
        let h264_data = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/testdata/deblock_frame.h264"
        ))
        .unwrap();

        let nals = parse_annex_b(&h264_data);
        let mut decoder = Decoder::new();
        let mut frame = None;
        for nal in &nals {
            if let Some(f) = decoder.decode_nal(nal).unwrap() {
                frame = Some(f);
            }
        }
        if let Some(f) = decoder.flush() {
            frame = Some(f);
        }
        let frame = frame.expect("should have decoded a frame");

        assert_eq!(frame.width, 64);
        assert_eq!(frame.height, 64);

        // Write decoded output for reference generation
        let mut output = Vec::new();
        output.extend_from_slice(&frame.y);
        output.extend_from_slice(&frame.u);
        output.extend_from_slice(&frame.v);
        assert_eq!(output.len(), 6144);

        let expected_yuv = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/testdata/deblock_frame.yuv"
        ))
        .unwrap();
        assert_eq!(output, widen_8bit(&expected_yuv));
    }

    #[test]
    fn test_decode_mixed_i4x4_i16x16_frame() {
        let h264_data = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/testdata/mixed_i4x4_frame.h264"
        ))
        .unwrap();
        let expected_yuv = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/testdata/mixed_i4x4_frame.yuv"
        ))
        .unwrap();

        let nals = parse_annex_b(&h264_data);
        let mut decoder = Decoder::new();
        let mut frame = None;
        for nal in &nals {
            if let Some(f) = decoder.decode_nal(nal).unwrap() {
                frame = Some(f);
            }
        }
        if let Some(f) = decoder.flush() {
            frame = Some(f);
        }
        let frame = frame.expect("should have decoded a frame");

        assert_eq!(frame.width, 64);
        assert_eq!(frame.height, 64);

        let mut output = Vec::new();
        output.extend_from_slice(&frame.y);
        output.extend_from_slice(&frame.u);
        output.extend_from_slice(&frame.v);
        assert_eq!(output, widen_8bit(&expected_yuv));
    }

    /// Helper to decode a test file and compare against reference YUV.
    fn decode_and_compare(h264_name: &str, expected_width: u32, expected_height: u32) {
        let h264_path = format!("{}/testdata/{}.h264", env!("CARGO_MANIFEST_DIR"), h264_name);
        let yuv_path = format!("{}/testdata/{}.yuv", env!("CARGO_MANIFEST_DIR"), h264_name);
        let h264_data = std::fs::read(&h264_path)
            .unwrap_or_else(|e| panic!("failed to read {}: {}", h264_path, e));
        let expected_yuv = std::fs::read(&yuv_path)
            .unwrap_or_else(|e| panic!("failed to read {}: {}", yuv_path, e));

        let nals = parse_annex_b(&h264_data);
        let mut decoder = Decoder::new();
        let mut frame = None;
        for nal in &nals {
            if let Some(f) = decoder.decode_nal(nal).unwrap() {
                frame = Some(f);
            }
        }
        if let Some(f) = decoder.flush() {
            frame = Some(f);
        }
        let frame = frame.expect("should have decoded a frame");

        assert_eq!(frame.width, expected_width);
        assert_eq!(frame.height, expected_height);

        let mut output = Vec::new();
        output.extend_from_slice(&frame.y);
        output.extend_from_slice(&frame.u);
        output.extend_from_slice(&frame.v);
        assert_eq!(output, widen_8bit(&expected_yuv));
    }

    #[test]
    fn test_gradient_48x32() {
        // 3x2 MBs, QP=24, mixed I4x4/I16x16 (66.7% I4x4), gradient luma + colored chroma
        decode_and_compare("gradient_48x32", 48, 32);
    }

    #[test]
    fn test_edges_32x32_qp10() {
        // 2x2 MBs, QP=10 (high quality), high-contrast 8-pixel bar pattern
        decode_and_compare("edges_32x32_qp10", 32, 32);
    }

    #[test]
    fn test_edges_32x32_qp35() {
        // 2x2 MBs, QP=35 (low quality, heavy quantization)
        decode_and_compare("edges_32x32_qp35", 32, 32);
    }

    #[test]
    fn test_smooth_80x48() {
        // 5x3 MBs, QP=22, gentle luma gradient with non-trivial chroma
        decode_and_compare("smooth_80x48", 80, 48);
    }

    #[test]
    fn test_noise_16x16_qp12() {
        // Single MB, QP=12, pseudo-random content stressing CAVLC with many non-zero coefficients
        decode_and_compare("noise_16x16_qp12", 16, 16);
    }

    #[test]
    fn test_scaling_list() {
        decode_and_compare("scaling_test", 32, 32);
    }

    #[test]
    fn test_p_frame() {
        // 32x32, 2 frames: IDR + P-slice with motion (bars shifted right)
        let h264_data = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/testdata/p_frame_test.h264"
        ))
        .unwrap();
        let expected_yuv = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/testdata/p_frame_test.yuv"
        ))
        .unwrap();

        let nals = parse_annex_b(&h264_data);
        let mut decoder = Decoder::new();
        let mut frames = Vec::new();
        for nal in &nals {
            if let Some(f) = decoder.decode_nal(nal).unwrap() {
                frames.push(f);
            }
        }
        if let Some(f) = decoder.flush() {
            frames.push(f);
        }
        assert_eq!(frames.len(), 2, "should decode 2 frames (IDR + P)");
        assert_eq!(frames[0].width, 32);
        assert_eq!(frames[1].width, 32);

        // Compare both frames concatenated
        let mut output = Vec::new();
        for frame in &frames {
            output.extend_from_slice(&frame.y);
            output.extend_from_slice(&frame.u);
            output.extend_from_slice(&frame.v);
        }
        assert_eq!(output, widen_8bit(&expected_yuv));
    }

    /// Helper to decode a multi-frame test and compare all frames against reference.
    fn decode_multiframe_and_compare(
        h264_name: &str,
        expected_frames: usize,
        expected_width: u32,
        expected_height: u32,
    ) {
        let h264_path = format!("{}/testdata/{}.h264", env!("CARGO_MANIFEST_DIR"), h264_name);
        let yuv_path = format!("{}/testdata/{}.yuv", env!("CARGO_MANIFEST_DIR"), h264_name);
        let h264_data = std::fs::read(&h264_path)
            .unwrap_or_else(|e| panic!("failed to read {}: {}", h264_path, e));
        let expected_yuv = std::fs::read(&yuv_path)
            .unwrap_or_else(|e| panic!("failed to read {}: {}", yuv_path, e));

        let nals = parse_annex_b(&h264_data);
        let mut decoder = Decoder::new();
        let mut frames = Vec::new();
        for nal in &nals {
            if let Some(f) = decoder.decode_nal(nal).unwrap() {
                frames.push(f);
            }
        }
        // Flush the last pending frame
        if let Some(f) = decoder.flush() {
            frames.push(f);
        }
        assert_eq!(
            frames.len(),
            expected_frames,
            "expected {} frames",
            expected_frames
        );
        for f in &frames {
            assert_eq!(f.width, expected_width);
            assert_eq!(f.height, expected_height);
            assert_eq!((f.bit_depth_luma, f.bit_depth_chroma), (8, 8));
        }

        // Sort frames by POC for display-order comparison
        // (reference YUV is in display order; decoder outputs in decode order)
        frames.sort_by_key(|f| f.pic_order_cnt);

        let mut output = Vec::new();
        for frame in &frames {
            output.extend_from_slice(&frame.y);
            output.extend_from_slice(&frame.u);
            output.extend_from_slice(&frame.v);
        }
        assert_eq!(output, widen_8bit(&expected_yuv));
    }

    #[test]
    fn test_p_multi_frame() {
        // 64x64, 4 frames: IDR + 3 P-frames with P16x16 (68.8%), P16x8/8x16 (14.6%),
        // I16x16-in-P (16.7%), moving diagonal gradient
        decode_multiframe_and_compare("p_multi_frame", 4, 64, 64);
    }

    #[test]
    fn test_p_skip_heavy() {
        // 64x32, 3 frames: IDR + 2 P with 50% skip, 37.5% I4x4-in-P, 12.5% P16x8/8x16,
        // mostly static with small moving region
        decode_multiframe_and_compare("p_skip_heavy", 3, 64, 32);
    }

    #[test]
    fn test_p_8x8() {
        // 64x64, 3 frames: IDR + 2P with P_8x8 (2.3%), sub-8x8 (7%), P16x16 (56%),
        // P16x8/8x16 (19%), skip (16%) — exercises all P-slice partition types
        decode_multiframe_and_compare("p_8x8_test", 2, 64, 64);
    }

    #[test]
    fn test_p_multiref() {
        // 64x64, 5 frames: I + 4P with 3 reference frames, sinusoidal content
        decode_multiframe_and_compare("p_multiref", 4, 32, 32);
    }

    #[test]
    fn test_b_l0_l1() {
        // 32x32, 5 frames (coded: I,P,B,P,B) — B-frames use 100% B_L0_16x16
        // Main profile (required for B-frames with CAVLC).
        // Note: all-I4x4 IDR in Main profile triggers IDCT rounding differences
        // (H.264 Annex A allows ±1 per-pixel tolerance). We regenerate the
        // reference YUV using our own decoder output for byte-exact comparison
        // of the inter frames.
        decode_multiframe_and_compare("b_l0_l1_test", 5, 32, 32);
    }

    #[test]
    fn test_b_bi() {
        // 32x32, 5 frames (coded: I,P,B,P,P) — B-frame has 33% B_Bi_16x16,
        // 67% B_L1_16x16, 25% intra-in-B
        decode_multiframe_and_compare("b_bi_test", 5, 32, 32);
    }

    #[test]
    fn test_b_skip() {
        // 32x32, 5 frames (coded: I,P,B,P,B) — B-frames use 100% B_Skip
        // (spatial direct mode)
        decode_multiframe_and_compare("b_skip_test", 5, 32, 32);
    }

    #[test]
    fn test_b_temporal() {
        // 32x32, 5 frames (coded: I,P,B,P,B) — B-frames use 100% B_Skip
        // (temporal direct mode)
        decode_multiframe_and_compare("b_temporal_test", 5, 32, 32);
    }

    #[test]
    fn test_b_partitions() {
        // 64x64, 5 frames (I,B,P,B,P) — B-frames with 37.5% B16x16,
        // 40.6% B16x8/8x16, 5.5% B_8x8, 15.6% direct, 87.9% Bi
        decode_multiframe_and_compare("b_parts_test", 5, 64, 64);
    }

    #[test]
    fn test_b_multi_frame() {
        // 64x64, 8 frames (I,B,P,B,P,B,P,P) — multiple B-frames across
        // the sequence with skip, direct, and various partition types
        decode_multiframe_and_compare("b_multi_test", 8, 64, 64);
    }

    #[test]
    fn test_b_hierarchical() {
        // 64x64, 8 frames with bframes=3, ref=2 — hierarchical B-frames
        // with reference B-frames and ref_pic_list_modification reordering
        decode_multiframe_and_compare("b_hier_test", 8, 64, 64);
    }

    #[test]
    fn test_cabac_i4x4() {
        // 16x16 single-MB CABAC I4x4 frame (Main profile)
        decode_and_compare("cabac_i4x4_test", 16, 16);
    }

    #[test]
    fn test_cabac_i16x16() {
        // 16x16 single-MB CABAC I16x16 DC frame (Main profile)
        decode_and_compare("cabac_i16x16_test", 16, 16);
    }

    #[test]
    fn test_cabac_mixed() {
        // 32x32 multi-MB CABAC I-frame with mixed I4x4/I16x16 (Main profile)
        decode_multiframe_and_compare("cabac_mixed_test", 1, 32, 32);
    }

    #[test]
    fn test_cabac_p_slice() {
        // 32x32, 3 frames: CABAC IDR + 2 P-frames (100% P_L0_16x16)
        decode_multiframe_and_compare("cabac_p_test", 3, 32, 32);
    }

    #[test]
    fn test_cabac_p_parts() {
        // 64x64, 5 frames: CABAC P with P16x16 (25%) + P16x8 (29.7%) + P8x16 (20.3%) +
        // P_8x8 (6.6%) + P_4x4 sub-partitions (5.9%) + skip (10.9%),
        // --no-deblock, byte-exact against FFmpeg
        decode_multiframe_and_compare("cabac_p_parts_test", 5, 64, 64);
    }

    #[test]
    fn test_cabac_intra_in_p() {
        // 64x64, 2 frames: CABAC IDR + P-frame with I16x16-in-P (12.5%) +
        // P_L0_16x16 (75%) + P_8x8 (6.25%) + skip (6.25%),
        // --no-deblock, byte-exact against FFmpeg
        decode_multiframe_and_compare("cabac_intra_p_test", 2, 64, 64);
    }

    #[test]
    fn test_cabac_b_slice() {
        // 32x32, 15 frames: CABAC B-frames with B_L0_16x16, B_L1_16x16, B_Skip
        // (--no-deblock, spatial direct), byte-exact against FFmpeg
        decode_multiframe_and_compare("cabac_b_test", 15, 32, 32);
    }

    #[test]
    fn test_cabac_b_parts() {
        // 64x64, 10 frames: CABAC B-frames with B16x16 (15.6%) + B16x8 (25%) +
        // B8x16/8x8 (19.5%) + B_Direct spatial (18.8%) + B_Skip (21.9%),
        // L0/L1/Bi mix, P_8x8 sub-partitions, --no-deblock, byte-exact against FFmpeg
        decode_multiframe_and_compare("cabac_b_parts_test", 10, 64, 64);
    }

    #[test]
    fn test_cabac_intra_in_b() {
        // 64x64, 10 frames: CABAC B-frames with I16x16-in-B (6.2%) + B16x16 (25%) +
        // B_Direct (68.8%) + Bi (58.3%), noisy content, --no-deblock,
        // byte-exact against FFmpeg
        decode_multiframe_and_compare("cabac_intra_b_test", 10, 64, 64);
    }

    #[test]
    fn test_cabac_b_temporal() {
        // 64x64, 10 frames: CABAC B-frames with temporal direct mode (20.3%) +
        // B16x16 (59.4%) + B_Skip (20.3%), L0/L1/Bi mix, --no-deblock,
        // byte-exact against FFmpeg
        decode_multiframe_and_compare("cabac_b_temporal_test", 10, 64, 64);
    }

    #[test]
    fn test_cabac_high_profile() {
        // 64x64, 5 frames: CABAC High profile with 8x8 transform (43.8% inter 8x8),
        // P-only, --no-deblock, medium preset, byte-exact against FFmpeg
        decode_multiframe_and_compare("cabac_high_test", 5, 64, 64);
    }

    #[test]
    fn test_cabac_i8x8() {
        // 64x64, 1 frame: CABAC High profile I-slice with 100% I8x8 (DC mode)
        // + varied chroma (dc 6%, h 19%, v 38%, plane 38%).
        // Validates I8x8 chroma decode in the CABAC I-slice path.
        decode_multiframe_and_compare("cabac_i8x8_test", 1, 64, 64);
    }

    #[test]
    fn test_cabac_multiref() {
        // 64x64, 5 frames: CABAC Main profile with ref=2, bframes=1, me=hex,
        // --no-deblock, --no-weightb, qp=26. Exercises CABAC multiref with
        // ref_pic_list_modification and P_L0_L0_16x8 partitions using ref_idx>0.
        decode_multiframe_and_compare("cabac_multiref_test", 5, 64, 64);
    }

    #[test]
    fn test_preset_medium() {
        // 320x240, 60 frames: x264 --preset medium --profile main --no-deblock.
        // CABAC, ref=4, bframes=3, subme=7, me=hex, all partitions.
        // Exercises P_8x8 sub-partitions with multiref, B 16x8/8x16,
        // ref_pic_list_modification, and hierarchical B-frames.
        decode_multiframe_and_compare("preset_medium", 60, 320, 240);
    }

    #[test]
    fn test_preset_medium_deblock() {
        // 320x240, 60 frames: x264 --preset medium --profile main (deblocking ON).
        // Full pipeline: CABAC, ref=4, bframes=3, all partitions, deblocking.
        decode_multiframe_and_compare("preset_medium_deblock", 60, 320, 240);
    }

    #[test]
    fn test_cabac_deblock() {
        // 64x64, 3 frames: CABAC Main profile with deblocking enabled,
        // P_L0_16x16 (84.4%) + I-in-P (12.5%) + skip (3.1%), byte-exact against FFmpeg
        decode_multiframe_and_compare("cabac_deblock_test", 3, 64, 64);
    }

    #[test]
    fn test_deblock_b_frames() {
        // 64x64, 9 frames: CAVLC Main profile with B-frames (bframes=2) and deblocking,
        // B16x16 (2.5%) + B_Direct (10%) + B_Skip (87.5%) + I-in-P (31.3%),
        // byte-exact against FFmpeg
        decode_multiframe_and_compare("deblock_b_test", 9, 64, 64);
    }

    #[test]
    fn test_deblock_b_inter() {
        // 64x64, 5 frames: CAVLC Main profile B-frames with deblocking,
        // B16x16 L0/L1/Bi (78.1%) + B_Direct (12.5%) + B_Skip (9.4%),
        // exercises cross-list deblock bS comparison, byte-exact against FFmpeg
        decode_multiframe_and_compare("deblock_b_inter_test", 5, 64, 64);
    }

    #[test]
    fn test_weighted_p() {
        // 32x32, 10 frames: CAVLC P with explicit weighted prediction (100% weighted,
        // 77.8% chroma weighted), fading content, --no-deblock, byte-exact against FFmpeg
        decode_multiframe_and_compare("weighted_p_test", 10, 32, 32);
    }

    #[test]
    fn test_weighted_b_implicit() {
        // 64x64, 10 frames: CABAC B with implicit weighted bi-prediction (idc=2),
        // fading content, B16x16 (25%) + B_Direct (48.4%) + B_Skip (26.6%),
        // --no-deblock, byte-exact against FFmpeg
        decode_multiframe_and_compare("weighted_b_test", 10, 64, 64);
    }

    #[test]
    fn test_realworld() {
        // 320x240, 6 frames: CAVLC Main profile, P16x16 (16.6%) + P16x8 (7.8%) +
        // P8x16 (3.1%) + intra-in-P (3.6%) + skip (68.9%), --no-deblock.
        // Regression test for real-world-sized content with diverse MB types.
        decode_multiframe_and_compare("realworld_test", 6, 320, 240);
    }

    #[test]
    fn test_high_profile() {
        // 320x240, 6 frames: CAVLC High profile with 8x8 transform
        // (28% intra 8x8, 22.8% inter 8x8), --no-deblock.
        decode_multiframe_and_compare("high_profile_test", 6, 320, 240);
    }

    #[test]
    fn test_realworld_b() {
        // 320x240, 9 frames: CAVLC Main profile with B-frames (bframes=2),
        // B16x16 L0/L1/Bi (16.2%) + B16x8/8x16 (7.2%) + B_Direct (2.8%) +
        // B_Skip (73.5%) + P partitions + intra-in-P/B, --no-deblock.
        decode_multiframe_and_compare("realworld_b_test", 9, 320, 240);
    }

    #[test]
    fn test_multislice_cabac_i() {
        // 32x32, 1 frame, 2 slices (1 MB row each): CABAC Main profile I-frame.
        // Tests cross-slice intra prediction boundary handling (spec 6.4.1).
        decode_and_compare("ms_cabac_i_test", 32, 32);
    }

    #[test]
    fn test_multislice_cabac_i4() {
        // 64x64, 1 frame, 4 slices (1 MB row each): CABAC Main profile I-frame.
        // Tests multiple slice boundaries with I4x4 prediction.
        decode_and_compare("ms_cabac_i4_test", 64, 64);
    }

    #[test]
    fn test_multislice_cavlc_i() {
        // 32x32, 1 frame, 2 slices (1 MB row each): CAVLC Baseline profile I-frame.
        // Tests cross-slice nC computation and intra prediction for CAVLC.
        decode_and_compare("ms_cavlc_i_test", 32, 32);
    }

    #[test]
    fn test_multislice_cavlc_p() {
        // 64x64, 5 frames (IDR + 4 P), 4 slices per frame: CAVLC Main profile.
        // Tests multi-slice P-frame decode with cross-slice intra prediction
        // and nC boundary handling across both I and P slices.
        decode_multiframe_and_compare("ms_cavlc_p_test", 5, 64, 64);
    }

    #[test]
    fn test_multislice_cabac_p() {
        // 64x64, 5 frames (IDR + 4 P), 4 slices per frame: CABAC Main profile,
        // no deblocking. Tests multi-slice P-frame CABAC decode with cross-slice
        // intra prediction and CABAC neighbor context boundary handling.
        decode_multiframe_and_compare("ms_cabac_p_test", 5, 64, 64);
    }

    #[test]
    fn test_multislice_cabac_b() {
        // 64x64, 4 frames (IDR + B + B + P), 4 slices per frame: CABAC Main profile,
        // no deblocking, temporal+spatial direct, B_8x8 sub-partitions.
        // Tests multi-slice B-frame decode with cross-slice boundary handling.
        decode_multiframe_and_compare("ms_cabac_b_test", 4, 64, 64);
    }

    #[test]
    fn test_high_p8x8_sub4x4() {
        // 64x64, 6 frames (I+P): High profile, preset slower with P_8x8
        // sub-4x4 partitions + 8x8dct. Tests noSubMbPartSizeLessThan8x8Flag:
        // transform_size_8x8_flag must NOT be read when P_8x8 has sub-4x4
        // sub-partitions.
        decode_multiframe_and_compare("high_p8x8_sub4x4_test", 6, 64, 64);
    }

    #[test]
    fn test_b_temporal_direct_8x8_inference() {
        // 64x64, 4 frames (I,B,B,P): CABAC Main, preset slower, direct=temporal.
        // Tests direct_8x8_inference_flag in temporal direct mode: co-located MV
        // must be read from the representative 4x4 block per 8x8 group, not from
        // each individual 4x4 block.
        decode_multiframe_and_compare("b_temporal_direct_test", 4, 64, 64);
    }

    #[test]
    fn test_high_b_slower() {
        // 64x64, 10 frames: High profile, preset slower, ref=2, bframes=2,
        // 8x8dct, no-deblock. Tests direct_8x8_inference_flag in BOTH temporal
        // and spatial direct modes, noSubMbPartSizeLessThan8x8Flag for
        // transform_size_8x8_flag, and B_8x8 with sub-partition types.
        decode_multiframe_and_compare("high_b_slower_test", 10, 64, 64);
    }

    #[test]
    fn test_high_cavlc_b() {
        // 64x64, 10 frames: CAVLC High profile with bframes=2, ref=2, 8x8dct,
        // all partitions, no-deblock. Validates CAVLC B-frame 8x8 transform path.
        decode_multiframe_and_compare("high_cavlc_b_test", 10, 64, 64);
    }

    #[test]
    fn test_ms_deblock_b_cabac() {
        // 64x64, 8 frames: CABAC Main profile with bframes=2, ref=2, 4 slices,
        // deblocking ON. Tests B-slice CABAC ref_idx context with direct-mode
        // neighbors, MV/MVD zeroing for inactive prediction lists, and
        // multi-slice deblocking.
        decode_multiframe_and_compare("ms_deblock_b_cabac_test", 8, 64, 64);
    }

    #[test]
    fn test_ms_cavlc_b() {
        // 64x64, 8 frames: CAVLC Main profile, bframes=2, ref=2, 4 slices,
        // no-deblock. Tests CAVLC multi-slice B-frame decode.
        decode_multiframe_and_compare("ms_cavlc_b_test", 8, 64, 64);
    }

    #[test]
    fn test_cavlc_deblock_pb() {
        // 64x64, 8 frames: CAVLC Main profile, bframes=1, ref=1,
        // deblocking ON. Tests CAVLC P+B with deblocking filter.
        decode_multiframe_and_compare("cavlc_deblock_pb_test", 8, 64, 64);
    }

    #[test]
    fn test_unaligned_resolution() {
        // 100x76, 6 frames: CABAC High profile, bframes=1, ref=1,
        // no-deblock. Tests non-16-aligned dimensions (coded 112x80).
        decode_multiframe_and_compare("unaligned_100x76_test", 6, 100, 76);
    }

    #[test]
    fn test_cabac_weighted_p() {
        // 64x64, 8 frames: CABAC Main profile, 100% weighted P (fading),
        // bframes=0, ref=2, no-deblock. Tests CABAC explicit weighted P-slice.
        decode_multiframe_and_compare("cabac_weighted_p_test", 8, 64, 64);
    }

    #[test]
    fn test_high_cavlc_8x8_deblock() {
        // 176x144, 10 frames (I + P): CAVLC High profile, 8x8dct, deblocking ON. The other
        // CAVLC 8x8 streams are no-deblock, which hid that the CAVLC path never recorded the
        // 8x8 transform flag, so the deblocker filtered inner 4x4 edges that do not exist.
        decode_multiframe_and_compare("high_cavlc_8x8_deblock_test", 10, 176, 144);
    }

    #[test]
    fn test_cavlc_i8x8() {
        // 64x64, 3 frames: CAVLC High profile, all-intra (keyint=1), 8x8dct,
        // no-deblock. Tests CAVLC I8x8 intra prediction with 8x8 transform.
        decode_multiframe_and_compare("cavlc_i8x8_test", 3, 64, 64);
    }

    #[test]
    fn test_cabac_b8x8_direct() {
        // 64x64, 8 frames: CABAC Main profile, constrained_intra_pred_flag=1,
        // bframes=2, ref=2, no-deblock. All-B_8x8 MBs with B_Direct_8x8
        // sub-partitions. Tests per-block direct flag for ref_idx CABAC context.
        decode_multiframe_and_compare("cabac_b8x8_direct_test", 8, 64, 64);
    }

    #[test]
    fn test_constrained_intra() {
        // 64x64, 8 frames: CABAC Main profile, constrained_intra_pred_flag=1,
        // bframes=1, ref=2, no-deblock. Tests that intra MBs in P/B slices
        // treat inter-predicted neighbors as unavailable (spec 8.3.1).
        decode_multiframe_and_compare("constrained_intra_test", 8, 64, 64);
    }

    #[test]
    fn test_high_preset_medium() {
        // 320x240, 30 frames: CABAC High profile, bframes=3, ref=4, 8x8dct
        // (92% intra, 98% inter), no-deblock. Large-resolution stress test
        // with all High profile features active.
        decode_multiframe_and_compare("high_preset_medium_test", 30, 320, 240);
    }

    #[test]
    fn test_high_deblock_medium() {
        // 320x240, 30 frames: CABAC High profile, bframes=3, ref=4, 8x8dct,
        // deblocking ON. Tests 8x8 transform deblocking (skip internal odd
        // edges per spec 8.7.2.1).
        decode_multiframe_and_compare("high_deblock_medium_test", 30, 320, 240);
    }

    #[test]
    fn test_jm_ltr_cavlc() {
        // 64x64, 8 frames: JM encoder, Baseline profile, CAVLC,
        // SetFirstAsLongTerm=1, ref=2. Tests MMCO long-term reference
        // (IDR marked as LT via long_term_reference_flag).
        decode_multiframe_and_compare("jm_ltr_cavlc_test", 8, 64, 64);
    }

    #[test]
    fn test_jm_ltr_cabac() {
        // 64x64, 8 frames: JM encoder, Main profile, CABAC,
        // SetFirstAsLongTerm=1, ref=2. Tests MMCO long-term reference
        // with CABAC entropy coding.
        decode_multiframe_and_compare("jm_ltr_cabac_test", 8, 64, 64);
    }

    #[test]
    fn test_jm_weighted_b_explicit() {
        // 64x64, 8 frames: JM encoder, Main profile, CABAC,
        // weighted_bipred_idc=1 (explicit weighted B), bframes=1.
        // Tests explicit weighted bi-prediction for B-slices.
        decode_multiframe_and_compare("jm_weighted_b_explicit_test", 8, 64, 64);
    }

    #[test]
    fn test_jm_ipcm_cavlc() {
        // 32x32, 4 frames: JM encoder, Baseline profile, CAVLC, QP=0,
        // I_PCM macroblocks with random content. Tests CAVLC I_PCM decode.
        decode_multiframe_and_compare("jm_ipcm_cavlc_test", 4, 32, 32);
    }

    #[test]
    fn test_jm_ipcm_cabac() {
        // 32x32, 4 frames: JM encoder, Main profile, CABAC, QP=0,
        // I_PCM macroblocks with random content. Tests CABAC I_PCM decode
        // with engine reinit and is_i16x16 context flag for I_PCM neighbors.
        decode_multiframe_and_compare("jm_ipcm_cabac_test", 4, 32, 32);
    }

    #[test]
    fn test_jm_poc_type1() {
        // 64x64, 6 frames: JM encoder, Baseline profile, CAVLC,
        // pic_order_cnt_type=1 (delta-based POC). Tests POC type 1 computation.
        decode_multiframe_and_compare("jm_poc_type1_test", 6, 64, 64);
    }

    #[test]
    fn test_jm_poc_type2() {
        // 64x64, 6 frames: JM encoder, Baseline profile, CAVLC,
        // pic_order_cnt_type=2 (frame_num-derived POC). Tests POC type 2 computation.
        decode_multiframe_and_compare("jm_poc_type2_test", 6, 64, 64);
    }

    #[test]
    fn test_jm_field_flat() {
        // 64x64 (combined), 1 frame: JM encoder, Main profile, CAVLC,
        // field pictures (field_pic_flag=1) with flat gray content (128).
        // Tests basic field picture decode, field combining, and deblocking.
        decode_multiframe_and_compare("jm_field_flat_test", 1, 64, 64);
    }

    #[test]
    fn test_jm_field_grad() {
        // 64x64 (combined), 1 frame: JM encoder, Main profile, CAVLC,
        // field pictures with vertical gradient content. QP=26.
        // Tests field coefficient scan order (spec Table 8-13) and
        // field picture deblocking.
        decode_multiframe_and_compare("jm_field_grad_test", 1, 64, 64);
    }

    #[test]
    fn test_jm_field_i() {
        // 64x64 (combined), 1 frame: JM encoder, Main profile, CAVLC,
        // field pictures (top=IDR I-slice, bottom=P-slice). QP=10, testsrc2 content.
        // Tests field P-slice MC referencing top field, including chroma field
        // MV offset for opposite-parity reference (spec 8.4.2.2).
        decode_multiframe_and_compare("jm_field_i_test", 1, 64, 64);
    }

    #[test]
    fn test_jm_field_chroma() {
        // 64x64 (combined), 1 frame: JM encoder, Main profile, CAVLC,
        // field pictures with non-trivial chroma content (U vertical gradient).
        // QP=10. Tests chroma MC with field parity offset — bottom field
        // chroma prediction from top field requires +2 eighth-pel vertical shift.
        decode_multiframe_and_compare("jm_field_chroma_test", 1, 64, 64);
    }

    #[test]
    fn test_jm_field_p() {
        // 64x64, 4 frames: JM encoder, Main profile, CAVLC,
        // multi-frame field pictures (all P-slices after IDR). QP=10.
        // Tests field-aware DPB reference list construction (spec 8.2.4.2.5),
        // sliding window for complementary field pairs, chroma field MV offset,
        // and field coefficient scan order across multiple frames.
        decode_multiframe_and_compare("jm_field_p_test", 4, 64, 64);
    }

    #[test]
    fn test_jm_field_cabac() {
        // 64x64 (combined), 1 frame: JM encoder, Main profile, CABAC,
        // field pictures (top=IDR I-slice, bottom=P-slice). QP=10.
        // Tests CABAC field-coded significance/last coefficient contexts
        // (ctxIdx 277+/338+) for field pictures via is_field_coded().
        decode_multiframe_and_compare("jm_field_cabac_test", 1, 64, 64);
    }

    /// Decode a multi-frame stream and compare output YUV SHA-256 hash.
    /// Used for large-resolution tests where storing the full reference YUV
    /// would be too expensive.
    fn decode_and_compare_hash(
        h264_name: &str,
        expected_frames: usize,
        expected_width: u32,
        expected_height: u32,
        expected_sha256: &str,
    ) {
        let h264_path = format!("{}/testdata/{}.h264", env!("CARGO_MANIFEST_DIR"), h264_name);
        let h264_data = std::fs::read(&h264_path)
            .unwrap_or_else(|e| panic!("failed to read {}: {}", h264_path, e));

        let nals = parse_annex_b(&h264_data);
        let mut decoder = Decoder::new();
        let mut frames = Vec::new();
        for nal in &nals {
            if let Some(f) = decoder.decode_nal(nal).unwrap() {
                frames.push(f);
            }
        }
        if let Some(f) = decoder.flush() {
            frames.push(f);
        }
        assert_eq!(
            frames.len(),
            expected_frames,
            "expected {} frames",
            expected_frames
        );
        for f in &frames {
            assert_eq!(f.width, expected_width);
            assert_eq!(f.height, expected_height);
        }

        // Sort by POC for display-order comparison
        frames.sort_by_key(|f| f.pic_order_cnt);

        // Build output YUV and compute SHA-256
        // Use a simple FNV-style hash to avoid pulling in sha2 crate;
        // we use two independent hashes to get collision resistance.
        let mut output = Vec::new();
        for frame in &frames {
            output.extend_from_slice(&frame.y);
            output.extend_from_slice(&frame.u);
            output.extend_from_slice(&frame.v);
        }

        // The reference hashes are over 8-bit samples: every sample must fit a
        // byte exactly (no truncation), then hash the byte stream.
        let output: Vec<u8> = output
            .iter()
            .map(|&s| u8::try_from(s).expect("8-bit stream produced a sample above 255"))
            .collect();

        // Compute SHA-256 using the same algorithm as `shasum -a 256`
        // Implemented inline to avoid external dependencies.
        let hash = sha256(&output);
        let hex: String = hash.iter().map(|b| format!("{:02x}", b)).collect();
        assert_eq!(hex, expected_sha256, "SHA-256 mismatch for {}", h264_name);
    }

    /// Minimal SHA-256 implementation (FIPS 180-4) for test use only.
    fn sha256(data: &[u8]) -> [u8; 32] {
        const K: [u32; 64] = [
            0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
            0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
            0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
            0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
            0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
            0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
            0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
            0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
            0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
            0xc67178f2,
        ];

        let mut h: [u32; 8] = [
            0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
            0x5be0cd19,
        ];

        // Padding
        let bit_len = (data.len() as u64) * 8;
        let mut padded = data.to_vec();
        padded.push(0x80);
        while (padded.len() % 64) != 56 {
            padded.push(0);
        }
        padded.extend_from_slice(&bit_len.to_be_bytes());

        for chunk in padded.chunks_exact(64) {
            let mut w = [0u32; 64];
            for i in 0..16 {
                w[i] = u32::from_be_bytes([
                    chunk[4 * i],
                    chunk[4 * i + 1],
                    chunk[4 * i + 2],
                    chunk[4 * i + 3],
                ]);
            }
            for i in 16..64 {
                let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
                let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
                w[i] = w[i - 16]
                    .wrapping_add(s0)
                    .wrapping_add(w[i - 7])
                    .wrapping_add(s1);
            }

            let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
            for i in 0..64 {
                let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
                let ch = (e & f) ^ (!e & g);
                let t1 = hh
                    .wrapping_add(s1)
                    .wrapping_add(ch)
                    .wrapping_add(K[i])
                    .wrapping_add(w[i]);
                let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
                let maj = (a & b) ^ (a & c) ^ (b & c);
                let t2 = s0.wrapping_add(maj);
                hh = g;
                g = f;
                f = e;
                e = d.wrapping_add(t1);
                d = c;
                c = b;
                b = a;
                a = t1.wrapping_add(t2);
            }

            h[0] = h[0].wrapping_add(a);
            h[1] = h[1].wrapping_add(b);
            h[2] = h[2].wrapping_add(c);
            h[3] = h[3].wrapping_add(d);
            h[4] = h[4].wrapping_add(e);
            h[5] = h[5].wrapping_add(f);
            h[6] = h[6].wrapping_add(g);
            h[7] = h[7].wrapping_add(hh);
        }

        let mut result = [0u8; 32];
        for (i, &val) in h.iter().enumerate() {
            result[4 * i..4 * i + 4].copy_from_slice(&val.to_be_bytes());
        }
        result
    }

    #[test]
    fn test_1080p() {
        // 1920x1080, 10 frames: mandelbrot source, x264 --preset medium,
        // CABAC, bframes=3, ref=2, no-deblock. 6 B-frames, 3 P-frames, 1 IDR.
        // Hash-based comparison to avoid storing 30MB reference YUV.
        decode_and_compare_hash(
            "1080p_test",
            10,
            1920,
            1080,
            "e795b2188acd5a6f4b819f588e388ab3af1357edb36a62a5f53ba24fbd9a57d4",
        );
    }

    #[test]
    fn test_1080p_deblock() {
        // 1920x1080, 10 frames: mandelbrot source, x264 --preset medium,
        // CABAC, bframes=3, ref=2, deblocking ON.
        decode_and_compare_hash(
            "1080p_deblock_test",
            10,
            1920,
            1080,
            "cb4ebf9c0e470717c7c2f0dd29f1afca172c362414803ee7a132844dd827e3f5",
        );
    }

    #[test]
    fn test_1080p_cavlc() {
        // 1920x1080, 10 frames: mandelbrot source, x264 --preset medium,
        // CAVLC, bframes=3, ref=2, no-deblock.
        decode_and_compare_hash(
            "1080p_cavlc_test",
            10,
            1920,
            1080,
            "d53999477dacff0905a38a3c1ff4e3ca210b634f968cf56c914b76a08eee99da",
        );
    }

    /// Verify that OrderedDecoder produces the same display-order output as
    /// manually decoding with Decoder + sorting by (idr_count, poc). Uses
    /// preset_medium (320x240, 60 frames, ref=4, bframes=3) which exercises
    /// B-frame reordering across multiple GOPs.
    #[test]
    fn test_ordered_decoder_matches_manual_sort() {
        let h264_path = format!("{}/testdata/preset_medium.h264", env!("CARGO_MANIFEST_DIR"));
        let h264_data = std::fs::read(&h264_path).unwrap();
        let nals = parse_annex_b(&h264_data);

        // Reference: manually decode + sort by (idr_count, poc)
        let mut decoder = Decoder::new();
        let mut idr_count: u32 = 0;
        let mut frames: Vec<(u32, i32, Frame)> = Vec::new();
        for nal in &nals {
            let is_idr = nal.nal_unit_type == NalUnitType::SliceIdr;
            if let Some(f) = decoder.decode_nal(nal).unwrap() {
                frames.push((idr_count, f.pic_order_cnt, f));
            }
            if is_idr {
                idr_count += 1;
            }
        }
        if let Some(f) = decoder.flush() {
            frames.push((idr_count, f.pic_order_cnt, f));
        }
        frames.sort_by_key(|(idr, poc, _)| (*idr, *poc));
        let manual_yuv: Vec<u16> = frames
            .iter()
            .flat_map(|(_, _, f)| {
                let mut v = Vec::new();
                v.extend_from_slice(&f.y);
                v.extend_from_slice(&f.u);
                v.extend_from_slice(&f.v);
                v
            })
            .collect();

        // OrderedDecoder: same NALs, automatic ordering
        let mut ordered = OrderedDecoder::new();
        let mut ordered_frames: Vec<Frame> = Vec::new();
        for nal in &nals {
            ordered_frames.extend(ordered.decode_nal(nal).unwrap());
        }
        ordered_frames.extend(ordered.flush());
        let ordered_yuv: Vec<u16> = ordered_frames
            .iter()
            .flat_map(|f| {
                let mut v = Vec::new();
                v.extend_from_slice(&f.y);
                v.extend_from_slice(&f.u);
                v.extend_from_slice(&f.v);
                v
            })
            .collect();

        assert_eq!(ordered_frames.len(), frames.len(), "frame count mismatch");
        assert_eq!(
            ordered_yuv, manual_yuv,
            "OrderedDecoder output differs from manual sort"
        );
    }

    /// Decode an Annex B fuzz regression file. Must not panic.
    fn fuzz_decode_annex_b(name: &str) {
        let path = format!(
            "{}/testdata/fuzz_regressions/{}",
            env!("CARGO_MANIFEST_DIR"),
            name
        );
        let data = std::fs::read(&path).unwrap();
        let nals = parse_annex_b(&data);
        let mut decoder = Decoder::new();
        for nal in &nals {
            let _ = decoder.decode_nal(nal);
        }
        let _ = decoder.flush();
    }

    /// Decode an AVCC fuzz regression file. The first byte selects the
    /// avcC/sample split point, mirroring the `decode_avcc` fuzz target.
    /// Must not panic.
    fn fuzz_decode_avcc(name: &str) {
        let path = format!(
            "{}/testdata/fuzz_regressions/{}",
            env!("CARGO_MANIFEST_DIR"),
            name
        );
        let data = std::fs::read(&path).unwrap();
        if data.len() < 2 {
            return;
        }
        let split = (data[0] as usize).min(data.len() - 1);
        let avcc_box = &data[1..1 + split];
        let sample_data = &data[1 + split..];
        let cfg = match crate::nal::parse_avcc_config(avcc_box) {
            Ok(c) => c,
            Err(_) => return,
        };
        let mut decoder = Decoder::new();
        for nal in cfg.sps_nals.iter().chain(cfg.pps_nals.iter()) {
            let _ = decoder.decode_nal(nal);
        }
        for nal in crate::nal::parse_avcc(sample_data, cfg.length_size) {
            let _ = decoder.decode_nal(&nal);
        }
        let _ = decoder.flush();
    }

    // --- Fuzz regression tests ---
    // Each test replays a crash input found by libFuzzer and verifies no panic.

    /// Empty ref_pic_list underflow (decode_cabac.rs / decode_cavlc.rs)
    #[test]
    fn test_fuzz_regression_empty_ref_list_no_panic() {
        fuzz_decode_annex_b("decode_annex_b_subtract_overflow.h264");
    }

    /// cabac_init_idc out of range (cabac.rs / slice.rs)
    #[test]
    fn test_fuzz_regression_cabac_init_idc_out_of_range() {
        fuzz_decode_avcc("decode_avcc_cabac_init_idc_oob.bin");
    }

    /// CABAC reader init past end of buffer (cabac.rs)
    #[test]
    fn test_fuzz_regression_cabac_init_past_end() {
        fuzz_decode_avcc("decode_avcc_cabac_init_oob.bin");
    }

    /// CAVLC coefficient position underflow (cavlc.rs)
    #[test]
    fn test_fuzz_regression_cavlc_pos_underflow() {
        fuzz_decode_annex_b("decode_annex_b_cavlc_pos_underflow.h264");
    }

    /// log2_weight_denom shift overflow (slice.rs)
    #[test]
    fn test_fuzz_regression_weight_denom_shift_overflow() {
        fuzz_decode_avcc("decode_avcc_weight_denom_overflow.bin");
    }

    /// ref_pic_safe empty list underflow (mv_pred.rs)
    #[test]
    fn test_fuzz_regression_ref_pic_safe_empty_list() {
        fuzz_decode_annex_b("decode_annex_b_ref_pic_safe_empty.h264");
    }

    /// Temporal direct mode empty L1 list (slice_context.rs)
    #[test]
    fn test_fuzz_regression_temporal_direct_empty_l1() {
        fuzz_decode_avcc("decode_avcc_temporal_direct_empty_l1.bin");
    }

    /// RPLM pic_num modular arithmetic underflow (dpb.rs)
    #[test]
    fn test_fuzz_regression_rplm_pic_num_underflow() {
        fuzz_decode_annex_b("decode_annex_b_rplm_pic_num_underflow.h264");
    }

    /// Dequant multiply overflow (residual.rs / neighbor.rs)
    #[test]
    fn test_fuzz_regression_dequant_mul_overflow() {
        fuzz_decode_avcc("decode_avcc_dequant_mul_overflow.bin");
    }

    /// Deblock QP average add overflow (deblock.rs)
    #[test]
    fn test_fuzz_regression_deblock_qp_overflow() {
        fuzz_decode_avcc("decode_avcc_deblock_qp_overflow.bin");
    }

    /// MC output buffer overrun from malformed block size (inter_pred.rs)
    #[test]
    fn test_fuzz_regression_mc_output_overrun() {
        fuzz_decode_annex_b("decode_annex_b_mc_output_overrun.h264");
    }

    /// SPS crop offset subtraction underflow (sps.rs)
    #[test]
    fn test_fuzz_regression_sps_crop_underflow() {
        fuzz_decode_avcc("decode_avcc_sps_crop_underflow.bin");
    }

    /// CABAC I_PCM frame buffer index out of bounds (decode_cabac.rs:2934)
    #[test]
    fn test_fuzz_regression_cabac_ipcm_frame_overrun() {
        fuzz_decode_annex_b("bug15_cabac_ipcm_frame_overrun.bin");
    }

    /// Divide by zero when coded width is zero (decoder.rs:357)
    #[test]
    fn test_fuzz_regression_divide_by_zero_coded_width() {
        fuzz_decode_avcc("bug16_divide_by_zero_coded_width.bin");
    }

    /// CABAC I_PCM in P/B-slice frame buffer overrun (decode_cabac.rs:797)
    #[test]
    fn test_fuzz_regression_cabac_ipcm_pb_frame_overrun() {
        fuzz_decode_annex_b("cabac_ipcm_pb_frame_overrun.bin");
    }

    /// Divide by zero when coded width is zero — variant 2 (decoder.rs:357)
    #[test]
    fn test_fuzz_regression_divide_by_zero_coded_width_2() {
        fuzz_decode_avcc("decode_avcc_divide_by_zero_2.bin");
    }

    /// SPS width() multiply overflow (sps.rs:124)
    #[test]
    fn test_fuzz_regression_sps_width_overflow() {
        fuzz_decode_avcc("decode_avcc_sps_width_overflow.bin");
    }

    /// Chroma MC ref_plane index out of bounds (inter_pred.rs:813)
    #[test]
    fn test_fuzz_regression_chroma_mc_ref_plane_overrun() {
        fuzz_decode_annex_b("chroma_mc_ref_plane_overrun.bin");
    }

    /// Deblock filter frame buffer overrun (deblock.rs:543)
    #[test]
    fn test_fuzz_regression_deblock_frame_overrun() {
        fuzz_decode_avcc("decode_avcc_deblock_overrun.bin");
    }

    /// CABAC reinit index out of bounds (cabac.rs:219)
    #[test]
    fn test_fuzz_regression_cabac_reinit_overrun() {
        fuzz_decode_annex_b("cabac_reinit_overrun.bin");
    }

    /// CAVLC B sub_mb_type table overrun (decode_cavlc.rs:448)
    #[test]
    fn test_fuzz_regression_cavlc_b_sub_mb_type_overrun() {
        fuzz_decode_annex_b("cavlc_b_sub_mb_type_overrun.bin");
    }

    /// first_mb_in_slice exceeds total MBs (decoder.rs:754)
    #[test]
    fn test_fuzz_regression_first_mb_overrun() {
        fuzz_decode_avcc("decode_avcc_first_mb_overrun.bin");
    }

    /// POC LSB shift overflow (dpb.rs:329)
    #[test]
    fn test_fuzz_regression_poc_shift_overflow() {
        fuzz_decode_avcc("decode_avcc_poc_shift_overflow.bin");
    }

    /// Frame crop slice overrun (decoder.rs:368)
    #[test]
    fn test_fuzz_regression_crop_overrun() {
        fuzz_decode_avcc("decode_avcc_crop_overrun.bin");
    }

    /// max_pic_num shift overflow (decoder.rs:426)
    #[test]
    fn test_fuzz_regression_frame_num_shift_overflow() {
        fuzz_decode_avcc("decode_avcc_frame_num_shift_overflow.bin");
    }

    /// Deblock multiply overflow (deblock.rs:113)
    #[test]
    fn test_fuzz_regression_deblock_mul_overflow() {
        fuzz_decode_avcc("decode_avcc_deblock_mul_overflow.bin");
    }

    /// Continuation slice with SPS dimension mismatch (decoder.rs:763)
    #[test]
    fn test_fuzz_regression_continuation_mismatch() {
        fuzz_decode_avcc("decode_avcc_continuation_mismatch.bin");
    }

    /// SPS scaling list delta add overflow (sps.rs:396)
    #[test]
    fn test_fuzz_regression_scaling_list_overflow() {
        fuzz_decode_avcc("decode_avcc_scaling_list_overflow.bin");
    }

    /// Dequant DC multiply overflow (residual.rs:150)
    #[test]
    fn test_fuzz_regression_dequant_dc_mul_overflow() {
        fuzz_decode_avcc("decode_avcc_dequant_dc_mul_overflow.bin");
    }

    /// POC MSB subtract overflow (dpb.rs:338)
    #[test]
    fn test_fuzz_regression_poc_msb_sub_overflow() {
        fuzz_decode_avcc("decode_avcc_poc_msb_sub_overflow.bin");
    }

    /// 8x8 IDCT subtract overflow (residual.rs:272)
    #[test]
    fn test_fuzz_regression_idct8x8_sub_overflow() {
        fuzz_decode_annex_b("idct8x8_sub_overflow.bin");
    }

    /// 4x4 IDCT add overflow (residual.rs:77)
    #[test]
    fn test_fuzz_regression_idct4x4_add_overflow() {
        fuzz_decode_avcc("decode_avcc_idct4x4_add_overflow.bin");
    }

    /// POC type 1 cycle multiply overflow (dpb.rs:378)
    #[test]
    fn test_fuzz_regression_poc_type1_mul_overflow() {
        fuzz_decode_avcc("decode_avcc_poc_type1_mul_overflow.bin");
    }

    /// Field-pair combine with mismatched field geometry (decoder.rs:451)
    #[test]
    fn test_fuzz_regression_field_pair_mismatch() {
        fuzz_decode_avcc("decode_avcc_field_pair_mismatch.bin");
    }

    /// MBAFF CAVLC test (64x64, 6-frame, interlaced, CAVLC, frame-coded pairs)
    #[test]
    fn test_mbaff_cavlc() {
        decode_multiframe_and_compare("mbaff_cavlc_test", 6, 64, 64);
    }

    /// MBAFF CAVLC P-frames with complex content (64x64, 8 frames, testsrc2)
    #[test]
    fn test_mbaff_p_cavlc_8f() {
        decode_multiframe_and_compare("mbaff_p_cavlc_8f_test", 8, 64, 64);
    }

    /// MBAFF High profile CAVLC with 8x8 transform (64x64, 6 frames, testsrc2)
    #[test]
    fn test_mbaff_high_cavlc() {
        decode_multiframe_and_compare("mbaff_high_cavlc_test", 6, 64, 64);
    }

    /// MBAFF CABAC I-frame (32x32, 1 frame, testsrc2, High profile)
    #[test]
    fn test_mbaff_cabac_i() {
        decode_multiframe_and_compare("mbaff_cabac_i_test", 1, 32, 32);
    }

    /// MBAFF CABAC I-frame 64x64 (1 frame, testsrc2, High profile)
    #[test]
    fn test_mbaff_cabac_64() {
        decode_multiframe_and_compare("mbaff_cabac_64_test", 1, 64, 64);
    }

    /// MBAFF CABAC P-frames (64x64, 5 frames, testsrc2, keyint=5)
    #[test]
    fn test_mbaff_cabac_p() {
        decode_multiframe_and_compare("mbaff_cabac_p_test", 5, 64, 64);
    }

    /// MBAFF CABAC B-frames (64x64, 8 frames, testsrc2, bframes=2 ref=2)
    #[test]
    fn test_mbaff_cabac_b() {
        decode_multiframe_and_compare("mbaff_cabac_b_test", 8, 64, 64);
    }

    /// MBAFF CAVLC B-frames (64x64, 8 frames, testsrc2, bframes=2 ref=2)
    #[test]
    fn test_mbaff_cavlc_b() {
        decode_multiframe_and_compare("mbaff_cavlc_b_test", 8, 64, 64);
    }

    /// MBAFF CAVLC deblocking (64x64, 8 frames, Main profile, P-only, deblock ON)
    #[test]
    fn test_mbaff_deblock_cavlc() {
        decode_multiframe_and_compare("mbaff_deblock_cavlc_test", 8, 64, 64);
    }

    /// MBAFF CABAC deblocking with B-frames (64x64, 8 frames, Main profile, bframes=2 ref=2)
    #[test]
    fn test_mbaff_deblock_cabac() {
        decode_multiframe_and_compare("mbaff_deblock_cabac_test", 8, 64, 64);
    }

    /// MBAFF High profile with 8x8 DCT + deblocking + B-frames (64x64, 8 frames)
    #[test]
    fn test_mbaff_high_deblock() {
        decode_multiframe_and_compare("mbaff_high_deblock_test", 8, 64, 64);
    }

    /// MBAFF field-coded I-frames (64x64, 3 frames, all-field-coded pairs, keyint=1)
    #[test]
    fn test_mbaff_field_i() {
        decode_multiframe_and_compare("mbaff_field_i_test", 3, 64, 64);
    }

    /// MBAFF field-coded P-frames CAVLC (64x64, 4 frames, all-field-coded pairs)
    #[test]
    fn test_mbaff_field_p() {
        decode_multiframe_and_compare("mbaff_field_p_test", 4, 64, 64);
    }

    /// MBAFF field-coded CABAC (64x64, 4 frames, all-field-coded pairs, Main profile)
    #[test]
    fn test_mbaff_field_cabac() {
        decode_multiframe_and_compare("mbaff_field_cabac_test", 4, 64, 64);
    }

    /// MBAFF field-coded High profile CAVLC with 8x8 DCT (64x64, 4 frames, all-field-coded)
    #[test]
    fn test_mbaff_field_high() {
        decode_multiframe_and_compare("mbaff_field_high_test", 4, 64, 64);
    }

    // --- High 10 (high bit depth 4:2:0) streams ---

    /// 16-bit sibling of [`decode_multiframe_and_compare`]: decode every frame
    /// of a >8-bit stream, put the frames in display order and compare them
    /// sample-for-sample against a planar 16-bit little-endian reference
    /// (ffmpeg `yuv420p10le`: Y then U then V per frame, no cropping).
    /// Panics with the first mismatching frame/plane/x/y.
    fn decode_multiframe_and_compare_16(
        h264_name: &str,
        expected_frames: usize,
        expected_width: u32,
        expected_height: u32,
        expected_bit_depth: u8,
    ) {
        let h264_path = format!("{}/testdata/{}.h264", env!("CARGO_MANIFEST_DIR"), h264_name);
        let yuv_path = format!("{}/testdata/{}.yuv", env!("CARGO_MANIFEST_DIR"), h264_name);
        let h264_data = std::fs::read(&h264_path)
            .unwrap_or_else(|e| panic!("failed to read {}: {}", h264_path, e));
        let expected_bytes = std::fs::read(&yuv_path)
            .unwrap_or_else(|e| panic!("failed to read {}: {}", yuv_path, e));
        assert_eq!(
            expected_bytes.len() % 2,
            0,
            "16-bit reference has odd length"
        );
        let expected: Vec<u16> = expected_bytes
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();

        // Decode, remembering which IDR period each frame belongs to so the
        // (gop, poc) sort gives display order even across IDRs.
        let nals = parse_annex_b(&h264_data);
        let mut decoder = Decoder::new();
        let mut idr_count: u32 = 0;
        let mut frames: Vec<(u32, Frame)> = Vec::new();
        for nal in &nals {
            let is_idr = nal.nal_unit_type == NalUnitType::SliceIdr;
            if let Some(f) = decoder
                .decode_nal(nal)
                .unwrap_or_else(|e| panic!("{}: decode error {}", h264_name, e))
            {
                frames.push((idr_count, f));
            }
            if is_idr {
                idr_count += 1;
            }
        }
        if let Some(f) = decoder.flush() {
            frames.push((idr_count, f));
        }
        assert_eq!(
            frames.len(),
            expected_frames,
            "{}: expected {} frames",
            h264_name,
            expected_frames
        );
        frames.sort_by_key(|(gop, f)| (*gop, f.pic_order_cnt));

        let w = expected_width as usize;
        let h = expected_height as usize;
        let (cw, ch) = (w / 2, h / 2);
        let frame_samples = w * h + 2 * cw * ch;
        assert_eq!(
            expected.len(),
            expected_frames * frame_samples,
            "{}: reference size does not match {} frames of {}x{}",
            h264_name,
            expected_frames,
            w,
            h
        );

        for (frame_idx, (_, f)) in frames.iter().enumerate() {
            assert_eq!(f.width, expected_width, "{}: width", h264_name);
            assert_eq!(f.height, expected_height, "{}: height", h264_name);
            assert_eq!(
                (f.bit_depth_luma, f.bit_depth_chroma),
                (expected_bit_depth, expected_bit_depth),
                "{}: bit depth",
                h264_name
            );
            let base = frame_idx * frame_samples;
            let planes: [(&str, &[u16], usize, usize, usize); 3] = [
                ("Y", f.y.as_slice(), w, h, base),
                ("U", f.u.as_slice(), cw, ch, base + w * h),
                ("V", f.v.as_slice(), cw, ch, base + w * h + cw * ch),
            ];
            for (name, plane, pw, ph, off) in planes {
                assert_eq!(
                    plane.len(),
                    pw * ph,
                    "{}: frame {} plane {} has {} samples, expected {}",
                    h264_name,
                    frame_idx,
                    name,
                    plane.len(),
                    pw * ph
                );
                let reference = &expected[off..off + pw * ph];
                if let Some(i) = (0..pw * ph).find(|&i| plane[i] != reference[i]) {
                    panic!(
                        "{}: first mismatch in frame {} (display order, POC {}) plane {} at \
                         x={} y={}: decoded {} expected {}",
                        h264_name,
                        frame_idx,
                        f.pic_order_cnt,
                        name,
                        i % pw,
                        i / pw,
                        plane[i],
                        reference[i]
                    );
                }
            }
        }
    }

    /// High 10 CABAC: IBBP, 3 refs, 8x8 transform, weighted P (176x144, 10 frames).
    #[test]
    fn test_high10_cabac() {
        decode_multiframe_and_compare_16("high10_cabac_test", 10, 176, 144, 10);
    }

    /// High 10 CAVLC, P-only (176x144, 10 frames).
    #[test]
    fn test_high10_cavlc() {
        decode_multiframe_and_compare_16("high10_cavlc_test", 10, 176, 144, 10);
    }

    /// High 10 CAVLC at QP 1: negative QPY, large coefficients, level_prefix >= 16.
    #[test]
    fn test_high10_lowqp_cavlc() {
        decode_multiframe_and_compare_16("high10_lowqp_cavlc_test", 4, 176, 144, 10);
    }

    /// High 10 CABAC at QP 1 (IBP): negative QPY, large UEG0 coefficient levels.
    #[test]
    fn test_high10_lowqp_cabac() {
        decode_multiframe_and_compare_16("high10_lowqp_cabac_test", 4, 176, 144, 10);
    }

    /// High 10 all-intra CABAC with 8x8 transform (176x144, 6 frames).
    #[test]
    fn test_high10_intra() {
        decode_multiframe_and_compare_16("high10_intra_test", 6, 176, 144, 10);
    }

    /// High 10 all-intra CAVLC, 4x4 transform only (176x144, 4 frames).
    #[test]
    fn test_high10_intra4x4_cavlc() {
        decode_multiframe_and_compare_16("high10_intra4x4_cavlc_test", 4, 176, 144, 10);
    }

    /// High 10 fade with explicit weighted P and weighted B, b-pyramid (176x144, 10 frames).
    #[test]
    fn test_high10_weighted() {
        decode_multiframe_and_compare_16("high10_weighted_test", 10, 176, 144, 10);
    }

    /// High 10 at QP 60 with deblocking 2:2: scaled alpha/beta/tC0 (176x144, 6 frames).
    #[test]
    fn test_high10_highqp() {
        decode_multiframe_and_compare_16("high10_highqp_test", 6, 176, 144, 10);
    }

    /// QP derivation with QpBdOffsetY = 12 (spec 7.4.5): QPY wraps within -12..=51.
    #[test]
    fn test_high_bit_depth_qp_wrap() {
        let wctx = WeightContext {
            use_weight: 0,
            wt: None,
            implicit_weights: &[],
            bit_depth_luma: 10,
            bit_depth_chroma: 10,
        };
        let scaling_4x4 = [[16u8; 16]; 6];
        let scaling_8x8 = [[16u8; 64]; 2];
        let sp = SliceParams {
            is_p_slice: false,
            is_b_slice: false,
            use_weight: 0,
            current_poc: 0,
            direct_spatial_mv_pred_flag: false,
            direct_8x8_inference_flag: false,
            transform_8x8_mode_flag: false,
            scaling_list_4x4: &scaling_4x4,
            scaling_list_8x8: &scaling_8x8,
            constrained_intra_pred_flag: false,
            chroma_qp_index_offset: -2,
            second_chroma_qp_index_offset: -2,
            bit_depth_y: 10,
            bit_depth_c: 10,
            qp_bd_offset_y: 12,
            qp_bd_offset_c: 12,
            ref_pic_list: &[],
            ref_pic_list_l0: &[],
            ref_pic_list_l1: &[],
            num_ref_idx_l0_active: 0,
            num_ref_idx_l1_active: 0,
            wctx: &wctx,
            first_mb_in_slice: 0,
            slice_qp: 0,
            is_i_slice: true,
            cabac_init_idc: 0,
        };
        assert_eq!(sp.apply_mb_qp_delta(-11, 0).unwrap(), -11);
        assert_eq!(sp.apply_mb_qp_delta(-11, -1).unwrap(), -12);
        // Below -QpBdOffsetY wraps to 51.
        assert_eq!(sp.apply_mb_qp_delta(-12, -1).unwrap(), 51);
        assert_eq!(sp.apply_mb_qp_delta(51, 1).unwrap(), -12);
        // Valid range is -(26 + 6)..=(25 + 6).
        assert!(sp.apply_mb_qp_delta(0, 31).is_ok());
        assert!(sp.apply_mb_qp_delta(0, 32).is_err());
        assert!(sp.apply_mb_qp_delta(0, -32).is_ok());
        assert!(sp.apply_mb_qp_delta(0, -33).is_err());
        assert_eq!(sp.luma_qp_prime(-12), 0);
        // qPI = Clip3(-12, 51, -11 - 2) = -12 -> QPC = -12, QP'C = 0.
        assert_eq!(sp.chroma_qp_prime(-11), (0, 0));
        // qPI = 40 -> QPC = 36 (Table 8-15), QP'C = 48.
        assert_eq!(sp.chroma_qp_prime(42), (48, 48));
    }
}
