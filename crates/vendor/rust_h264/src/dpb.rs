//! Decoded Picture Buffer (H.264 spec 8.2.5).
//!
//! Stores decoded reference frames for use by P/B slice motion compensation.
//! Manages short-term reference marking via sliding window (spec 8.2.5.3).

use std::rc::Rc;

use crate::nal::NalUnitType;
use crate::slice::SliceHeader;
use crate::sps::Sps;

/// Reference status of a picture in the DPB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReferenceStatus {
    /// Not used for reference.
    Unused,
    /// Short-term reference (identified by frame_num).
    ShortTerm,
    /// Long-term reference (identified by long_term_frame_idx).
    LongTerm(u32),
}

/// Whether a picture is a frame or a field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PictureStructure {
    Frame,
    TopField,
    BottomField,
}

/// Immutable decoded picture data shared via Rc.
#[derive(Debug)]
pub struct DecodedPicture {
    /// Luma samples (one u16 per sample, `0..(1 << bit_depth_luma)`).
    pub y: Vec<u16>,
    /// Cb samples (one u16 per sample, `0..(1 << bit_depth_chroma)`).
    pub u: Vec<u16>,
    /// Cr samples.
    pub v: Vec<u16>,
    pub width: u32,
    pub height: u32,
    pub frame_num: u32,
    pub pic_order_cnt: i32,
    /// Per-4x4-block L0 motion vectors (for temporal direct mode co-located access).
    pub mv_l0: Vec<[i16; 2]>,
    /// Per-4x4-block L0 reference indices.
    pub ref_idx_l0: Vec<i8>,
    /// Per-4x4-block POC of the L0 reference picture used (for temporal direct mode mapping).
    pub ref_poc_l0: Vec<i32>,
    /// Per-4x4-block L1 motion vectors (for spatial direct co-located L1 check).
    pub mv_l1: Vec<[i16; 2]>,
    /// Per-4x4-block L1 reference indices (for spatial direct co-located L1 check).
    pub ref_idx_l1: Vec<i8>,
    /// Width in macroblocks (for indexing into mv_l0/ref_idx_l0).
    pub mb_width: u32,
    /// Whether this picture is intra-only (all MBs are intra).
    pub is_intra: bool,
    /// Whether this decoded picture is a frame, top field, or bottom field.
    pub structure: PictureStructure,
    /// BitDepthY of the samples in `y` (8..=14).
    pub bit_depth_luma: u8,
    /// BitDepthC of the samples in `u`/`v` (8..=14).
    pub bit_depth_chroma: u8,
}

/// A DPB entry wrapping an Rc<DecodedPicture> with mutable status.
struct DpbEntry {
    pic: Rc<DecodedPicture>,
    reference: ReferenceStatus,
}

/// The Decoded Picture Buffer.
pub struct Dpb {
    max_ref_frames: usize,
    entries: Vec<DpbEntry>,
    // POC type 0 state
    prev_poc_msb: i32,
    prev_poc_lsb: u32,
}

impl Dpb {
    pub fn new(max_ref_frames: usize) -> Self {
        Self {
            max_ref_frames,
            entries: Vec::new(),
            prev_poc_msb: 0,
            prev_poc_lsb: 0,
        }
    }

    /// Update capacity when a new SPS is parsed.
    pub fn set_max_ref_frames(&mut self, max_ref_frames: u32) {
        self.max_ref_frames = max_ref_frames as usize;
    }

    /// Clear the entire DPB. Called on IDR.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.prev_poc_msb = 0;
        self.prev_poc_lsb = 0;
    }

    /// Insert a decoded picture into the DPB.
    /// Applies sliding window marking if needed (spec 8.2.5.3).
    pub fn insert(&mut self, pic: Rc<DecodedPicture>, reference: ReferenceStatus) {
        // Sliding window only applies to short-term references (spec 8.2.5.3)
        if reference == ReferenceStatus::ShortTerm {
            // Don't evict the complementary field of the picture being inserted.
            // Two fields with the same frame_num form a pair and count as one
            // reference frame (spec 7.4.3.1). Skip eviction if the new field
            // completes an existing pair.
            let completes_pair = pic.structure != PictureStructure::Frame
                && self.entries.iter().any(|e| {
                    e.reference == ReferenceStatus::ShortTerm
                        && e.pic.frame_num == pic.frame_num
                        && e.pic.structure != pic.structure
                        && e.pic.structure != PictureStructure::Frame
                });
            if !completes_pair {
                self.sliding_window_mark();
            }
        }
        self.entries.push(DpbEntry { pic, reference });
        self.remove_unused();
    }

    /// Get the list of short-term reference pictures, sorted by descending PicNum.
    /// Used to build ref_pic_list_0 for P slices (spec 8.2.4.2.1 / 8.2.4.2.5).
    ///
    /// For frame pictures (`is_field_pic=false`): standard frame ordering by descending POC.
    /// For field pictures (`is_field_pic=true`): field ordering per spec 8.2.4.2.5 —
    /// group fields by frame_num, sort groups by descending FrameNumWrap (POC proxy),
    /// within each group emit same-parity field before opposite-parity field.
    pub fn short_term_ref_list(
        &self,
        is_field_pic: bool,
        bottom_field_flag: bool,
    ) -> Vec<Rc<DecodedPicture>> {
        let mut refs: Vec<_> = self
            .entries
            .iter()
            .filter(|e| e.reference == ReferenceStatus::ShortTerm)
            .map(|e| e.pic.clone())
            .collect();

        if is_field_pic {
            // Spec 8.2.4.2.5: field picture P-slice reference list initialization.
            // Group by frame_num, sort groups by descending POC (proxy for FrameNumWrap),
            // within each group same-parity field first.
            refs.sort_by(|a, b| {
                // Primary: descending by FrameNumWrap (use POC as proxy)
                let poc_cmp = b.pic_order_cnt.cmp(&a.pic_order_cnt);
                if a.frame_num != b.frame_num {
                    return poc_cmp;
                }
                // Same frame_num: same-parity field first
                let a_same = (a.structure == PictureStructure::BottomField) == bottom_field_flag;
                let b_same = (b.structure == PictureStructure::BottomField) == bottom_field_flag;
                // true (same parity) should come first → sort descending
                b_same.cmp(&a_same)
            });
        } else {
            // Sort by descending POC as a proxy for recency (handles frame_num wraparound).
            refs.sort_by(|a, b| b.pic_order_cnt.cmp(&a.pic_order_cnt));
        }
        // Append long-term refs sorted by ascending long_term_frame_idx (spec 8.2.4.2.1)
        refs.extend(self.long_term_ref_list());
        refs
    }

    /// Build ref_pic_list_0 for B slices (spec 8.2.4.2.3 / 8.2.4.2.5).
    /// Short-term refs with POC < current sorted by descending POC,
    /// then refs with POC > current sorted by ascending POC.
    /// For field pictures, additionally group by frame_num with same-parity first.
    pub fn ref_list_l0_b(
        &self,
        current_poc: i32,
        is_field_pic: bool,
        bottom_field_flag: bool,
    ) -> Vec<Rc<DecodedPicture>> {
        let short_term: Vec<_> = self
            .entries
            .iter()
            .filter(|e| e.reference == ReferenceStatus::ShortTerm)
            .map(|e| e.pic.clone())
            .collect();

        let mut before: Vec<_> = short_term
            .iter()
            .filter(|p| p.pic_order_cnt <= current_poc)
            .cloned()
            .collect();
        before.sort_by(|a, b| b.pic_order_cnt.cmp(&a.pic_order_cnt)); // descending

        let mut after: Vec<_> = short_term
            .iter()
            .filter(|p| p.pic_order_cnt > current_poc)
            .cloned()
            .collect();
        after.sort_by(|a, b| a.pic_order_cnt.cmp(&b.pic_order_cnt)); // ascending

        before.extend(after);

        // For field pictures: within each frame_num group, same-parity first
        if is_field_pic {
            Self::field_parity_interleave(&mut before, bottom_field_flag);
        }

        // Append long-term refs (spec 8.2.4.2.3)
        before.extend(self.long_term_ref_list());
        before
    }

    /// Build ref_pic_list_1 for B slices (spec 8.2.4.2.4 / 8.2.4.2.5).
    /// Short-term refs with POC > current sorted by ascending POC,
    /// then refs with POC <= current sorted by descending POC.
    /// If L1 == L0 and has more than one entry, swap the first two.
    pub fn ref_list_l1_b(
        &self,
        current_poc: i32,
        is_field_pic: bool,
        bottom_field_flag: bool,
    ) -> Vec<Rc<DecodedPicture>> {
        let short_term: Vec<_> = self
            .entries
            .iter()
            .filter(|e| e.reference == ReferenceStatus::ShortTerm)
            .map(|e| e.pic.clone())
            .collect();

        let mut after: Vec<_> = short_term
            .iter()
            .filter(|p| p.pic_order_cnt > current_poc)
            .cloned()
            .collect();
        after.sort_by(|a, b| a.pic_order_cnt.cmp(&b.pic_order_cnt)); // ascending

        let mut before: Vec<_> = short_term
            .iter()
            .filter(|p| p.pic_order_cnt <= current_poc)
            .cloned()
            .collect();
        before.sort_by(|a, b| b.pic_order_cnt.cmp(&a.pic_order_cnt)); // descending

        after.extend(before);

        // For field pictures: within each frame_num group, same-parity first
        if is_field_pic {
            Self::field_parity_interleave(&mut after, bottom_field_flag);
        }

        // Append long-term refs (spec 8.2.4.2.4)
        after.extend(self.long_term_ref_list());

        // Spec 8.2.4.2.4: if L1 == L0 and has more than one entry, swap first two
        let l0 = self.ref_list_l0_b(current_poc, is_field_pic, bottom_field_flag);
        if after.len() > 1
            && after.len() == l0.len()
            && after
                .iter()
                .zip(l0.iter())
                .all(|(a, b)| a.pic_order_cnt == b.pic_order_cnt)
        {
            after.swap(0, 1);
        }

        after
    }

    /// For field pictures: within runs of entries sharing the same frame_num,
    /// put same-parity fields before opposite-parity (spec 8.2.4.2.5).
    fn field_parity_interleave(list: &mut [Rc<DecodedPicture>], bottom_field_flag: bool) {
        let mut i = 0;
        while i < list.len() {
            let fn_val = list[i].frame_num;
            let mut j = i + 1;
            while j < list.len() && list[j].frame_num == fn_val {
                j += 1;
            }
            // [i..j) is a run with the same frame_num
            if j - i == 2 {
                let a_same =
                    (list[i].structure == PictureStructure::BottomField) == bottom_field_flag;
                let b_same =
                    (list[i + 1].structure == PictureStructure::BottomField) == bottom_field_flag;
                if !a_same && b_same {
                    list.swap(i, i + 1);
                }
            }
            i = j;
        }
    }

    /// Apply ref_pic_list_modification to reorder a reference list (spec 8.2.4.3).
    /// `ops`: list of (modification_of_pic_nums_idc, abs_diff_pic_num_minus1)
    /// `curr_pic_num`: CurrPicNum (= frame_num for frames, 2*frame_num+1 for fields)
    /// `max_pic_num`: MaxPicNum (= MaxFrameNum for frames, 2*MaxFrameNum for fields)
    /// `is_field_pic`: true for field pictures (PicNum = 2*frame_num + parity_bit)
    /// `bottom_field_flag`: current field parity (only meaningful when is_field_pic)
    pub fn apply_ref_list_modification(
        ref_list: &mut Vec<Rc<DecodedPicture>>,
        ops: &[(u32, u32)],
        curr_pic_num: u32,
        max_pic_num: u32,
        is_field_pic: bool,
        bottom_field_flag: bool,
    ) {
        if ops.is_empty() || ref_list.is_empty() {
            return;
        }
        let num_active = ref_list.len();
        let mut pred_pic_num = curr_pic_num;
        let mut ref_idx_lx = 0usize;

        for &(idc, val) in ops {
            // Per spec 8.2.4.3.1, ref_idx_lx must stay in [0, num_active - 1].
            // A malformed bitstream may encode more RPLM commands than there
            // are slots — bail out instead of indexing out of bounds.
            if ref_idx_lx >= num_active {
                return;
            }
            if idc == 2 {
                // Long-term ref reordering (spec 8.2.4.3.2)
                let long_term_pic_num = val; // long_term_pic_num directly
                if let Some(found_pos) = ref_list.iter().position(|p| {
                    // Match by frame_num used as long_term_frame_idx proxy
                    // In practice, the DPB entry's long_term_frame_idx is stored
                    // but DecodedPicture doesn't carry it. Match by position in list.
                    // Actually, long_term_pic_num == long_term_frame_idx for frames.
                    // We find LT refs appended at the end of the list.
                    p.frame_num == long_term_pic_num
                }) {
                    let pic = ref_list[found_pos].clone();
                    ref_list.push(ref_list.last().unwrap().clone());
                    let end = ref_list.len() - 1;
                    for c in (ref_idx_lx + 1..=end).rev() {
                        ref_list[c] = ref_list[c - 1].clone();
                    }
                    ref_list[ref_idx_lx] = pic.clone();
                    // Remove duplicate: entries after ref_idx_lx with same pic
                    let mut n = ref_idx_lx + 1;
                    for c in (ref_idx_lx + 1)..ref_list.len() {
                        if !Rc::ptr_eq(&ref_list[c], &pic) {
                            ref_list[n] = ref_list[c].clone();
                            n += 1;
                        }
                    }
                    ref_list.truncate(num_active);
                }
                ref_idx_lx += 1;
                continue;
            }
            if idc > 1 {
                continue; // idc=3 terminates the loop (handled by caller)
            }
            let abs_diff = val + 1;
            // Spec 8.2.4.3.1: modular arithmetic on pic_num. Use wrapping
            // ops to handle malformed streams where abs_diff exceeds the
            // valid range without panicking.
            let pic_num = if idc == 0 {
                if pred_pic_num >= abs_diff {
                    pred_pic_num - abs_diff
                } else {
                    pred_pic_num
                        .wrapping_add(max_pic_num)
                        .wrapping_sub(abs_diff)
                }
            } else {
                let sum = pred_pic_num.wrapping_add(abs_diff);
                if sum >= max_pic_num {
                    sum.wrapping_sub(max_pic_num)
                } else {
                    sum
                }
            };
            pred_pic_num = pic_num;

            // Spec 8.2.4.3.1: find the picture, shift entries right to make room,
            // insert at ref_idx_lx, then remove duplicates after ref_idx_lx.
            // For field pictures, PicNum = 2*frame_num + parity_bit (spec 8.2.4.1).
            let pic_num_match = |p: &DecodedPicture| -> bool {
                if is_field_pic {
                    let p_is_bottom = p.structure == PictureStructure::BottomField;
                    let same_parity = p_is_bottom == bottom_field_flag;
                    let p_pic_num = p.frame_num * 2 + if same_parity { 1 } else { 0 };
                    p_pic_num == pic_num
                } else {
                    p.frame_num == pic_num
                }
            };
            if let Some(found_pos) = ref_list.iter().position(|p| pic_num_match(p)) {
                let pic = ref_list[found_pos].clone();

                // Shift right: make room at ref_idx_lx
                // Temporarily grow the list by 1
                ref_list.push(ref_list.last().unwrap().clone());
                let end = ref_list.len() - 1;
                for c in (ref_idx_lx + 1..=end).rev() {
                    ref_list[c] = ref_list[c - 1].clone();
                }
                ref_list[ref_idx_lx] = pic.clone();

                // Remove duplicate: compact entries after ref_idx_lx that
                // match the same PicNum as the inserted picture
                let mut n = ref_idx_lx + 1;
                for c in (ref_idx_lx + 1)..ref_list.len() {
                    if !pic_num_match(&ref_list[c]) {
                        ref_list[n] = ref_list[c].clone();
                        n += 1;
                    }
                }
                ref_list.truncate(num_active);
            }

            ref_idx_lx += 1;
        }
    }

    /// Compute Picture Order Count for the current picture (spec 8.2.1).
    pub fn compute_poc(
        &mut self,
        sps: &Sps,
        header: &SliceHeader,
        nal_unit_type: NalUnitType,
        nal_ref_idc: u8,
    ) -> i32 {
        match sps.pic_order_cnt_type {
            0 => self.compute_poc_type0(sps, header, nal_unit_type, nal_ref_idc),
            1 => self.compute_poc_type1(sps, header, nal_unit_type, nal_ref_idc),
            2 => Self::compute_poc_type2(header.frame_num, nal_unit_type, nal_ref_idc),
            _ => 0,
        }
    }

    /// POC type 0 (spec 8.2.1.1) — uses pic_order_cnt_lsb with MSB wrapping.
    fn compute_poc_type0(
        &mut self,
        sps: &Sps,
        header: &SliceHeader,
        nal_unit_type: NalUnitType,
        nal_ref_idc: u8,
    ) -> i32 {
        if nal_unit_type == NalUnitType::SliceIdr {
            self.prev_poc_msb = 0;
            self.prev_poc_lsb = 0;
            return 0;
        }

        let shift = (sps.log2_max_pic_order_cnt_lsb_minus4 + 4).min(31);
        let max_poc_lsb = 1u32 << shift;
        let poc_lsb = header.pic_order_cnt_lsb;

        let poc_msb = if poc_lsb < self.prev_poc_lsb
            && (self.prev_poc_lsb - poc_lsb) >= max_poc_lsb / 2
        {
            self.prev_poc_msb.wrapping_add(max_poc_lsb as i32)
        } else if poc_lsb > self.prev_poc_lsb && (poc_lsb - self.prev_poc_lsb) > max_poc_lsb / 2 {
            self.prev_poc_msb.wrapping_sub(max_poc_lsb as i32)
        } else {
            self.prev_poc_msb
        };

        let top_field_order_cnt = poc_msb.wrapping_add(poc_lsb as i32);
        let poc = if header.field_pic_flag {
            // Field picture: POC is just this field's order count
            if header.bottom_field_flag {
                top_field_order_cnt.wrapping_add(header.delta_pic_order_cnt_bottom)
            } else {
                top_field_order_cnt
            }
        } else {
            // Frame: PicOrderCnt = min(TopFieldOrderCnt, BottomFieldOrderCnt)
            let bottom_field_order_cnt =
                top_field_order_cnt.wrapping_add(header.delta_pic_order_cnt_bottom);
            top_field_order_cnt.min(bottom_field_order_cnt)
        };

        if nal_ref_idc > 0 {
            self.prev_poc_msb = poc_msb;
            self.prev_poc_lsb = poc_lsb;
        }

        poc
    }

    /// POC type 1 (spec 8.2.1.2) — uses delta_pic_order_cnt with cycle offsets.
    fn compute_poc_type1(
        &self,
        sps: &Sps,
        header: &SliceHeader,
        nal_unit_type: NalUnitType,
        nal_ref_idc: u8,
    ) -> i32 {
        if nal_unit_type == NalUnitType::SliceIdr {
            return 0;
        }

        let num_ref_frames_in_cycle = sps.num_ref_frames_in_pic_order_cnt_cycle as usize;
        let expected_delta_per_cycle: i32 = sps.offset_for_ref_frame.iter().sum();

        let abs_frame_num = if num_ref_frames_in_cycle > 0 {
            header.frame_num as i32
        } else {
            0
        };

        let expected_poc = if nal_ref_idc == 0 && abs_frame_num > 0 {
            let cycle = (abs_frame_num - 1) / num_ref_frames_in_cycle.max(1) as i32;
            let idx = ((abs_frame_num - 1) % num_ref_frames_in_cycle.max(1) as i32) as usize;
            let partial: i32 = sps.offset_for_ref_frame[..=idx]
                .iter()
                .copied()
                .fold(0i32, |a, b| a.wrapping_add(b));
            cycle
                .wrapping_mul(expected_delta_per_cycle)
                .wrapping_add(partial)
                .wrapping_add(sps.offset_for_non_ref_pic)
        } else if abs_frame_num > 0 {
            let cycle = (abs_frame_num - 1) / num_ref_frames_in_cycle.max(1) as i32;
            let idx = ((abs_frame_num - 1) % num_ref_frames_in_cycle.max(1) as i32) as usize;
            let partial: i32 = sps.offset_for_ref_frame[..=idx]
                .iter()
                .copied()
                .fold(0i32, |a, b| a.wrapping_add(b));
            cycle
                .wrapping_mul(expected_delta_per_cycle)
                .wrapping_add(partial)
        } else {
            0
        };

        let top_field_order_cnt = expected_poc + header.delta_pic_order_cnt[0];
        if header.field_pic_flag {
            // Field picture: POC is just this field's order count
            if header.bottom_field_flag {
                top_field_order_cnt
                    .wrapping_add(sps.offset_for_top_to_bottom_field)
                    .wrapping_add(header.delta_pic_order_cnt[1])
            } else {
                top_field_order_cnt
            }
        } else {
            // Frame: PicOrderCnt = min(TopFieldOrderCnt, BottomFieldOrderCnt)
            let bottom_field_order_cnt = top_field_order_cnt
                .wrapping_add(sps.offset_for_top_to_bottom_field)
                .wrapping_add(header.delta_pic_order_cnt[1]);
            top_field_order_cnt.min(bottom_field_order_cnt)
        }
    }

    /// POC type 2 (spec 8.2.1.3) — derived directly from frame_num.
    fn compute_poc_type2(frame_num: u32, nal_unit_type: NalUnitType, nal_ref_idc: u8) -> i32 {
        if nal_unit_type == NalUnitType::SliceIdr {
            0
        } else if nal_ref_idc > 0 {
            2 * frame_num as i32
        } else {
            2 * frame_num as i32 - 1
        }
    }

    /// Sliding window reference marking (spec 8.2.5.3).
    /// While short-term ref count >= max_ref_frames, mark the oldest as unused.
    fn sliding_window_mark(&mut self) {
        let max = self.max_ref_frames.max(1);
        while self.max_ref_frames > 0 {
            // Total ref count includes both short-term and long-term (spec 8.2.5.3).
            // For field pictures, two fields from the same frame_num count as ONE
            // toward the limit. Count distinct frame_nums among reference entries.
            let mut seen_frame_nums = std::collections::HashSet::new();
            for e in &self.entries {
                if e.reference != ReferenceStatus::Unused {
                    seen_frame_nums.insert(e.pic.frame_num);
                }
            }
            let total_ref_count = seen_frame_nums.len();
            if total_ref_count < max {
                break;
            }
            // Evict the oldest short-term reference (first in insertion order).
            // Using insertion order handles frame_num wraparound correctly
            // (spec 8.2.5.3: evict smallest FrameNumWrap, which is the oldest).
            if let Some(idx) = self
                .entries
                .iter()
                .position(|e| e.reference == ReferenceStatus::ShortTerm)
            {
                // Evict both fields of this frame_num (if any)
                let evict_fn = self.entries[idx].pic.frame_num;
                for e in &mut self.entries {
                    if e.reference == ReferenceStatus::ShortTerm && e.pic.frame_num == evict_fn {
                        e.reference = ReferenceStatus::Unused;
                    }
                }
            } else {
                break;
            }
        }
    }

    /// Mark a short-term reference as unused by frame_num (MMCO op=1).
    pub fn mark_short_term_unused(&mut self, frame_num: u32) {
        for entry in &mut self.entries {
            if entry.reference == ReferenceStatus::ShortTerm && entry.pic.frame_num == frame_num {
                entry.reference = ReferenceStatus::Unused;
                break;
            }
        }
        self.remove_unused();
    }

    /// Mark a long-term reference as unused by long_term_pic_num (MMCO op=2).
    pub fn mark_long_term_unused(&mut self, long_term_pic_num: u32) {
        for entry in &mut self.entries {
            if entry.reference == ReferenceStatus::LongTerm(long_term_pic_num) {
                entry.reference = ReferenceStatus::Unused;
                break;
            }
        }
        self.remove_unused();
    }

    /// Assign a short-term reference to long-term with given index (MMCO op=3).
    /// First marks any existing long-term with the same index as unused.
    pub fn assign_long_term(&mut self, frame_num: u32, long_term_frame_idx: u32) {
        // Remove any existing long-term with this index
        for entry in &mut self.entries {
            if entry.reference == ReferenceStatus::LongTerm(long_term_frame_idx) {
                entry.reference = ReferenceStatus::Unused;
            }
        }
        // Convert the short-term ref to long-term
        for entry in &mut self.entries {
            if entry.reference == ReferenceStatus::ShortTerm && entry.pic.frame_num == frame_num {
                entry.reference = ReferenceStatus::LongTerm(long_term_frame_idx);
                break;
            }
        }
        self.remove_unused();
    }

    /// Set max long-term frame index (MMCO op=4).
    /// All long-term refs with index > max are marked unused.
    /// max_long_term_frame_idx_plus1 = 0 means no long-term refs allowed.
    pub fn set_max_long_term_frame_idx(&mut self, max_long_term_frame_idx_plus1: u32) {
        if max_long_term_frame_idx_plus1 == 0 {
            // Mark ALL long-term refs as unused
            for entry in &mut self.entries {
                if matches!(entry.reference, ReferenceStatus::LongTerm(_)) {
                    entry.reference = ReferenceStatus::Unused;
                }
            }
        } else {
            let max_idx = max_long_term_frame_idx_plus1 - 1;
            for entry in &mut self.entries {
                if let ReferenceStatus::LongTerm(idx) = entry.reference {
                    if idx > max_idx {
                        entry.reference = ReferenceStatus::Unused;
                    }
                }
            }
        }
        self.remove_unused();
    }

    /// Clear all reference pictures (MMCO op=5).
    /// Marks all refs as unused and resets POC state.
    pub fn clear_all_refs(&mut self) {
        for entry in &mut self.entries {
            entry.reference = ReferenceStatus::Unused;
        }
        self.remove_unused();
        self.prev_poc_msb = 0;
        self.prev_poc_lsb = 0;
    }

    /// Insert current picture as long-term reference with given index (MMCO op=6).
    /// Called after the picture is decoded and inserted into DPB.
    /// The picture must already be in the DPB as ShortTerm.
    pub fn mark_current_as_long_term(&mut self, long_term_frame_idx: u32, frame_num: u32) {
        // Remove any existing long-term with this index
        for entry in &mut self.entries {
            if entry.reference == ReferenceStatus::LongTerm(long_term_frame_idx) {
                entry.reference = ReferenceStatus::Unused;
            }
        }
        // Find the just-inserted picture and change to long-term
        for entry in self.entries.iter_mut().rev() {
            if entry.reference == ReferenceStatus::ShortTerm && entry.pic.frame_num == frame_num {
                entry.reference = ReferenceStatus::LongTerm(long_term_frame_idx);
                break;
            }
        }
        self.remove_unused();
    }

    /// Get list of long-term reference pictures, sorted by ascending long_term_frame_idx.
    /// Used to append to reference lists after short-term refs (spec 8.2.4.2.1, 8.2.4.2.3).
    pub fn long_term_ref_list(&self) -> Vec<Rc<DecodedPicture>> {
        let mut refs: Vec<_> = self
            .entries
            .iter()
            .filter(|e| matches!(e.reference, ReferenceStatus::LongTerm(_)))
            .collect::<Vec<_>>();
        refs.sort_by_key(|e| match e.reference {
            ReferenceStatus::LongTerm(idx) => idx,
            _ => u32::MAX,
        });
        refs.iter().map(|e| e.pic.clone()).collect()
    }

    /// Remove entries that are unused for reference (freeing memory).
    fn remove_unused(&mut self) {
        self.entries
            .retain(|e| e.reference != ReferenceStatus::Unused);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_pic(frame_num: u32, poc: i32) -> Rc<DecodedPicture> {
        Rc::new(DecodedPicture {
            y: vec![],
            u: vec![],
            v: vec![],
            width: 16,
            height: 16,
            frame_num,
            pic_order_cnt: poc,
            mv_l0: vec![],
            ref_idx_l0: vec![],
            ref_poc_l0: vec![],
            mv_l1: vec![],
            ref_idx_l1: vec![],
            mb_width: 1,
            is_intra: false,
            structure: crate::dpb::PictureStructure::Frame,
            bit_depth_luma: 8,
            bit_depth_chroma: 8,
        })
    }

    #[test]
    fn test_dpb_insert_and_clear() {
        let mut dpb = Dpb::new(4);
        dpb.insert(make_pic(0, 0), ReferenceStatus::ShortTerm);
        dpb.insert(make_pic(1, 2), ReferenceStatus::ShortTerm);
        assert_eq!(dpb.short_term_ref_list(false, false).len(), 2);
        dpb.clear();
        assert_eq!(dpb.short_term_ref_list(false, false).len(), 0);
    }

    #[test]
    fn test_sliding_window() {
        let mut dpb = Dpb::new(2);
        dpb.insert(make_pic(0, 0), ReferenceStatus::ShortTerm);
        dpb.insert(make_pic(1, 2), ReferenceStatus::ShortTerm);
        assert_eq!(dpb.short_term_ref_list(false, false).len(), 2);

        // Third insert triggers sliding window — oldest (frame_num=0) gets evicted
        dpb.insert(make_pic(2, 4), ReferenceStatus::ShortTerm);
        let refs = dpb.short_term_ref_list(false, false);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].frame_num, 2); // newest first (descending)
        assert_eq!(refs[1].frame_num, 1);
    }

    #[test]
    fn test_poc_type2() {
        assert_eq!(Dpb::compute_poc_type2(0, NalUnitType::SliceIdr, 3), 0);
        assert_eq!(Dpb::compute_poc_type2(1, NalUnitType::Slice, 3), 2);
        assert_eq!(Dpb::compute_poc_type2(1, NalUnitType::Slice, 0), 1);
        assert_eq!(Dpb::compute_poc_type2(5, NalUnitType::Slice, 1), 10);
    }

    #[test]
    fn test_ref_list_sorted_descending() {
        let mut dpb = Dpb::new(4);
        dpb.insert(make_pic(3, 6), ReferenceStatus::ShortTerm);
        dpb.insert(make_pic(1, 2), ReferenceStatus::ShortTerm);
        dpb.insert(make_pic(5, 10), ReferenceStatus::ShortTerm);

        let refs = dpb.short_term_ref_list(false, false);
        assert_eq!(refs[0].frame_num, 5);
        assert_eq!(refs[1].frame_num, 3);
        assert_eq!(refs[2].frame_num, 1);
    }

    #[test]
    fn test_unused_not_in_ref_list() {
        let mut dpb = Dpb::new(4);
        dpb.insert(make_pic(0, 0), ReferenceStatus::ShortTerm);
        dpb.insert(make_pic(1, 2), ReferenceStatus::Unused); // non-reference
        assert_eq!(dpb.short_term_ref_list(false, false).len(), 1);
    }

    #[test]
    fn test_ref_list_l0_b() {
        let mut dpb = Dpb::new(5);
        // POCs: 0, 2, 4, 6, 8 — current POC is 5
        dpb.insert(make_pic(0, 0), ReferenceStatus::ShortTerm);
        dpb.insert(make_pic(1, 2), ReferenceStatus::ShortTerm);
        dpb.insert(make_pic(2, 4), ReferenceStatus::ShortTerm);
        dpb.insert(make_pic(3, 6), ReferenceStatus::ShortTerm);
        dpb.insert(make_pic(4, 8), ReferenceStatus::ShortTerm);

        let l0 = dpb.ref_list_l0_b(5, false, false);
        // Before (POC <= 5): 4, 2, 0 (descending POC)
        // After (POC > 5): 6, 8 (ascending POC)
        assert_eq!(l0.len(), 5);
        assert_eq!(l0[0].pic_order_cnt, 4);
        assert_eq!(l0[1].pic_order_cnt, 2);
        assert_eq!(l0[2].pic_order_cnt, 0);
        assert_eq!(l0[3].pic_order_cnt, 6);
        assert_eq!(l0[4].pic_order_cnt, 8);
    }

    #[test]
    fn test_ref_list_l1_b() {
        let mut dpb = Dpb::new(5);
        dpb.insert(make_pic(0, 0), ReferenceStatus::ShortTerm);
        dpb.insert(make_pic(1, 2), ReferenceStatus::ShortTerm);
        dpb.insert(make_pic(2, 4), ReferenceStatus::ShortTerm);
        dpb.insert(make_pic(3, 6), ReferenceStatus::ShortTerm);
        dpb.insert(make_pic(4, 8), ReferenceStatus::ShortTerm);

        let l1 = dpb.ref_list_l1_b(5, false, false);
        // After (POC > 5): 6, 8 (ascending POC)
        // Before (POC <= 5): 4, 2, 0 (descending POC)
        assert_eq!(l1.len(), 5);
        assert_eq!(l1[0].pic_order_cnt, 6);
        assert_eq!(l1[1].pic_order_cnt, 8);
        assert_eq!(l1[2].pic_order_cnt, 4);
        assert_eq!(l1[3].pic_order_cnt, 2);
        assert_eq!(l1[4].pic_order_cnt, 0);
    }

    /// Regression test for a fuzz-discovered panic.
    ///
    /// `apply_ref_list_modification` panicked with "index out of bounds"
    /// when a malformed bitstream contained more RPLM commands than there
    /// were active reference slots. `ref_idx_lx` grew past `num_active`,
    /// causing an out-of-bounds index after the shift-right + push.
    ///
    /// Fix: bail out of the loop when `ref_idx_lx >= num_active`.
    #[test]
    fn test_fuzz_regression_rplm_too_many_ops_no_panic() {
        // Build a ref list with 2 entries
        let mut ref_list = vec![make_pic(0, 0), make_pic(1, 2)];
        // Feed 10 RPLM ops (far more than the 2 active slots)
        // idc=0 means "subtract abs_diff_pic_num from pred_pic_num"
        // With val=0 → abs_diff=1 → pic_num = pred - 1
        let ops: Vec<(u32, u32)> = (0..10).map(|_| (0u32, 0u32)).collect();
        // This must not panic
        Dpb::apply_ref_list_modification(&mut ref_list, &ops, 5, 16, false, false);
    }
}
