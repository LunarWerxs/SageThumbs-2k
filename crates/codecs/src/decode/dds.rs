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
mod layout;
use layout::*;
mod header;
use header::*;
mod blocks;
use blocks::*;
mod masks;
use masks::*;
mod float;
use float::*;

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
const OFF_DEPTH: usize = 4 + 20;
const OFF_CAPS2: usize = 4 + 108;

/// `DDS_HEADER.dwCaps2` bit: the surface is a 3D volume texture, so `dwDepth` names a
/// real slice count rather than being an unused field a 2D exporter left as garbage.
const DDSCAPS2_VOLUME: u32 = 0x0020_0000;

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

/// The block-compressed family a DX10 `DXGI_FORMAT` decodes as, named for display (the
/// `strip` DDS summary), or `None` for an uncompressed or unknown format. Read off
/// [`dxgi_layout`], so the summary can never disagree with the decoder about which number is
/// which block (it once reported a `BC6H_SF16` texture as "BC7").
pub(crate) fn dxgi_block_name(dxgi: u32) -> Option<&'static str> {
    let Layout::Block(block) = dxgi_layout(dxgi)? else {
        return None;
    };
    Some(match block {
        Block::Bc1 => "BC1",
        Block::Bc2 => "BC2",
        Block::Bc3 => "BC3",
        Block::Bc4 { signed: false } => "BC4",
        Block::Bc4 { signed: true } => "BC4 (signed)",
        Block::Bc5 { signed: false } => "BC5",
        Block::Bc5 { signed: true } => "BC5 (signed)",
        Block::Bc6h { signed: false } => "BC6H",
        Block::Bc6h { signed: true } => "BC6H (signed)",
        Block::Bc7 => "BC7",
    })
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
    /// Depth-slice count of the CURRENT mip level: 1 for every ordinary 2D texture, and
    /// `dwDepth` (halving each mip step, floor 1) for a `DDSCAPS2_VOLUME` texture. Only
    /// [`select_mip`] uses this — depth slices beyond slice 0 are never decoded (see the
    /// module doc), but they still occupy space in each mip level that must be skipped to
    /// reach the next one.
    depth: u32,
}

fn le32(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        *b.get(off..off + 4)?.first_chunk::<4>()?,
    ))
}

/// Direct entry points for the mutation fuzzer, plus the structurally valid seeds it needs.
///
/// **The DDS decoder had NO fuzz target at all before 2026-08-19**, which is the same gap the
/// APK parsers had and for a related reason: the only DDS the harness carried was an 8-byte
/// magic stub (a bare eight-byte magic stub), and a mutation of eight bytes cannot get past
/// `parse_header` to the part that reads attacker-controlled block data. Every mutation died at
/// the door and the suite stayed green testing nothing.
///
/// This matters more here than the byte count suggests. `blocks_rgba8` and `block_mean_fast`
/// index a compressed payload using a width, a height and a mip offset that all come OUT OF THE
/// FILE, and they run IN-PROCESS inside `explorer.exe` under `panic = "abort"` (the classic
/// right-click preview tile reaches DDS through the cheap tiers). A slice panic here is the
/// user's shell dying on a downloaded texture.
///
/// Both targets exist because they are different code: `Some(target)` selects a mip level and
/// takes the block-average path, `None` decodes the full surface. Only the first reaches
/// [`block_mean_fast`].
#[cfg(test)]
pub(crate) mod fuzzapi {
    use super::*;

    /// The targeted decode: mip selection plus the block-average fast path.
    pub(crate) fn decode_targeted(b: &[u8]) {
        let _ = decode_dds(b, Some(256));
    }

    /// The untargeted decode: level 0, full expansion, and the float (BC6H) arm.
    pub(crate) fn decode_untargeted(b: &[u8]) {
        let _ = decode_dds(b, None);
    }

    /// A structurally valid DDS the mutator can meaningfully damage. `fourcc` picks the block
    /// format, so BC1's punch-through alpha, BC3's interpolated alpha and BC7's mode parsing
    /// each get a seed that actually reaches them. `mips` writes a real chain, so the mip walk
    /// (offsets accumulated from file-supplied sizes) is reachable too. `dxgi` is read only
    /// for the `DX10` FourCC, which is the only route to BC6H (the float arm) and BC7.
    pub(crate) fn seed(fourcc: &[u8; 4], dxgi: u32, w: u32, h: u32, mips: u32) -> Vec<u8> {
        let block_bytes = if fourcc == b"DXT1" || dxgi == 71 {
            8
        } else {
            16
        };
        let mut v = Vec::from(*b"DDS ");
        let mut hdr = [0u8; HEADER_LEN];
        hdr[0..4].copy_from_slice(&(HEADER_LEN as u32).to_le_bytes());
        hdr[4..8].copy_from_slice(&0x0002_1007u32.to_le_bytes());
        hdr[8..12].copy_from_slice(&h.to_le_bytes());
        hdr[12..16].copy_from_slice(&w.to_le_bytes());
        hdr[24..28].copy_from_slice(&mips.max(1).to_le_bytes());
        hdr[72..76].copy_from_slice(&32u32.to_le_bytes());
        hdr[76..80].copy_from_slice(&0x4u32.to_le_bytes()); // DDPF_FOURCC
        hdr[80..84].copy_from_slice(fourcc);
        v.extend_from_slice(&hdr);
        if fourcc == b"DX10" {
            // DDS_HEADER_DXT10: the 20 bytes `parse_header` requires before the payload.
            // Without them a DX10 seed dies at "truncated DX10 header" and BC7/BC6H, the two
            // block families with the most parsing to get wrong, would never be reached.
            let mut ext = [0u8; DXT10_LEN];
            ext[0..4].copy_from_slice(&dxgi.to_le_bytes()); // dxgiFormat
            ext[4..8].copy_from_slice(&3u32.to_le_bytes()); // TEXTURE2D
            ext[12..16].copy_from_slice(&1u32.to_le_bytes()); // arraySize
            v.extend_from_slice(&ext);
        }

        // Varied payload, not a flat colour: a mutation of a flat block is far likelier to
        // land somewhere that changes nothing, and the index histogram in `block_mean_fast`
        // only has more than one bin to weight when the indices differ.
        let (mut lw, mut lh) = (w, h);
        let mut n = 0u32;
        for _ in 0..mips.max(1) {
            let blocks = (lw.div_ceil(4) as usize) * (lh.div_ceil(4) as usize);
            for _ in 0..blocks {
                for _ in 0..block_bytes {
                    n = n.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    v.push((n >> 24) as u8);
                }
            }
            lw = lw.div_ceil(2).max(1);
            lh = lh.div_ceil(2).max(1);
        }
        v
    }

    /// The seed self-check's half: a seed that its own parser rejects is worse than no seed,
    /// because the fuzzer mutates it happily while every iteration dies at the header.
    pub(crate) fn seed_decodes(b: &[u8]) -> bool {
        decode_dds(b, Some(256)).is_ok()
    }
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
        decode_rgba8(bytes, &s, target)
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

    let (mut w, mut h, mut off, mut depth) = (s.width, s.height, s.data, s.depth);
    for _ in 1..count {
        match next_mip_level(bytes, s.layout, w, h, off, depth, target) {
            Some((nw, nh, next_off, nd)) => {
                w = nw;
                h = nh;
                off = next_off;
                depth = nd;
            }
            None => break,
        }
    }
    s.width = w;
    s.height = h;
    s.data = off;
    s.depth = depth;
}

/// The next mip level's dimensions and data offset, or `None` when the walk should stop
/// (target reached, 1x1, an overflow, or a truncated chain). Best-effort by design: the
/// chain is an optimisation and level 0 is always the correct answer.
fn next_mip_level(
    bytes: &[u8],
    layout: Layout,
    w: u32,
    h: u32,
    off: usize,
    depth: u32,
    target: u32,
) -> Option<(u32, u32, usize, u32)> {
    if w.max(h) <= target {
        return None;
    }
    // A volume texture's mip level is `depth` full slices, not one — the file lays
    // them out contiguously, and `depth` itself halves (floor 1) with every mip step.
    let this_level =
        surface_bytes(layout, w, h).and_then(|slice| slice.checked_mul(depth as usize))?;
    let next_off = off.checked_add(this_level)?;
    let (nw, nh) = ((w >> 1).max(1), (h >> 1).max(1));
    let nd = (depth >> 1).max(1);
    // Only step down when the NEXT level is genuinely there; a file whose chain is
    // truncated must still render the level we already have.
    let next_level =
        surface_bytes(layout, nw, nh).and_then(|slice| slice.checked_mul(nd as usize))?;
    if next_off.checked_add(next_level)? > bytes.len() {
        return None;
    }
    // Stepping past the target would give a tile smaller than asked for, which the
    // caller would have to upscale — worse than decoding one level too big.
    if nw.max(nh) < target {
        return None;
    }
    Some((nw, nh, next_off, nd))
}

fn fail(msg: impl AsRef<str>) -> Error {
    Error::new(E_FAIL, format!("dds: {}", msg.as_ref()))
}

#[cfg(test)]
mod dds_cost_tests;
#[cfg(test)]
mod dds_fuzzseed_tests;
#[cfg(test)]
mod dds_mean_tests;
#[cfg(test)]
mod mip_tests;
#[cfg(test)]
mod tests;
