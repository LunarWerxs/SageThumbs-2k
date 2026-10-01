use crate::bitstream::BitstreamReader;

/// H.264 Table 7-2: Default 4x4 scaling list for Intra (in scan order).
#[rustfmt::skip]
pub const DEFAULT_SCALING_4X4_INTRA: [u8; 16] = [
     6, 13, 13, 20,
    20, 20, 28, 28,
    28, 28, 32, 32,
    32, 37, 37, 42,
];

/// H.264 Table 7-2: Default 4x4 scaling list for Inter (in scan order).
#[rustfmt::skip]
pub const DEFAULT_SCALING_4X4_INTER: [u8; 16] = [
    10, 14, 14, 20,
    20, 20, 24, 24,
    24, 24, 27, 27,
    27, 30, 30, 34,
];

/// Flat scaling list (no custom scaling) — all 16s.
pub const FLAT_SCALING_4X4: [u8; 16] = [16; 16];

/// Sequence Parameter Set (H.264 spec section 7.3.2.1).
#[derive(Debug)]
pub struct Sps {
    pub profile_idc: u8,
    pub constraint_set0_flag: bool,
    pub constraint_set1_flag: bool,
    pub constraint_set2_flag: bool,
    pub constraint_set3_flag: bool,
    pub constraint_set4_flag: bool,
    pub constraint_set5_flag: bool,
    pub level_idc: u8,
    pub seq_parameter_set_id: u32,

    // High profile and above fields
    pub chroma_format_idc: u32,
    pub separate_colour_plane_flag: bool,
    pub bit_depth_luma_minus8: u32,
    pub bit_depth_chroma_minus8: u32,
    pub qpprime_y_zero_transform_bypass_flag: bool,
    pub seq_scaling_matrix_present_flag: bool,
    /// 4x4 scaling matrices [0..5]: Intra Y, Intra Cb, Intra Cr, Inter Y, Inter Cb, Inter Cr.
    /// Default is all 16s (flat scaling). Stored in raster scan order within each 4x4 block.
    pub scaling_list_4x4: [[u8; 16]; 6],
    /// 8x8 scaling matrices [0..1]: Intra Y, Inter Y (for 4:2:0).
    /// Default is all 16s. Stored in raster scan order within each 8x8 block.
    pub scaling_list_8x8: [[u8; 64]; 2],

    pub log2_max_frame_num_minus4: u32,
    pub pic_order_cnt_type: u32,
    pub log2_max_pic_order_cnt_lsb_minus4: u32,
    pub delta_pic_order_always_zero_flag: bool,
    pub offset_for_non_ref_pic: i32,
    pub offset_for_top_to_bottom_field: i32,
    pub num_ref_frames_in_pic_order_cnt_cycle: u32,
    pub offset_for_ref_frame: Vec<i32>,

    pub max_num_ref_frames: u32,
    pub gaps_in_frame_num_value_allowed_flag: bool,
    pub pic_width_in_mbs_minus1: u32,
    pub pic_height_in_map_units_minus1: u32,
    pub frame_mbs_only_flag: bool,
    pub mb_adaptive_frame_field_flag: bool,
    pub direct_8x8_inference_flag: bool,

    pub frame_cropping_flag: bool,
    pub frame_crop_left_offset: u32,
    pub frame_crop_right_offset: u32,
    pub frame_crop_top_offset: u32,
    pub frame_crop_bottom_offset: u32,

    pub vui_parameters_present_flag: bool,
    /// VUI sample aspect ratio (E.2.1, Table E-1) as `(width, height)`, `(1, 1)` for square
    /// pixels and when the VUI does not say (or says 0, "unspecified").
    pub sample_aspect_ratio: (u16, u16),
    /// VUI `video_full_range_flag` (E.2.1): samples use the full 0..2^bd-1 range rather than
    /// studio swing. `false` when the VUI or its video signal type is absent.
    pub video_full_range_flag: bool,
    /// VUI `matrix_coefficients` (E.2.1, ITU-T H.273): 1 = BT.709, 5/6 = BT.601, 9 = BT.2020
    /// non-constant, 2 = unspecified (also the value when the VUI does not carry one).
    pub matrix_coefficients: u8,

    /// VUI timing info: numerator of clock tick. Set when
    /// `vui_parameters_present_flag` and `timing_info_present_flag` are both
    /// true. The frame rate is `time_scale / (2 * num_units_in_tick)` for
    /// progressive content (spec E.2.1).
    pub num_units_in_tick: Option<u32>,
    /// VUI timing info: denominator of clock tick (typically a multiple of
    /// the frame rate, e.g. 60000 for 29.97 fps).
    pub time_scale: Option<u32>,
    /// VUI timing info: when set, frames are emitted at a fixed rate of
    /// `time_scale / (2 * num_units_in_tick)`.
    pub fixed_frame_rate_flag: bool,
}

impl Sps {
    /// Frame rate from VUI timing info, if present, as `(numerator, denominator)`.
    ///
    /// For progressive content (the common case), this is
    /// `time_scale / (2 * num_units_in_tick)`. For example, an x264 stream
    /// at 29.97 fps would return `Some((60000, 2002))`.
    ///
    /// Returns `None` if the SPS does not contain VUI timing info, or if
    /// the values are zero (which would be a division-by-zero).
    pub fn frame_rate(&self) -> Option<(u32, u32)> {
        let num_units = self.num_units_in_tick?;
        let time_scale = self.time_scale?;
        if num_units == 0 || time_scale == 0 {
            return None;
        }
        // Spec E.2.1: clock_tick = num_units_in_tick / time_scale
        // For progressive frame coding, each frame is 2 ticks → fps = time_scale / (2 * num_units_in_tick)
        Some((time_scale, 2u32.saturating_mul(num_units)))
    }

    /// Frame rate as a single floating-point value, computed from
    /// [`frame_rate`](Self::frame_rate). Returns `None` if VUI timing info
    /// is not present.
    pub fn frame_rate_f64(&self) -> Option<f64> {
        let (num, den) = self.frame_rate()?;
        Some(num as f64 / den as f64)
    }

    /// Width in pixels (accounting for cropping).
    pub fn width(&self) -> u32 {
        let crop_x = if self.frame_cropping_flag {
            self.frame_crop_left_offset
                .saturating_add(self.frame_crop_right_offset)
                .saturating_mul(self.crop_unit_x())
        } else {
            0
        };
        self.pic_width_in_mbs_minus1
            .saturating_add(1)
            .saturating_mul(16)
            .saturating_sub(crop_x)
    }

    /// Height in pixels (accounting for cropping).
    pub fn height(&self) -> u32 {
        let crop_y = if self.frame_cropping_flag {
            self.frame_crop_top_offset
                .saturating_add(self.frame_crop_bottom_offset)
                .saturating_mul(self.crop_unit_y())
        } else {
            0
        };
        self.pic_height_in_map_units_minus1
            .saturating_add(1)
            .saturating_mul(16)
            .saturating_mul(if self.frame_mbs_only_flag { 1 } else { 2 })
            .saturating_sub(crop_y)
    }

    fn crop_unit_x(&self) -> u32 {
        if self.chroma_format_idc == 0 {
            1
        } else {
            // SubWidthC: 2 for 4:2:0 and 4:2:2, 1 for 4:4:4
            if self.chroma_format_idc == 3 {
                1
            } else {
                2
            }
        }
    }

    fn crop_unit_y(&self) -> u32 {
        let sub_height_c = if self.chroma_format_idc == 1 { 2 } else { 1 };
        let frame_factor = if self.frame_mbs_only_flag { 1 } else { 2 };
        if self.chroma_format_idc == 0 {
            frame_factor
        } else {
            sub_height_c * frame_factor
        }
    }
}

fn is_high_profile(profile_idc: u8) -> bool {
    matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    )
}

/// Table E-1: `aspect_ratio_idc` 1..=16 as (sar_width, sar_height); index 0 is
/// "unspecified", read as square.
const SAR_TABLE: [(u16, u16); 17] = [
    (1, 1),
    (1, 1),
    (12, 11),
    (10, 11),
    (16, 11),
    (40, 33),
    (24, 11),
    (20, 11),
    (32, 11),
    (80, 33),
    (18, 11),
    (15, 11),
    (64, 33),
    (160, 99),
    (4, 3),
    (3, 2),
    (2, 1),
];

/// Parse an SPS from RBSP data (NAL header byte already stripped).
pub fn parse_sps(rbsp: &[u8]) -> Result<Sps, &'static str> {
    let mut r = BitstreamReader::new(rbsp);

    let profile_idc = r.read_bits(8)? as u8;
    let constraint_set0_flag = r.read_bit()? != 0;
    let constraint_set1_flag = r.read_bit()? != 0;
    let constraint_set2_flag = r.read_bit()? != 0;
    let constraint_set3_flag = r.read_bit()? != 0;
    let constraint_set4_flag = r.read_bit()? != 0;
    let constraint_set5_flag = r.read_bit()? != 0;
    let _reserved_zero_2bits = r.read_bits(2)?;
    let level_idc = r.read_bits(8)? as u8;
    let seq_parameter_set_id = r.read_ue()?;

    let mut chroma_format_idc = 1; // default
    let mut separate_colour_plane_flag = false;
    let mut bit_depth_luma_minus8 = 0;
    let mut bit_depth_chroma_minus8 = 0;
    let mut qpprime_y_zero_transform_bypass_flag = false;
    let mut seq_scaling_matrix_present_flag = false;
    // Default: flat scaling (all 16s) when seq_scaling_matrix_present_flag is false
    let mut scaling_list_4x4 = [FLAT_SCALING_4X4; 6];
    let mut scaling_list_8x8 = [[16u8; 64]; 2];

    if is_high_profile(profile_idc) {
        chroma_format_idc = r.read_ue()?;
        if chroma_format_idc == 3 {
            separate_colour_plane_flag = r.read_bit()? != 0;
        }
        bit_depth_luma_minus8 = r.read_ue()?;
        bit_depth_chroma_minus8 = r.read_ue()?;
        qpprime_y_zero_transform_bypass_flag = r.read_bit()? != 0;
        seq_scaling_matrix_present_flag = r.read_bit()? != 0;
        if seq_scaling_matrix_present_flag {
            let count = if chroma_format_idc != 3 { 8 } else { 12 };
            for i in 0..count {
                let present = r.read_bit()? != 0;
                if present {
                    if i < 6 {
                        scaling_list_4x4[i] = parse_scaling_list::<16>(&mut r, 16)?;
                    } else if i < 8 {
                        scaling_list_8x8[i - 6] = parse_scaling_list::<64>(&mut r, 64)?;
                    } else {
                        let _: [u8; 64] = parse_scaling_list::<64>(&mut r, 64)?;
                    }
                } else if i < 6 {
                    // Fallback per H.264 Table 7-2:
                    // i=0: Default_4x4_Intra, i=3: Default_4x4_Inter
                    // i=1,2: copy from previous, i=4,5: copy from previous
                    scaling_list_4x4[i] = match i {
                        0 => DEFAULT_SCALING_4X4_INTRA,
                        3 => DEFAULT_SCALING_4X4_INTER,
                        _ => scaling_list_4x4[i - 1],
                    };
                }
            }
        }
    }

    let log2_max_frame_num_minus4 = r.read_ue()?;
    let pic_order_cnt_type = r.read_ue()?;

    let mut log2_max_pic_order_cnt_lsb_minus4 = 0;
    let mut delta_pic_order_always_zero_flag = false;
    let mut offset_for_non_ref_pic = 0;
    let mut offset_for_top_to_bottom_field = 0;
    let mut num_ref_frames_in_pic_order_cnt_cycle = 0;
    let mut offset_for_ref_frame = Vec::new();

    if pic_order_cnt_type == 0 {
        log2_max_pic_order_cnt_lsb_minus4 = r.read_ue()?;
    } else if pic_order_cnt_type == 1 {
        delta_pic_order_always_zero_flag = r.read_bit()? != 0;
        offset_for_non_ref_pic = r.read_se()?;
        offset_for_top_to_bottom_field = r.read_se()?;
        num_ref_frames_in_pic_order_cnt_cycle = r.read_ue()?;
        for _ in 0..num_ref_frames_in_pic_order_cnt_cycle {
            offset_for_ref_frame.push(r.read_se()?);
        }
    }

    let max_num_ref_frames = r.read_ue()?;
    let gaps_in_frame_num_value_allowed_flag = r.read_bit()? != 0;
    let pic_width_in_mbs_minus1 = r.read_ue()?;
    let pic_height_in_map_units_minus1 = r.read_ue()?;
    let frame_mbs_only_flag = r.read_bit()? != 0;

    let mut mb_adaptive_frame_field_flag = false;
    if !frame_mbs_only_flag {
        mb_adaptive_frame_field_flag = r.read_bit()? != 0;
    }

    let direct_8x8_inference_flag = r.read_bit()? != 0;

    let frame_cropping_flag = r.read_bit()? != 0;
    let mut frame_crop_left_offset = 0;
    let mut frame_crop_right_offset = 0;
    let mut frame_crop_top_offset = 0;
    let mut frame_crop_bottom_offset = 0;
    if frame_cropping_flag {
        frame_crop_left_offset = r.read_ue()?;
        frame_crop_right_offset = r.read_ue()?;
        frame_crop_top_offset = r.read_ue()?;
        frame_crop_bottom_offset = r.read_ue()?;
    }

    let vui_parameters_present_flag = r.read_bit()? != 0;
    let mut num_units_in_tick: Option<u32> = None;
    let mut time_scale: Option<u32> = None;
    let mut fixed_frame_rate_flag = false;
    let mut video_full_range_flag = false;
    let mut sample_aspect_ratio = (1u16, 1u16);
    let mut matrix_coefficients = 2u8;
    if vui_parameters_present_flag {
        // Parse just enough of the VUI to extract timing info (spec E.1.1).
        // We read the early optional sub-flags so we can reach
        // timing_info_present_flag, then parse the timing fields.
        let aspect_ratio_info_present_flag = r.read_bit()? != 0;
        if aspect_ratio_info_present_flag {
            let aspect_ratio_idc = r.read_bits(8)? as u8;
            if aspect_ratio_idc == 255 {
                // Extended_SAR: sar_width (16) + sar_height (16)
                let w = r.read_bits(16)? as u16;
                let h = r.read_bits(16)? as u16;
                if w != 0 && h != 0 {
                    sample_aspect_ratio = (w, h);
                }
            } else if let Some(&sar) = SAR_TABLE.get(aspect_ratio_idc as usize) {
                sample_aspect_ratio = sar;
            }
        }
        let overscan_info_present_flag = r.read_bit()? != 0;
        if overscan_info_present_flag {
            r.skip_bits(1); // overscan_appropriate_flag
        }
        let video_signal_type_present_flag = r.read_bit()? != 0;
        if video_signal_type_present_flag {
            r.skip_bits(3); // video_format
            video_full_range_flag = r.read_bit()? != 0;
            let colour_description_present_flag = r.read_bit()? != 0;
            if colour_description_present_flag {
                r.skip_bits(8); // colour_primaries
                r.skip_bits(8); // transfer_characteristics
                matrix_coefficients = r.read_bits(8)? as u8;
            }
        }
        let chroma_loc_info_present_flag = r.read_bit()? != 0;
        if chroma_loc_info_present_flag {
            let _ = r.read_ue()?; // chroma_sample_loc_type_top_field
            let _ = r.read_ue()?; // chroma_sample_loc_type_bottom_field
        }
        let timing_info_present_flag = r.read_bit()? != 0;
        if timing_info_present_flag {
            // Both fields are 32 bits — read in two 16-bit halves since
            // read_bits takes u8 and we want to be safe with bit ordering.
            let hi = r.read_bits(16)?;
            let lo = r.read_bits(16)?;
            num_units_in_tick = Some((hi << 16) | lo);
            let hi = r.read_bits(16)?;
            let lo = r.read_bits(16)?;
            time_scale = Some((hi << 16) | lo);
            fixed_frame_rate_flag = r.read_bit()? != 0;
        }
        // The remaining VUI fields (HRD, bitstream_restriction, etc.) are
        // not used by this decoder, so we stop parsing here.
    }

    Ok(Sps {
        profile_idc,
        constraint_set0_flag,
        constraint_set1_flag,
        constraint_set2_flag,
        constraint_set3_flag,
        constraint_set4_flag,
        constraint_set5_flag,
        level_idc,
        seq_parameter_set_id,
        chroma_format_idc,
        separate_colour_plane_flag,
        bit_depth_luma_minus8,
        bit_depth_chroma_minus8,
        qpprime_y_zero_transform_bypass_flag,
        seq_scaling_matrix_present_flag,
        scaling_list_4x4,
        scaling_list_8x8,
        log2_max_frame_num_minus4,
        pic_order_cnt_type,
        log2_max_pic_order_cnt_lsb_minus4,
        delta_pic_order_always_zero_flag,
        offset_for_non_ref_pic,
        offset_for_top_to_bottom_field,
        num_ref_frames_in_pic_order_cnt_cycle,
        offset_for_ref_frame,
        max_num_ref_frames,
        gaps_in_frame_num_value_allowed_flag,
        pic_width_in_mbs_minus1,
        pic_height_in_map_units_minus1,
        frame_mbs_only_flag,
        mb_adaptive_frame_field_flag,
        direct_8x8_inference_flag,
        frame_cropping_flag,
        frame_crop_left_offset,
        frame_crop_right_offset,
        frame_crop_top_offset,
        frame_crop_bottom_offset,
        vui_parameters_present_flag,
        sample_aspect_ratio,
        video_full_range_flag,
        matrix_coefficients,
        num_units_in_tick,
        time_scale,
        fixed_frame_rate_flag,
    })
}

/// Parse a scaling list from the bitstream (H.264 spec 7.3.2.1.1).
/// Returns a flat array of `size` scale values in scan order.
pub fn parse_scaling_list<const N: usize>(
    r: &mut BitstreamReader,
    size: usize,
) -> Result<[u8; N], &'static str> {
    let mut scaling_list = [0u8; N];
    let mut last_scale: i32 = 8;
    let mut next_scale: i32 = 8;
    for entry in scaling_list.iter_mut().take(size) {
        if next_scale != 0 {
            let delta = r.read_se()?;
            next_scale = (last_scale.wrapping_add(delta).wrapping_add(256)).rem_euclid(256);
        }
        let val = if next_scale == 0 {
            last_scale
        } else {
            next_scale
        };
        *entry = val as u8;
        last_scale = val;
    }
    Ok(scaling_list)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nal::{parse_annex_b, NalUnitType};

    #[test]
    fn test_parse_sps_single_frame() {
        let data = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/testdata/single_frame.h264"
        ))
        .unwrap();
        let nals = parse_annex_b(&data);
        let sps_nal = nals
            .iter()
            .find(|n| n.nal_unit_type == NalUnitType::Sps)
            .unwrap();
        let sps = parse_sps(&sps_nal.rbsp).unwrap();

        assert_eq!(sps.profile_idc, 66); // Baseline
        assert!(sps.constraint_set1_flag); // Constrained Baseline
        assert_eq!(sps.level_idc, 10); // Level 1.0
        assert_eq!(sps.seq_parameter_set_id, 0);
        assert_eq!(sps.chroma_format_idc, 1); // 4:2:0 (default for baseline)
        assert_eq!(sps.width(), 16);
        assert_eq!(sps.height(), 16);
        assert!(sps.frame_mbs_only_flag);
        assert_eq!(sps.max_num_ref_frames, 0);
        assert_eq!(sps.pic_order_cnt_type, 2);
    }

    #[test]
    fn test_parse_sps_vui_frame_rate() {
        // preset_medium.h264's SPS contains VUI timing info with
        // num_units_in_tick=1, time_scale=60, which gives a frame rate of
        // 60 / (2 * 1) = 30 fps per spec E.2.1. (Note that ffprobe's
        // avg_frame_rate is derived from frame count / duration and may
        // differ — this test verifies the SPS parser, not the avg rate.)
        let data = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/testdata/preset_medium.h264"
        ))
        .unwrap();
        let nals = parse_annex_b(&data);
        let sps_nal = nals
            .iter()
            .find(|n| n.nal_unit_type == NalUnitType::Sps)
            .unwrap();
        let sps = parse_sps(&sps_nal.rbsp).unwrap();

        assert!(sps.vui_parameters_present_flag);
        assert_eq!(sps.num_units_in_tick, Some(1));
        assert_eq!(sps.time_scale, Some(60));
        let (num, den) = sps.frame_rate().expect("expected VUI timing info");
        let fps = num as f64 / den as f64;
        assert!(
            (fps - 30.0).abs() < 1e-6,
            "expected 30 fps, got {} ({}/{})",
            fps,
            num,
            den
        );
        assert_eq!(sps.frame_rate_f64(), Some(30.0));
    }

    #[test]
    fn test_frame_rate_returns_none_without_timing() {
        // Construct a synthetic Sps with no timing info to verify
        // frame_rate() returns None gracefully.
        let mut sps = Sps {
            profile_idc: 66,
            constraint_set0_flag: false,
            constraint_set1_flag: false,
            constraint_set2_flag: false,
            constraint_set3_flag: false,
            constraint_set4_flag: false,
            constraint_set5_flag: false,
            level_idc: 30,
            seq_parameter_set_id: 0,
            chroma_format_idc: 1,
            separate_colour_plane_flag: false,
            bit_depth_luma_minus8: 0,
            bit_depth_chroma_minus8: 0,
            qpprime_y_zero_transform_bypass_flag: false,
            seq_scaling_matrix_present_flag: false,
            scaling_list_4x4: [[16; 16]; 6],
            scaling_list_8x8: [[16; 64]; 2],
            log2_max_frame_num_minus4: 0,
            pic_order_cnt_type: 0,
            log2_max_pic_order_cnt_lsb_minus4: 0,
            delta_pic_order_always_zero_flag: false,
            offset_for_non_ref_pic: 0,
            offset_for_top_to_bottom_field: 0,
            num_ref_frames_in_pic_order_cnt_cycle: 0,
            offset_for_ref_frame: vec![],
            max_num_ref_frames: 1,
            gaps_in_frame_num_value_allowed_flag: false,
            pic_width_in_mbs_minus1: 0,
            pic_height_in_map_units_minus1: 0,
            frame_mbs_only_flag: true,
            mb_adaptive_frame_field_flag: false,
            direct_8x8_inference_flag: true,
            frame_cropping_flag: false,
            frame_crop_left_offset: 0,
            frame_crop_right_offset: 0,
            frame_crop_top_offset: 0,
            frame_crop_bottom_offset: 0,
            vui_parameters_present_flag: false,
            sample_aspect_ratio: (1, 1),
            video_full_range_flag: false,
            matrix_coefficients: 2,
            num_units_in_tick: None,
            time_scale: None,
            fixed_frame_rate_flag: false,
        };
        assert!(sps.frame_rate().is_none());
        assert!(sps.frame_rate_f64().is_none());

        // Also: zero values should yield None (avoid division by zero)
        sps.num_units_in_tick = Some(0);
        sps.time_scale = Some(60);
        assert!(sps.frame_rate().is_none());
    }
}
