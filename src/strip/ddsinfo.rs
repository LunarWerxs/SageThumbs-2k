//! DDS facts for "Image info": mip levels and the actual compression format.
//!
//! A DDS thumbnail tells a game artist almost nothing - what they need to know is
//! whether the texture is BC1 or BC7 and whether it shipped with a mip chain.
//! Both are in the fixed 124-byte `DDS_HEADER` (plus the `DDS_HEADER_DXT10`
//! extension when the FourCC is `DX10`), so this is a header read, not a decode.
//!
//! Deliberately NOT an Explorer column: Windows has no `System.*` property that
//! means "mip level count", and a made-up one would never appear in the "Choose
//! columns" picker, so it would be invisible plumbing.

/// Offsets from the start of the file (the 4-byte `DDS ` magic included).
const OFF_MIPCOUNT: usize = 4 + 28;
const OFF_PF_FLAGS: usize = 4 + 76 + 4;
const OFF_PF_FOURCC: usize = 4 + 76 + 8;
const OFF_PF_RGB_BITS: usize = 4 + 76 + 12;
/// `DDS_HEADER_DXT10` follows the 124-byte header plus the 4-byte magic.
const OFF_DXT10_FORMAT: usize = 4 + 124;

const DDPF_FOURCC: u32 = 0x4;

fn le32(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(*b.get(o..o + 4)?.first_chunk::<4>()?))
}

/// `(mip levels, compression)` for a DDS, or `None` if it isn't one.
pub(super) fn describe(bytes: &[u8]) -> Option<(u32, String)> {
    if bytes.get(0..4) != Some(b"DDS ") {
        return None;
    }
    // 0 and 1 both mean "just the base image"; report it as 1 either way.
    let mips = le32(bytes, OFF_MIPCOUNT)?.max(1);
    let flags = le32(bytes, OFF_PF_FLAGS)?;
    let format = if flags & DDPF_FOURCC != 0 {
        let cc = bytes.get(OFF_PF_FOURCC..OFF_PF_FOURCC + 4)?;
        match cc {
            b"DXT1" => "BC1 (DXT1)".to_string(),
            b"DXT2" | b"DXT3" => "BC2 (DXT3)".to_string(),
            b"DXT4" | b"DXT5" => "BC3 (DXT5)".to_string(),
            b"ATI1" | b"BC4U" | b"BC4S" => "BC4".to_string(),
            b"ATI2" | b"BC5U" | b"BC5S" => "BC5".to_string(),
            b"DX10" => dxgi_name(le32(bytes, OFF_DXT10_FORMAT).unwrap_or(0)),
            other => format!("FourCC {}", String::from_utf8_lossy(other).trim()),
        }
    } else {
        // Uncompressed: the bit depth is the interesting part.
        match le32(bytes, OFF_PF_RGB_BITS).unwrap_or(0) {
            0 => "uncompressed".to_string(),
            n => format!("uncompressed, {n}-bit"),
        }
    };
    Some((mips, format))
}

/// The DXGI formats a `DX10` DDS realistically carries. Anything else is reported
/// by number rather than guessed at.
fn dxgi_name(id: u32) -> String {
    let name = match id {
        70..=72 => "BC1",
        73..=75 => "BC2",
        76..=78 => "BC3",
        79..=80 => "BC4",
        82..=83 => "BC5",
        94..=95 => "BC6H",
        96..=98 => "BC7",
        2 => "RGBA32F",
        10 => "RGBA16F",
        28 => "RGBA8",
        _ => return format!("DX10, DXGI format {id}"),
    };
    format!("{name} (DX10)")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dds(fourcc: &[u8; 4], mips: u32, dxgi: Option<u32>) -> Vec<u8> {
        let mut v = b"DDS ".to_vec();
        v.resize(4 + 124, 0);
        v[OFF_MIPCOUNT..OFF_MIPCOUNT + 4].copy_from_slice(&mips.to_le_bytes());
        v[OFF_PF_FLAGS..OFF_PF_FLAGS + 4].copy_from_slice(&DDPF_FOURCC.to_le_bytes());
        v[OFF_PF_FOURCC..OFF_PF_FOURCC + 4].copy_from_slice(fourcc);
        if let Some(d) = dxgi {
            v.extend_from_slice(&d.to_le_bytes());
            v.resize(4 + 124 + 20, 0);
        }
        v
    }

    #[test]
    fn classic_fourcc_formats() {
        assert_eq!(
            describe(&dds(b"DXT1", 9, None)).unwrap(),
            (9, "BC1 (DXT1)".into())
        );
        assert_eq!(describe(&dds(b"DXT5", 1, None)).unwrap().1, "BC3 (DXT5)");
        assert_eq!(describe(&dds(b"ATI2", 1, None)).unwrap().1, "BC5");
    }

    #[test]
    fn dx10_reads_the_extension_header() {
        assert_eq!(
            describe(&dds(b"DX10", 11, Some(98))).unwrap(),
            (11, "BC7 (DX10)".into())
        );
        assert_eq!(
            describe(&dds(b"DX10", 1, Some(95))).unwrap().1,
            "BC6H (DX10)"
        );
    }

    #[test]
    fn zero_mips_reads_as_one() {
        assert_eq!(describe(&dds(b"DXT1", 0, None)).unwrap().0, 1);
    }

    #[test]
    fn unknown_things_are_reported_not_guessed() {
        assert_eq!(describe(&dds(b"XXXX", 1, None)).unwrap().1, "FourCC XXXX");
        assert!(describe(&dds(b"DX10", 1, Some(1234)))
            .unwrap()
            .1
            .contains("1234"));
    }

    #[test]
    fn not_a_dds() {
        assert!(describe(b"\x89PNG\r\n\x1a\n").is_none());
        assert!(describe(b"DDS ").is_none());
    }
}
