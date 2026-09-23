//! H.264 SPS parsing: just enough of the sequence parameter set to read the coded picture size.

/// Remove H.264 emulation-prevention bytes: any 0x03 that follows two 0x00s is an escape,
/// not data.
pub(super) fn strip_emulation_prevention(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.len());
    let mut zeros = 0u32;
    for &x in b {
        if zeros >= 2 && x == 3 {
            zeros = 0;
            continue;
        }
        zeros = if x == 0 { zeros + 1 } else { 0 };
        out.push(x);
    }
    out
}

/// MSB-first bit reader over a byte slice. Every read is bounds-checked (`None` past the
/// end), so a truncated input aborts the parse instead of reading garbage.
///
/// `pub`: reused by `vdec::vp9`'s VP9 keyframe-header prologue reader
/// (`sagethumbs2k_core::flv::Bits`) — the child binary's own private copy used to drift from
/// this one independently. Fields stay private; construct via [`Bits::new`].
pub struct Bits<'a> {
    pub(super) d: &'a [u8],
    pub(super) pos: usize, // in bits
}

impl<'a> Bits<'a> {
    pub fn new(d: &'a [u8]) -> Self {
        Bits { d, pos: 0 }
    }

    pub fn bit(&mut self) -> Option<u32> {
        let byte = *self.d.get(self.pos / 8)?;
        let b = (byte >> (7 - (self.pos % 8))) & 1;
        self.pos = self.pos.checked_add(1)?;
        Some(u32::from(b))
    }

    pub fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n.min(32) {
            v = (v << 1) | self.bit()?;
        }
        Some(v)
    }

    /// Exp-Golomb unsigned: count leading zeros, then read that many bits.
    pub(super) fn ue(&mut self) -> Option<u32> {
        let mut leading = 0u32;
        while self.bit()? == 0 {
            leading += 1;
            if leading > 31 {
                return None; // no valid SPS field needs codes this long
            }
        }
        let rest = self.bits(leading)?;
        1u32.checked_shl(leading)?.checked_sub(1)?.checked_add(rest)
    }

    /// Exp-Golomb signed, in i64 so the mapping can't overflow on hostile input.
    pub(super) fn se(&mut self) -> Option<i64> {
        let k = i64::from(self.ue()?);
        Some(if k % 2 == 0 { -(k / 2) } else { (k + 1) / 2 })
    }
}

/// Skip one scaling list (ITU-T H.264 §7.3.2.1.1.1) — the values are irrelevant here, but
/// the delta run must be consumed exactly for everything after it to line up.
pub(super) fn skip_scaling_list(b: &mut Bits, size: u32) -> Option<()> {
    let mut last: i64 = 8;
    let mut next: i64 = 8;
    for _ in 0..size {
        if next != 0 {
            let delta = b.se()?;
            next = (last + delta).rem_euclid(256);
        }
        if next != 0 {
            last = next;
        }
    }
    Some(())
}

/// The chroma-format prelude gated on `profile_idc` (§7.3.2.1.1: only certain High-profile
/// variants carry it): reads `chroma_format_idc`, `separate_colour_planes`, and any scaling
/// matrix (skipped via [`skip_scaling_list`], not decoded — the values are irrelevant here).
/// Consumes those bits from `b` only when the profile actually carries them.
/// `seq_scaling_matrix_present`'s list walk: 12 lists for 4:4:4 (`chroma_format_idc == 3`),
/// else 8, each optionally present and skipped via [`skip_scaling_list`] (16 entries for the
/// first six lists, 64 for the rest).
pub(super) fn skip_scaling_matrix(b: &mut Bits, chroma_format_idc: u32) -> Option<()> {
    let lists = if chroma_format_idc == 3 { 12 } else { 8 };
    for i in 0..lists {
        if b.bit()? == 1 {
            skip_scaling_list(b, if i < 6 { 16 } else { 64 })?;
        }
    }
    Some(())
}

/// The chroma-format + optional scaling-matrix fields (§7.3.2.1.1), read only when
/// [`parse_chroma_format`]'s profile check says this profile carries them.
pub(super) fn parse_chroma_and_scaling_fields(b: &mut Bits) -> Option<(u32, bool)> {
    let chroma_format_idc = b.ue()?;
    if chroma_format_idc > 3 {
        return None;
    }
    let separate_colour_planes = if chroma_format_idc == 3 {
        b.bit()? == 1
    } else {
        false
    };
    b.ue()?; // bit_depth_luma_minus8
    b.ue()?; // bit_depth_chroma_minus8
    b.bit()?; // qpprime_y_zero_transform_bypass_flag
    if b.bit()? == 1 {
        skip_scaling_matrix(b, chroma_format_idc)?; // seq_scaling_matrix_present
    }
    Some((chroma_format_idc, separate_colour_planes))
}

pub(super) fn parse_chroma_format(b: &mut Bits, profile_idc: u32) -> Option<(u32, bool)> {
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        parse_chroma_and_scaling_fields(b)
    } else {
        Some((1, false)) // 4:2:0 unless the profile carries it explicitly
    }
}

/// The picture-order-count fields (§7.3.2.1.1). Consumed but not returned — only their
/// effect on `b`'s bit position matters to the fields that follow.
pub(super) fn skip_pic_order_cnt_fields(b: &mut Bits) -> Option<()> {
    b.ue()?; // log2_max_frame_num_minus4
    let pic_order_cnt_type = b.ue()?;
    if pic_order_cnt_type == 0 {
        b.ue()?; // log2_max_pic_order_cnt_lsb_minus4
    } else if pic_order_cnt_type == 1 {
        skip_poc_type1_fields(b)?;
    } else if pic_order_cnt_type > 2 {
        return None;
    }
    Some(())
}

/// The `pic_order_cnt_type == 1` fields (§7.3.2.1.1): a flag, two signed offsets, and the
/// `num_ref_frames_in_pic_order_cnt_cycle`-sized offset run.
pub(super) fn skip_poc_type1_fields(b: &mut Bits) -> Option<()> {
    b.bit()?; // delta_pic_order_always_zero_flag
    b.se()?; // offset_for_non_ref_pic
    b.se()?; // offset_for_top_to_bottom_field
    let n = b.ue()?;
    if n > 255 {
        return None; // spec caps num_ref_frames_in_pic_order_cnt_cycle at 255
    }
    for _ in 0..n {
        b.se()?;
    }
    Some(())
}

/// The frame-size fields (§7.3.2.1.1): mb-unit dimensions, `frame_mbs_only_flag`, and the
/// optional frame-cropping rectangle, in bitstream order.
pub(super) struct FrameGeomFields {
    pub(super) pic_width_in_mbs_minus1: u32,
    pub(super) pic_height_in_map_units_minus1: u32,
    pub(super) frame_mbs_only: u32,
    pub(super) crop: (u32, u32, u32, u32), // (left, right, top, bottom)
}

pub(super) fn parse_frame_geom_fields(b: &mut Bits) -> Option<FrameGeomFields> {
    let pic_width_in_mbs_minus1 = b.ue()?;
    let pic_height_in_map_units_minus1 = b.ue()?;
    let frame_mbs_only = b.bit()?;
    if frame_mbs_only == 0 {
        b.bit()?; // mb_adaptive_frame_field_flag
    }
    b.bit()?; // direct_8x8_inference_flag

    let crop = parse_frame_crop(b)?;
    Some(FrameGeomFields {
        pic_width_in_mbs_minus1,
        pic_height_in_map_units_minus1,
        frame_mbs_only,
        crop,
    })
}

/// The optional `frame_cropping_flag` rectangle (§7.3.2.1.1) as `(left, right, top, bottom)`,
/// all zero when the flag is absent.
pub(super) fn parse_frame_crop(b: &mut Bits) -> Option<(u32, u32, u32, u32)> {
    if b.bit()? == 1 {
        return Some((b.ue()?, b.ue()?, b.ue()?, b.ue()?));
    }
    Some((0, 0, 0, 0))
}

/// Pixel width/height from the mb-unit fields + crop rectangle (§7.4.2.1.1). Crop units
/// scale with the chroma sampling: SubWidthC/SubHeightC for 4:2:0/4:2:2, unity for
/// monochrome, 4:4:4, and separate colour planes.
pub(super) fn frame_geom_to_pixels(
    g: &FrameGeomFields,
    chroma_format_idc: u32,
    separate_colour_planes: bool,
) -> Option<(u16, u16)> {
    let width_px = g.pic_width_in_mbs_minus1.checked_add(1)?.checked_mul(16)?;
    let height_px = g
        .pic_height_in_map_units_minus1
        .checked_add(1)?
        .checked_mul(16)?
        .checked_mul(2u32.checked_sub(g.frame_mbs_only)?)?;

    let (unit_x, unit_y) =
        chroma_crop_units(chroma_format_idc, separate_colour_planes, g.frame_mbs_only)?;

    let (crop_l, crop_r, crop_t, crop_b) = g.crop;
    let w = width_px.checked_sub(crop_l.checked_add(crop_r)?.checked_mul(unit_x)?)?;
    let h = height_px.checked_sub(crop_t.checked_add(crop_b)?.checked_mul(unit_y)?)?;
    if !(1..=16384).contains(&w) || !(1..=16384).contains(&h) {
        return None;
    }
    Some((w as u16, h as u16))
}

/// Crop-unit scale factors `(SubWidthC, SubHeightC)` for the chroma array type, with the
/// field-height doubling folded in (`unit_y` is doubled when the frame is not frame-only).
pub(super) fn chroma_crop_units(
    chroma_format_idc: u32,
    separate_colour_planes: bool,
    frame_mbs_only: u32,
) -> Option<(u32, u32)> {
    let chroma_array_type = if separate_colour_planes {
        0
    } else {
        chroma_format_idc
    };
    let (unit_x, unit_y_base) = match chroma_array_type {
        1 => (2u32, 2u32), // 4:2:0
        2 => (2, 1),       // 4:2:2
        _ => (1, 1),       // mono / 4:4:4 / separate planes
    };
    let unit_y = unit_y_base.checked_mul(2u32.checked_sub(frame_mbs_only)?)?;
    Some((unit_x, unit_y))
}

/// Frame geometry from an SPS RBSP (ITU-T H.264 §7.3.2.1.1): walk every field ahead of
/// `pic_width_in_mbs_minus1`, apply the frame-cropping rectangle in chroma-scaled units.
pub(super) fn parse_sps(rbsp: &[u8]) -> Option<(u16, u16)> {
    let mut b = Bits { d: rbsp, pos: 0 };
    let profile_idc = b.bits(8)?;
    b.bits(8)?; // constraint_set flags + reserved
    b.bits(8)?; // level_idc
    b.ue()?; // seq_parameter_set_id

    let (chroma_format_idc, separate_colour_planes) = parse_chroma_format(&mut b, profile_idc)?;
    skip_pic_order_cnt_fields(&mut b)?;
    b.ue()?; // max_num_ref_frames
    b.bit()?; // gaps_in_frame_num_value_allowed_flag

    let geom = parse_frame_geom_fields(&mut b)?;
    frame_geom_to_pixels(&geom, chroma_format_idc, separate_colour_planes)
}
