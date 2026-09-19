//! PrusaSlicer binary G-code (`.bgcode`): the slicer's own previews, stored as whole image
//! blocks rather than the base64-in-comments of text G-code (`gcode.rs`). Layout from Prusa's
//! `libbgcode/doc/specifications.md` (read 2026-09-17) and checked against real MK4 files: a
//! 10-byte header (`GCDE`, version u32, checksum type u16), then blocks - an 8-byte header
//! (type u16, compression u16, uncompressed size u32) plus a u32 compressed size when the
//! block is compressed, a short parameter section, the data, and a CRC32 after every block when
//! the header says so. Thumbnail blocks (type 5) carry format u16 (0 PNG, 1 JPG, 2 QOI), width
//! u16 and height u16 before the image bytes.
//!
//! Real files carry several: 16x16 and 313x173 QOI for the printer's screen, up to a 640x480
//! PNG. The largest PNG or JPG wins; a QOI is decoded and re-encoded only when nothing else is
//! there, since QOI is not one of the raster kinds the decode tiers are handed as bytes.
//! Thumbnails are uncompressed in every file seen; a deflated one is inflated within the block's
//! declared size, and the heatshrink variants (which only the G-code block uses) are skipped.
//!
//! The walk stops at the G-code block, which the spec puts last, or after a fixed number of
//! blocks, and every malformed size is `None`.

use std::io::Read;

use crate::decode::limits::MAX_DIM;

const MAGIC: &[u8; 4] = b"GCDE";
const BLOCK_THUMBNAIL: u16 = 5;
const BLOCK_GCODE: u16 = 1;
const MAX_BLOCKS: usize = 64;
/// A single thumbnail past this is not a preview.
const MAX_THUMB_BYTES: usize = 16 * 1024 * 1024;

pub fn looks_like_bgcode(head: &[u8]) -> bool {
    head.len() >= 10 && &head[..4] == MAGIC && le32(head, 4) == Some(1)
}

fn le16(b: &[u8], o: usize) -> Option<u16> {
    b.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]]))
}
fn le32(b: &[u8], o: usize) -> Option<u32> {
    b.get(o..o + 4)
        .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// Parameter-section length per block type, from the spec.
fn param_len(kind: u16) -> Option<usize> {
    match kind {
        BLOCK_THUMBNAIL => Some(6),
        0..=4 => Some(2),
        _ => None,
    }
}

/// A preview as stored: whether it is PNG/JPG (as opposed to QOI), its area, and its bytes
/// (inflated when the block was deflated).
type Candidate = (bool, u64, Vec<u8>);

/// One block as it sits in the file: its header words, its parameter bytes and its data.
struct Block<'a> {
    kind: u16,
    compression: u16,
    uncompressed: usize,
    params: &'a [u8],
    data: &'a [u8],
    /// Offset of the next block's header, past this block's checksum.
    next: usize,
}

/// The block at `off`, whose kind carries `plen` parameter bytes, or `None` when any size runs
/// past the file.
fn read_block(bytes: &[u8], off: usize, plen: usize, checksum_len: usize) -> Option<Block<'_>> {
    let kind = le16(bytes, off)?;
    let compression = le16(bytes, off + 2)?;
    let uncompressed = le32(bytes, off + 4)? as usize;
    let (stored, header_len) = if compression == 0 {
        (uncompressed, 8)
    } else {
        (le32(bytes, off + 8)? as usize, 12)
    };
    let params_at = off.checked_add(header_len)?;
    let data_at = params_at.checked_add(plen)?;
    let data_end = data_at.checked_add(stored)?;
    Some(Block {
        kind,
        compression,
        uncompressed,
        params: bytes.get(params_at..data_at)?,
        data: bytes.get(data_at..data_end)?,
        next: data_end.checked_add(checksum_len)?,
    })
}

/// A thumbnail block's preview with its rank, or `None` when its sizes, dimensions or
/// compression are not a preview this module hands on.
fn thumbnail_candidate(block: &Block<'_>) -> Option<Candidate> {
    if block.data.len() > MAX_THUMB_BYTES || block.uncompressed > MAX_THUMB_BYTES {
        return None;
    }
    let format = le16(block.params, 0)?;
    let w = u32::from(le16(block.params, 2)?);
    let h = u32::from(le16(block.params, 4)?);
    if w == 0 || h == 0 || w > MAX_DIM || h > MAX_DIM {
        return None;
    }
    let raw = match block.compression {
        0 => block.data.to_vec(),
        1 => inflate(block.data, block.uncompressed)?,
        _ => return None,
    };
    Some((matches!(format, 0 | 1), u64::from(w) * u64::from(h), raw))
}

/// The higher-ranked of two candidates: a PNG/JPG beats a QOI, then the larger area wins, and
/// on a tie the one already held stays.
fn better_of(held: Option<Candidate>, found: Option<Candidate>) -> Option<Candidate> {
    match (held, found) {
        (Some(a), Some(b)) => Some(if (b.0, b.1) > (a.0, a.1) { b } else { a }),
        (a, b) => a.or(b),
    }
}

/// The chosen preview as bytes the decode tiers accept: PNG/JPG as stored, QOI decoded here
/// (the image crate reads it) and handed back as PNG.
fn encode_candidate(rasterish: bool, raw: Vec<u8>) -> Option<Vec<u8>> {
    if rasterish {
        return super::util::decodable_image(raw);
    }
    let img = image::load_from_memory_with_format(&raw, image::ImageFormat::Qoi).ok()?;
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).ok()?;
    Some(out.into_inner())
}

/// The best embedded preview as encoded image bytes (PNG/JPG as stored, QOI re-encoded to PNG),
/// or `None`.
pub fn extract(bytes: &[u8]) -> Option<Vec<u8>> {
    if !looks_like_bgcode(bytes) {
        return None;
    }
    let checksum_len = if le16(bytes, 8)? == 1 { 4 } else { 0 };
    let mut off = 10usize;
    let mut best: Option<Candidate> = None;
    for _ in 0..MAX_BLOCKS {
        // A kind the spec does not define ends the walk with whatever was found so far.
        let Some(plen) = param_len(le16(bytes, off)?) else {
            break;
        };
        let block = read_block(bytes, off, plen, checksum_len)?;
        if block.kind == BLOCK_THUMBNAIL {
            best = better_of(best, thumbnail_candidate(&block));
        }
        if block.kind == BLOCK_GCODE {
            break;
        }
        off = block.next;
        if off >= bytes.len() {
            break;
        }
    }
    let (rasterish, _, raw) = best?;
    encode_candidate(rasterish, raw)
}

/// A deflated thumbnail block: zlib-wrapped first (what `deflate()` emits), raw deflate as the
/// fallback, both bounded to the block's declared uncompressed size.
fn inflate(data: &[u8], uncompressed: usize) -> Option<Vec<u8>> {
    let cap = uncompressed as u64;
    let zlib = flate2::read::ZlibDecoder::new(data);
    if let Ok(v) = crate::decode::read_bounded(zlib.take(cap + 1), cap) {
        if !v.is_empty() {
            return Some(v);
        }
    }
    let raw = flate2::read::DeflateDecoder::new(data);
    crate::decode::read_bounded(raw.take(cap + 1), cap)
        .ok()
        .filter(|v| !v.is_empty())
}

/// Build a minimal file in memory: the header, one metadata block, the given thumbnail blocks
/// (format, width, height, bytes), no checksums. Shared by the tests and the fuzz seed.
///
/// (Reached only from `fuzzseed::seeds()` and this module's own tests, and `seeds()` is itself
/// called only from the `cfg(test)` fuzz harness - so a plain `cargo build --lib` sees no caller,
/// hence the same `allow(dead_code)` shape `container/mod.rs` uses for its drift-test helpers.)
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn synth(thumbs: &[(u16, u16, u16, &[u8])]) -> Vec<u8> {
    let mut v = MAGIC.to_vec();
    v.extend_from_slice(&1u32.to_le_bytes());
    v.extend_from_slice(&0u16.to_le_bytes()); // no checksum
                                              // File metadata block: type 0, no compression, encoding 0, "a=b\n".
    let meta = b"a=b\n";
    v.extend_from_slice(&0u16.to_le_bytes());
    v.extend_from_slice(&0u16.to_le_bytes());
    v.extend_from_slice(&(meta.len() as u32).to_le_bytes());
    v.extend_from_slice(&0u16.to_le_bytes());
    v.extend_from_slice(meta);
    for &(format, w, h, data) in thumbs {
        v.extend_from_slice(&BLOCK_THUMBNAIL.to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&(data.len() as u32).to_le_bytes());
        v.extend_from_slice(&format.to_le_bytes());
        v.extend_from_slice(&w.to_le_bytes());
        v.extend_from_slice(&h.to_le_bytes());
        v.extend_from_slice(data);
    }
    // The G-code block, last: type 1, encoding 0, a comment line.
    let g = b"; hello\n";
    v.extend_from_slice(&BLOCK_GCODE.to_le_bytes());
    v.extend_from_slice(&0u16.to_le_bytes());
    v.extend_from_slice(&(g.len() as u32).to_le_bytes());
    v.extend_from_slice(&0u16.to_le_bytes());
    v.extend_from_slice(g);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32, rgba: [u8; 4]) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(w, h, image::Rgba(rgba));
        let mut out = std::io::Cursor::new(Vec::new());
        let _ = image::DynamicImage::ImageRgba8(img).write_to(&mut out, image::ImageFormat::Png);
        out.into_inner()
    }

    fn qoi(w: u32, h: u32, rgba: [u8; 4]) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(w, h, image::Rgba(rgba));
        let mut out = std::io::Cursor::new(Vec::new());
        let _ = image::DynamicImage::ImageRgba8(img).write_to(&mut out, image::ImageFormat::Qoi);
        out.into_inner()
    }

    /// The largest PNG/JPG wins over a larger QOI, and over smaller PNGs.
    #[test]
    fn picks_the_largest_png_over_qoi_and_smaller_pngs() {
        let small = png(4, 4, [255, 0, 0, 255]);
        let big = png(8, 8, [0, 255, 0, 255]);
        let huge_qoi = qoi(16, 16, [0, 0, 255, 255]);
        let file = synth(&[(0, 4, 4, &small), (2, 16, 16, &huge_qoi), (0, 8, 8, &big)]);
        assert!(looks_like_bgcode(&file));
        let out = extract(&file).expect("thumbnail");
        assert_eq!(out, big);
    }

    /// QOI alone is decoded and handed back as PNG.
    #[test]
    fn a_lone_qoi_thumbnail_is_re_encoded_as_png() {
        let file = synth(&[(2, 3, 2, &qoi(3, 2, [9, 8, 7, 255]))]);
        let out = extract(&file).expect("thumbnail");
        assert!(out.starts_with(b"\x89PNG"));
        let img = image::load_from_memory(&out).expect("png").to_rgba8();
        assert_eq!(img.dimensions(), (3, 2));
        assert_eq!(img.get_pixel(0, 0).0, [9, 8, 7, 255]);
    }

    /// Refusals: wrong magic, an unsupported version, no thumbnail block, a size past the file.
    #[test]
    fn refuses_what_it_cannot_read() {
        let mut bad = synth(&[(0, 4, 4, &png(4, 4, [1, 2, 3, 255]))]);
        bad[0] = b'X';
        assert!(!looks_like_bgcode(&bad));
        assert!(extract(&bad).is_none());

        let mut v2 = synth(&[(0, 4, 4, &png(4, 4, [1, 2, 3, 255]))]);
        v2[4..8].copy_from_slice(&2u32.to_le_bytes());
        assert!(
            extract(&v2).is_none(),
            "an unknown version is not guessed at"
        );

        assert!(extract(&synth(&[])).is_none(), "no thumbnail block");

        let mut cut = synth(&[(0, 4, 4, &png(4, 4, [1, 2, 3, 255]))]);
        cut.truncate(40);
        assert!(extract(&cut).is_none(), "a block past the end of the file");
    }

    /// The corpus-driven assertion: a real PrusaSlicer MK4 file yields its 640x480 PNG.
    #[test]
    fn a_real_prusaslicer_file_yields_its_largest_png() {
        let Ok(bytes) = std::fs::read(crate::testcorpus::dir().join("sample.bgcode")) else {
            return;
        };
        let out = extract(&bytes).expect("thumbnail");
        assert!(out.starts_with(b"\x89PNG"));
        let img = image::load_from_memory(&out).expect("png");
        assert_eq!((img.width(), img.height()), (640, 480));
    }
}
