/// SEI message (H.264 spec section 7.3.2.3).
#[derive(Debug)]
pub struct SeiMessage {
    pub payload_type: u32,
    pub payload: SeiPayload,
}

#[derive(Debug)]
pub enum SeiPayload {
    UserDataUnregistered { uuid: [u8; 16], data: Vec<u8> },
    Unknown { data: Vec<u8> },
}

/// Parse all SEI messages from RBSP data (NAL header byte already stripped).
pub fn parse_sei(rbsp: &[u8]) -> Result<Vec<SeiMessage>, &'static str> {
    let mut messages = Vec::new();
    let mut offset = 0;

    while offset < rbsp.len() {
        // Check for RBSP trailing bits (byte with MSB set and rest zero, e.g. 0x80)
        if rbsp[offset] == 0x80 && rbsp[offset..].iter().all(|&b| b == 0x80 || b == 0x00) {
            break;
        }

        // Read payloadType: sum of 0xFF bytes, plus final byte < 0xFF
        let mut payload_type: u32 = 0;
        while offset < rbsp.len() && rbsp[offset] == 0xFF {
            payload_type += 255;
            offset += 1;
        }
        if offset >= rbsp.len() {
            return Err("unexpected end of SEI payload type");
        }
        payload_type += rbsp[offset] as u32;
        offset += 1;

        // Read payloadSize: same encoding
        let mut payload_size: u32 = 0;
        while offset < rbsp.len() && rbsp[offset] == 0xFF {
            payload_size += 255;
            offset += 1;
        }
        if offset >= rbsp.len() {
            return Err("unexpected end of SEI payload size");
        }
        payload_size += rbsp[offset] as u32;
        offset += 1;

        let size = payload_size as usize;
        if offset + size > rbsp.len() {
            return Err("SEI payload size exceeds RBSP length");
        }

        let payload_data = &rbsp[offset..offset + size];
        offset += size;

        let payload = match payload_type {
            5 => parse_user_data_unregistered(payload_data)?,
            _ => SeiPayload::Unknown {
                data: payload_data.to_vec(),
            },
        };

        messages.push(SeiMessage {
            payload_type,
            payload,
        });
    }

    Ok(messages)
}

fn parse_user_data_unregistered(data: &[u8]) -> Result<SeiPayload, &'static str> {
    if data.len() < 16 {
        return Err("user_data_unregistered payload too short for UUID");
    }
    let mut uuid = [0u8; 16];
    uuid.copy_from_slice(&data[..16]);
    Ok(SeiPayload::UserDataUnregistered {
        uuid,
        data: data[16..].to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nal::{parse_annex_b, NalUnitType};

    #[test]
    fn test_parse_sei_single_frame() {
        let data = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/testdata/single_frame.h264"
        ))
        .unwrap();
        let nals = parse_annex_b(&data);
        let sei_nal = nals
            .iter()
            .find(|n| n.nal_unit_type == NalUnitType::Sei)
            .unwrap();
        let messages = parse_sei(&sei_nal.rbsp).unwrap();

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].payload_type, 5); // user_data_unregistered

        match &messages[0].payload {
            SeiPayload::UserDataUnregistered { uuid, data } => {
                // UUID should be 16 bytes (x264's UUID)
                assert_eq!(uuid.len(), 16);
                // Data should contain x264 encoder string
                let text = String::from_utf8_lossy(data);
                assert!(text.contains("x264"), "expected x264 string, got: {text}");
            }
            _ => panic!("expected UserDataUnregistered"),
        }
    }
}
