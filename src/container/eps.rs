//! Encapsulated PostScript (`.eps`) embedded preview.
//!
//! Rendering real PostScript needs Ghostscript (not bundled — see the EPS/PS
//! backlog item), but the common **DOS-EPS binary** flavor (what Adobe/Corel
//! tools export for Windows) wraps the PostScript in a 30-byte header that
//! carries offsets to a baked-in **TIFF** (or WMF) screen preview. We slice the
//! TIFF out and let the normal image tiers decode it — same trick as the PSD
//! resource-1036 thumbnail, zero new decode code.
//!
//! Plain-text EPS (`%!PS-Adobe…` with no binary header) has no raster preview;
//! it falls through to the ImageMagick tier, which renders it only where
//! Ghostscript is installed. All reads here are bounds-checked slices.

/// DOS-EPS binary-header magic.
const MAGIC: [u8; 4] = [0xC5, 0xD0, 0xD3, 0xC6];

use super::util::le32;

/// Extract the embedded TIFF preview from a DOS-EPS, or None.
///
/// Header layout (all little-endian u32 pairs after the magic):
/// PS (offset @4, len @8) · WMF (@12, @16) · TIFF (@20, @24) · checksum @28.
pub fn extract(bytes: &[u8]) -> Option<Vec<u8>> {
    if !bytes.starts_with(&MAGIC) {
        return None;
    }
    let off = le32(bytes, 20)? as usize;
    let len = le32(bytes, 24)? as usize;
    if off == 0 || len < 8 {
        return None; // no TIFF preview (WMF-only or bare PS) — we can't draw WMF
    }
    // Bound the preview we hand back (shared CBXMEM cap): the declared length is
    // attacker-controlled and the TIFF is decoded downstream under panic=abort.
    if len as u64 > crate::container::MAX_COVER {
        return None;
    }
    let tiff = bytes.get(off..off.checked_add(len)?)?;
    // Sanity: a real TIFF starts "II*\0" (LE) or "MM\0*" (BE).
    if tiff.starts_with(b"II\x2A\x00") || tiff.starts_with(b"MM\x00\x2A") {
        Some(tiff.to_vec())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_tiff() -> Vec<u8> {
        let mut buf = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(6, 4, image::Rgb([40, 160, 220])))
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Tiff)
            .unwrap();
        buf
    }

    /// Wrap `ps` + a TIFF preview in a DOS-EPS binary header, like Adobe exports.
    fn dos_eps(ps: &[u8], tiff: &[u8]) -> Vec<u8> {
        let ps_off = 30u32;
        let tiff_off = ps_off + ps.len() as u32;
        let mut out = Vec::new();
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&ps_off.to_le_bytes());
        out.extend_from_slice(&(ps.len() as u32).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // WMF offset (none)
        out.extend_from_slice(&0u32.to_le_bytes()); // WMF length
        out.extend_from_slice(&tiff_off.to_le_bytes());
        out.extend_from_slice(&(tiff.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xFF, 0xFF]); // checksum: FFFF = unused
        out.extend_from_slice(ps);
        out.extend_from_slice(tiff);
        out
    }

    #[test]
    fn extracts_dos_eps_tiff_preview() {
        let tiff = tiny_tiff();
        let eps = dos_eps(b"%!PS-Adobe-3.0 EPSF-3.0\n%%BoundingBox: 0 0 6 4\nshowpage\n", &tiff);
        let got = extract(&eps).expect("TIFF preview");
        assert_eq!(got, tiff);
        assert!(image::load_from_memory(&got).is_ok(), "preview should decode as TIFF");
    }

    #[test]
    fn rejects_plain_and_malformed_eps() {
        // Plain-text EPS: no binary header, nothing to extract.
        assert!(extract(b"%!PS-Adobe-3.0 EPSF-3.0\nshowpage\n").is_none());
        // Truncated header.
        assert!(extract(&MAGIC).is_none());
        // Header whose TIFF offsets point past EOF must fail cleanly, not panic.
        let mut bad = Vec::new();
        bad.extend_from_slice(&MAGIC);
        bad.extend_from_slice(&30u32.to_le_bytes());
        bad.extend_from_slice(&4u32.to_le_bytes());
        bad.extend_from_slice(&[0u8; 8]);
        bad.extend_from_slice(&0xFFFF_FF00u32.to_le_bytes()); // absurd TIFF offset
        bad.extend_from_slice(&0xFFFF_FF00u32.to_le_bytes()); // absurd TIFF length
        bad.extend_from_slice(&[0xFF, 0xFF]);
        assert!(extract(&bad).is_none());
    }
}
