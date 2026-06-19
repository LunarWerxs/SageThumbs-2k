//! Rhino 3D `.3dm` (openNURBS) embedded preview thumbnail.
//!
//! A Rhino-saved `.3dm` stores a viewport preview in the
//! `TCODE_PROPERTIES_COMPRESSED_PREVIEWIMAGE` chunk (LE typecode `0x20008025`).
//! It is NOT a carvable PNG/JPEG — it's a `BITMAPINFOHEADER` followed by a
//! zlib-DEFLATED raw DIB. We locate the chunk, read the 40-byte header, inflate
//! the zlib stream (pure-Rust `flate2`, already in-tree), and wrap the result as a
//! BMP. Verified against real Rhino 7 saves.
//!
//! Library/SDK-written `.3dm` (and tiny format-ID stubs) OMIT this chunk → None.
//! Runs under `panic = "abort"`: bounds-checked, with a hard inflate cap.

use std::io::Read;

use super::util::{dib_to_bmp, find, le32};

/// LE bytes of typecode `0x20008025` (TCODE_PROPERTIES_COMPRESSED_PREVIEWIMAGE).
const PREVIEW_TYPECODE: [u8; 4] = [0x25, 0x80, 0x00, 0x20];

/// Cap on the inflated DIB (decompression-bomb guard). A preview at ~1024² × 4 B
/// is ~4 MiB; 32 MiB is ample headroom and bounds a hostile stream.
const MAX_INFLATED: u64 = 32 * 1024 * 1024;

pub fn looks_like_3dm(head: &[u8]) -> bool {
    head.starts_with(b"3D Geometry File Format ")
}

/// Extract the preview as a BMP, or None if this `.3dm` has no embedded preview.
pub fn extract(bytes: &[u8]) -> Option<Vec<u8>> {
    let chunk = find(bytes, &PREVIEW_TYPECODE)?;
    // chunk content = after the 4-byte typecode + 8-byte LE chunk length.
    let content = chunk.checked_add(12)?;
    let bmih = bytes.get(content..content.checked_add(40)?)?.to_vec();
    // ON WriteCompressedBuffer framing: u32 uncompressedSize, u32 crc, u8 method,
    // then a nested sub-chunk header, then the zlib stream. Rather than walk every
    // field exactly, read the expected size and scan a short window for the zlib
    // magic (78 DA / 78 9C) — robust across openNURBS framing variations.
    let want = le32(bytes, content.checked_add(40)?)? as u64; // ON uncompressedSize
    let scan_lo = content.checked_add(40)?;
    let scan_hi = scan_lo.checked_add(96)?.min(bytes.len());
    let region = bytes.get(scan_lo..scan_hi)?;
    let z = find(region, &[0x78, 0xDA]).or_else(|| find(region, &[0x78, 0x9C]))?;
    let zstart = scan_lo + z;

    let cap = want.clamp(1, MAX_INFLATED);
    let mut inflated = Vec::new();
    flate2::read::ZlibDecoder::new(bytes.get(zstart..)?)
        .take(cap)
        .read_to_end(&mut inflated)
        .ok()?;
    if inflated.is_empty() {
        return None;
    }
    // dib = the 40-byte BITMAPINFOHEADER + the inflated pixel buffer (palette+bits).
    let mut dib = bmih;
    dib.extend_from_slice(&inflated);
    super::util::decodable_image(dib_to_bmp(&dib)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn rejects_non_3dm_and_previewless() {
        assert!(!looks_like_3dm(b"not a rhino file"));
        assert!(looks_like_3dm(b"3D Geometry File Format 7"));
        // A 3dm header with no preview typecode → None.
        assert!(extract(b"3D Geometry File Format  and then some bytes...").is_none());
    }

    #[test]
    fn inflates_and_wraps_a_dib() {
        // Build a tiny 2x2 24-bit BI_RGB DIB, deflate the pixel bytes, and assemble
        // the chunk the way openNURBS frames it (typecode + len + BMIH + size/crc/
        // method + nested chunk header + zlib stream).
        let w = 2i32;
        let h = 2i32;
        let stride = 2 * 3 + 2; // 6 bytes/row padded to 8
        let pixels = vec![0u8; stride * h as usize];
        let mut bmih = Vec::new();
        bmih.extend_from_slice(&40u32.to_le_bytes()); // biSize
        bmih.extend_from_slice(&w.to_le_bytes());
        bmih.extend_from_slice(&h.to_le_bytes());
        bmih.extend_from_slice(&1u16.to_le_bytes()); // planes
        bmih.extend_from_slice(&24u16.to_le_bytes()); // bitcount
        bmih.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
        bmih.extend_from_slice(&(pixels.len() as u32).to_le_bytes()); // biSizeImage
        bmih.extend_from_slice(&[0u8; 16]); // xppm/yppm/clrused/clrimportant

        let mut zlib = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        zlib.write_all(&pixels).unwrap();
        let zbytes = zlib.finish().unwrap();

        let mut chunk_content = Vec::new();
        chunk_content.extend_from_slice(&bmih);
        chunk_content.extend_from_slice(&(pixels.len() as u32).to_le_bytes()); // uncompressedSize
        chunk_content.extend_from_slice(&0u32.to_le_bytes()); // crc (unchecked)
        chunk_content.push(1); // method = deflate
        chunk_content.extend_from_slice(&0u32.to_le_bytes()); // nested typecode
        chunk_content.extend_from_slice(&(zbytes.len() as u64).to_le_bytes()); // nested len
        chunk_content.extend_from_slice(&zbytes);

        let mut f = b"3D Geometry File Format 7\0".to_vec();
        f.extend_from_slice(&PREVIEW_TYPECODE);
        f.extend_from_slice(&(chunk_content.len() as u64).to_le_bytes());
        f.extend_from_slice(&chunk_content);

        let bmp = extract(&f).expect("should inflate + wrap the DIB");
        assert!(bmp.starts_with(b"BM"));
        assert!(image::load_from_memory(&bmp).is_ok(), "wrapped BMP must decode");
    }
}
