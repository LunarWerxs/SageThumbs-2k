use crate::bitstream::BitstreamReader;
use crate::sps::Sps;

/// Picture Parameter Set (H.264 spec section 7.3.2.2).
#[derive(Debug)]
pub struct Pps {
    pub pic_parameter_set_id: u32,
    pub seq_parameter_set_id: u32,
    pub entropy_coding_mode_flag: bool,
    pub bottom_field_pic_order_in_frame_present_flag: bool,
    pub num_slice_groups_minus1: u32,
    pub num_ref_idx_l0_default_active_minus1: u32,
    pub num_ref_idx_l1_default_active_minus1: u32,
    pub weighted_pred_flag: bool,
    pub weighted_bipred_idc: u8,
    pub pic_init_qp_minus26: i32,
    pub pic_init_qs_minus26: i32,
    pub chroma_qp_index_offset: i32,
    pub deblocking_filter_control_present_flag: bool,
    pub constrained_intra_pred_flag: bool,
    pub redundant_pic_cnt_present_flag: bool,

    // Extended fields (present when more_rbsp_data after the above)
    pub transform_8x8_mode_flag: bool,
    pub pic_scaling_matrix_present_flag: bool,
    pub second_chroma_qp_index_offset: i32,
    /// Effective 4x4 scaling matrices (PPS overrides SPS if present, else SPS, else default).
    pub scaling_list_4x4: [[u8; 16]; 6],
    /// Effective 8x8 scaling matrices [0]=Intra Y, [1]=Inter Y.
    pub scaling_list_8x8: [[u8; 64]; 2],
}

/// Parse a PPS from RBSP data (NAL header byte already stripped).
/// `sps` is needed to inherit scaling lists when PPS doesn't override them.
pub fn parse_pps(rbsp: &[u8], sps: Option<&Sps>) -> Result<Pps, &'static str> {
    let mut r = BitstreamReader::new(rbsp);

    let pic_parameter_set_id = r.read_ue()?;
    let seq_parameter_set_id = r.read_ue()?;
    let entropy_coding_mode_flag = r.read_bit()? != 0;
    let bottom_field_pic_order_in_frame_present_flag = r.read_bit()? != 0;
    let num_slice_groups_minus1 = r.read_ue()?;

    if num_slice_groups_minus1 > 0 {
        // Slice group map parsing - skip for now, not used in Baseline with single slice group
        return Err("slice groups not yet supported");
    }

    let num_ref_idx_l0_default_active_minus1 = r.read_ue()?;
    let num_ref_idx_l1_default_active_minus1 = r.read_ue()?;
    let weighted_pred_flag = r.read_bit()? != 0;
    let weighted_bipred_idc = r.read_bits(2)? as u8;
    let pic_init_qp_minus26 = r.read_se()?;
    let pic_init_qs_minus26 = r.read_se()?;
    let chroma_qp_index_offset = r.read_se()?;
    let deblocking_filter_control_present_flag = r.read_bit()? != 0;
    let constrained_intra_pred_flag = r.read_bit()? != 0;
    let redundant_pic_cnt_present_flag = r.read_bit()? != 0;
    // Extended fields for High profile
    let mut transform_8x8_mode_flag = false;
    let mut pic_scaling_matrix_present_flag = false;
    let mut second_chroma_qp_index_offset = chroma_qp_index_offset;

    // Start with SPS scaling lists (or flat defaults)
    let mut scaling_list_4x4 = sps
        .map(|s| s.scaling_list_4x4)
        .unwrap_or([crate::sps::FLAT_SCALING_4X4; 6]);
    let mut scaling_list_8x8 = sps.map(|s| s.scaling_list_8x8).unwrap_or([[16u8; 64]; 2]);

    if r.more_rbsp_data() {
        transform_8x8_mode_flag = r.read_bit()? != 0;
        pic_scaling_matrix_present_flag = r.read_bit()? != 0;
        if pic_scaling_matrix_present_flag {
            let count = 6 + if transform_8x8_mode_flag { 2 } else { 0 };
            for i in 0..count {
                let present = r.read_bit()? != 0;
                if present {
                    if i < 6 {
                        scaling_list_4x4[i] = crate::sps::parse_scaling_list::<16>(&mut r, 16)?;
                    } else {
                        scaling_list_8x8[i - 6] = crate::sps::parse_scaling_list::<64>(&mut r, 64)?;
                    }
                } else if i < 6 {
                    // Fallback: use SPS list, or default from Table 7-2
                    let sps_has_list = sps.is_some_and(|s| s.seq_scaling_matrix_present_flag);
                    if !sps_has_list {
                        scaling_list_4x4[i] = match i {
                            0 => crate::sps::DEFAULT_SCALING_4X4_INTRA,
                            3 => crate::sps::DEFAULT_SCALING_4X4_INTER,
                            _ => scaling_list_4x4[i - 1],
                        };
                    }
                    // If SPS has lists, scaling_list_4x4[i] already has the SPS value
                }
            }
        }
        second_chroma_qp_index_offset = r.read_se()?;
    }
    // Spec 7.4.2.2: both chroma QP index offsets lie in -12..=12.
    if !(-12..=12).contains(&chroma_qp_index_offset)
        || !(-12..=12).contains(&second_chroma_qp_index_offset)
    {
        return Err("chroma_qp_index_offset out of range");
    }

    Ok(Pps {
        pic_parameter_set_id,
        seq_parameter_set_id,
        entropy_coding_mode_flag,
        bottom_field_pic_order_in_frame_present_flag,
        num_slice_groups_minus1,
        num_ref_idx_l0_default_active_minus1,
        num_ref_idx_l1_default_active_minus1,
        weighted_pred_flag,
        weighted_bipred_idc,
        pic_init_qp_minus26,
        pic_init_qs_minus26,
        chroma_qp_index_offset,
        deblocking_filter_control_present_flag,
        constrained_intra_pred_flag,
        redundant_pic_cnt_present_flag,
        transform_8x8_mode_flag,
        pic_scaling_matrix_present_flag,
        second_chroma_qp_index_offset,
        scaling_list_4x4,
        scaling_list_8x8,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nal::{parse_annex_b, NalUnitType};

    #[test]
    fn test_parse_pps_single_frame() {
        let data = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/testdata/single_frame.h264"
        ))
        .unwrap();
        let nals = parse_annex_b(&data);
        let pps_nal = nals
            .iter()
            .find(|n| n.nal_unit_type == NalUnitType::Pps)
            .unwrap();
        let pps = parse_pps(&pps_nal.rbsp, None).unwrap();

        assert_eq!(pps.pic_parameter_set_id, 0);
        assert_eq!(pps.seq_parameter_set_id, 0);
        assert!(!pps.entropy_coding_mode_flag); // CAVLC
        assert!(!pps.weighted_pred_flag);
        assert!(pps.deblocking_filter_control_present_flag);
        assert!(!pps.constrained_intra_pred_flag);
        assert!(!pps.redundant_pic_cnt_present_flag);
    }
}
