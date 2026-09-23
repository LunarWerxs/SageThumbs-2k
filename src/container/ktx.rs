//! Khronos KTX 1 `.ktx` textures: mip level 0, re-wrapped as a DDS for `decode/dds.rs`.
//!
//! Layout, per the Khronos KTX File Format Specification 1.0 and checked against the real
//! files in KTX-Software's `tests/resources/ktx` (written by its `toktx`), 2026-09-22: a
//! 12-byte identifier, an endianness word, twelve `u32` fields naming the OpenGL type,
//! format and internal format, the size and the counts, a key/value block, then the levels
//! LARGEST first, each prefixed by its byte size. So level 0 is the first thing after the
//! key/value block and nothing has to be summed to find it.
//!
//! What is read: the uncompressed 8-bit layouts (`GL_RGBA`, `GL_RGB`, their BGR orders,
//! luminance, luminance-alpha, red, red-green, alpha) and the block formats `decode/dds.rs`
//! already decodes (S3TC DXT1/3/5, RGTC, BPTC). What is NOT: ETC1/ETC2/EAC and ASTC, which
//! Android and mobile toolchains write and no tier here decodes, so those files keep the
//! stock icon, as does a big-endian file (the spec allows one; no tool here writes it).
//!
//! Uncompressed rows are padded to four bytes (`GL_UNPACK_ALIGNMENT`), so they are repacked
//! tight for the DDS. `KTXorientation` `T=u` (rows stored bottom-up) is honoured by
//! reversing the rows of an uncompressed level; a block-compressed level stored that way is
//! refused rather than drawn upside down.

use super::ddswrap::{
    self, masks, Pixels, DDPF_ALPHA, DDPF_ALPHAPIXELS, DDPF_LUMINANCE, DDPF_RGB, DDPF_RGBA,
};
use super::util::le32;
use crate::decode::limits::MAX_DIM;

const IDENTIFIER: [u8; 12] = [
    0xAB, b'K', b'T', b'X', b' ', b'1', b'1', 0xBB, b'\r', b'\n', 0x1A, b'\n',
];
const HEADER_LEN: usize = 64;
const GL_UNSIGNED_BYTE: u32 = 0x1401;

pub fn looks_like_ktx(b: &[u8]) -> bool {
    b.starts_with(&IDENTIFIER) && le32(b, 12) == Some(0x0403_0201)
}

/// Field `i` of the twelve after the endianness word (`0` = `glType` ... `11` = the key/value
/// length). The header is a fixed array, so these offsets cannot run past it.
fn field(h: &[u8; HEADER_LEN], i: usize) -> u32 {
    let o = 16 + i * 4;
    u32::from_le_bytes([h[o], h[o + 1], h[o + 2], h[o + 3]])
}

/// The 8-bit uncompressed layouts, by `glFormat` (`glType` is `GL_UNSIGNED_BYTE`).
const UNCOMPRESSED: &[(u32, Pixels)] = &[
    (
        0x1908, // GL_RGBA
        masks(DDPF_RGBA, 32, [0xFF, 0xFF00, 0xFF_0000, 0xFF00_0000]),
    ),
    (
        0x80E1, // GL_BGRA
        masks(DDPF_RGBA, 32, [0xFF_0000, 0xFF00, 0xFF, 0xFF00_0000]),
    ),
    (0x1907, masks(DDPF_RGB, 24, [0xFF, 0xFF00, 0xFF_0000, 0])), // GL_RGB
    (0x80E0, masks(DDPF_RGB, 24, [0xFF_0000, 0xFF00, 0xFF, 0])), // GL_BGR
    (0x8227, masks(DDPF_RGB, 16, [0xFF, 0xFF00, 0, 0])),         // GL_RG
    (0x1903, masks(DDPF_LUMINANCE, 8, [0xFF, 0, 0, 0])),         // GL_RED
    (0x1909, masks(DDPF_LUMINANCE, 8, [0xFF, 0, 0, 0])),         // GL_LUMINANCE
    (
        0x190A, // GL_LUMINANCE_ALPHA
        masks(DDPF_LUMINANCE | DDPF_ALPHAPIXELS, 16, [0xFF, 0, 0, 0xFF00]),
    ),
    (0x1906, masks(DDPF_ALPHA, 8, [0, 0, 0, 0xFF])), // GL_ALPHA
];

/// The block formats, by `glInternalFormat` (`glType` is 0): S3TC linear and sRGB, RGTC, and
/// BPTC (BC7 unorm / sRGB, BC6H signed / unsigned float).
const BLOCKS: &[(u32, Pixels)] = &[
    (0x83F0, Pixels::FourCc(*b"DXT1")),
    (0x83F1, Pixels::FourCc(*b"DXT1")),
    (0x8C4C, Pixels::FourCc(*b"DXT1")),
    (0x8C4D, Pixels::FourCc(*b"DXT1")),
    (0x83F2, Pixels::FourCc(*b"DXT3")),
    (0x8C4E, Pixels::FourCc(*b"DXT3")),
    (0x83F3, Pixels::FourCc(*b"DXT5")),
    (0x8C4F, Pixels::FourCc(*b"DXT5")),
    (0x8DBB, Pixels::FourCc(*b"ATI1")),
    (0x8DBC, Pixels::FourCc(*b"BC4S")),
    (0x8DBD, Pixels::FourCc(*b"ATI2")),
    (0x8DBE, Pixels::FourCc(*b"BC5S")),
    (0x8E8C, Pixels::Dxgi(98)),
    (0x8E8D, Pixels::Dxgi(99)),
    (0x8E8E, Pixels::Dxgi(96)),
    (0x8E8F, Pixels::Dxgi(95)),
];

/// How level 0's bytes are laid out, or `None` for a format no tier here decodes.
fn layout(gl_type: u32, gl_format: u32, internal: u32) -> Option<Pixels> {
    let (table, key) = match gl_type {
        GL_UNSIGNED_BYTE => (UNCOMPRESSED, gl_format),
        0 => (BLOCKS, internal),
        _ => return None,
    };
    table.iter().find(|(k, _)| *k == key).map(|&(_, p)| p)
}

/// Does the key/value block say the rows run bottom-up (`KTXorientation` with `T=u`)?
fn rows_bottom_up(kv: &[u8]) -> bool {
    let mut at = 0usize;
    while let Some(n) = le32(kv, at) {
        let Some(pair) = kv.get(at + 4..(at + 4).saturating_add(n as usize)) else {
            return false;
        };
        if let Some(value) = pair.strip_prefix(b"KTXorientation\0") {
            return value.windows(3).any(|w| w == b"T=u");
        }
        at = at + 4 + (n as usize).next_multiple_of(4);
    }
    false
}

/// Level 0 as the file holds it: its size, layout, orientation and bytes.
struct Level<'a> {
    width: u32,
    height: u32,
    px: Pixels,
    flip: bool,
    data: &'a [u8],
}

fn level0(b: &[u8]) -> Option<Level<'_>> {
    let h: &[u8; HEADER_LEN] = b.get(..HEADER_LEN)?.try_into().ok()?;
    if !looks_like_ktx(h) {
        return None;
    }
    let (width, height) = (field(h, 5), field(h, 6).max(1));
    if width == 0 || width > MAX_DIM || height > MAX_DIM {
        return None;
    }
    let px = layout(field(h, 0), field(h, 2), field(h, 3))?;
    let kv_end = HEADER_LEN.checked_add(field(h, 11) as usize)?;
    let size = le32(b, kv_end)? as usize;
    let start = kv_end + 4;
    Some(Level {
        width,
        height,
        px,
        flip: rows_bottom_up(b.get(HEADER_LEN..kv_end)?),
        data: b.get(start..start.checked_add(size)?)?,
    })
}

/// Level 0 (first face, array element and slice) as a DDS.
pub fn extract(b: &[u8]) -> Option<Vec<u8>> {
    let l = level0(b)?;
    if !matches!(l.px, Pixels::Masks { .. }) {
        // Block data has no row padding, and `imageSize` covers every face or array element
        // of the level, so the first surface is its first bytes. Stored bottom-up it would
        // need its blocks flipped too, which is not done here: refused.
        if l.flip {
            return None;
        }
        let tight = usize::try_from(l.px.surface_len(l.width, l.height)?).ok()?;
        return ddswrap::wrap(l.width, l.height, l.px, l.data.get(..tight)?);
    }
    ddswrap::wrap(l.width, l.height, l.px, &repack(&l)?)
}

/// Uncompressed rows, padded to four bytes in the file, made tight and put top-down.
/// Nothing is reserved until the level is known to hold every row: the header alone would
/// otherwise let a 68-byte file ask for a gigabyte.
fn repack(l: &Level) -> Option<Vec<u8>> {
    let Pixels::Masks { bpp, .. } = l.px else {
        return None;
    };
    let tight = usize::try_from(l.px.surface_len(l.width, l.height)?).ok()?;
    let row = (l.width * (bpp / 8)) as usize;
    let pitch = row.next_multiple_of(4);
    let needed = (l.height as usize - 1)
        .checked_mul(pitch)?
        .checked_add(row)?;
    if tight as u64 + ddswrap::MAX_HEADER > super::MAX_COVER || l.data.len() < needed {
        return None;
    }
    let last = l.height as usize - 1;
    let mut surface = Vec::with_capacity(tight);
    for y in 0..=last {
        let src = (if l.flip { last - y } else { y }) * pitch;
        surface.extend_from_slice(l.data.get(src..src + row)?);
    }
    Some(surface)
}

/// A minimal valid KTX 1 file: uncompressed `GL_RGB` `w` x `h` (so rows need padding when
/// `w * 3` is not a multiple of four), every pixel `rgb`, optionally marked bottom-up and
/// with the bottom row painted `bottom`.
#[cfg(test)]
pub(crate) fn synth_rgb(w: u32, h: u32, rgb: [u8; 3], bottom: [u8; 3], bottom_up: bool) -> Vec<u8> {
    let mut v = IDENTIFIER.to_vec();
    let mut put = |x: u32| v.extend_from_slice(&x.to_le_bytes());
    let kv: &[u8] = if bottom_up {
        b"KTXorientation\0S=r,T=u\0"
    } else {
        b"KTXorientation\0S=r,T=d\0"
    };
    let kv_padded = kv.len().next_multiple_of(4);
    for x in [
        0x0403_0201,
        GL_UNSIGNED_BYTE,
        1,
        0x1907,
        0x8051,
        0x1907,
        w,
        h,
        0,
        0,
        1,
        1,
        4 + kv_padded as u32,
    ] {
        put(x);
    }
    put(kv.len() as u32);
    v.extend_from_slice(kv);
    v.resize(v.len() + kv_padded - kv.len(), 0);
    let pitch = (w * 3).next_multiple_of(4);
    v.extend_from_slice(&(pitch * h).to_le_bytes());
    for y in 0..h {
        // "Bottom" is the last row as displayed: the first stored row when bottom-up.
        let is_bottom = if bottom_up { y == 0 } else { y == h - 1 };
        let c = if is_bottom { bottom } else { rgb };
        for _ in 0..w {
            v.extend_from_slice(&c);
        }
        v.resize(v.len() + (pitch - w * 3) as usize, 0);
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(v: &[u8]) -> image::RgbaImage {
        let dds = extract(v).expect("ktx wraps");
        crate::decode::decode_preview(&dds)
            .expect("decodes")
            .to_rgba8()
    }

    #[test]
    fn padded_rows_are_repacked_and_orientation_is_honoured() {
        for bottom_up in [false, true] {
            // Width 5: 15-byte rows padded to 16.
            let v = synth_rgb(5, 3, [10, 20, 30], [200, 100, 50], bottom_up);
            let img = decode(&v);
            assert_eq!(img.dimensions(), (5, 3));
            assert_eq!(
                img.get_pixel(4, 0).0,
                [10, 20, 30, 255],
                "bottom_up={bottom_up}"
            );
            assert_eq!(
                img.get_pixel(4, 2).0,
                [200, 100, 50, 255],
                "bottom_up={bottom_up}"
            );
        }
    }

    #[test]
    fn truncated_unsupported_and_big_endian_files_are_refused() {
        let v = synth_rgb(4, 4, [1, 2, 3], [1, 2, 3], false);
        assert!(extract(&v[..v.len() - 1]).is_none());
        let mut etc = v.clone();
        etc[16..20].copy_from_slice(&0u32.to_le_bytes()); // glType 0: compressed
        etc[28..32].copy_from_slice(&0x8D64u32.to_le_bytes()); // ETC1
        assert!(extract(&etc).is_none());
        let mut be = v.clone();
        be[12..16].copy_from_slice(&0x0102_0304u32.to_le_bytes());
        assert!(!looks_like_ktx(&be));
        // A header claiming a 16384 x 16384 RGBA level with no data behind it is refused
        // before anything is reserved for it.
        let mut huge = v[..68].to_vec();
        huge[24..28].copy_from_slice(&0x1908u32.to_le_bytes()); // GL_RGBA
        huge[36..44].copy_from_slice(&[0, 0x40, 0, 0, 0, 0x40, 0, 0]);
        huge[60..64].copy_from_slice(&0u32.to_le_bytes()); // no key/value data
        huge[64..68].copy_from_slice(&0u32.to_le_bytes()); // imageSize 0
        assert!(extract(&huge).is_none());
    }

    /// KTX-Software's own test textures, where the corpus has them.
    #[test]
    fn real_textures_decode() {
        for (name, dims) in [
            ("real.ktx", (180, 94)),
            ("real-bc2.ktx", (1024, 1024)),
            ("real-not4.ktx", (270, 270)),
        ] {
            let Some(bytes) = crate::testcorpus::read(name) else {
                eprintln!("NOT MEASURED: {name} absent");
                continue;
            };
            assert_eq!(decode(&bytes).dimensions(), dims, "{name}");
        }
    }
}
