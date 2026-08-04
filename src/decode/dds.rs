//! Native DDS (DirectDraw Surface) decoding — pure Rust, every block-compressed
//! layout plus the uncompressed ones textures actually ship in.
//!
//! WHY THIS TIER EXISTS (measured 2026-08-03, v1.7.2 — an uninstall comment said
//! "breaks on modern DDS like BC7 and BC6H", and it was right):
//!   * the `image` crate's DDS decoder handles **only** DXT1/DXT3/DXT5 and
//!     refuses every uncompressed layout outright;
//!   * Windows' own WIC "DDS Decoder" also stops at DXT1/3/5 — it rejects BC4,
//!     BC5, BC6H, BC7 and all uncompressed DDS;
//!   * ImageMagick (bundled with the FULL install only) covers BC7 and the 8-bit
//!     uncompressed layouts but NOT BC4, BC5_SNORM, BC6H, or float DDS.
//!
//! So BC7 — the format every modern game texture uses — worked only on a Full
//! install and via a 20 s subprocess, and BC6H/BC4 worked nowhere at all.
//!
//! Block decoding is delegated to `bcdec_rs` (MIT, `no_std`, zero-dependency, and
//! fuzzed against the original C `bcdec` for identical behaviour). The container
//! parsing — headers, DXGI/D3DFMT dispatch, bit-mask layouts, bomb guards — is
//! ours, because it is the part that reads untrusted bytes.
//!
//! We render array element 0, cube face +X, depth slice 0 — and the SMALLEST MIP that
//! still covers the requested thumbnail size.
//!
//! Mip selection is not a micro-optimisation: a 16384x16384 BC7 texture is 268 megapixels
//! and 256 MiB of blocks, while the 256-px mip sitting a few hundred KB further into the
//! same file is 1/4096th of the work for a tile nobody can tell apart. Textures are the one
//! format that routinely ships its own thumbnail chain, so decoding level 0 to build a
//! 96-px tile is throwing away the exact thing the format already did for us.
//!
//! Face 0's whole mip chain precedes every other face or array slice, so walking levels
//! stays inside the first surface and needs no cube/array math. A truncated chain simply
//! stops the walk and we render the last level that is fully present.

use super::*;

/// `DDS_HEADER` is a fixed 124 bytes and follows the 4-byte magic.
const HEADER_LEN: usize = 124;
/// `DDS_HEADER_DXT10` (present only when the FourCC is `DX10`) is a further 20.
const DXT10_LEN: usize = 20;
/// First byte of surface data for a classic (non-`DX10`) file.
const DATA_OFF: usize = 4 + HEADER_LEN;

// Offsets from the start of the FILE, magic included. `DDS_PIXELFORMAT` sits 72
// bytes into the 124-byte header (`dwReserved1` is 11 dwords), i.e. at file offset
// 76 — the same arithmetic `strip/ddsinfo.rs` got 4 bytes wrong until 2026-08-03.
const OFF_HEIGHT: usize = 4 + 8;
const OFF_WIDTH: usize = 4 + 12;
const OFF_PF_FLAGS: usize = 4 + 72 + 4;
const OFF_PF_FOURCC: usize = 4 + 72 + 8;
const OFF_PF_BITCOUNT: usize = 4 + 72 + 12;
const OFF_PF_MASK_R: usize = 4 + 72 + 16;
const OFF_DXGI_FORMAT: usize = DATA_OFF;
const OFF_MISC_FLAGS2: usize = DATA_OFF + 16;

// `DDS_PIXELFORMAT.dwFlags`
const DDPF_ALPHAPIXELS: u32 = 0x1;
const DDPF_ALPHA: u32 = 0x2;
const DDPF_FOURCC: u32 = 0x4;
const DDPF_RGB: u32 = 0x40;
const DDPF_LUMINANCE: u32 = 0x2_0000;
/// Signed bump/dU dV data. Rendered through the mask path as unsigned — wrong in
/// absolute terms but recognizable, which is all a thumbnail owes it.
const DDPF_BUMPDUDV: u32 = 0x8_0000;

/// `DDS_HEADER_DXT10.miscFlags2 & DDS_ALPHA_MODE_MASK`.
const ALPHA_MODE_MASK: u32 = 0x7;
const ALPHA_MODE_PREMULTIPLIED: u32 = 2;
const ALPHA_MODE_OPAQUE: u32 = 3;

/// Cheap magic test so the tier only runs on actual DDS bytes.
pub(super) fn is_dds(bytes: &[u8]) -> bool {
    bytes.len() > DATA_OFF && bytes.starts_with(b"DDS ")
}

/// One of the seven block-compressed layouts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Block {
    /// 8-byte blocks, RGB + 1-bit punch-through alpha.
    Bc1,
    /// 16-byte blocks, explicit 4-bit alpha.
    Bc2,
    /// 16-byte blocks, interpolated alpha.
    Bc3,
    /// 8-byte blocks, one interpolated channel.
    Bc4 { signed: bool },
    /// 16-byte blocks, two interpolated channels (the classic normal-map format).
    Bc5 { signed: bool },
    /// 16-byte blocks, three HDR float channels, no alpha.
    Bc6h { signed: bool },
    /// 16-byte blocks, full RGBA — what modern engines ship.
    Bc7,
}

impl Block {
    /// Compressed bytes per 4×4 block.
    fn block_bytes(self) -> usize {
        match self {
            Block::Bc1 | Block::Bc4 { .. } => 8,
            _ => 16,
        }
    }
}

/// Integer channels carved out of a ≤32-bit little-endian pixel by bit masks. This
/// one shape covers every legacy `DDPF_RGB`/`DDPF_LUMINANCE`/`DDPF_ALPHA` layout
/// AND most of the integer DXGI formats, so they share one decoder.
#[derive(Clone, Copy, Debug)]
struct Masks {
    /// 8, 16, 24 or 32.
    bpp: u32,
    r: u32,
    g: u32,
    b: u32,
    a: u32,
    /// Replicate the R channel across G and B (single-channel / luminance data,
    /// which reads as a recognizable greyscale image rather than a red one —
    /// matching what ImageMagick renders for `R8_UNORM`).
    grey: bool,
}

/// How the surface bytes are laid out. `n` is a channel count in 1..=4, expanded to
/// RGBA as grey / R,G,0 / RGB / RGBA.
#[derive(Clone, Copy, Debug)]
enum Layout {
    Block(Block),
    Masks(Masks),
    /// 8-bit signed-normalized channels.
    Snorm8(u8),
    Unorm16(u8),
    Snorm16(u8),
    /// IEEE half floats (HDR).
    Half(u8),
    /// IEEE single floats (HDR).
    Float(u8),
    /// Packed 11/11/10 unsigned floats (HDR).
    R11G11B10,
    /// 9-bit mantissas with a shared 5-bit exponent (HDR).
    Rgb9E5,
}

impl Layout {
    /// True for the HDR layouts, which decode to `Rgb32F`/`Rgba32F` and are
    /// tone-mapped by the caller exactly like EXR/Radiance.
    fn is_float(self) -> bool {
        matches!(
            self,
            Layout::Block(Block::Bc6h { .. })
                | Layout::Half(_)
                | Layout::Float(_)
                | Layout::R11G11B10
                | Layout::Rgb9E5
        )
    }
}

struct Surface {
    width: u32,
    height: u32,
    layout: Layout,
    /// First byte of mip 0.
    data: usize,
    alpha_mode: u32,
}

fn le32(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        *b.get(off..off + 4)?.first_chunk::<4>()?,
    ))
}

/// Decode a DDS to RGBA8, or to `Rgb32F`/`Rgba32F` for the HDR layouts (BC6H and
/// the float/shared-exponent uncompressed ones), which the caller tone-maps.
///
/// Every failure is an `E_FAIL` with a human-readable reason so `-Debug` logging
/// names the layout we couldn't read, and the caller falls through to the old
/// tiers — so a file that thumbnailed before still does.
/// `target` renders the smallest mip level still at least that many px on its long side,
/// when the file ships a mip chain. `None` keeps level 0, for callers that want full
/// fidelity (Convert, Image info).
pub(super) fn decode_dds(bytes: &[u8], target: Option<u32>) -> Result<DynamicImage> {
    let mut s = parse_header(bytes)?;
    if let Some(t) = target {
        select_mip(bytes, &mut s, t.max(1));
    }
    if s.layout.is_float() {
        decode_float(bytes, &s)
    } else {
        decode_rgba8(bytes, &s)
    }
}

/// Walk the mip chain, moving `s` to the smallest level whose long edge still covers
/// `target`. Best-effort by design: any overflow, a level whose bytes are not fully present
/// (truncated chain), or a header claiming no mips leaves `s` on whatever level was last
/// known good — never an error, because the mip chain is an optimisation and level 0 is
/// always the correct answer.
fn select_mip(bytes: &[u8], s: &mut Surface, target: u32) {
    // dwMipMapCount is the 7th dword of DDS_HEADER, i.e. 24 bytes in, past the 4-byte magic.
    const OFF_MIPMAPCOUNT: usize = 4 + 24;
    let count = le32(bytes, OFF_MIPMAPCOUNT).unwrap_or(0);
    if count <= 1 {
        return;
    }
    // A hostile count cannot make us walk forever: the loop also stops when a level runs
    // past the file or reaches 1x1.
    let count = count.min(32);

    let (mut w, mut h, mut off) = (s.width, s.height, s.data);
    for _ in 1..count {
        if w.max(h) <= target || (w == 1 && h == 1) {
            break;
        }
        let Some(this_level) = surface_bytes(s.layout, w, h) else {
            break;
        };
        let Some(next_off) = off.checked_add(this_level) else {
            break;
        };
        let (nw, nh) = (w.div_ceil(2).max(1), h.div_ceil(2).max(1));
        // Only step down when the NEXT level is genuinely there; a file whose chain is
        // truncated must still render the level we already have.
        match surface_bytes(s.layout, nw, nh) {
            Some(n) if next_off.checked_add(n).is_some_and(|e| e <= bytes.len()) => {}
            _ => break,
        }
        // Stepping past the target would give a tile smaller than asked for, which the
        // caller would have to upscale — worse than decoding one level too big.
        if nw.max(nh) < target {
            break;
        }
        w = nw;
        h = nh;
        off = next_off;
    }
    s.width = w;
    s.height = h;
    s.data = off;
}

fn fail(msg: impl AsRef<str>) -> Error {
    Error::new(E_FAIL, format!("dds: {}", msg.as_ref()))
}

fn parse_header(bytes: &[u8]) -> Result<Surface> {
    if !is_dds(bytes) {
        return Err(fail("not a DDS"));
    }
    // dwSize is a fixed 124; a different value means this isn't a DDS_HEADER.
    if le32(bytes, 4) != Some(HEADER_LEN as u32) {
        return Err(fail("bad header size"));
    }
    let (Some(height), Some(width)) = (le32(bytes, OFF_HEIGHT), le32(bytes, OFF_WIDTH)) else {
        return Err(fail("truncated header"));
    };
    // Bomb guard, shared with every other tier.
    if width == 0 || height == 0 || width > MAX_DIM || height > MAX_DIM {
        return Err(fail(format!("refusing {width}x{height}")));
    }
    let pf_flags = le32(bytes, OFF_PF_FLAGS).ok_or_else(|| fail("truncated pixel format"))?;
    let fourcc = bytes
        .get(OFF_PF_FOURCC..OFF_PF_FOURCC + 4)
        .ok_or_else(|| fail("truncated FourCC"))?;

    let mut data = DATA_OFF;
    let mut alpha_mode = 0;
    let layout = if pf_flags & DDPF_FOURCC != 0 {
        if fourcc == b"DX10" {
            data = DATA_OFF + DXT10_LEN;
            let dxgi = le32(bytes, OFF_DXGI_FORMAT).ok_or_else(|| fail("truncated DX10 header"))?;
            alpha_mode = le32(bytes, OFF_MISC_FLAGS2).unwrap_or(0) & ALPHA_MODE_MASK;
            dxgi_layout(dxgi).ok_or_else(|| fail(format!("unsupported DXGI format {dxgi}")))?
        } else {
            fourcc_layout(fourcc, &mut alpha_mode)
                .ok_or_else(|| fail(format!("unsupported FourCC {}", ascii(fourcc))))?
        }
    } else {
        Layout::Masks(mask_layout(bytes, pf_flags)?)
    };
    Ok(Surface {
        width,
        height,
        layout,
        data,
        alpha_mode,
    })
}

fn ascii(fourcc: &[u8]) -> String {
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
fn fourcc_layout(fourcc: &[u8], alpha_mode: &mut u32) -> Option<Layout> {
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
        _ => {
            let d3dfmt = u32::from_le_bytes([fourcc[0], fourcc[1], fourcc[2], fourcc[3]]);
            return match d3dfmt {
                36 => Some(Layout::Unorm16(4)),  // A16B16G16R16
                110 => Some(Layout::Snorm16(4)), // Q16W16V16U16
                111 => Some(Layout::Half(1)),    // R16F
                112 => Some(Layout::Half(2)),    // G16R16F
                113 => Some(Layout::Half(4)),    // A16B16G16R16F
                114 => Some(Layout::Float(1)),   // R32F
                115 => Some(Layout::Float(2)),   // G32R32F
                116 => Some(Layout::Float(4)),   // A32B32G32R32F
                _ => None,
            };
        }
    };
    Some(Layout::Block(block))
}

/// DXGI format → layout. Every TYPELESS/UNORM/SNORM/SRGB spelling of a layout maps
/// to the same decoder; the `_UINT`/`_SINT` integer views are deliberately absent
/// (they are compute data, not pictures, and guessing a scale would invent pixels).
///
/// The numbers are `DXGI_FORMAT` enum values. They are dense and easy to get off
/// by one — BC6H is 94..=96 and BC7 is 97..=99, NOT the 94..=95/96..=98 an older
/// copy of this table in `strip/ddsinfo.rs` used to claim.
fn dxgi_layout(dxgi: u32) -> Option<Layout> {
    /// `R8G8B8A8` and friends: channel N occupies byte N.
    const RGBA8: Masks = Masks {
        bpp: 32,
        r: 0x0000_00FF,
        g: 0x0000_FF00,
        b: 0x00FF_0000,
        a: 0xFF00_0000,
        grey: false,
    };
    const BGRA8: Masks = Masks {
        bpp: 32,
        r: 0x00FF_0000,
        g: 0x0000_FF00,
        b: 0x0000_00FF,
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
        33..=34 => Layout::Half(2),      // R16G16_FLOAT
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
        73..=75 => Layout::Block(Block::Bc2),
        76..=78 => Layout::Block(Block::Bc3),
        79..=80 => Layout::Block(Block::Bc4 { signed: false }),
        81 => Layout::Block(Block::Bc4 { signed: true }),
        82..=83 => Layout::Block(Block::Bc5 { signed: false }),
        84 => Layout::Block(Block::Bc5 { signed: true }),
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

/// A classic header with no FourCC describes its channels with bit masks. Trust
/// them rather than pattern-matching known layouts: that is what makes the odd
/// `X8R8G8B8`/`A8L8`/`A4R4G4B4` files from decade-old tools render.
fn mask_layout(bytes: &[u8], pf_flags: u32) -> Result<Masks> {
    let bpp = le32(bytes, OFF_PF_BITCOUNT).ok_or_else(|| fail("truncated bit count"))?;
    if !matches!(bpp, 8 | 16 | 24 | 32) {
        return Err(fail(format!("{bpp}-bit uncompressed")));
    }
    let mut m = [0u32; 4];
    for (i, slot) in m.iter_mut().enumerate() {
        *slot = le32(bytes, OFF_PF_MASK_R + i * 4).ok_or_else(|| fail("truncated masks"))?;
    }
    let [r, g, b, mut a] = m;
    // DDPF_ALPHAPIXELS is what says the alpha mask is meaningful; without it the
    // 4th channel is padding (X8R8G8B8), and honoring it would render the image
    // fully transparent.
    if pf_flags & (DDPF_ALPHAPIXELS | DDPF_ALPHA) == 0 {
        a = 0;
    }
    // Alpha-only (A8): show the alpha as luminance — see the R8/A8 note above.
    // Returns BEFORE the contiguity check below on purpose: `Channel` is total for
    // any mask (a sparse one just shifts to the wrong place), so for the one-channel
    // case rendering something beats refusing the file.
    if pf_flags & DDPF_ALPHA != 0 && r == 0 && g == 0 && b == 0 {
        return Ok(Masks {
            bpp,
            r: a,
            g: 0,
            b: 0,
            a: 0,
            grey: true,
        });
    }
    if pf_flags & (DDPF_RGB | DDPF_LUMINANCE | DDPF_BUMPDUDV) == 0 {
        return Err(fail(format!("pixel-format flags {pf_flags:#x}")));
    }
    if r == 0 && g == 0 && b == 0 {
        return Err(fail("no colour mask"));
    }
    // Every mask must be a single contiguous run of bits — the shift/scale in
    // `Channel` assumes it, and a hostile file can otherwise claim a sparse mask.
    for mask in [r, g, b, a] {
        // An absent channel is mask 0 — and `0.trailing_zeros()` is 32, which is an
        // overflowing shift (a debug panic, a wrapping no-op in release), so the
        // zero case has to be skipped BEFORE normalizing. X8R8G8B8 hits this on
        // every file.
        if mask == 0 {
            continue;
        }
        let run = mask >> mask.trailing_zeros();
        if run != u32::MAX && (run + 1) & run != 0 {
            return Err(fail(format!("non-contiguous mask {mask:#x}")));
        }
    }
    Ok(Masks {
        bpp,
        r,
        g,
        b,
        a,
        // DDPF_LUMINANCE with no green/blue mask means the single channel is
        // brightness, not red.
        grey: pf_flags & DDPF_LUMINANCE != 0 && g == 0 && b == 0,
    })
}

/// Bytes one mip-0 surface needs, or `None` on overflow / over budget.
fn surface_bytes(layout: Layout, width: u32, height: u32) -> Option<usize> {
    let n = match layout {
        Layout::Block(b) => {
            let bw = (width as u64).div_ceil(4);
            let bh = (height as u64).div_ceil(4);
            bw.checked_mul(bh)?.checked_mul(b.block_bytes() as u64)?
        }
        Layout::Masks(m) => {
            // Rows are packed at the computed pitch; the header's
            // dwPitchOrLinearSize is famously unreliable, so it is not consulted.
            let pitch = (width as u64).checked_mul(m.bpp as u64)?.div_ceil(8);
            pitch.checked_mul(height as u64)?
        }
        Layout::Snorm8(n) => (width as u64)
            .checked_mul(height as u64)?
            .checked_mul(n as u64)?,
        Layout::Unorm16(n) | Layout::Snorm16(n) | Layout::Half(n) => (width as u64)
            .checked_mul(height as u64)?
            .checked_mul(n as u64 * 2)?,
        Layout::Float(n) => (width as u64)
            .checked_mul(height as u64)?
            .checked_mul(n as u64 * 4)?,
        Layout::R11G11B10 | Layout::Rgb9E5 => {
            (width as u64).checked_mul(height as u64)?.checked_mul(4)?
        }
    };
    (n <= MAX_ALLOC).then_some(n as usize)
}

/// The mip-0 bytes, checked to be actually present.
fn surface<'a>(bytes: &'a [u8], s: &Surface) -> Result<&'a [u8]> {
    let need = surface_bytes(s.layout, s.width, s.height)
        .ok_or_else(|| fail("surface too large to decode"))?;
    bytes
        .get(s.data..)
        .filter(|rest| rest.len() >= need)
        .map(|rest| &rest[..need])
        .ok_or_else(|| fail("truncated surface data"))
}

/// Allocate the RGBA output, refusing anything over the shared bomb budget.
fn out_buffer(width: u32, height: u32, channels: u64) -> Result<usize> {
    let n = (width as u64)
        .checked_mul(height as u64)
        .and_then(|px| px.checked_mul(channels))
        .filter(|n| *n <= MAX_ALLOC)
        .ok_or_else(|| fail("output too large"))?;
    Ok(n as usize)
}

// ---------------------------------------------------------------------------
// 8-bit path
// ---------------------------------------------------------------------------

fn decode_rgba8(bytes: &[u8], s: &Surface) -> Result<DynamicImage> {
    let src = surface(bytes, s)?;
    let len = out_buffer(s.width, s.height, 4)?;
    let mut out = vec![0u8; len];
    match s.layout {
        Layout::Block(b) => blocks_rgba8(src, s.width, s.height, b, &mut out),
        Layout::Masks(m) => masks_rgba8(src, s.width, s.height, m, &mut out),
        Layout::Snorm8(n) => snorm_rgba8(src, s.width, s.height, n, 1, &mut out),
        Layout::Snorm16(n) => snorm_rgba8(src, s.width, s.height, n, 2, &mut out),
        Layout::Unorm16(n) => unorm16_rgba8(src, s.width, s.height, n, &mut out),
        // is_float() routed these to decode_float.
        Layout::Half(_) | Layout::Float(_) | Layout::R11G11B10 | Layout::Rgb9E5 => {
            return Err(fail("float layout on the 8-bit path"))
        }
    }
    apply_alpha_mode(&mut out, s.alpha_mode);
    image::RgbaImage::from_raw(s.width, s.height, out)
        .map(DynamicImage::ImageRgba8)
        .ok_or_else(|| fail("buffer size mismatch"))
}

/// Walk the 4×4 block grid, decoding each into a scratch tile and copying the
/// in-bounds part out. The tile hop is what makes a texture whose dimensions are
/// not a multiple of 4 work — the last row/column of blocks is partly padding.
fn blocks_rgba8(src: &[u8], width: u32, height: u32, block: Block, out: &mut [u8]) {
    let bw = width.div_ceil(4) as usize;
    let bh = height.div_ceil(4) as usize;
    let bytes = block.block_bytes();
    let row = width as usize * 4;
    // 4×4 RGBA scratch. Every arm below writes all 16 pixels, so it never carries
    // a previous block's contents.
    let mut tile = [0u8; 4 * 4 * 4];
    for by in 0..bh {
        for bx in 0..bw {
            let off = (by * bw + bx) * bytes;
            let Some(blk) = src.get(off..off + bytes) else {
                return;
            };
            match block {
                Block::Bc1 => bcdec_rs::bc1(blk, &mut tile, 16),
                Block::Bc2 => bcdec_rs::bc2(blk, &mut tile, 16),
                Block::Bc3 => bcdec_rs::bc3(blk, &mut tile, 16),
                // BC4/BC5 decode to 1 or 2 tightly packed channels; expand after.
                Block::Bc4 { signed } => {
                    let mut one = [0u8; 16];
                    bcdec_rs::bc4(blk, &mut one, 4, signed);
                    for (i, v) in one.iter().enumerate() {
                        tile[i * 4..i * 4 + 4].copy_from_slice(&[*v, *v, *v, 255]);
                    }
                }
                Block::Bc5 { signed } => {
                    let mut two = [0u8; 32];
                    bcdec_rs::bc5(blk, &mut two, 8, signed);
                    for i in 0..16 {
                        // R,G,0 — the third channel genuinely is not stored, and
                        // this matches what ImageMagick renders for ATI2/BC5.
                        tile[i * 4..i * 4 + 4].copy_from_slice(&[
                            two[i * 2],
                            two[i * 2 + 1],
                            0,
                            255,
                        ]);
                    }
                }
                Block::Bc7 => bcdec_rs::bc7(blk, &mut tile, 16),
                // Float-only; never reaches the 8-bit path.
                Block::Bc6h { .. } => return,
            }
            copy_tile(&tile, 4, bx, by, width, height, row, out);
        }
    }
}

/// Copy the in-bounds pixels of one decoded 4×4 tile into the output image.
#[allow(clippy::too_many_arguments)]
fn copy_tile(
    tile: &[u8],
    channels: usize,
    bx: usize,
    by: usize,
    width: u32,
    height: u32,
    row: usize,
    out: &mut [u8],
) {
    let px = bx * 4;
    let py = by * 4;
    let w = 4.min(width as usize - px);
    let h = 4.min(height as usize - py);
    for y in 0..h {
        let src = (y * 4) * channels;
        let dst = (py + y) * row + px * channels;
        let n = w * channels;
        if let (Some(s), Some(d)) = (tile.get(src..src + n), out.get_mut(dst..dst + n)) {
            d.copy_from_slice(s);
        }
    }
}

fn masks_rgba8(src: &[u8], width: u32, height: u32, m: Masks, out: &mut [u8]) {
    let pitch = (width as usize * m.bpp as usize).div_ceil(8);
    let step = (m.bpp / 8) as usize;
    let ch = [
        Channel::new(m.r),
        Channel::new(m.g),
        Channel::new(m.b),
        Channel::new(m.a),
    ];
    for y in 0..height as usize {
        let line = y * pitch;
        for x in 0..width as usize {
            let Some(px) = src.get(line + x * step..line + x * step + step) else {
                return;
            };
            let mut v = 0u32;
            for (i, b) in px.iter().enumerate() {
                v |= (*b as u32) << (8 * i);
            }
            let r = ch[0].get(v);
            let (g, b) = if m.grey {
                (r, r)
            } else {
                (ch[1].get(v), ch[2].get(v))
            };
            let a = if m.a == 0 { 255 } else { ch[3].get(v) };
            let dst = (y * width as usize + x) * 4;
            if let Some(d) = out.get_mut(dst..dst + 4) {
                d.copy_from_slice(&[r, g, b, a]);
            }
        }
    }
}

/// One bit-mask channel, pre-resolved to a shift and a scale so the per-pixel loop
/// stays cheap.
#[derive(Clone, Copy)]
struct Channel {
    shift: u32,
    mask: u32,
    /// Number of bits the channel occupies (0 = channel absent).
    bits: u32,
}

impl Channel {
    fn new(mask: u32) -> Self {
        if mask == 0 {
            return Self {
                shift: 0,
                mask: 0,
                bits: 0,
            };
        }
        let shift = mask.trailing_zeros();
        Self {
            shift,
            mask,
            bits: (mask >> shift).count_ones(),
        }
    }

    /// Extract and scale to 8-bit. Narrow channels are bit-replicated (5-bit 31 →
    /// 255, not 248) so a 565 texture reaches full white.
    fn get(self, v: u32) -> u8 {
        if self.bits == 0 {
            return 0;
        }
        let raw = (v & self.mask) >> self.shift;
        if self.bits >= 8 {
            (raw >> (self.bits - 8)) as u8
        } else {
            let max = (1u32 << self.bits) - 1;
            ((raw * 255 + max / 2) / max) as u8
        }
    }
}

/// Signed-normalized integer channels, remapped from [-1, 1] to [0, 255] so the
/// negative half is visible rather than clamped flat.
fn snorm_rgba8(src: &[u8], width: u32, height: u32, channels: u8, size: usize, out: &mut [u8]) {
    let n = channels as usize;
    let step = n * size;
    for i in 0..(width as usize * height as usize) {
        let Some(px) = src.get(i * step..i * step + step) else {
            return;
        };
        let mut c = [0u8; 4];
        for (k, slot) in c.iter_mut().enumerate().take(n) {
            let raw = if size == 1 {
                px[k] as i8 as f32 / 127.0
            } else {
                i16::from_le_bytes([px[k * 2], px[k * 2 + 1]]) as f32 / 32767.0
            };
            *slot = ((raw.clamp(-1.0, 1.0) * 0.5 + 0.5) * 255.0 + 0.5) as u8;
        }
        write_channels(out, i, n, c[0], c[1], c[2], c[3]);
    }
}

fn unorm16_rgba8(src: &[u8], width: u32, height: u32, channels: u8, out: &mut [u8]) {
    let n = channels as usize;
    let step = n * 2;
    for i in 0..(width as usize * height as usize) {
        let Some(px) = src.get(i * step..i * step + step) else {
            return;
        };
        let mut c = [0u8; 4];
        for (k, slot) in c.iter_mut().enumerate().take(n) {
            *slot = (u16::from_le_bytes([px[k * 2], px[k * 2 + 1]]) >> 8) as u8;
        }
        write_channels(out, i, n, c[0], c[1], c[2], c[3]);
    }
}

/// Expand an `n`-channel pixel to RGBA: 1 → grey, 2 → R,G,0, 3 → RGB, 4 → RGBA.
fn write_channels(out: &mut [u8], i: usize, n: usize, c0: u8, c1: u8, c2: u8, c3: u8) {
    let px = match n {
        1 => [c0, c0, c0, 255],
        2 => [c0, c1, 0, 255],
        3 => [c0, c1, c2, 255],
        _ => [c0, c1, c2, c3],
    };
    if let Some(d) = out.get_mut(i * 4..i * 4 + 4) {
        d.copy_from_slice(&px);
    }
}

/// `DDS_HEADER_DXT10.miscFlags2` can declare the stored alpha premultiplied (undo
/// it, or every semi-transparent pixel renders too dark over the checkerboard) or
/// meaningless (force opaque, or the thumbnail is invisible).
fn apply_alpha_mode(out: &mut [u8], mode: u32) {
    match mode {
        ALPHA_MODE_PREMULTIPLIED => {
            for px in out.chunks_exact_mut(4) {
                let a = px[3];
                if a > 0 && a < 255 {
                    for c in &mut px[..3] {
                        *c = ((*c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8;
                    }
                }
            }
        }
        ALPHA_MODE_OPAQUE => {
            for px in out.chunks_exact_mut(4) {
                px[3] = 255;
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// HDR path
// ---------------------------------------------------------------------------

/// The float layouts decode to linear `Rgb32F`, which the caller tone-maps through
/// the same Reinhard + sRGB transfer EXR and Radiance HDR already use — so a BC6H
/// texture thumbnails on the compact (no-ImageMagick) install too.
fn decode_float(bytes: &[u8], s: &Surface) -> Result<DynamicImage> {
    let src = surface(bytes, s)?;
    let len = out_buffer(s.width, s.height, 3 * 4)? / 4;
    let mut out = vec![0f32; len];
    match s.layout {
        Layout::Block(Block::Bc6h { signed }) => {
            blocks_bc6h(src, s.width, s.height, signed, &mut out)
        }
        Layout::Half(n) => half_rgb32f(src, s.width, s.height, n, &mut out),
        Layout::Float(n) => float_rgb32f(src, s.width, s.height, n, &mut out),
        Layout::R11G11B10 => r11g11b10_rgb32f(src, s.width, s.height, &mut out),
        Layout::Rgb9E5 => rgb9e5_rgb32f(src, s.width, s.height, &mut out),
        _ => return Err(fail("non-float layout on the HDR path")),
    }
    image::Rgb32FImage::from_raw(s.width, s.height, out)
        .map(DynamicImage::ImageRgb32F)
        .ok_or_else(|| fail("buffer size mismatch"))
}

fn blocks_bc6h(src: &[u8], width: u32, height: u32, signed: bool, out: &mut [f32]) {
    let bw = width.div_ceil(4) as usize;
    let bh = height.div_ceil(4) as usize;
    let row = width as usize * 3;
    let mut tile = [0f32; 4 * 4 * 3];
    for by in 0..bh {
        for bx in 0..bw {
            let off = (by * bw + bx) * 16;
            let Some(blk) = src.get(off..off + 16) else {
                return;
            };
            bcdec_rs::bc6h_float(blk, &mut tile, 12, signed);
            let px = bx * 4;
            let py = by * 4;
            let w = 4.min(width as usize - px);
            let h = 4.min(height as usize - py);
            for y in 0..h {
                let s = y * 12;
                let d = (py + y) * row + px * 3;
                let n = w * 3;
                if let (Some(sl), Some(dl)) = (tile.get(s..s + n), out.get_mut(d..d + n)) {
                    dl.copy_from_slice(sl);
                }
            }
        }
    }
}

/// Reconstruct an IEEE half. `f16::from_bits` is not stable on this toolchain and
/// the `half` crate is not in the tree, so this is the classic shift-and-fix-up.
fn half_to_f32(h: u16) -> f32 {
    let sign = ((h >> 15) & 1) as u32;
    let exp = ((h >> 10) & 0x1F) as u32;
    let man = (h & 0x3FF) as u32;
    let bits = match exp {
        0 if man == 0 => sign << 31,
        // Subnormal: shift the mantissa up until the implicit 1 appears, then bias
        // the exponent by however many shifts that took (113 is 127 - 24 + 10).
        0 => {
            let mut shifts = 0u32;
            let mut m = man;
            while m & 0x400 == 0 {
                m <<= 1;
                shifts += 1;
            }
            (sign << 31) | ((113 - shifts) << 23) | ((m & 0x3FF) << 13)
        }
        // Inf / NaN.
        31 => (sign << 31) | (0xFF << 23) | (man << 13),
        _ => (sign << 31) | ((exp + 127 - 15) << 23) | (man << 13),
    };
    f32::from_bits(bits)
}

fn half_rgb32f(src: &[u8], width: u32, height: u32, channels: u8, out: &mut [f32]) {
    let n = channels as usize;
    let step = n * 2;
    for i in 0..(width as usize * height as usize) {
        let Some(px) = src.get(i * step..i * step + step) else {
            return;
        };
        let mut c = [0f32; 3];
        for (k, slot) in c.iter_mut().enumerate().take(n.min(3)) {
            *slot = half_to_f32(u16::from_le_bytes([px[k * 2], px[k * 2 + 1]]));
        }
        write_rgb(out, i, n, c);
    }
}

fn float_rgb32f(src: &[u8], width: u32, height: u32, channels: u8, out: &mut [f32]) {
    let n = channels as usize;
    let step = n * 4;
    for i in 0..(width as usize * height as usize) {
        let Some(px) = src.get(i * step..i * step + step) else {
            return;
        };
        let mut c = [0f32; 3];
        for (k, slot) in c.iter_mut().enumerate().take(n.min(3)) {
            let b = &px[k * 4..k * 4 + 4];
            *slot = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        }
        write_rgb(out, i, n, c);
    }
}

/// 11/11/10 unsigned floats (5-bit exponents, no sign) packed into 32 bits.
fn r11g11b10_rgb32f(src: &[u8], width: u32, height: u32, out: &mut [f32]) {
    let unpack = |bits: u32, mantissa_bits: u32| -> f32 {
        let exp = bits >> mantissa_bits;
        let man = bits & ((1 << mantissa_bits) - 1);
        let scale = 1.0 / (1u32 << mantissa_bits) as f32;
        match exp {
            0 => man as f32 * scale * (2f32).powi(-14),
            31 => f32::INFINITY,
            _ => (1.0 + man as f32 * scale) * (2f32).powi(exp as i32 - 15),
        }
    };
    for i in 0..(width as usize * height as usize) {
        let Some(px) = src.get(i * 4..i * 4 + 4) else {
            return;
        };
        let v = u32::from_le_bytes([px[0], px[1], px[2], px[3]]);
        write_rgb(
            out,
            i,
            3,
            [
                unpack(v & 0x7FF, 6),
                unpack((v >> 11) & 0x7FF, 6),
                unpack((v >> 22) & 0x3FF, 5),
            ],
        );
    }
}

/// Three 9-bit mantissas sharing one 5-bit exponent.
fn rgb9e5_rgb32f(src: &[u8], width: u32, height: u32, out: &mut [f32]) {
    for i in 0..(width as usize * height as usize) {
        let Some(px) = src.get(i * 4..i * 4 + 4) else {
            return;
        };
        let v = u32::from_le_bytes([px[0], px[1], px[2], px[3]]);
        // exponent bias 15, mantissa denominator 2^9
        let scale = (2f32).powi(((v >> 27) & 0x1F) as i32 - 15 - 9);
        write_rgb(
            out,
            i,
            3,
            [
                (v & 0x1FF) as f32 * scale,
                ((v >> 9) & 0x1FF) as f32 * scale,
                ((v >> 18) & 0x1FF) as f32 * scale,
            ],
        );
    }
}

/// Expand an `n`-channel float pixel to RGB: 1 → grey, 2 → R,G,0, else RGB.
fn write_rgb(out: &mut [f32], i: usize, n: usize, c: [f32; 3]) {
    let px = match n {
        1 => [c[0], c[0], c[0]],
        2 => [c[0], c[1], 0.0],
        _ => c,
    };
    if let Some(d) = out.get_mut(i * 3..i * 3 + 3) {
        d.copy_from_slice(&px);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::GenericImageView;

    /// A REAL 4×4 `BC7_UNORM` DDS written by Microsoft's `texconv` (DirectXTex,
    /// may2026) from red/green/blue/white quadrants, with the expected pixels
    /// taken from `texconv -ft png` on the same file — so this asserts our output
    /// against Microsoft's own reference decoder, not against ourselves.
    ///
    /// The colours look wrong for the source art on purpose: four saturated
    /// quadrants inside ONE block is a worst case for any block compressor, and
    /// the loss is the ENCODER's. The reference decode is byte-identical to ours.
    const BC7_4X4: &str = "\
        444453207c00000007100a000400000004000000100000000100000001000000\
        0000000000000000000000000000000000000000000000000000000000000000\
        0000000000000000000000002000000004000000445831300000000000000000\
        0000000000000000000000000010000000000000000000000000000000000000\
        6200000003000000000000000100000000000000023ff00300f0ff3ff003be7f\
        fb376003";

    fn unhex(s: &str) -> Vec<u8> {
        let h: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        (0..h.len() / 2)
            .map(|i| u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).unwrap())
            .collect()
    }

    /// Build a classic (non-`DX10`) DDS from LITERAL field offsets — never from the
    /// `OFF_*` constants, so a skew in those can't hide inside a self-consistent
    /// fixture the way it did in `strip/ddsinfo.rs` until 2026-08-03.
    fn classic(
        w: u32,
        h: u32,
        pf_flags: u32,
        fourcc: &[u8; 4],
        bits: u32,
        masks: [u32; 4],
    ) -> Vec<u8> {
        let mut v = b"DDS ".to_vec();
        v.resize(4 + 124, 0);
        let put =
            |v: &mut Vec<u8>, at: usize, n: u32| v[at..at + 4].copy_from_slice(&n.to_le_bytes());
        put(&mut v, 4, 124);
        put(&mut v, 4 + 4, 0x1 | 0x2 | 0x4 | 0x1000);
        put(&mut v, 4 + 8, h);
        put(&mut v, 4 + 12, w);
        put(&mut v, 4 + 24, 1);
        // A writer signature in dwReserved1: the bytes a 4-byte-skewed offset
        // table would read as the mip count / pixel format.
        v[4 + 28..4 + 28 + 11].copy_from_slice(b"IMAGEMAGICK");
        put(&mut v, 4 + 72, 32);
        put(&mut v, 4 + 72 + 4, pf_flags);
        v[4 + 72 + 8..4 + 72 + 12].copy_from_slice(fourcc);
        put(&mut v, 4 + 72 + 12, bits);
        for (i, m) in masks.iter().enumerate() {
            put(&mut v, 4 + 72 + 16 + i * 4, *m);
        }
        put(&mut v, 4 + 104, 0x1000);
        v
    }

    fn rgba(img: &DynamicImage, x: u32, y: u32) -> [u8; 4] {
        img.get_pixel(x, y).0
    }

    #[test]
    fn bc7_matches_the_directxtex_reference_decode() {
        let img = decode_dds(&unhex(BC7_4X4), None).unwrap();
        assert_eq!(img.dimensions(), (4, 4));
        assert_eq!(rgba(&img, 0, 0), [146, 0, 146, 255]);
        assert_eq!(rgba(&img, 2, 0), [2, 255, 2, 255]);
        assert_eq!(rgba(&img, 2, 2), [255, 255, 255, 255]);
    }

    /// The header skew that made `strip/ddsinfo.rs` report ImageMagick's
    /// `dwReserved1` signature as a mip count: a real DXT5 header must be read as
    /// BC3, not as whatever sits four bytes later.
    #[test]
    fn classic_pixel_format_is_read_at_the_right_offset() {
        let mut f = classic(4, 4, DDPF_FOURCC, b"DXT5", 0, [0; 4]);
        f.extend_from_slice(&[0u8; 16]);
        let s = parse_header(&f).unwrap();
        assert!(matches!(s.layout, Layout::Block(Block::Bc3)));
        assert_eq!((s.width, s.height), (4, 4));
        assert_eq!(s.data, DATA_OFF);
    }

    /// BC1's rare "punch-through" mode (`c0 <= c1`) makes index 3 transparent
    /// black — the 1-bit alpha the `image` crate's DXT1 path drops entirely.
    #[test]
    fn bc1_punchthrough_index_is_transparent() {
        let mut f = classic(4, 4, DDPF_FOURCC, b"DXT1", 0, [0; 4]);
        // c0 = 0x0000 (black) <= c1 = 0xF800 (red) selects the alpha mode; every
        // index is 3 (0xFF bytes) => the whole block is transparent.
        f.extend_from_slice(&[0x00, 0x00, 0x00, 0xF8, 0xFF, 0xFF, 0xFF, 0xFF]);
        let img = decode_dds(&f, None).unwrap();
        assert_eq!(rgba(&img, 0, 0), [0, 0, 0, 0]);
        assert_eq!(rgba(&img, 3, 3), [0, 0, 0, 0]);
    }

    /// The block-compressed DXGI runs are three values wide each. These bounds are
    /// exactly what an off-by-one table gets wrong.
    #[test]
    fn dxgi_block_runs_are_three_wide() {
        let block = |id| match dxgi_layout(id) {
            Some(Layout::Block(b)) => b,
            other => panic!("dxgi {id} => {other:?}"),
        };
        for id in 79..=80 {
            assert_eq!(block(id), Block::Bc4 { signed: false });
        }
        assert_eq!(block(81), Block::Bc4 { signed: true });
        for id in 82..=83 {
            assert_eq!(block(id), Block::Bc5 { signed: false });
        }
        assert_eq!(block(84), Block::Bc5 { signed: true });
        for id in 94..=95 {
            assert_eq!(block(id), Block::Bc6h { signed: false });
        }
        assert_eq!(block(96), Block::Bc6h { signed: true });
        for id in 97..=99 {
            assert_eq!(block(id), Block::Bc7);
        }
    }

    #[test]
    fn mask_layouts_extract_exact_channels() {
        // A8R8G8B8, one pixel: 0xAARRGGBB little-endian.
        let mut f = classic(
            1,
            1,
            DDPF_RGB | DDPF_ALPHAPIXELS,
            &[0; 4],
            32,
            [0x00FF_0000, 0x0000_FF00, 0x0000_00FF, 0xFF00_0000],
        );
        f.extend_from_slice(&0x8012_3456u32.to_le_bytes());
        assert_eq!(
            rgba(&decode_dds(&f, None).unwrap(), 0, 0),
            [0x12, 0x34, 0x56, 0x80]
        );

        // R5G6B5: a narrow channel is bit-REPLICATED, so all-ones reaches 255
        // rather than 248 — otherwise a 565 texture never renders true white.
        let mut f = classic(1, 1, DDPF_RGB, &[0; 4], 16, [0xF800, 0x07E0, 0x001F, 0]);
        f.extend_from_slice(&0xFFFFu16.to_le_bytes());
        assert_eq!(
            rgba(&decode_dds(&f, None).unwrap(), 0, 0),
            [255, 255, 255, 255]
        );
    }

    /// `X8R8G8B8` carries a padding byte, not alpha. Honouring it (it is usually
    /// zero) would render the whole texture invisible.
    #[test]
    fn padding_byte_is_not_alpha() {
        let mut f = classic(
            1,
            1,
            DDPF_RGB,
            &[0; 4],
            32,
            [0x00FF_0000, 0x0000_FF00, 0x0000_00FF, 0],
        );
        f.extend_from_slice(&0x0011_2233u32.to_le_bytes());
        assert_eq!(
            rgba(&decode_dds(&f, None).unwrap(), 0, 0),
            [0x11, 0x22, 0x33, 255]
        );

        // Even a DECLARED alpha mask is ignored without DDPF_ALPHAPIXELS.
        let mut f = classic(
            1,
            1,
            DDPF_RGB,
            &[0; 4],
            32,
            [0x00FF_0000, 0x0000_FF00, 0x0000_00FF, 0xFF00_0000],
        );
        f.extend_from_slice(&0x0011_2233u32.to_le_bytes());
        assert_eq!(rgba(&decode_dds(&f, None).unwrap(), 0, 0)[3], 255);
    }

    /// `Channel` assumes one contiguous run of bits; a sparse mask must be refused
    /// rather than silently mis-shifted.
    #[test]
    fn non_contiguous_mask_is_refused() {
        let mut f = classic(
            1,
            1,
            DDPF_RGB,
            &[0; 4],
            32,
            [0x00FF_00FF, 0x0000_FF00, 0, 0],
        );
        f.extend_from_slice(&[0u8; 4]);
        assert!(decode_dds(&f, None).is_err());
    }

    /// A texture whose edges are not a multiple of 4 still fills exactly its own
    /// pixels — the trailing block is partly padding.
    #[test]
    fn dimensions_not_a_multiple_of_four() {
        let mut f = classic(5, 3, DDPF_FOURCC, b"DXT1", 0, [0; 4]);
        // 2×1 blocks of solid white (c0 = c1 = 0xFFFF, all indices 0).
        for _ in 0..2 {
            f.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0x00]);
        }
        let img = decode_dds(&f, None).unwrap();
        assert_eq!(img.dimensions(), (5, 3));
        for (x, y) in [(0, 0), (4, 0), (4, 2), (0, 2)] {
            assert_eq!(rgba(&img, x, y), [255, 255, 255, 255], "at {x},{y}");
        }
    }

    /// `DDS_ALPHA_MODE_PREMULTIPLIED` (and `DXT2`/`DXT4`, its classic spelling)
    /// must be undone, or every semi-transparent pixel renders too dark.
    #[test]
    fn premultiplied_alpha_is_undone() {
        let mut px = vec![64u8, 64, 64, 128];
        apply_alpha_mode(&mut px, ALPHA_MODE_PREMULTIPLIED);
        assert_eq!(px, vec![128, 128, 128, 128]);
        // DXT2/DXT4 are DXT3/DXT5 with premultiplied alpha, so they set the mode
        // rather than getting their own block decoders.
        let mut mode = 0;
        assert!(matches!(
            fourcc_layout(b"DXT2", &mut mode),
            Some(Layout::Block(Block::Bc2))
        ));
        assert_eq!(mode, ALPHA_MODE_PREMULTIPLIED);
        let mut mode = 0;
        assert!(matches!(
            fourcc_layout(b"DXT4", &mut mode),
            Some(Layout::Block(Block::Bc3))
        ));
        assert_eq!(mode, ALPHA_MODE_PREMULTIPLIED);
    }

    #[test]
    fn half_floats_round_trip_including_subnormals() {
        // The subnormal expectations are written as their exact definition
        // (mantissa × 2⁻²⁴) rather than as decimal literals, so they document the
        // rule the shift-and-fix-up path has to reproduce.
        let sub = |mantissa: u32| mantissa as f32 * (2f32).powi(-24);
        for (bits, want) in [
            (0x0000u16, 0.0f32),
            (0x3C00, 1.0),
            (0xBC00, -1.0),
            (0x3800, 0.5),
            (0x3E00, 1.5),       // exercises the mantissa bits, not just the exponent
            (0x0001, sub(1)),    // smallest subnormal
            (0x0200, sub(512)),  // mid subnormal
            (0x03FF, sub(1023)), // largest subnormal
            (0x7BFF, 65504.0),   // largest normal
        ] {
            let got = half_to_f32(bits);
            assert!(
                (got - want).abs() <= want.abs() * 1e-6 + 1e-12,
                "half {bits:#06x} => {got} want {want}"
            );
        }
        assert!(half_to_f32(0x7C00).is_infinite());
        assert!(half_to_f32(0xFE00).is_nan());
    }

    /// The shared decompression-bomb budget applies here too: a header may DECLARE
    /// any size, and the surface/output maths must refuse the absurd ones before
    /// allocating rather than after.
    #[test]
    fn refuses_declared_bombs() {
        let mut f = classic(100_000, 100_000, DDPF_FOURCC, b"DXT1", 0, [0; 4]);
        f.extend_from_slice(&[0u8; 64]);
        assert!(decode_dds(&f, None).is_err());

        // In-bounds dimensions whose RGBA output still exceeds MAX_ALLOC.
        let mut f = classic(MAX_DIM, MAX_DIM, DDPF_FOURCC, b"DXT1", 0, [0; 4]);
        f.extend_from_slice(&[0u8; 64]);
        assert!(decode_dds(&f, None).is_err());

        // A truthful header whose surface data simply is not there.
        let f = classic(1024, 1024, DDPF_FOURCC, b"DXT5", 0, [0; 4]);
        assert!(decode_dds(&f, None).is_err());
    }

    /// These bytes arrive from the shell, unvalidated, and the classic context-menu
    /// tile decodes them INSIDE explorer.exe under `panic = "abort"` — so a panic
    /// here takes down the user's desktop. Every prefix of a valid file, and a
    /// deterministic sweep of single-field corruptions, must fail cleanly instead.
    #[test]
    fn hostile_input_never_panics() {
        let valid = unhex(BC7_4X4);
        for n in 0..valid.len() {
            let _ = decode_dds(&valid[..n], None);
        }
        // Walk every 4-byte-aligned header field through a set of nasty values.
        for field in (0..valid.len().min(148)).step_by(4) {
            for probe in [
                0u32,
                1,
                u32::MAX,
                u32::MAX - 3,
                0x8000_0000,
                124,
                0xFFFF,
                0x7FFF_FFFF,
            ] {
                let mut f = valid.clone();
                f[field..field + 4].copy_from_slice(&probe.to_le_bytes());
                let _ = decode_dds(&f, None);
            }
        }
        // And a cheap deterministic byte-flip fuzz over the same region.
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        for _ in 0..4000 {
            let mut f = valid.clone();
            for _ in 0..3 {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let at = (seed >> 33) as usize % f.len();
                f[at] = (seed >> 11) as u8;
            }
            let _ = decode_dds(&f, None);
        }
    }

    /// Not-a-DDS must be declined so the other tiers still get their shot.
    #[test]
    fn declines_other_formats() {
        assert!(!is_dds(b"\x89PNG\r\n\x1a\n"));
        assert!(!is_dds(b"DDS "));
        assert!(decode_dds(b"\x89PNG\r\n\x1a\n", None).is_err());
    }
}

#[cfg(test)]
mod mip_tests {
    use super::*;

    /// Build a BC1 DDS whose mip chain is deliberately DIFFERENT per level: level 0 is red,
    /// level 1 green, level 2 blue. A decoder that ignores mips returns red; one that picks
    /// the right level returns the colour that belongs to it. Colour, not size, is the
    /// assertion — sizes alone would pass even if we read the wrong offset.
    fn bc1_mip_chain(w: u32, h: u32, colours: &[[u8; 3]]) -> Vec<u8> {
        fn bc1_block(c: [u8; 3]) -> [u8; 8] {
            let c565 =
                (((c[0] as u16 >> 3) << 11) | ((c[1] as u16 >> 2) << 5) | (c[2] as u16 >> 3))
                    .to_le_bytes();
            // Both endpoints the same colour, all indices 0 -> a flat block.
            [c565[0], c565[1], c565[0], c565[1], 0, 0, 0, 0]
        }
        let mut v = Vec::new();
        v.extend_from_slice(b"DDS ");
        let mut hdr = [0u8; HEADER_LEN];
        hdr[0..4].copy_from_slice(&(HEADER_LEN as u32).to_le_bytes()); // dwSize
        hdr[4..8].copy_from_slice(&0x0002_1007u32.to_le_bytes()); // flags incl. MIPMAPCOUNT
        hdr[8..12].copy_from_slice(&h.to_le_bytes());
        hdr[12..16].copy_from_slice(&w.to_le_bytes());
        hdr[24..28].copy_from_slice(&(colours.len() as u32).to_le_bytes()); // dwMipMapCount
        hdr[72..76].copy_from_slice(&32u32.to_le_bytes()); // pixel format dwSize
        hdr[76..80].copy_from_slice(&0x4u32.to_le_bytes()); // DDPF_FOURCC
        hdr[80..84].copy_from_slice(b"DXT1");
        v.extend_from_slice(&hdr);
        let (mut lw, mut lh) = (w, h);
        for c in colours {
            let blocks = (lw.div_ceil(4) as usize) * (lh.div_ceil(4) as usize);
            for _ in 0..blocks {
                v.extend_from_slice(&bc1_block(*c));
            }
            lw = lw.div_ceil(2).max(1);
            lh = lh.div_ceil(2).max(1);
        }
        v
    }

    fn centre(img: &DynamicImage) -> [u8; 3] {
        let rgba = img.to_rgba8();
        let p = rgba.get_pixel(rgba.width() / 2, rgba.height() / 2).0;
        [p[0], p[1], p[2]]
    }

    fn near(a: [u8; 3], b: [u8; 3]) -> bool {
        // BC1 endpoints are 5/6/5, so an exact match is not available.
        a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= 10)
    }

    #[test]
    fn picks_the_mip_that_covers_the_target() {
        let red = [255, 0, 0];
        let green = [0, 255, 0];
        let blue = [0, 0, 255];
        let dds = bc1_mip_chain(64, 64, &[red, green, blue]);

        // No target: level 0, full size, red.
        let full = decode_dds(&dds, None).expect("level 0");
        assert_eq!((full.width(), full.height()), (64, 64));
        assert!(
            near(centre(&full), red),
            "untargeted decode must stay on mip 0"
        );

        // 64 is exactly level 0, so it must NOT step down.
        let l0 = decode_dds(&dds, Some(64)).expect("target 64");
        assert_eq!((l0.width(), l0.height()), (64, 64));
        assert!(near(centre(&l0), red));

        // 32 is level 1 exactly.
        let l1 = decode_dds(&dds, Some(32)).expect("target 32");
        assert_eq!((l1.width(), l1.height()), (32, 32));
        assert!(
            near(centre(&l1), green),
            "target 32 must read mip 1, not mip 0"
        );

        // 16 is level 2, the last one present.
        let l2 = decode_dds(&dds, Some(16)).expect("target 16");
        assert_eq!((l2.width(), l2.height()), (16, 16));
        assert!(near(centre(&l2), blue), "target 16 must read mip 2");

        // Below the chain: stop at the smallest level present rather than overshooting into
        // data that is not there.
        let small = decode_dds(&dds, Some(4)).expect("target 4");
        assert_eq!((small.width(), small.height()), (16, 16));
        assert!(near(centre(&small), blue));
    }

    #[test]
    fn a_truncated_mip_chain_still_renders() {
        let dds = bc1_mip_chain(64, 64, &[[255, 0, 0], [0, 255, 0], [0, 0, 255]]);
        // Chop the tail so levels 1 and 2 are no longer fully present.
        let cut = dds.len() - 8;
        let truncated = &dds[..cut];
        let img = decode_dds(truncated, Some(16)).expect("must still decode something");
        assert!(
            img.width() >= 16,
            "a truncated chain must fall back to a level that IS present, got {}x{}",
            img.width(),
            img.height()
        );
    }

    #[test]
    fn a_lying_mipmap_count_cannot_walk_off_the_end() {
        let mut dds = bc1_mip_chain(64, 64, &[[255, 0, 0]]);
        // Claim 20 mip levels while shipping one.
        dds[4 + 24..4 + 28].copy_from_slice(&20u32.to_le_bytes());
        let img = decode_dds(&dds, Some(1)).expect("must not fail on a lying header");
        assert_eq!((img.width(), img.height()), (64, 64));
    }
}
