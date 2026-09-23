//! Valve Texture Format `.vtf` (the Source engine's textures): the full-resolution image,
//! re-wrapped as a DDS for the decoder in `decode/dds.rs`.
//!
//! Layout, per the Valve Developer Wiki's "VTF (Valve Texture Format)" page and VTFLib, and
//! checked against real textures (srctools' `tests/test_vtf`, one per pixel format, and a
//! game's `hotspot.vtf`), 2026-09-22: a header of `header_size` bytes, a small low-resolution
//! copy (usually 16x16 DXT1), then the image proper as a mip chain stored SMALLEST level
//! first, each level holding every frame, cube face and depth slice in that order. From 7.3
//! the header ends in a resource directory and the image's offset is the `\x30\0\0` entry;
//! before 7.3 it simply follows the low-resolution copy.
//!
//! So the one surface a thumbnail wants - level 0, frame 0, face 0, slice 0 - sits after
//! every smaller level. Its offset is the sum of their sizes, which is why every format this
//! reads must have a known size; P8 (palettised, never shipped by Valve's tools) is refused.
//! When level 0 is larger than a cover may be, the next level down is used instead.

use super::ddswrap::{
    self, masks, Pixels, DDPF_ALPHA, DDPF_ALPHAPIXELS, DDPF_LUMINANCE, DDPF_RGB, DDPF_RGBA as RGBA,
};
use super::util::le32;
use crate::decode::limits::MAX_DIM;

/// The `\x30\0\0` resource: where the full-resolution image data starts (7.3+).
const RES_HIGH_RES: [u8; 3] = [0x30, 0, 0];
const TEXTUREFLAGS_ENVMAP: u32 = 0x4000;
/// More resources than any Source tool writes (the known tags are fewer than ten).
const MAX_RESOURCES: usize = 32;
/// The bytes every version's header reaches: 7.2's ends at 80 and 7.3's resource count sits
/// at 68. Older headers are shorter, but the low-resolution copy always follows them.
const HEAD: usize = 80;

pub fn looks_like_vtf(b: &[u8]) -> bool {
    b.starts_with(b"VTF\0") && le32(b, 4) == Some(7) && le32(b, 8).is_some_and(|m| m <= 5)
}

// Field readers over the fixed head. Every offset is a constant below `HEAD`, which is why
// these index rather than check.
fn u16_at(h: &[u8; HEAD], o: usize) -> u16 {
    u16::from_le_bytes([h[o], h[o + 1]])
}

fn u32_at(h: &[u8; HEAD], o: usize) -> u32 {
    u32::from_le_bytes([h[o], h[o + 1], h[o + 2], h[o + 3]])
}

struct Header {
    width: u32,
    height: u32,
    frames: u64,
    faces: u64,
    depth: u32,
    mips: u32,
    pixels: Pixels,
    /// Where the level chain starts.
    data: usize,
}

fn header(b: &[u8]) -> Option<Header> {
    let head: &[u8; HEAD] = b.get(..HEAD)?.try_into().ok()?;
    if !looks_like_vtf(head) {
        return None;
    }
    let minor = u32_at(head, 8);
    let (width, height) = (u32::from(u16_at(head, 16)), u32::from(u16_at(head, 18)));
    let mips = u32::from(head[56]).max(1);
    let sane = (1..=MAX_DIM).contains(&width) && (1..=MAX_DIM).contains(&height) && mips <= 16;
    if !sane {
        return None;
    }
    Some(Header {
        width,
        height,
        frames: u64::from(u16_at(head, 24).max(1)),
        faces: face_count(head, minor),
        depth: depth(head, minor),
        mips,
        pixels: pixels(u32_at(head, 52))?,
        data: data_offset(b, head, minor)?,
    })
}

/// Depth slices: a 7.2 field; earlier textures are flat.
fn depth(head: &[u8; HEAD], minor: u32) -> u32 {
    if minor >= 2 {
        u32::from(u16_at(head, 63).max(1))
    } else {
        1
    }
}

/// Where the level chain starts: named by the resource directory from 7.3, after the
/// low-resolution copy before it.
fn data_offset(b: &[u8], head: &[u8; HEAD], minor: u32) -> Option<usize> {
    if minor >= 3 {
        high_res_offset(b)
    } else {
        after_low_res(head)
    }
}

/// VTFLib's rule: an environment map has six faces, plus a seventh sphere map below 7.5
/// unless the start frame says there is none.
fn face_count(head: &[u8; HEAD], minor: u32) -> u64 {
    if u32_at(head, 20) & TEXTUREFLAGS_ENVMAP == 0 {
        1
    } else if minor < 5 && u16_at(head, 26) != 0xFFFF {
        7
    } else {
        6
    }
}

/// Before 7.3 the image follows the header and the low-resolution copy (format `-1`: none).
fn after_low_res(head: &[u8; HEAD]) -> Option<usize> {
    let low = match u32_at(head, 57) {
        u32::MAX => 0,
        f => pixels(f)?.surface_len(u32::from(head[61]), u32::from(head[62]))?,
    };
    (u32_at(head, 12) as usize).checked_add(usize::try_from(low).ok()?)
}

/// The 7.3+ resource directory's image-data entry.
fn high_res_offset(b: &[u8]) -> Option<usize> {
    let count = (le32(b, 68)? as usize).min(MAX_RESOURCES);
    (0..count).find_map(|i| {
        let e = 80 + i * 8;
        (b.get(e..e + 3)? == RES_HIGH_RES).then(|| le32(b, e + 4).map(|o| o as usize))?
    })
}

/// A VTF `IMAGE_FORMAT` as a DDS pixel layout. The byte order is the name's (`BGRA8888` is
/// blue, green, red, alpha in memory) except `ARGB8888`, which Valve's tools store as green,
/// blue, alpha, red. Measured against srctools' reference decodes of its one-per-format
/// samples, 2026-09-22: 21 of 24 identical (within one level for the 16-bit and block
/// formats). The other three are presentation, not layout: `A8` is drawn as grey (the DDS
/// decoder's rule for an alpha-only surface, where srctools draws black with alpha), and the
/// two blue-screen formats keep their pure-blue key colour rather than cutting it out. The UV
/// formats are normal and bump maps, drawn as their raw channels.
const FORMATS: &[(u32, Pixels)] = &[
    (0, masks(RGBA, 32, [0xFF, 0xFF00, 0xFF_0000, 0xFF00_0000])), // RGBA8888
    (1, masks(RGBA, 32, [0xFF00_0000, 0xFF_0000, 0xFF00, 0xFF])), // ABGR8888
    (2, masks(DDPF_RGB, 24, [0xFF, 0xFF00, 0xFF_0000, 0])),       // RGB888
    (3, masks(DDPF_RGB, 24, [0xFF_0000, 0xFF00, 0xFF, 0])),       // BGR888
    (4, masks(DDPF_RGB, 16, [0x001F, 0x07E0, 0xF800, 0])),        // RGB565
    (5, masks(DDPF_LUMINANCE, 8, [0xFF, 0, 0, 0])),               // I8
    (
        6,
        masks(DDPF_LUMINANCE | DDPF_ALPHAPIXELS, 16, [0xFF, 0, 0, 0xFF00]),
    ), // IA88
    (8, masks(DDPF_ALPHA, 8, [0, 0, 0, 0xFF])),                   // A8
    (9, masks(DDPF_RGB, 24, [0xFF, 0xFF00, 0xFF_0000, 0])),       // RGB888_BLUESCREEN
    (10, masks(DDPF_RGB, 24, [0xFF_0000, 0xFF00, 0xFF, 0])),      // BGR888_BLUESCREEN
    (11, masks(RGBA, 32, [0xFF00_0000, 0xFF, 0xFF00, 0xFF_0000])), // ARGB8888 (G, B, A, R)
    (12, masks(RGBA, 32, [0xFF_0000, 0xFF00, 0xFF, 0xFF00_0000])), // BGRA8888
    (13, Pixels::FourCc(*b"DXT1")),
    (14, Pixels::FourCc(*b"DXT3")),
    (15, Pixels::FourCc(*b"DXT5")),
    (16, masks(DDPF_RGB, 32, [0xFF_0000, 0xFF00, 0xFF, 0])), // BGRX8888
    (17, masks(DDPF_RGB, 16, [0xF800, 0x07E0, 0x001F, 0])),  // BGR565
    (18, masks(DDPF_RGB, 16, [0x7C00, 0x03E0, 0x001F, 0])),  // BGRX5551
    (19, masks(RGBA, 16, [0x0F00, 0x00F0, 0x000F, 0xF000])), // BGRA4444
    (20, Pixels::FourCc(*b"DXT1")),                          // DXT1_ONEBITALPHA
    (21, masks(RGBA, 16, [0x7C00, 0x03E0, 0x001F, 0x8000])), // BGRA5551
    (22, masks(DDPF_RGB, 16, [0xFF, 0xFF00, 0, 0])),         // UV88
    (23, masks(RGBA, 32, [0xFF, 0xFF00, 0xFF_0000, 0xFF00_0000])), // UVWQ8888
    (24, Pixels::FourCc(ddswrap::D3DFMT_RGBA16F)),           // RGBA16161616F
    (25, Pixels::FourCc(ddswrap::D3DFMT_RGBA16)),            // RGBA16161616
    (26, masks(RGBA, 32, [0xFF, 0xFF00, 0xFF_0000, 0xFF00_0000])), // UVLX8888
];

fn pixels(format: u32) -> Option<Pixels> {
    FORMATS.iter().find(|(f, _)| *f == format).map(|&(_, p)| p)
}

/// Bytes one mip level occupies: every frame, face and depth slice of it.
fn level_len(h: &Header, level: u32) -> Option<u64> {
    let (w, ht, d) = level_dims(h, level);
    h.pixels
        .surface_len(w, ht)?
        .checked_mul(h.frames)?
        .checked_mul(h.faces)?
        .checked_mul(u64::from(d))
}

fn level_dims(h: &Header, level: u32) -> (u32, u32, u32) {
    let at = |v: u32| (v >> level).max(1);
    (at(h.width), at(h.height), at(h.depth))
}

/// The largest level whose first surface fits in a cover, as a DDS.
pub fn extract(b: &[u8]) -> Option<Vec<u8>> {
    let h = header(b)?;
    // The first level small enough to be a cover is the answer, whole or not: a file cut
    // short inside it is refused rather than answered from a smaller level.
    let level = (0..h.mips).find(|&l| fits_a_cover(&h, l))?;
    let (w, ht, _) = level_dims(&h, level);
    let len = usize::try_from(h.pixels.surface_len(w, ht)?).ok()?;
    let start = usize::try_from(level_offset(&h, level)?).ok()?;
    let surface = b.get(start..start.checked_add(len)?)?;
    ddswrap::wrap(w, ht, h.pixels, surface)
}

/// Does one surface of `level`, with the DDS header in front of it, fit in a cover?
fn fits_a_cover(h: &Header, level: u32) -> bool {
    let (w, ht, _) = level_dims(h, level);
    h.pixels
        .surface_len(w, ht)
        .is_some_and(|n| n + ddswrap::MAX_HEADER <= super::MAX_COVER)
}

/// Where `level` starts: after every smaller level, which the file stores first.
fn level_offset(h: &Header, level: u32) -> Option<u64> {
    ((level + 1)..h.mips).try_fold(h.data as u64, |at, l| at.checked_add(level_len(h, l)?))
}

/// A minimal real-shaped 7.2 file (no resource directory) or 7.3 file (with one), for tests
/// and the fuzz seed: `format`, `w` x `h`, `mips` levels, level `n` filled with the byte
/// `n * 40 + 20`.
#[cfg(test)]
pub(crate) fn synth(format: u32, w: u16, h: u16, mips: u8, with_resources: bool) -> Vec<u8> {
    let px = pixels(format).expect("known format");
    let header_len: u32 = if with_resources { 96 } else { 80 };
    let low = Pixels::FourCc(*b"DXT1").surface_len(16, 16).unwrap() as usize;
    let mut v = Vec::new();
    v.extend_from_slice(b"VTF\0");
    v.extend_from_slice(&7u32.to_le_bytes());
    v.extend_from_slice(&(if with_resources { 3u32 } else { 2 }).to_le_bytes());
    v.extend_from_slice(&header_len.to_le_bytes());
    v.extend_from_slice(&w.to_le_bytes());
    v.extend_from_slice(&h.to_le_bytes());
    v.extend_from_slice(&0u32.to_le_bytes()); // flags
    v.extend_from_slice(&1u16.to_le_bytes()); // frames
    v.extend_from_slice(&0u16.to_le_bytes()); // first frame
    v.extend_from_slice(&[0; 4]); // padding
    v.extend_from_slice(&[0; 12]); // reflectivity
    v.extend_from_slice(&[0; 4]); // padding
    v.extend_from_slice(&1f32.to_le_bytes()); // bumpmap scale
    v.extend_from_slice(&format.to_le_bytes());
    v.push(mips);
    v.extend_from_slice(&13u32.to_le_bytes()); // low-res format: DXT1
    v.extend_from_slice(&[16, 16]);
    v.extend_from_slice(&1u16.to_le_bytes()); // depth
    v.extend_from_slice(&[0; 3]);
    let data_at = header_len as usize + low;
    if with_resources {
        v.extend_from_slice(&2u32.to_le_bytes()); // resources
        v.extend_from_slice(&[0; 8]);
        v.extend_from_slice(&[0x01, 0, 0, 0]);
        v.extend_from_slice(&header_len.to_le_bytes());
        v.extend_from_slice(&[0x30, 0, 0, 0]);
        v.extend_from_slice(&(data_at as u32).to_le_bytes());
    }
    v.resize(header_len as usize, 0);
    v.resize(data_at, 0x55); // the low-resolution copy
    for level in (0..u32::from(mips)).rev() {
        let (lw, lh) = (
            (u32::from(w) >> level).max(1),
            (u32::from(h) >> level).max(1),
        );
        let n = px.surface_len(lw, lh).unwrap() as usize;
        v.extend(std::iter::repeat_n(
            (level as u8).wrapping_mul(40).wrapping_add(20),
            n,
        ));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_zero_is_found_after_the_smaller_levels() {
        for with_resources in [false, true] {
            let v = synth(12, 32, 16, 4, with_resources);
            let dds = extract(&v).expect("vtf wraps");
            let img = crate::decode::decode_preview(&dds)
                .expect("decodes")
                .to_rgba8();
            assert_eq!(img.dimensions(), (32, 16), "resources={with_resources}");
            // Level 0 is filled with 20 in every byte; any other level would be 60+.
            assert_eq!(img.get_pixel(5, 5).0, [20, 20, 20, 20]);
        }
    }

    #[test]
    fn block_formats_and_truncation() {
        let v = synth(13, 64, 64, 7, false);
        assert!(extract(&v).is_some(), "dxt1");
        assert!(extract(&v[..v.len() - 1]).is_none(), "level 0 cut short");
        assert!(extract(&synth(15, 8, 8, 1, true)).is_some(), "dxt5");
    }

    /// A level 0 of exactly MAX_COVER bytes leaves no room for the DDS header, so the next
    /// level down answers instead of no level at all.
    #[test]
    fn a_level_zero_at_the_cover_cap_falls_back_to_level_one() {
        let v = synth(12, 4096, 2048, 2, false); // BGRA8888: 4096 * 2048 * 4 = MAX_COVER
        let img = crate::decode::decode_preview(&extract(&v).expect("level 1")).unwrap();
        assert_eq!((img.width(), img.height()), (2048, 1024));
    }

    #[test]
    fn unknown_formats_and_versions_are_refused() {
        let mut v = synth(0, 8, 8, 1, false);
        v[52..56].copy_from_slice(&7u32.to_le_bytes()); // P8
        assert!(extract(&v).is_none());
        let mut v = synth(0, 8, 8, 1, false);
        v[8] = 6;
        assert!(!looks_like_vtf(&v));
    }

    /// The real textures, where the corpus has them: srctools' per-format samples decode to
    /// the picture srctools' own reader produced for them.
    #[test]
    fn real_textures_decode() {
        for name in ["real.vtf", "real-dxt5.vtf", "real-bgra8888.vtf"] {
            let Some(bytes) = crate::testcorpus::read(name) else {
                eprintln!("NOT MEASURED: {name} absent");
                continue;
            };
            let dds = extract(&bytes).unwrap_or_else(|| panic!("{name} wraps"));
            let img = crate::decode::decode_preview(&dds).expect("decodes");
            assert!(img.width() >= 64 && img.height() >= 64, "{name}");
        }
    }
}
