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

/// Offsets from the start of the FILE (the 4-byte `DDS ` magic included). The
/// 124-byte `DDS_HEADER` therefore spans 4..128, and its embedded 32-byte
/// `DDS_PIXELFORMAT` starts 72 bytes into it — i.e. at file offset 76, because
/// `dwReserved1` is 11 dwords (28 + 44 = 72). These constants were each 4 too high
/// until 2026-08-03, so "mip levels" read the first dword of `dwReserved1` (the
/// writer's signature — ImageMagick's files reported 1,195,461,449 levels) and the
/// compression name read the R bit-mask. The unit tests missed it because the
/// fixture builder used the same wrong constants; it now writes real offsets.
const OFF_MIPCOUNT: usize = 4 + 24;
const OFF_PF_FLAGS: usize = 4 + 72 + 4;
const OFF_PF_FOURCC: usize = 4 + 72 + 8;
const OFF_PF_RGB_BITS: usize = 4 + 72 + 12;
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
            b"ATI1" | b"BC4U" => "BC4".to_string(),
            b"BC4S" => "BC4 (signed)".to_string(),
            b"ATI2" | b"BC5U" => "BC5".to_string(),
            b"BC5S" => "BC5 (signed)".to_string(),
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
///
/// The block-compressed runs are each THREE values wide (TYPELESS, UNORM, SNORM —
/// or UNORM_SRGB), so BC4 is 79..=81, BC5 82..=84, BC6H 94..=96 and BC7 97..=99.
/// An earlier table here shifted the last two down by one and reported a
/// `BC6H_SF16` texture (96) as "BC7". The same numbers drive the real decoder in
/// `decode/dds.rs`; keep the two in agreement.
fn dxgi_name(id: u32) -> String {
    let name = match id {
        70..=72 => "BC1",
        73..=75 => "BC2",
        76..=78 => "BC3",
        79..=80 => "BC4",
        81 => "BC4 (signed)",
        82..=83 => "BC5",
        84 => "BC5 (signed)",
        94..=95 => "BC6H",
        96 => "BC6H (signed)",
        97..=99 => "BC7",
        2 => "RGBA32F",
        10 => "RGBA16F",
        11 => "RGBA16",
        24 => "RGB10A2",
        26 => "RG11B10F",
        28 | 29 => "RGBA8",
        61 => "R8",
        65 => "A8",
        85 => "RGB565",
        87 | 91 => "BGRA8",
        88 | 93 => "BGRX8",
        _ => return format!("DX10, DXGI format {id}"),
    };
    format!("{name} (DX10)")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a spec-shaped `DDS ` + `DDS_HEADER` from LITERAL field offsets, so the
    /// fixture can never drift into agreement with a wrong constant the way the
    /// pre-2026-08-03 one did (it reused `OFF_*`, which hid a 4-byte skew).
    fn dds(fourcc: &[u8; 4], mips: u32, dxgi: Option<u32>) -> Vec<u8> {
        let mut v = b"DDS ".to_vec();
        v.resize(4 + 124, 0);
        let put = |v: &mut Vec<u8>, at: usize, n: u32| {
            v[at..at + 4].copy_from_slice(&n.to_le_bytes());
        };
        put(&mut v, 4, 124); // dwSize
        put(&mut v, 4 + 24, mips); // dwMipMapCount
        put(&mut v, 4 + 72, 32); // ddspf.dwSize
        put(&mut v, 4 + 72 + 4, DDPF_FOURCC); // ddspf.dwFlags
        v[4 + 72 + 8..4 + 72 + 12].copy_from_slice(fourcc); // ddspf.dwFourCC
                                                            // A writer signature in dwReserved1 — the bytes the old mip-count offset
                                                            // was actually reading.
        v[4 + 28..4 + 28 + 11].copy_from_slice(b"SAGETHUMBS ");
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

    /// The block-compressed DXGI runs are three wide; these are the boundary
    /// values an off-by-one table gets wrong (96 used to report as "BC7").
    #[test]
    fn dx10_block_format_boundaries() {
        for (dxgi, want) in [
            (79, "BC4 (DX10)"),
            (80, "BC4 (DX10)"),
            (81, "BC4 (signed) (DX10)"),
            (82, "BC5 (DX10)"),
            (84, "BC5 (signed) (DX10)"),
            (94, "BC6H (DX10)"),
            (96, "BC6H (signed) (DX10)"),
            (97, "BC7 (DX10)"),
            (99, "BC7 (DX10)"),
        ] {
            assert_eq!(describe(&dds(b"DX10", 1, Some(dxgi))).unwrap().1, want);
        }
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
