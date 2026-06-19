//! AutoCAD `.dwg` embedded preview/thumbnail.
//!
//! Since R13 (AC1012) a DWG carries a "Preview/Thumbnail Image" section. The file
//! header holds a 4-byte LE seeker at offset `0x0D` pointing at that section, which
//! starts with a fixed 16-byte sentinel, then a record table: each record is a
//! 1-byte code + 4-byte LE absolute offset + 4-byte LE size. Code 2 = BMP/DIB,
//! 3 = WMF, 6 = PNG (newer saves). We pick the best, carve it, and (for a bare DIB)
//! prepend a `BITMAPFILEHEADER`. No CAD parsing, no SDK.
//!
//! Verified against real R13/2000/2004/2018 saves (DIB for older, PNG for 2018).
//! Files saved with the thumbnail preview disabled (`RASTERPREVIEW`/`THUMBSAVE`=0)
//! have a zero-size record → we return None. Runs under `panic = "abort"`: every
//! read is bounds-checked.

use super::util::{dib_to_bmp, le32};

const SENTINEL: [u8; 16] = [
    0x1F, 0x25, 0x6D, 0x07, 0xD4, 0x36, 0x28, 0x28, 0x9D, 0x57, 0xCA, 0x3F, 0x9D, 0x44, 0x10, 0x2B,
];

/// DWG version strings since R10 are "AC10xx" (R13=AC1012 … 2018=AC1032). The
/// preview section only exists R13+, gated by the sentinel check in `extract`.
pub fn looks_like_dwg(head: &[u8]) -> bool {
    head.starts_with(b"AC10")
}

/// Extract the embedded preview (PNG as-is, DIB wrapped to BMP, or raw WMF), or None.
pub fn extract(bytes: &[u8]) -> Option<Vec<u8>> {
    let imgptr = le32(bytes, 0x0D)? as usize;
    if bytes.get(imgptr..imgptr.checked_add(16)?)? != SENTINEL {
        return None; // no preview section (or pre-R13)
    }
    let mut p = imgptr + 16;
    p = p.checked_add(4)?; // skip the overall image-data size (u32)
    let count = *bytes.get(p)?;
    p = p.checked_add(1)?;

    // Walk the records; prefer PNG (6) > DIB (2) > WMF (3).
    let (mut png, mut dib, mut wmf) = (None, None, None);
    for _ in 0..count {
        let code = *bytes.get(p)?;
        let off = le32(bytes, p.checked_add(1)?)? as usize;
        let size = le32(bytes, p.checked_add(5)?)? as usize;
        p = p.checked_add(9)?;
        if size == 0 || size as u64 > super::MAX_COVER {
            continue;
        }
        match code {
            6 => png = png.or(Some((off, size))),
            2 => dib = dib.or(Some((off, size))),
            3 => wmf = wmf.or(Some((off, size))),
            _ => {} // 1 = header/palette block, etc.
        }
    }

    if let Some((off, size)) = png {
        let p = bytes.get(off..off.checked_add(size)?)?;
        return super::util::decodable_image(p.to_vec());
    }
    if let Some((off, size)) = dib {
        let d = bytes.get(off..off.checked_add(size)?)?;
        return super::util::decodable_image(dib_to_bmp(d)?);
    }
    if let Some((off, size)) = wmf {
        let w = bytes.get(off..off.checked_add(size)?)?;
        return super::util::decodable_image(w.to_vec());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_sentinel_no_preview() {
        // "AC1032" header but a seeker pointing at non-sentinel bytes → None, no panic.
        let mut f = b"AC1032\x00\x00\x00\x00\x00".to_vec();
        f.resize(64, 0);
        f[0x0D..0x11].copy_from_slice(&20u32.to_le_bytes()); // seeker → offset 20 (zeros)
        assert!(extract(&f).is_none());
        assert!(looks_like_dwg(&f));
        assert!(!looks_like_dwg(b"not a dwg"));
    }

    #[test]
    fn extracts_png_record() {
        // Build a minimal DWG with a sentinel + one PNG record.
        let png = {
            let mut b = Vec::new();
            image::DynamicImage::ImageRgba8(image::RgbaImage::new(2, 2))
                .write_to(&mut std::io::Cursor::new(&mut b), image::ImageFormat::Png)
                .unwrap();
            b
        };
        let mut f = b"AC1032\x00\x00\x00\x00\x00".to_vec();
        f.resize(13, 0);
        let imgptr = 64u32;
        f.extend_from_slice(&imgptr.to_le_bytes()); // seeker at 0x0D
        f.resize(imgptr as usize, 0);
        f.extend_from_slice(&SENTINEL); // sentinel
        f.extend_from_slice(&0u32.to_le_bytes()); // overall size (ignored)
        f.push(1); // 1 record
        let png_off = (f.len() + 9) as u32; // record is 9 bytes, then the PNG
        f.push(6); // code = PNG
        f.extend_from_slice(&png_off.to_le_bytes());
        f.extend_from_slice(&(png.len() as u32).to_le_bytes());
        f.extend_from_slice(&png);

        let got = extract(&f).expect("should extract the PNG record");
        assert!(got.starts_with(&[0x89, b'P', b'N', b'G']));
    }
}
