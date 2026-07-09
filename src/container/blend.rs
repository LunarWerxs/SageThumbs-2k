//! Blender `.blend` embedded thumbnail. Blender bakes a small RGBA screenshot
//! into a `TEST` file-block — no Blender install, no rendering. We walk the
//! file-block stream to that block and read its raw pixels.
//!
//! Header magic `BLENDER`, then either the legacy 12-byte header (pointer-size
//! byte `_`=32-bit / `-`=64-bit, endian byte `v`/`V`) or the Blender-5.x 17-byte
//! v1 header (`17`, always 64-bit little-endian). Multi-byte ints are
//! little-endian on every real file. Compressed `.blend` (gzip/zstd) has no
//! `BLENDER` magic → we return None (handled by the normal tiers / icon).

use image::{DynamicImage, RgbaImage};

const MAX_EDGE: u32 = 4096;

/// Extract the embedded thumbnail as decoded pixels, or None.
pub fn extract(bytes: &[u8]) -> Option<DynamicImage> {
    if bytes.len() < 12 || &bytes[0..7] != b"BLENDER" {
        return None;
    }
    // Pointer size + where the block stream starts + which block-header variant.
    let b7 = bytes[7];
    let (ptr_size, block_start, v1) = if b7 == b'_' {
        (4usize, 12usize, false)
    } else if b7 == b'-' {
        (8, 12, false)
    } else if &bytes[7..9] == b"17" {
        (8, 17, true) // Blender 5.x LargeBHead8
    } else {
        return None;
    };
    let legacy_hdr = 16 + ptr_size; // BHead4 = 20, SmallBHead8 = 24

    let mut off = block_start;
    while off + 8 <= bytes.len() {
        let code = bytes.get(off..off + 4)?;
        if code == b"ENDB" {
            break;
        }
        let (len, hdr) = if v1 {
            // LargeBHead8: code i32, sdna i32, old u64, len i64@16, count i64
            (i64::from_le_bytes(bytes.get(off + 16..off + 24)?.try_into().ok()?) as usize, 32usize)
        } else {
            // BHead4 / SmallBHead8: len i32 at +4
            (i32::from_le_bytes(bytes.get(off + 4..off + 8)?.try_into().ok()?) as usize, legacy_hdr)
        };

        if code == b"TEST" {
            let body = off.checked_add(hdr)?;
            let w = i32::from_le_bytes(bytes.get(body..body + 4)?.try_into().ok()?);
            let h = i32::from_le_bytes(bytes.get(body + 4..body + 8)?.try_into().ok()?);
            if w > 0 && h > 0 && w as u32 <= MAX_EDGE && h as u32 <= MAX_EDGE {
                let (w, h) = (w as u32, h as u32);
                let px_bytes = (w as usize).checked_mul(h as usize)?.checked_mul(4)?;
                // Integrity check: the block length is exactly 8 + w*h*4.
                if len == 8 + px_bytes {
                    let px = bytes.get(body + 8..body + 8 + px_bytes)?;
                    let img = RgbaImage::from_raw(w, h, px.to_vec())?;
                    return Some(DynamicImage::ImageRgba8(img));
                }
            }
        }
        off = off.checked_add(hdr)?.checked_add(len)?;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_legacy_test_block_thumbnail() {
        let (w, h) = (4u32, 3u32);
        let px = vec![200u8; (w * h * 4) as usize];

        let mut b = Vec::new();
        b.extend_from_slice(b"BLENDER");
        b.push(b'_'); // 32-bit pointers
        b.push(b'v'); // little-endian
        b.extend_from_slice(b"277"); // 3 version digits → 12-byte header
        // BHead4 (20 bytes): code, len, old(4), sdna, nr
        b.extend_from_slice(b"TEST");
        b.extend_from_slice(&((8 + w * h * 4) as i32).to_le_bytes());
        b.extend_from_slice(&[0u8; 12]); // old(4) + sdna(4) + nr(4)
        // body: width, height, RGBA
        b.extend_from_slice(&(w as i32).to_le_bytes());
        b.extend_from_slice(&(h as i32).to_le_bytes());
        b.extend_from_slice(&px);
        b.extend_from_slice(b"ENDB");
        b.extend_from_slice(&[0u8; 16]);

        let img = extract(&b).expect("thumbnail");
        assert_eq!((img.width(), img.height()), (4, 3));
        assert!(extract(b"not a blend file").is_none());

        // The oversized-file rescue hands extract() a bounded HEAD PREFIX of a much
        // larger file. TEST sits near the head, so a prefix that contains it must
        // still extract; a prefix cut BEFORE/INSIDE the TEST body must return None
        // (bounds-checked walk), never panic or mis-decode.
        let mut padded = b.clone();
        padded.extend_from_slice(&vec![0u8; 4096]); // simulated giant tail
        let full_end = b.len() - 20; // prefix that still contains all of TEST
        assert!(extract(&padded[..full_end]).is_some(), "prefix containing TEST extracts");
        assert!(extract(&padded[..40]).is_none(), "prefix truncating TEST is a clean miss");
    }
}
