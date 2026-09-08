//! FGbz foreground-palette parser.
//!
//! Decodes the `FGbz` chunk payload into a color palette and an optional
//! per-blit color-index table. The render module consumes the already-decoded
//! [`FgbzPalette`]; keeping the parser here means [`crate::djvu_render`] never
//! touches BZZ directly — the only `bzz_decode` call for the index table lives
//! in this module.

#[cfg(not(feature = "std"))]
use alloc::vec::Vec;

use crate::error::BzzError;

/// An RGB color from the FGbz palette.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PaletteColor {
    pub(crate) r: u8,
    pub(crate) g: u8,
    pub(crate) b: u8,
}

/// Parsed FGbz data: palette colors and optional per-blit color indices.
pub(crate) struct FgbzPalette {
    pub(crate) colors: Vec<PaletteColor>,
    /// Per-blit color index: `indices[blit_idx]` → index into `colors`.
    /// Empty when the FGbz chunk has no index table (version bit 7 unset).
    pub(crate) indices: Vec<i16>,
}

/// Parse the FGbz chunk into palette colors and per-blit index table.
///
/// FGbz format:
/// - byte 0: version (bit 7 = has index table, bits 6-0 must be 0)
/// - byte 1-2: big-endian u16 palette size (number of colors)
/// - next `palette_size * 3` bytes: BGR triples (raw if version=0, BZZ if version has bit 0 set)
/// - if bit 7 set: 3-byte big-endian count + BZZ-compressed i16be index table
pub(crate) fn parse_fgbz(data: &[u8]) -> Result<FgbzPalette, BzzError> {
    if data.len() < 3 {
        return Ok(FgbzPalette {
            colors: Vec::new(),
            indices: Vec::new(),
        });
    }

    let version = data[0];
    let has_indices = (version & 0x80) != 0;

    let n_colors =
        u16::from_be_bytes([*data.get(1).unwrap_or(&0), *data.get(2).unwrap_or(&0)]) as usize;

    if n_colors == 0 {
        return Ok(FgbzPalette {
            colors: Vec::new(),
            indices: Vec::new(),
        });
    }

    // Colors: raw BGR triples starting at byte 3
    let color_bytes = n_colors * 3;
    let color_data = data.get(3..).unwrap_or(&[]);

    let mut colors = Vec::with_capacity(n_colors);
    for i in 0..n_colors {
        let base = i * 3;
        if base + 2 < color_data.len().min(color_bytes) {
            colors.push(PaletteColor {
                r: color_data[base + 2],
                g: color_data[base + 1],
                b: color_data[base],
            });
        } else {
            colors.push(PaletteColor { r: 0, g: 0, b: 0 });
        }
    }

    // Per-blit index table
    let mut indices = Vec::new();
    if has_indices {
        let idx_start = 3 + color_bytes;
        if idx_start + 3 <= data.len() {
            let num_indices = ((data[idx_start] as u32) << 16)
                | ((data[idx_start + 1] as u32) << 8)
                | (data[idx_start + 2] as u32);

            let bzz_data = data.get(idx_start + 3..).unwrap_or(&[]);
            let decoded = crate::bzz::bzz_decode(bzz_data)?;

            let n = num_indices as usize;
            // `num_indices` is a raw 24-bit field (up to ~16 M → 32 MB reserve).
            // The loop only pushes while `i*2+1 < decoded.len()`, so never more than
            // decoded.len()/2 entries — cap the reservation to that.
            indices.reserve(n.min(decoded.len() / 2));
            for i in 0..n {
                if i * 2 + 1 < decoded.len() {
                    indices.push(i16::from_be_bytes([decoded[i * 2], decoded[i * 2 + 1]]));
                }
            }
        }
    }

    Ok(FgbzPalette { colors, indices })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_fgbz_short_data_returns_empty() {
        // data.len() < 3 → empty palette
        let result = parse_fgbz(&[]).unwrap();
        assert!(result.colors.is_empty());
        assert!(result.indices.is_empty());

        let result = parse_fgbz(&[0x00, 0x00]).unwrap();
        assert!(result.colors.is_empty());
    }

    #[test]
    fn parse_fgbz_zero_colors_returns_empty() {
        // version=0, n_colors=0 → empty palette
        let data = [0x00u8, 0x00, 0x00]; // version, n_colors (u16be) = 0
        let result = parse_fgbz(&data).unwrap();
        assert!(result.colors.is_empty());
    }

    #[test]
    fn parse_fgbz_truncated_color_data_fills_black() {
        // n_colors=2 but only 3 bytes of color data (1 full color, 1 truncated)
        let mut data = Vec::new();
        data.push(0x00); // version: no index table
        data.extend_from_slice(&2u16.to_be_bytes()); // n_colors = 2
        // Only 3 bytes for first color (BGR): b=10, g=20, r=30
        data.extend_from_slice(&[10, 20, 30]);
        // Second color is truncated (not enough bytes)
        let result = parse_fgbz(&data).unwrap();
        assert_eq!(result.colors.len(), 2);
        // First color parsed: r=30, g=20, b=10
        assert_eq!(result.colors[0].r, 30);
        // Second color falls back to black (0,0,0)
        assert_eq!(result.colors[1].r, 0);
        assert_eq!(result.colors[1].g, 0);
        assert_eq!(result.colors[1].b, 0);
    }
}
