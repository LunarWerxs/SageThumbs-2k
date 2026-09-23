//! A DDS header in front of one texture surface, so the game-texture containers (`vtf.rs`,
//! `ktx.rs`) hand their pixels to `decode/dds.rs` instead of carrying decoders of their own.
//!
//! Every block format they use (BC1-BC7) and every plain channel layout is already decoded
//! there, with its bomb budget and its reduced decode for a small thumbnail. So the texture
//! modules only find the surface and describe it; this writes the 128-byte header (plus the
//! 20-byte DX10 extension when a `DXGI_FORMAT` is needed) and the bytes go back through the
//! normal decode tiers like any other cover.

use super::MAX_COVER;

/// `DDS_PIXELFORMAT.dwFlags` bits, the same values `decode/dds.rs` reads.
pub(super) const DDPF_ALPHAPIXELS: u32 = 0x1;
pub(super) const DDPF_ALPHA: u32 = 0x2;
const DDPF_FOURCC: u32 = 0x4;
pub(super) const DDPF_RGB: u32 = 0x40;
pub(super) const DDPF_LUMINANCE: u32 = 0x2_0000;
/// Colour with alpha: the flags every RGBA mask layout carries.
pub(super) const DDPF_RGBA: u32 = DDPF_RGB | DDPF_ALPHAPIXELS;

/// The most [`wrap`] puts in front of a surface: the 128-byte header plus the DX10 extension.
/// A surface is a cover only if it fits under `MAX_COVER` together with this.
pub(super) const MAX_HEADER: u64 = 128 + 20;

/// How the surface's pixels are laid out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Pixels {
    /// A classic FourCC (`DXT1`, `DXT3`, `DXT5`, `ATI1`, ...) or a D3DFORMAT number.
    FourCc([u8; 4]),
    /// A DX10 extension header naming a `DXGI_FORMAT` (BC6H, BC7).
    Dxgi(u32),
    /// Uncompressed: the `DDPF_*` flags, bits per pixel, then the R, G, B and A masks.
    Masks {
        flags: u32,
        bpp: u32,
        rgba: [u32; 4],
    },
}

/// An uncompressed layout, for the containers' format tables.
pub(super) const fn masks(flags: u32, bpp: u32, rgba: [u32; 4]) -> Pixels {
    Pixels::Masks { flags, bpp, rgba }
}

/// The two 64-bit-a-pixel D3DFORMAT numbers the containers use, as the FourCC field holds
/// them: `A16B16G16R16F` (113) and `A16B16G16R16` (36).
pub(super) const D3DFMT_RGBA16F: [u8; 4] = 113u32.to_le_bytes();
pub(super) const D3DFMT_RGBA16: [u8; 4] = 36u32.to_le_bytes();

impl Pixels {
    /// Bytes one `w` x `h` surface of this layout occupies, or `None` on overflow or for a
    /// FourCC this module does not know the size of. Block formats are 4x4 blocks.
    pub(super) fn surface_len(self, w: u32, h: u32) -> Option<u64> {
        let pixels = u64::from(w).checked_mul(u64::from(h))?;
        match self {
            Pixels::Masks { bpp, .. } => pixels.checked_mul(u64::from(bpp / 8)),
            Pixels::FourCc(cc) if cc == D3DFMT_RGBA16F || cc == D3DFMT_RGBA16 => {
                pixels.checked_mul(8)
            }
            _ => {
                let blocks = u64::from(w.div_ceil(4).max(1)) * u64::from(h.div_ceil(4).max(1));
                blocks.checked_mul(self.block_bytes()?)
            }
        }
    }

    /// Bytes one 4x4 block takes, for the block formats.
    fn block_bytes(self) -> Option<u64> {
        match self {
            Pixels::FourCc(cc) => match &cc {
                b"DXT1" | b"ATI1" | b"BC4S" => Some(8),
                b"DXT3" | b"DXT5" | b"ATI2" | b"BC5S" => Some(16),
                _ => None,
            },
            Pixels::Dxgi(_) => Some(16),
            Pixels::Masks { .. } => None,
        }
    }
}

/// `"DDS "` + a DDS header for one `width` x `height` surface + `surface`. `None` when the
/// surface is not exactly the size its layout needs, or the result is past [`MAX_COVER`].
pub(super) fn wrap(width: u32, height: u32, px: Pixels, surface: &[u8]) -> Option<Vec<u8>> {
    if width == 0 || height == 0 || px.surface_len(width, height)? != surface.len() as u64 {
        return None;
    }
    let dx10 = matches!(px, Pixels::Dxgi(_));
    let total = 128 + if dx10 { 20 } else { 0 } + surface.len();
    if total as u64 > MAX_COVER {
        return None;
    }
    let (pf_flags, fourcc, bpp, rgba) = match px {
        Pixels::FourCc(cc) => (DDPF_FOURCC, cc, 0, [0; 4]),
        Pixels::Dxgi(_) => (DDPF_FOURCC, *b"DX10", 0, [0; 4]),
        Pixels::Masks { flags, bpp, rgba } => (flags, [0; 4], bpp, rgba),
    };
    let mut out = Vec::with_capacity(total);
    let mut put = |v: u32| out.extend_from_slice(&v.to_le_bytes());
    put(u32::from_le_bytes(*b"DDS "));
    put(124); // dwSize
    put(0x1007); // DDSD_CAPS | DDSD_HEIGHT | DDSD_WIDTH | DDSD_PIXELFORMAT
    put(height);
    put(width);
    put(0); // dwPitchOrLinearSize
    put(0); // dwDepth
    put(1); // dwMipMapCount: only this surface follows
    for _ in 0..11 {
        put(0); // dwReserved1
    }
    put(32); // DDS_PIXELFORMAT.dwSize
    put(pf_flags);
    put(u32::from_le_bytes(fourcc));
    put(bpp);
    for m in rgba {
        put(m);
    }
    put(0x1000); // DDSCAPS_TEXTURE
    for _ in 0..4 {
        put(0); // dwCaps2..4, dwReserved2
    }
    if let Pixels::Dxgi(format) = px {
        put(format);
        put(3); // D3D10_RESOURCE_DIMENSION_TEXTURE2D
        put(0); // miscFlag
        put(1); // arraySize
        put(0); // miscFlags2: alpha mode unknown
    }
    out.extend_from_slice(surface);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wrapped_surface_decodes_through_the_dds_tier() {
        // 2x2 RGBA8, bytes R G B A: red, green, blue, white.
        let px = Pixels::Masks {
            flags: DDPF_RGB | DDPF_ALPHAPIXELS,
            bpp: 32,
            rgba: [0xFF, 0xFF00, 0xFF_0000, 0xFF00_0000],
        };
        let surface = [
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
        ];
        let dds = wrap(2, 2, px, &surface).expect("wraps");
        let img = crate::decode::decode_preview(&dds)
            .expect("decodes")
            .to_rgba8();
        assert_eq!(img.dimensions(), (2, 2));
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0, 255]);
        assert_eq!(img.get_pixel(1, 0).0, [0, 255, 0, 255]);
        assert_eq!(img.get_pixel(0, 1).0, [0, 0, 255, 255]);
    }

    #[test]
    fn a_surface_of_the_wrong_length_is_refused() {
        let dxt1 = Pixels::FourCc(*b"DXT1");
        assert_eq!(dxt1.surface_len(5, 5), Some(4 * 8));
        assert!(wrap(5, 5, dxt1, &[0; 31]).is_none());
        assert!(wrap(5, 5, dxt1, &[0; 32]).is_some());
        assert!(wrap(0, 5, dxt1, &[]).is_none());
        assert_eq!(Pixels::Dxgi(98).surface_len(8, 4), Some(2 * 16));
    }
}
