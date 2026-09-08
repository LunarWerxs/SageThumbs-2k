//! FGbz foreground palette encoder — produces FGbz chunk payloads.
//!
//! Encoding counterpart to the decoder in [`crate::djvu_render`]
//! (`parse_fgbz`). FGbz carries the foreground palette of DjVu's "color
//! text" layered model: a flat 24-bit palette plus an optional table of
//! per-blit indices selecting which palette entry colours each Sjbz
//! shape on the page.
//!
//! ## Wire format
//!
//! ```text
//! byte 0:        version; bit 7 set ⇒ index table follows; other bits MBZ
//! bytes 1..3:    big-endian u16 — palette size N (≤ 65 535)
//! bytes 3..3+3N: N × BGR triples (one byte each: B, G, R) — raw, not BZZ
//! if bit 7 set:
//!     bytes [3+3N .. 3+3N+3]: big-endian u24 — index count M (≤ 2²⁴−1)
//!     bytes [3+3N+3 ..]:      BZZ-compressed payload of M × i16 (BE)
//! ```
//!
//! Round-trip is bit-exact for the raw palette section. The index
//! section is round-trip-equal at the *value* level (the BZZ envelope
//! may differ byte-for-byte vs. another implementation, since BZZ is
//! not canonical, but `decode(encode(idx)) == idx`).

use crate::bzz::bzz_decode;
use crate::bzz_encode::bzz_encode;
use crate::chunk_encode::EncodeError;
use crate::error::DjVuError;

/// One palette entry (24-bit RGB).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FgbzColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// Encode an FGbz chunk payload from a palette and optional per-blit
/// index table.
///
/// # Errors
///
/// - [`EncodeError::PaletteOverflow`] if `palette.len() > 65 535`.
/// - [`EncodeError::IndexCountOverflow`] if `indices.map(len) ≥ 2²⁴`.
///
/// Both are wire-format limits; this replaces the previous `assert!` panic.
pub fn encode_fgbz(palette: &[FgbzColor], indices: Option<&[i16]>) -> Result<Vec<u8>, EncodeError> {
    if palette.len() > u16::MAX as usize {
        return Err(EncodeError::PaletteOverflow {
            found: palette.len(),
        });
    }
    if let Some(idx) = indices
        && idx.len() >= (1usize << 24)
    {
        return Err(EncodeError::IndexCountOverflow { found: idx.len() });
    }

    let cap = 3 + palette.len() * 3 + indices.map_or(0, |i| 3 + i.len() * 2);
    let mut out = Vec::with_capacity(cap);

    out.push(if indices.is_some() { 0x80 } else { 0 });
    out.extend_from_slice(&(palette.len() as u16).to_be_bytes());

    for c in palette {
        out.push(c.b);
        out.push(c.g);
        out.push(c.r);
    }

    if let Some(idx) = indices {
        let n = idx.len() as u32;
        out.push(((n >> 16) & 0xff) as u8);
        out.push(((n >> 8) & 0xff) as u8);
        out.push((n & 0xff) as u8);

        let mut raw = Vec::with_capacity(idx.len() * 2);
        for &v in idx {
            raw.extend_from_slice(&v.to_be_bytes());
        }
        out.extend_from_slice(&bzz_encode(&raw));
    }

    Ok(out)
}

/// Decode an FGbz chunk payload.
///
/// Inverse of [`encode_fgbz`] — returns the palette and (when present)
/// the per-blit index table. Independent of the renderer's internal
/// `parse_fgbz`; both share the same wire spec.
pub fn decode_fgbz(data: &[u8]) -> Result<(Vec<FgbzColor>, Vec<i16>), DjVuError> {
    if data.len() < 3 {
        return Ok((Vec::new(), Vec::new()));
    }

    let version = data[0];
    let has_indices = (version & 0x80) != 0;
    if version & 0x7f != 0 {
        return Err(DjVuError::InvalidStructure(
            "FGbz: reserved version bits set",
        ));
    }

    let n_colors = u16::from_be_bytes([data[1], data[2]]) as usize;
    let color_bytes = n_colors * 3;
    if data.len() < 3 + color_bytes {
        return Err(DjVuError::InvalidStructure("FGbz: palette truncated"));
    }

    let mut colors = Vec::with_capacity(n_colors);
    for i in 0..n_colors {
        let base = 3 + i * 3;
        colors.push(FgbzColor {
            b: data[base],
            g: data[base + 1],
            r: data[base + 2],
        });
    }

    let mut indices = Vec::new();
    if has_indices {
        let idx_start = 3 + color_bytes;
        if idx_start + 3 > data.len() {
            return Err(DjVuError::InvalidStructure("FGbz: index header truncated"));
        }
        let m = ((data[idx_start] as u32) << 16)
            | ((data[idx_start + 1] as u32) << 8)
            | (data[idx_start + 2] as u32);
        let bzz = &data[idx_start + 3..];
        let raw = bzz_decode(bzz)?;
        if raw.len() < (m as usize) * 2 {
            return Err(DjVuError::InvalidStructure(
                "FGbz: index payload shorter than declared count",
            ));
        }
        indices.reserve(m as usize);
        for i in 0..(m as usize) {
            indices.push(i16::from_be_bytes([raw[i * 2], raw[i * 2 + 1]]));
        }
    }

    Ok((colors, indices))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_palette_no_indices() {
        let bytes = encode_fgbz(&[], None).unwrap();
        assert_eq!(bytes, [0x00, 0x00, 0x00]);
        let (colors, idx) = decode_fgbz(&bytes).unwrap();
        assert!(colors.is_empty());
        assert!(idx.is_empty());
    }

    #[test]
    fn palette_only_byte_layout() {
        let palette = [
            FgbzColor {
                r: 0x11,
                g: 0x22,
                b: 0x33,
            },
            FgbzColor {
                r: 0xaa,
                g: 0xbb,
                b: 0xcc,
            },
        ];
        let bytes = encode_fgbz(&palette, None).unwrap();
        // version=0, n=0x0002, then BGR triples
        assert_eq!(
            bytes,
            [0x00, 0x00, 0x02, 0x33, 0x22, 0x11, 0xcc, 0xbb, 0xaa]
        );
    }

    #[test]
    fn version_byte_signals_index_presence() {
        let palette = [FgbzColor { r: 1, g: 2, b: 3 }];
        let with_idx = encode_fgbz(&palette, Some(&[0])).unwrap();
        let without_idx = encode_fgbz(&palette, None).unwrap();
        assert_eq!(with_idx[0] & 0x80, 0x80);
        assert_eq!(without_idx[0] & 0x80, 0x00);
    }

    #[test]
    fn roundtrip_palette_only() {
        let palette: Vec<FgbzColor> = (0..256)
            .map(|i| FgbzColor {
                r: i as u8,
                g: (i * 3) as u8,
                b: (i * 7) as u8,
            })
            .collect();
        let bytes = encode_fgbz(&palette, None).unwrap();
        let (decoded, idx) = decode_fgbz(&bytes).unwrap();
        assert_eq!(decoded, palette);
        assert!(idx.is_empty());
    }

    #[test]
    fn roundtrip_with_indices() {
        let palette = [
            FgbzColor { r: 255, g: 0, b: 0 },
            FgbzColor { r: 0, g: 255, b: 0 },
            FgbzColor { r: 0, g: 0, b: 255 },
        ];
        let indices: Vec<i16> = (0..1000).map(|i| (i % 3) as i16).collect();
        let bytes = encode_fgbz(&palette, Some(&indices)).unwrap();
        let (dp, di) = decode_fgbz(&bytes).unwrap();
        assert_eq!(dp.as_slice(), palette.as_slice());
        assert_eq!(di, indices);
    }

    #[test]
    fn roundtrip_negative_indices_preserved() {
        // Per the wire spec, indices are signed i16. -1 is sometimes used
        // by encoders to mean "no palette entry / transparent".
        let palette = [FgbzColor { r: 1, g: 2, b: 3 }];
        let indices: Vec<i16> = vec![0, -1, 0, -1, 0];
        let bytes = encode_fgbz(&palette, Some(&indices)).unwrap();
        let (_, di) = decode_fgbz(&bytes).unwrap();
        assert_eq!(di, indices);
    }

    #[test]
    fn roundtrip_via_renderer_parse_fgbz() {
        // Sanity-check that our wire output is byte-compatible with the
        // renderer's independent `parse_fgbz` implementation in
        // `djvu_render` — they must agree on the spec.
        let palette = [
            FgbzColor {
                r: 10,
                g: 20,
                b: 30,
            },
            FgbzColor {
                r: 40,
                g: 50,
                b: 60,
            },
            FgbzColor {
                r: 70,
                g: 80,
                b: 90,
            },
        ];
        let indices: Vec<i16> = vec![0, 1, 2, 1, 0, 2];
        let bytes = encode_fgbz(&palette, Some(&indices)).unwrap();

        let parsed = crate::fgbz::parse_fgbz(&bytes).expect("parse_fgbz");
        assert_eq!(parsed.colors.len(), palette.len());
        for (a, b) in parsed.colors.iter().zip(palette.iter()) {
            assert_eq!((a.r, a.g, a.b), (b.r, b.g, b.b));
        }
        assert_eq!(parsed.indices, indices);
    }

    #[test]
    fn decode_rejects_truncated_palette() {
        // Declared 5 colours but only 1 BGR triple of bytes after the header.
        let bytes = [0x00, 0x00, 0x05, 0x01, 0x02, 0x03];
        assert!(decode_fgbz(&bytes).is_err());
    }

    #[test]
    fn decode_short_data_returns_empty() {
        let (colors, idx) = decode_fgbz(&[]).unwrap();
        assert!(colors.is_empty());
        assert!(idx.is_empty());
        let (colors2, _) = decode_fgbz(&[0x00, 0x00]).unwrap();
        assert!(colors2.is_empty());
    }

    #[test]
    fn decode_rejects_reserved_version_bits() {
        // version byte = 0x01 → lower 7 bits non-zero → reserved bits set
        let data = [0x01u8, 0x00, 0x00];
        assert!(decode_fgbz(&data).is_err());
    }

    #[test]
    fn decode_rejects_index_payload_too_short() {
        // BZZ-encode 2 bytes (enough for 1 i16), but declare m=2 (needs 4 bytes).
        // This makes bzz_decode succeed but raw.len()=2 < m*2=4 → line 133 error.
        let bzz = crate::bzz_encode::bzz_encode(&[0x00, 0x01]);
        let mut data = Vec::new();
        data.push(0x80); // version: has index table
        data.extend_from_slice(&1u16.to_be_bytes()); // n_colors = 1
        data.extend_from_slice(&[0x01, 0x02, 0x03]); // BGR triple
        data.extend_from_slice(&[0x00, 0x00, 0x02]); // m = 2 (needs 4 decoded bytes)
        data.extend_from_slice(&bzz); // encodes only 2 bytes
        assert!(decode_fgbz(&data).is_err());
    }

    #[test]
    fn decode_rejects_truncated_index_header() {
        // version=0x80 (has indices), 1 colour, then nothing.
        let bytes = [0x80, 0x00, 0x01, 0x01, 0x02, 0x03];
        assert!(decode_fgbz(&bytes).is_err());
    }
}
