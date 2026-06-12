//! Adobe Photoshop `.psd` / `.psb` embedded thumbnail. Photoshop bakes a JFIF
//! JPEG preview into Image Resource Block **1036** (0x040C). We pull that out
//! directly — no layer compositing — so PSD thumbnails work even on the
//! ImageMagick-free compact install (and far faster than rendering the layers).
//!
//! All multi-byte fields are big-endian. The header + Color-Mode-Data + Image-
//! Resources sections use 4-byte lengths in both PSD and PSB, so one parser does
//! both. Every read is bounds-checked (we run under `panic = "abort"`).

/// Thumbnail resource ids: 1036 (Photoshop 5.0+, RGB) and the legacy 1033
/// (Photoshop 4.0) — both carry a JFIF JPEG when `format == 1`.
const THUMBNAIL_IDS: [u16; 2] = [1036, 1033];

fn be32(b: &[u8], o: usize) -> Option<u32> {
    b.get(o..o + 4).map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}
fn be16(b: &[u8], o: usize) -> Option<u16> {
    b.get(o..o + 2).map(|s| u16::from_be_bytes([s[0], s[1]]))
}

/// The document's REAL canvas size from the file header (height @14, width @18 —
/// same layout in PSD and PSB). The extracted thumbnail is ~160px; captions and
/// info displays should show these instead of the preview's dimensions.
pub fn header_dims(bytes: &[u8]) -> Option<(u32, u32)> {
    if !bytes.starts_with(b"8BPS") {
        return None;
    }
    let h = be32(bytes, 14)?;
    let w = be32(bytes, 18)?;
    (w > 0 && h > 0).then_some((w, h))
}

/// Extract the embedded JPEG thumbnail from a PSD/PSB, or None.
pub fn extract(bytes: &[u8]) -> Option<Vec<u8>> {
    if !bytes.starts_with(b"8BPS") {
        return None;
    }
    // File header is 26 bytes; then the Color Mode Data section (4-byte length).
    let cmd_len = be32(bytes, 26)? as usize;
    let res_len_off = 26usize.checked_add(4)?.checked_add(cmd_len)?;
    // Image Resources section: 4-byte length, then a run of resource blocks.
    let res_len = be32(bytes, res_len_off)? as usize;
    let res_start = res_len_off + 4;
    let res_end = res_start.checked_add(res_len)?.min(bytes.len());

    let mut o = res_start;
    while o + 8 <= res_end {
        if bytes.get(o..o + 4)? != b"8BIM" {
            break; // not a well-formed resource run
        }
        let id = be16(bytes, o + 4)?;
        // Pascal name: 1 length byte + name, the whole field padded to even.
        let name_len = *bytes.get(o + 6)? as usize;
        let name_field = 1 + name_len;
        let name_padded = name_field + (name_field & 1);
        let size_off = o + 6 + name_padded;
        let size = be32(bytes, size_off)? as usize;
        let data_off = size_off + 4;
        let data_end = data_off.checked_add(size)?;
        if data_end > bytes.len() {
            break;
        }

        if THUMBNAIL_IDS.contains(&id) {
            // 28-byte thumbnail header, then the JPEG (when format == 1 = JPEG).
            if be32(bytes, data_off)? == 1 {
                let jpeg = bytes.get(data_off + 28..data_end)?;
                if jpeg.starts_with(&[0xFF, 0xD8, 0xFF]) {
                    return Some(jpeg.to_vec());
                }
            }
        }

        // Each resource's data is padded to an even length.
        o = data_off + size + (size & 1);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_jpeg() -> Vec<u8> {
        let mut buf = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(4, 4, image::Rgb([200, 50, 50])))
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Jpeg)
            .unwrap();
        buf
    }

    #[test]
    fn extracts_psd_thumbnail_resource_1036() {
        let jpeg = tiny_jpeg();
        // Build a minimal valid PSD: 26-byte header, empty color-mode data, one
        // image-resource block holding the 1036 JPEG thumbnail.
        let mut data = Vec::new();
        data.extend_from_slice(&1u32.to_be_bytes()); // format = JPEG
        data.extend_from_slice(&[0u8; 20]); // w/h/widthbytes/totalsize/sizeafter
        data.extend_from_slice(&[0, 24]); // bits/pixel
        data.extend_from_slice(&[0, 1]); // planes
        data.extend_from_slice(&jpeg);

        let mut res = Vec::new();
        res.extend_from_slice(b"8BIM");
        res.extend_from_slice(&1036u16.to_be_bytes());
        res.extend_from_slice(&[0, 0]); // empty Pascal name + pad
        res.extend_from_slice(&(data.len() as u32).to_be_bytes());
        res.extend_from_slice(&data);
        if data.len() & 1 == 1 {
            res.push(0);
        }

        let mut psd = Vec::new();
        psd.extend_from_slice(b"8BPS");
        psd.extend_from_slice(&[0, 1]); // version 1 (PSD)
        psd.extend_from_slice(&[0u8; 6]); // reserved
        psd.extend_from_slice(&[0, 3]); // channels
        psd.extend_from_slice(&100u32.to_be_bytes()); // height
        psd.extend_from_slice(&100u32.to_be_bytes()); // width
        psd.extend_from_slice(&[0, 8]); // depth
        psd.extend_from_slice(&[0, 3]); // color mode = RGB
        psd.extend_from_slice(&0u32.to_be_bytes()); // color-mode data length
        psd.extend_from_slice(&(res.len() as u32).to_be_bytes()); // resources length
        psd.extend_from_slice(&res);

        let got = extract(&psd).expect("thumbnail");
        assert!(got.starts_with(&[0xFF, 0xD8, 0xFF]));
        assert!(image::load_from_memory(&got).is_ok(), "extracted bytes should be a valid JPEG");

        // The header probe reports the CANVAS size (100×100 here), independent
        // of the extracted thumbnail's pixels.
        assert_eq!(header_dims(&psd), Some((100, 100)));

        assert!(extract(b"not a psd at all").is_none());
        assert!(header_dims(b"not a psd at all").is_none());
    }
}
