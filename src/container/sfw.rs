//! Seattle FilmWorks `.sfw` (and the PhotoWorks `.pwp` album around it): a JPEG in disguise.
//!
//! Seattle FilmWorks mailed 1990s customers their prints on a floppy ("Pictures on Disk"),
//! as files that are a baseline JPEG with every marker renumbered and the Huffman tables
//! left out to save space. The layout is the one ImageMagick's `sfw.c` documents, checked
//! here against real files (sembiance's `seattleFilmWorks` samples, 2026-09-17): the magic
//! `SFW94A`, then somewhere after it `FF C8 FF D0` - a renumbered SOI + APP0 - and a run of
//! length-prefixed segments up to the renumbered SOS, entropy-coded data, and a renumbered
//! EOI. Undoing it is byte shuffling: put the seven markers back, write `JFIF` into the APP0,
//! and splice the four standard Huffman tables (ITU T.81 Annex K.3, which is what the files
//! were encoded with) in front of the scan. The picture is stored bottom-up, so it is flipped
//! once decoded - without that every photo comes out standing on its head, mirrored.
//!
//! This exists natively because the ImageMagick coder cannot run on Windows at all: it
//! stages the rebuilt JPEG through a temp file it then fails to create ("Permission denied"
//! from `sfw.c`, bundled and stock builds alike), so until 2026-09-17 both extensions were
//! registered and no real file had ever thumbnailed. The rebuilt bytes go to the same JPEG
//! decoder every other tier trusts, under this module's own size ceilings; nothing here
//! interprets pixel data.
//!
//! `.pwp` (magic `SFW95A`) is an album holding several such images; the first one is the
//! cover. `SFW93A`, the older non-JPEG variant, is not handled: no decoder for it is public.

use image::{DynamicImage, ImageDecoder};

use super::util::{be16, find};
use crate::decode::limits::MAX_DIM;
// The standard Huffman tables (ITU T.81 Annex K.3) live where they are used to build
// encoders/decoders — `jpegtran::huffman` — rather than being re-typed here.
use crate::jpegtran::huffman::{
    AC_CHROMA_BITS, AC_CHROMA_VALS, AC_LUMA_BITS, AC_LUMA_VALS, DC_CHROMA_BITS, DC_LUMA_BITS,
    DC_VALS,
};

const SFW_MAGIC: &[u8] = b"SFW94";
const PWP_MAGIC: &[u8] = b"SFW95";
/// The renumbered SOI + APP0 pair that opens the picture inside the wrapper.
const START: &[u8] = &[0xFF, 0xC8, 0xFF, 0xD0];
/// A floppy-era photo is tens of kilobytes; this is a ceiling for a forged file, not a size
/// anyone's pictures reach.
const MAX_JPEG_BYTES: usize = 32 * 1024 * 1024;
/// Segments between APP0 and SOS. A real file has four or five (DQT, SOF, the odd comment).
const MAX_SEGMENTS: usize = 64;

pub fn looks_like_sfw(head: &[u8]) -> bool {
    head.starts_with(SFW_MAGIC) || head.starts_with(PWP_MAGIC)
}

/// Seattle FilmWorks' marker numbering back to JPEG's. Anything else passes through.
fn jpeg_marker(sfw: u8) -> u8 {
    match sfw {
        0xC8 => 0xD8, // SOI
        0xD0 => 0xE0, // APP0
        0xCB => 0xDB, // DQT
        0xA0 => 0xC0, // SOF0
        0xA4 => 0xC4, // DHT
        0xCA => 0xDA, // SOS
        0xC9 => 0xD9, // EOI
        other => other,
    }
}

/// The four standard Huffman tables as one DHT segment: DC and AC for luminance, then DC and
/// AC for chrominance, exactly as ITU T.81 Annex K.3 lists them.
fn standard_dht() -> Vec<u8> {
    let mut body = Vec::with_capacity(416);
    for (class_and_id, bits, values) in [
        (0x00u8, &DC_LUMA_BITS, &DC_VALS[..]),
        (0x10, &AC_LUMA_BITS, &AC_LUMA_VALS[..]),
        (0x01, &DC_CHROMA_BITS, &DC_VALS[..]),
        (0x11, &AC_CHROMA_BITS, &AC_CHROMA_VALS[..]),
    ] {
        body.push(class_and_id);
        body.extend_from_slice(bits);
        body.extend_from_slice(values);
    }
    let mut segment = vec![0xFF, 0xC4];
    segment.extend_from_slice(&((body.len() + 2) as u16).to_be_bytes());
    segment.extend(body);
    segment
}

/// Rebuild the JPEG a Seattle FilmWorks stream is hiding, or `None` when the markers do not
/// line up the way the format says they must.
fn to_jpeg(sfw: &[u8]) -> Option<Vec<u8>> {
    let body = sfw.get(find(sfw, START)?..)?;
    if body.len() > MAX_JPEG_BYTES {
        return None;
    }
    // SOI, then APP0 with `JFIF` written over whatever Seattle FilmWorks kept there.
    let app0_len = usize::from(be16(body, 4)?);
    if app0_len < 9 {
        return None;
    }
    let mut out = Vec::with_capacity(body.len() + 420);
    out.extend_from_slice(&[0xFF, 0xD8, 0xFF, 0xE0]);
    out.extend_from_slice(body.get(4..6)?);
    out.extend_from_slice(b"JFIF\0\x01\0");
    out.extend_from_slice(body.get(13..4 + app0_len)?);

    // Every segment up to the scan: marker renumbered, length and payload kept as they are.
    let (scan_at, has_own_tables) = append_segments(body, &mut out, 4 + app0_len)?;
    // Entropy-coded data cannot contain `FF C9` (a literal FF is always stuffed with 00), so
    // the first one after the scan header is the renumbered EOI.
    let scan = body.get(scan_at + 2..)?;
    let end = find(scan, &[0xFF, 0xC9])?;
    if !has_own_tables {
        out.extend(standard_dht());
    }
    out.extend_from_slice(&[0xFF, 0xDA]);
    out.extend_from_slice(scan.get(..end)?);
    out.extend_from_slice(&[0xFF, 0xD9]);
    Some(out)
}

/// What one renumbered segment in a Seattle FilmWorks stream turned out to be.
enum Segment {
    /// The `SOS` marker: the entropy-coded data starts at this offset.
    Sos,
    /// A segment to copy on: the next offset and whether it was a `DHT`.
    More { next: usize, own_dht: bool },
}

/// Walk the renumbered segments from `at` up to the `SOS`, copying each into `out`; returns
/// the `SOS` offset and whether the stream carried its own Huffman tables.
fn append_segments(body: &[u8], out: &mut Vec<u8>, mut at: usize) -> Option<(usize, bool)> {
    let mut has_own_tables = false;
    for _ in 0..MAX_SEGMENTS {
        match append_segment(body, out, at)? {
            Segment::Sos => return Some((at, has_own_tables)),
            Segment::More { next, own_dht } => {
                has_own_tables |= own_dht;
                at = next;
            }
        }
    }
    None
}

/// Copy the one renumbered segment at `at` into `out`, reporting what it was; `None` when
/// the markers do not line up.
fn append_segment(body: &[u8], out: &mut Vec<u8>, at: usize) -> Option<Segment> {
    if *body.get(at)? != 0xFF {
        return None;
    }
    let marker = jpeg_marker(*body.get(at + 1)?);
    if marker == 0xDA {
        return Some(Segment::Sos);
    }
    let len = usize::from(be16(body, at + 2)?);
    if len < 2 {
        return None;
    }
    out.extend_from_slice(&[0xFF, marker]);
    out.extend_from_slice(body.get(at + 2..at + 2 + len)?);
    Some(Segment::More {
        next: at + 2 + len,
        own_dht: marker == 0xC4,
    })
}

fn decode_jpeg(jpeg: &[u8]) -> Option<DynamicImage> {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DIM);
    limits.max_image_height = Some(MAX_DIM);
    limits.max_alloc = Some(256 * 1024 * 1024);
    let mut decoder =
        image::ImageReader::with_format(std::io::Cursor::new(jpeg), image::ImageFormat::Jpeg)
            .into_decoder()
            .ok()?;
    decoder.set_limits(limits).ok()?;
    DynamicImage::from_decoder(decoder).ok()
}

/// The picture in a `.sfw`, or the first picture of a `.pwp` album. `None` for anything that
/// is not one, including the older `SFW93A` variant.
pub fn extract(bytes: &[u8]) -> Option<DynamicImage> {
    let sfw = if bytes.starts_with(PWP_MAGIC) {
        // An album: each picture is a whole `SFW94A` file in turn. The first is the cover.
        let at = find(bytes.get(PWP_MAGIC.len()..)?, b"SFW94A")? + PWP_MAGIC.len();
        bytes.get(at..)?
    } else if bytes.starts_with(SFW_MAGIC) {
        bytes
    } else {
        return None;
    };
    // Stored bottom-up: see the module docs.
    Some(decode_jpeg(&to_jpeg(sfw)?)?.flipv())
}

/// Wrap a picture the way Seattle FilmWorks did, for the tests and the fuzz seed: the rows
/// flipped, a baseline JPEG written, its Huffman tables dropped and its markers renumbered.
///
/// (Reached only from `fuzzseed::seeds()` and this module's own tests, and `seeds()` is itself
/// called only from the `cfg(test)` fuzz harness - so a plain `cargo build --lib` sees no caller,
/// hence the same `allow(dead_code)` shape `container/mod.rs` uses for its drift-test helpers.)
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn synth(picture: &image::RgbImage, album: bool) -> Vec<u8> {
    let flipped = DynamicImage::ImageRgb8(picture.clone()).flipv().to_rgb8();
    let mut jpeg = std::io::Cursor::new(Vec::new());
    let _ =
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 95).encode_image(&flipped);
    let jpeg = jpeg.into_inner();

    let mut out = if album {
        let mut v = b"SFW95A".to_vec();
        v.extend_from_slice(&[0; 58]);
        v.extend_from_slice(b"SFW94A");
        v
    } else {
        b"SFW94A".to_vec()
    };
    out.extend_from_slice(&[0; 24]);
    // Walk the JPEG's segments: drop DHT, renumber the rest, copy the scan through to EOI.
    let mut at = 2;
    out.extend_from_slice(&[0xFF, 0xC8]);
    while at + 4 <= jpeg.len() {
        let marker = jpeg[at + 1];
        let len = usize::from(u16::from_be_bytes([jpeg[at + 2], jpeg[at + 3]]));
        let renumbered = match marker {
            0xE0 => 0xD0,
            0xDB => 0xCB,
            0xC0 => 0xA0,
            0xDA => 0xCA,
            other => other,
        };
        if marker == 0xDA {
            out.extend_from_slice(&[0xFF, renumbered]);
            out.extend_from_slice(&jpeg[at + 2..jpeg.len() - 2]);
            out.extend_from_slice(&[0xFF, 0xC9]);
            break;
        }
        if marker != 0xC4 {
            out.extend_from_slice(&[0xFF, renumbered]);
            out.extend_from_slice(&jpeg[at + 2..at + 2 + len]);
        }
        at += 2 + len;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Red over blue, so a missing flip shows up as the wrong colour on top.
    fn picture() -> image::RgbImage {
        image::RgbImage::from_fn(32, 32, |_, y| {
            if y < 16 {
                image::Rgb([220, 30, 30])
            } else {
                image::Rgb([30, 30, 220])
            }
        })
    }

    /// The whole round trip: markers back, tables spliced in, rows the right way up.
    #[test]
    fn unwraps_to_the_picture_the_right_way_up() {
        let file = synth(&picture(), false);
        assert!(looks_like_sfw(&file));
        let img = extract(&file).expect("decodes").to_rgb8();
        assert_eq!(img.dimensions(), (32, 32));
        let top = img.get_pixel(16, 4).0;
        let bottom = img.get_pixel(16, 27).0;
        assert!(top[0] > 150 && top[2] < 100, "top should be red: {top:?}");
        assert!(
            bottom[2] > 150 && bottom[0] < 100,
            "bottom should be blue: {bottom:?}"
        );
    }

    /// The splice is the standard table set, byte for byte the size the format documents.
    #[test]
    fn the_spliced_huffman_segment_is_the_standard_420_bytes() {
        let dht = standard_dht();
        assert_eq!(dht.len(), 420);
        assert_eq!(&dht[..4], &[0xFF, 0xC4, 0x01, 0xA2]);
    }

    /// An album's cover is its first picture.
    #[test]
    fn an_album_yields_its_first_picture() {
        let file = synth(&picture(), true);
        assert!(looks_like_sfw(&file));
        assert_eq!(
            extract(&file).expect("decodes").to_rgb8().dimensions(),
            (32, 32)
        );
    }

    /// Refusals: the older SFW93A variant, a wrapper with no picture, a scan with no end.
    #[test]
    fn refuses_what_it_cannot_unwrap() {
        let mut older = synth(&picture(), false);
        older[4] = b'3';
        assert!(!looks_like_sfw(&older));
        assert!(extract(&older).is_none());

        assert!(extract(b"SFW94A and then nothing that looks like a picture").is_none());

        let mut cut = synth(&picture(), false);
        cut.truncate(cut.len() - 2);
        assert!(extract(&cut).is_none(), "no end-of-image marker");
    }

    /// The corpus-driven assertion: a real mail-order photo decodes at its own size with real
    /// detail. Skips where the sibling corpus is absent (CI).
    #[test]
    fn a_real_seattle_filmworks_photo_decodes() {
        let Ok(bytes) = std::fs::read(crate::testcorpus::dir().join("real.sfw")) else {
            return;
        };
        let img = extract(&bytes)
            .expect("a real .sfw should decode")
            .to_rgb8();
        assert!(
            img.width() >= 200 && img.height() >= 200,
            "{:?}",
            img.dimensions()
        );
        let (mut lo, mut hi) = (255u8, 0u8);
        for p in img.pixels() {
            lo = lo.min(p[1]);
            hi = hi.max(p[1]);
        }
        assert!(hi - lo > 100, "a photograph has contrast; got {lo}..{hi}");
    }
}
