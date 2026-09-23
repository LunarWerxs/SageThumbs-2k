//! Which pixel [`Layout`] a FourCC, D3DFMT or DXGI format code names.

use super::*;

pub(super) fn ascii(fourcc: &[u8]) -> String {
    // A numeric D3DFMT FourCC is not printable; show it as a number instead.
    if fourcc.iter().all(|c| c.is_ascii_graphic()) {
        String::from_utf8_lossy(fourcc).into_owned()
    } else {
        format!(
            "{:#x}",
            u32::from_le_bytes([fourcc[0], fourcc[1], fourcc[2], fourcc[3]])
        )
    }
}

/// Classic FourCC dispatch. `DXT2`/`DXT4` are `DXT3`/`DXT5` with premultiplied
/// alpha, so they set the alpha mode rather than getting their own decoders.
pub(super) fn fourcc_layout(fourcc: &[u8], alpha_mode: &mut u32) -> Option<Layout> {
    let block = match fourcc {
        b"DXT1" => Block::Bc1,
        b"DXT2" => {
            *alpha_mode = ALPHA_MODE_PREMULTIPLIED;
            Block::Bc2
        }
        b"DXT3" => Block::Bc2,
        b"DXT4" => {
            *alpha_mode = ALPHA_MODE_PREMULTIPLIED;
            Block::Bc3
        }
        b"DXT5" => Block::Bc3,
        // ATI1/ATI2 are the pre-DX10 names for BC4/BC5; the BC4x/BC5x spellings
        // are what newer tools write into a classic header.
        b"ATI1" | b"BC4U" => Block::Bc4 { signed: false },
        b"BC4S" => Block::Bc4 { signed: true },
        b"ATI2" | b"BC5U" => Block::Bc5 { signed: false },
        b"BC5S" => Block::Bc5 { signed: true },
        // A FourCC that is really a small integer is a D3DFORMAT enum value —
        // how DX9-era tools stored the typed (16/32-bit, float) surfaces.
        _ => return d3dfmt_layout(fourcc),
    };
    Some(Layout::Block(block))
}

/// A D3DFORMAT enum value stored in the FourCC field, as DX9-era tools wrote it.
pub(super) fn d3dfmt_layout(fourcc: &[u8]) -> Option<Layout> {
    let d3dfmt = u32::from_le_bytes([fourcc[0], fourcc[1], fourcc[2], fourcc[3]]);
    match d3dfmt {
        36 => Some(Layout::Unorm16(4)),  // A16B16G16R16
        110 => Some(Layout::Snorm16(4)), // Q16W16V16U16
        111 => Some(Layout::Half(1)),    // R16F
        112 => Some(Layout::Half(2)),    // G16R16F
        113 => Some(Layout::Half(4)),    // A16B16G16R16F
        114 => Some(Layout::Float(1)),   // R32F
        115 => Some(Layout::Float(2)),   // G32R32F
        116 => Some(Layout::Float(4)),   // A32B32G32R32F
        _ => None,
    }
}

/// DXGI format → layout. Every TYPELESS/UNORM/SNORM/SRGB spelling of a layout maps
/// to the same decoder; the `_UINT`/`_SINT` integer views are deliberately absent
/// (they are compute data, not pictures, and guessing a scale would invent pixels).
///
/// The numbers are `DXGI_FORMAT` enum values. They are dense and easy to get off
/// by one — BC6H is 94..=96 and BC7 is 97..=99, NOT the 94..=95/96..=98 an older
/// copy of this table in `strip/ddsinfo.rs` used to claim.
pub(super) fn dxgi_layout(dxgi: u32) -> Option<Layout> {
    dxgi_layout_wide_channels(dxgi)
        .or_else(|| dxgi_layout_narrow_channels(dxgi))
        .or_else(|| dxgi_layout_bgra_and_block(dxgi))
}

/// The 32/64-bit-per-channel and two/four-channel formats: `DXGI_FORMAT` 1..=41
/// plus 89 (R10G10B10_XR_BIAS_A2_UNORM).
pub(super) fn dxgi_layout_wide_channels(dxgi: u32) -> Option<Layout> {
    /// `R8G8B8A8` and friends: channel N occupies byte N.
    const RGBA8: Masks = Masks {
        bpp: 32,
        r: 0x0000_00FF,
        g: 0x0000_FF00,
        b: 0x00FF_0000,
        a: 0xFF00_0000,
        grey: false,
    };
    let l = match dxgi {
        1..=2 => Layout::Float(4),   // R32G32B32A32
        5..=6 => Layout::Float(3),   // R32G32B32
        9..=10 => Layout::Half(4),   // R16G16B16A16_FLOAT
        11 => Layout::Unorm16(4),    // R16G16B16A16_UNORM
        13 => Layout::Snorm16(4),    // R16G16B16A16_SNORM
        15..=16 => Layout::Float(2), // R32G32_FLOAT
        23..=24 | 89 => Layout::Masks(Masks {
            // R10G10B10A2
            bpp: 32,
            r: 0x0000_03FF,
            g: 0x000F_FC00,
            b: 0x3FF0_0000,
            a: 0xC000_0000,
            grey: false,
        }),
        26 => Layout::R11G11B10,
        27..=29 => Layout::Masks(RGBA8), // R8G8B8A8_UNORM(_SRGB)
        31 => Layout::Snorm8(4),         // R8G8B8A8_SNORM
        _ => return dxgi_layout_wide_channels_tail(dxgi),
    };
    Some(l)
}

/// The 16/32-bit single-and-double-channel tail of the wide formats: `DXGI_FORMAT`
/// 33..=41.
pub(super) fn dxgi_layout_wide_channels_tail(dxgi: u32) -> Option<Layout> {
    let l = match dxgi {
        33..=34 => Layout::Half(2), // R16G16_FLOAT
        35 => Layout::Masks(Masks {
            // R16G16_UNORM
            bpp: 32,
            r: 0x0000_FFFF,
            g: 0xFFFF_0000,
            b: 0,
            a: 0,
            grey: false,
        }),
        37 => Layout::Snorm16(2),    // R16G16_SNORM
        39..=41 => Layout::Float(1), // R32_FLOAT / D32_FLOAT
        _ => return None,
    };
    Some(l)
}

/// The single-and-double-byte-channel formats and the block-compressed formats
/// that don't need the BGRA masks: `DXGI_FORMAT` 48..=84.
pub(super) fn dxgi_layout_narrow_channels(dxgi: u32) -> Option<Layout> {
    let l = match dxgi {
        48..=49 => Layout::Masks(Masks {
            // R8G8_UNORM
            bpp: 16,
            r: 0x00FF,
            g: 0xFF00,
            b: 0,
            a: 0,
            grey: false,
        }),
        51 => Layout::Snorm8(2),    // R8G8_SNORM
        53..=54 => Layout::Half(1), // R16_FLOAT
        55..=56 => Layout::Masks(Masks {
            // R16_UNORM / D16_UNORM
            bpp: 16,
            r: 0xFFFF,
            g: 0,
            b: 0,
            a: 0,
            grey: true,
        }),
        58 => Layout::Snorm16(1), // R16_SNORM
        // R8_UNORM and A8_UNORM both render as greyscale: a single-channel
        // texture shown as pure red — or an alpha-only one as fully transparent
        // black — is indistinguishable from a broken thumbnail.
        60..=62 | 65 => Layout::Masks(Masks {
            bpp: 8,
            r: 0xFF,
            g: 0,
            b: 0,
            a: 0,
            grey: true,
        }),
        63 => Layout::Snorm8(1), // R8_SNORM
        67 => Layout::Rgb9E5,
        70..=72 => Layout::Block(Block::Bc1),
        _ => return dxgi_layout_narrow_channels_tail(dxgi),
    };
    Some(l)
}

/// The block-compressed tail of the narrow formats: `DXGI_FORMAT` 73..=84.
pub(super) fn dxgi_layout_narrow_channels_tail(dxgi: u32) -> Option<Layout> {
    let l = match dxgi {
        73..=75 => Layout::Block(Block::Bc2),
        76..=78 => Layout::Block(Block::Bc3),
        79..=80 => Layout::Block(Block::Bc4 { signed: false }),
        81 => Layout::Block(Block::Bc4 { signed: true }),
        82..=83 => Layout::Block(Block::Bc5 { signed: false }),
        84 => Layout::Block(Block::Bc5 { signed: true }),
        _ => return None,
    };
    Some(l)
}

/// The BGRA-ordered 16/32-bit formats, the HDR block formats, and the legacy
/// tail: `DXGI_FORMAT` 85..=115.
pub(super) fn dxgi_layout_bgra_and_block(dxgi: u32) -> Option<Layout> {
    const BGRA8: Masks = Masks {
        bpp: 32,
        r: 0x00FF_0000,
        g: 0x0000_FF00,
        b: 0x0000_00FF,
        a: 0xFF00_0000,
        grey: false,
    };
    let l = match dxgi {
        85 => Layout::Masks(Masks {
            // B5G6R5
            bpp: 16,
            r: 0xF800,
            g: 0x07E0,
            b: 0x001F,
            a: 0,
            grey: false,
        }),
        86 => Layout::Masks(Masks {
            // B5G5R5A1
            bpp: 16,
            r: 0x7C00,
            g: 0x03E0,
            b: 0x001F,
            a: 0x8000,
            grey: false,
        }),
        87 | 90..=91 => Layout::Masks(BGRA8), // B8G8R8A8_UNORM(_SRGB)
        // B8G8R8X8: the 4th byte is padding, not alpha — zero the mask so the
        // surface renders opaque instead of invisible.
        88 | 92..=93 => Layout::Masks(Masks { a: 0, ..BGRA8 }),
        94..=95 => Layout::Block(Block::Bc6h { signed: false }),
        96 => Layout::Block(Block::Bc6h { signed: true }),
        97..=99 => Layout::Block(Block::Bc7),
        115 => Layout::Masks(Masks {
            // B4G4R4A4
            bpp: 16,
            r: 0x0F00,
            g: 0x00F0,
            b: 0x000F,
            a: 0xF000,
            grey: false,
        }),
        _ => return None,
    };
    Some(l)
}
