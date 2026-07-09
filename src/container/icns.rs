//! Apple Icon Image (`.icns`): pull the best embedded raster member.
//!
//! An icns file is a flat chunk list — `"icns"` magic + big-endian total length,
//! then `[4-byte type][u32 BE length incl. this 8-byte header][data]` entries.
//! Every icon macOS 10.7+ writes stores its large sizes as literal PNG members
//! (`ic07`…`ic13`), and 10.5-era files use JPEG 2000 — both formats our decode
//! tiers already handle, so we just slice out the largest such member (largest ≈
//! highest resolution; PNG preferred over JP2 since it decodes pure-Rust).
//! Legacy ARGB/RLE members (`it32`/`ih32`…) are skipped — files that ONLY carry
//! those (pre-2007 icons) return None and fall back to the default icon.

/// JPEG 2000 signature box (JP2 container).
const JP2_MAGIC: [u8; 12] = [0, 0, 0, 0x0C, 0x6A, 0x50, 0x20, 0x20, 0x0D, 0x0A, 0x87, 0x0A];

/// Extract the largest PNG (preferred) or JPEG 2000 member, or None.
pub fn extract(bytes: &[u8]) -> Option<Vec<u8>> {
    if bytes.len() < 8 || &bytes[0..4] != b"icns" {
        return None;
    }
    let total = u32::from_be_bytes(bytes[4..8].try_into().ok()?) as usize;
    let end = total.min(bytes.len());

    let mut best_png: Option<&[u8]> = None;
    let mut best_jp2: Option<&[u8]> = None;
    let mut off = 8usize;
    while off + 8 <= end {
        let len = u32::from_be_bytes(bytes.get(off + 4..off + 8)?.try_into().ok()?) as usize;
        if len < 8 {
            break; // corrupt length would loop forever
        }
        let data_end = off.checked_add(len)?;
        if data_end > end {
            break;
        }
        let data = &bytes[off + 8..data_end];
        if data.starts_with(&[0x89, b'P', b'N', b'G']) {
            if best_png.is_none_or(|b| data.len() > b.len()) {
                best_png = Some(data);
            }
        } else if data.starts_with(&JP2_MAGIC) && best_jp2.is_none_or(|b| data.len() > b.len()) {
            best_jp2 = Some(data);
        }
        off = data_end;
    }
    best_png.or(best_jp2).map(|d| d.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_bytes(edge: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(edge, edge, image::Rgba([10, 200, 30, 255]));
        let mut out = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut c = kind.to_vec();
        c.extend_from_slice(&((data.len() + 8) as u32).to_be_bytes());
        c.extend_from_slice(data);
        c
    }

    #[test]
    fn picks_the_largest_png_member() {
        let small = png_bytes(16);
        let large = png_bytes(64);
        let mut body = Vec::new();
        body.extend_from_slice(&chunk(b"TOC ", &[0u8; 16])); // non-image member
        body.extend_from_slice(&chunk(b"icp4", &small));
        body.extend_from_slice(&chunk(b"ic07", &large));
        let mut icns = b"icns".to_vec();
        icns.extend_from_slice(&((body.len() + 8) as u32).to_be_bytes());
        icns.extend_from_slice(&body);

        let got = extract(&icns).expect("largest PNG member");
        assert_eq!(got, large);
        // And it flows through the container dispatch + decodes end-to-end.
        let img = crate::decode::decode_preview(&icns).expect("icns decodes");
        assert_eq!((img.width(), img.height()), (64, 64));

        assert!(extract(b"not an icns").is_none());
        assert!(extract(&icns[..20]).is_none(), "truncated icns is a clean miss");
    }
}
