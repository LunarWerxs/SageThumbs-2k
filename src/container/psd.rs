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

use super::util::{be16, be32};

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

/// True when the document's merged composite carries a transparency (alpha)
/// channel — i.e. the header's channel count (@12) exceeds the colour channels for
/// its colour mode (@24). Photoshop bakes its embedded preview (resource 1036) as a
/// JPEG, which has NO alpha, so a transparent PSD (e.g. a removed background) would
/// thumbnail with a flat WHITE background off that preview. Callers use this to
/// render the real layer composite (which keeps alpha) for those instead. A rare
/// extra *spot* channel can also bump the count — that false positive just renders
/// the (still-correct) composite the slower way, never a wrong image.
pub fn has_alpha(bytes: &[u8]) -> bool {
    if !bytes.starts_with(b"8BPS") {
        return false;
    }
    let (Some(channels), Some(mode)) = (be16(bytes, 12), be16(bytes, 24)) else {
        return false;
    };
    // Colour channels per PSD colour mode: RGB/Lab = 3, CMYK = 4, everything else
    // (Bitmap/Grayscale/Indexed/Duotone/Multichannel) = 1. An alpha channel makes
    // `channels` exceed this base.
    let colour = match mode {
        3 | 9 => 3, // RGB, Lab
        4 => 4,     // CMYK
        _ => 1,     // Bitmap, Grayscale, Indexed, Duotone, Multichannel
    };
    channels > colour
}

/// How many leading bytes of a PSD/PSB hold everything the PREVIEW extractors
/// need — the 26-byte header, the Color Mode Data section, and the whole Image
/// Resources section (where the baked thumbnail, resource 1036, lives). Layer
/// and image data beyond that are irrelevant to a preview, so a reader can stop
/// here instead of buffering a multi-hundred-MB document.
///
/// Returns None when this isn't worth a bounded read: not a PSD, a transparent
/// document (its JPEG preview has no alpha — the composite path needs the FULL
/// file), or a malformed/truncated header. The caller then takes its normal
/// whole-file path. Generic over `Read + Seek` so the shell-IStream and by-path
/// front-ends share the math.
pub fn preview_prefix_len<R: std::io::Read + std::io::Seek>(r: &mut R) -> Option<u64> {
    use std::io::SeekFrom;
    // Header (26 bytes) + the Color-Mode-Data length field (4 bytes).
    let mut head = [0u8; 30];
    r.seek(SeekFrom::Start(0)).ok()?;
    r.read_exact(&mut head).ok()?;
    if !head.starts_with(b"8BPS") {
        return None;
    }
    if has_alpha(&head) {
        return None;
    }
    let cmd_len = be32(&head, 26)? as u64;
    // Image Resources section: its own 4-byte length right after the CMD data.
    let res_len_off = 30u64.checked_add(cmd_len)?;
    let mut len_buf = [0u8; 4];
    r.seek(SeekFrom::Start(res_len_off)).ok()?;
    r.read_exact(&mut len_buf).ok()?;
    let res_len = u32::from_be_bytes(len_buf) as u64;
    res_len_off.checked_add(4)?.checked_add(res_len)
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
                // Bound the cover we hand back (shared CBXMEM cap): a hostile PSD
                // could declare a huge resource block, and the JPEG is decoded
                // downstream under `panic = "abort"`.
                if jpeg.len() as u64 <= crate::container::MAX_COVER
                    && jpeg.starts_with(&[0xFF, 0xD8, 0xFF])
                {
                    return Some(jpeg.to_vec());
                }
            }
        }

        // Each resource's data is padded to an even length.
        o = data_off + size + (size & 1);
    }
    None
}

/// Test-only synthetic-PSD builder, shared with the `decode`/`streamsrc`
/// head-preview fast-path tests (re-exported as `container::psd_testutil`).
/// Lives outside `mod tests` so sibling modules can reach it under cfg(test).
#[cfg(test)]
pub(crate) mod testutil {
    /// Minimal valid RGB PSD: 26-byte header with `channels`, empty color-mode
    /// data, one image-resource block holding a 1036 JPEG thumbnail (when
    /// `with_thumb`), then `tail` zero bytes standing in for the layer/image
    /// data a real document is huge from. Returns the bytes plus the exact
    /// head-prefix length (header + CMD + resources) that
    /// [`super::preview_prefix_len`] should report.
    pub(crate) fn synthetic_psd(channels: u16, with_thumb: bool, tail: usize) -> (Vec<u8>, usize) {
        let mut res = Vec::new();
        if with_thumb {
            let jpeg = tiny_jpeg();
            let mut data = Vec::new();
            data.extend_from_slice(&1u32.to_be_bytes()); // format = JPEG
            data.extend_from_slice(&[0u8; 20]); // w/h/widthbytes/totalsize/sizeafter
            data.extend_from_slice(&[0, 24]); // bits/pixel
            data.extend_from_slice(&[0, 1]); // planes
            data.extend_from_slice(&jpeg);
            res.extend_from_slice(b"8BIM");
            res.extend_from_slice(&1036u16.to_be_bytes());
            res.extend_from_slice(&[0, 0]); // empty Pascal name + pad
            res.extend_from_slice(&(data.len() as u32).to_be_bytes());
            res.extend_from_slice(&data);
            if data.len() & 1 == 1 {
                res.push(0);
            }
        }

        let mut psd = Vec::new();
        psd.extend_from_slice(b"8BPS");
        psd.extend_from_slice(&[0, 1]); // version 1 (PSD)
        psd.extend_from_slice(&[0u8; 6]); // reserved
        psd.extend_from_slice(&channels.to_be_bytes());
        psd.extend_from_slice(&100u32.to_be_bytes()); // height
        psd.extend_from_slice(&100u32.to_be_bytes()); // width
        psd.extend_from_slice(&[0, 8]); // depth
        psd.extend_from_slice(&[0, 3]); // color mode = RGB
        psd.extend_from_slice(&0u32.to_be_bytes()); // color-mode data length
        psd.extend_from_slice(&(res.len() as u32).to_be_bytes()); // resources length
        psd.extend_from_slice(&res);
        let head_len = psd.len();
        psd.extend_from_slice(&vec![0u8; tail]);
        (psd, head_len)
    }

    fn tiny_jpeg() -> Vec<u8> {
        let mut buf = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(4, 4, image::Rgb([200, 50, 50])))
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Jpeg)
            .unwrap();
        buf
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::synthetic_psd;
    use super::*;

    #[test]
    fn extracts_psd_thumbnail_resource_1036() {
        let (psd, _) = synthetic_psd(3, true, 0);
        let got = extract(&psd).expect("thumbnail");
        assert!(got.starts_with(&[0xFF, 0xD8, 0xFF]));
        assert!(image::load_from_memory(&got).is_ok(), "extracted bytes should be a valid JPEG");

        // The header probe reports the CANVAS size (100×100 here), independent
        // of the extracted thumbnail's pixels.
        assert_eq!(header_dims(&psd), Some((100, 100)));

        assert!(extract(b"not a psd at all").is_none());
        assert!(header_dims(b"not a psd at all").is_none());
    }

    /// Build just the 26-byte PSD header for the given channel count + colour mode.
    fn psd_header(channels: u16, mode: u16) -> Vec<u8> {
        let mut h = Vec::new();
        h.extend_from_slice(b"8BPS");
        h.extend_from_slice(&[0, 1]); // version 1 (PSD)
        h.extend_from_slice(&[0u8; 6]); // reserved
        h.extend_from_slice(&channels.to_be_bytes());
        h.extend_from_slice(&100u32.to_be_bytes()); // height
        h.extend_from_slice(&100u32.to_be_bytes()); // width
        h.extend_from_slice(&[0, 8]); // depth
        h.extend_from_slice(&mode.to_be_bytes());
        h
    }

    #[test]
    fn preview_prefix_len_stops_at_the_resources_section() {
        use std::io::Cursor;
        // Multi-MB "layer/image data" tail (the part a real document is huge
        // from): the prefix stops at the resources section and STILL yields the
        // thumbnail — the load-bearing claim of the head-preview fast path.
        let (big, head_len) = synthetic_psd(3, true, 3 << 20);
        let len = preview_prefix_len(&mut Cursor::new(&big)).expect("prefix len");
        assert_eq!(len, head_len as u64);
        assert!(extract(&big[..len as usize]).is_some());

        // Transparent document (4 channels, RGB): bows out — the composite path
        // needs the full file.
        let (alpha, _) = synthetic_psd(4, true, 1024);
        assert_eq!(preview_prefix_len(&mut Cursor::new(&alpha)), None);

        // Not a PSD / truncated header: no fast path.
        assert_eq!(preview_prefix_len(&mut Cursor::new(b"not a psd at all")), None);
        assert_eq!(preview_prefix_len(&mut Cursor::new(&big[..26])), None);

        // A CMD length pointing past EOF fails the resources-length read, not math.
        let mut bad = big.clone();
        bad[26..30].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(preview_prefix_len(&mut Cursor::new(&bad)), None);
    }

    #[test]
    fn has_alpha_keys_off_channel_count_per_mode() {
        // RGB (mode 3): 3 channels = opaque, 4 = transparent (the removed-background case).
        assert!(!has_alpha(&psd_header(3, 3)));
        assert!(has_alpha(&psd_header(4, 3)));
        // CMYK (mode 4): base is 4 channels; a 5th is alpha.
        assert!(!has_alpha(&psd_header(4, 4)));
        assert!(has_alpha(&psd_header(5, 4)));
        // Grayscale (mode 1): base 1, a 2nd channel is alpha.
        assert!(!has_alpha(&psd_header(1, 1)));
        assert!(has_alpha(&psd_header(2, 1)));
        // Not a PSD / too short → never claims alpha.
        assert!(!has_alpha(b"not a psd"));
        assert!(!has_alpha(b"8BPS"));
    }
}
