//! Alias/Wavefront `.pix`: run-length pixels behind a ten-byte header, decoded here.
//!
//! The format PowerAnimator wrote and Maya still can: five big-endian `u16`s - width, height,
//! x offset, y offset, bits per pixel (24, or 8 for a matte) - then packets to the end of the
//! file, `count B G R` for colour and `count value` for grey, each a run of `count` pixels.
//! Checked against real files from FFmpeg's FATE suite (`aliaspix/first.pix`,
//! `firstgray.pix`) and sembiance's samples, 2026-09-17: in every one the runs add up to
//! exactly `width * height` on exactly the last byte of the file.
//!
//! That equation is the signature, because the format has none: the ten-byte header alone is
//! matched by countless unrelated files, so [`extract`] walks the packets FIRST, allocating
//! nothing, and only a file whose runs land on `width * height` at end-of-file is decoded.
//! It is the same idea as binary STL's `84 + 50 n` length check in `decode/mesh.rs`. BRender's
//! unrelated `.pix` textures and PCI Geomatics' `.pix` databases fail it on the first field.
//!
//! This is native because ImageMagick's `PIX` reader returns no image for any of those real
//! files (it only ever read back its own tests' output here), and because a run-length
//! expansion needs no codec: the work is bounded by the pixel ceiling below and by the file.

use image::{DynamicImage, GrayImage, RgbImage};

use super::util::be16;
use crate::decode::limits::MAX_DIM;

const HEADER_LEN: usize = 10;
/// 32 Mpx is ~96 MB of RGB - past any frame these tools rendered, and the ceiling on what one
/// forged header can make this module allocate.
const MAX_PIXELS: u64 = 32 * 1024 * 1024;

struct Header {
    width: u32,
    height: u32,
    /// Bytes per packet after the count: 3 for colour, 1 for a matte.
    sample_bytes: usize,
}

fn header(bytes: &[u8]) -> Option<Header> {
    let width = u32::from(be16(bytes, 0)?);
    let height = u32::from(be16(bytes, 2)?);
    let sample_bytes = match be16(bytes, 8)? {
        24 => 3,
        8 => 1,
        _ => return None,
    };
    let sane = width > 0
        && height > 0
        && width <= MAX_DIM
        && height <= MAX_DIM
        && u64::from(width) * u64::from(height) <= MAX_PIXELS;
    sane.then_some(Header {
        width,
        height,
        sample_bytes,
    })
}

/// Cheap plausibility only - the real test is [`runs_fill_the_picture_exactly`].
pub fn looks_like_alias_pix(head: &[u8]) -> bool {
    header(head).is_some()
}

/// The signature: every packet has a non-zero count, and the counts reach `width * height`
/// on the file's last byte. Nothing is allocated, so a near-miss costs one pass over the file.
fn runs_fill_the_picture_exactly(bytes: &[u8], hdr: &Header) -> bool {
    let want = u64::from(hdr.width) * u64::from(hdr.height);
    let packet = 1 + hdr.sample_bytes;
    let body = &bytes[HEADER_LEN..];
    if body.is_empty() || !body.len().is_multiple_of(packet) {
        return false;
    }
    let mut pixels = 0u64;
    for p in body.chunks_exact(packet) {
        if p[0] == 0 {
            return false;
        }
        pixels += u64::from(p[0]);
        if pixels > want {
            return false;
        }
    }
    pixels == want
}

/// The picture, or `None` for anything that is not an Alias PIX file end to end.
pub fn extract(bytes: &[u8]) -> Option<DynamicImage> {
    let hdr = header(bytes)?;
    if !runs_fill_the_picture_exactly(bytes, &hdr) {
        return None;
    }
    let total = (hdr.width as usize) * (hdr.height as usize) * hdr.sample_bytes;
    let mut out = Vec::with_capacity(total);
    for p in bytes[HEADER_LEN..].chunks_exact(1 + hdr.sample_bytes) {
        let run = usize::from(p[0]);
        if hdr.sample_bytes == 3 {
            // Stored B, G, R.
            let rgb = [p[3], p[2], p[1]];
            for _ in 0..run {
                out.extend_from_slice(&rgb);
            }
        } else {
            out.resize(out.len() + run, p[1]);
        }
    }
    if hdr.sample_bytes == 3 {
        RgbImage::from_raw(hdr.width, hdr.height, out).map(DynamicImage::ImageRgb8)
    } else {
        GrayImage::from_raw(hdr.width, hdr.height, out).map(DynamicImage::ImageLuma8)
    }
}

/// A small file in memory, one run per row: `rows` holds each row's colour (or `[v, v, v]`
/// for a matte, of which the first value is used). Shared by the tests and the fuzz seed.
///
/// (Reached only from `fuzzseed::seeds()` and this module's own tests, and `seeds()` is itself
/// called only from the `cfg(test)` fuzz harness - so a plain `cargo build --lib` sees no caller,
/// hence the same `allow(dead_code)` shape `container/mod.rs` uses for its drift-test helpers.)
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn synth(width: u16, rows: &[[u8; 3]], grey: bool) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&width.to_be_bytes());
    v.extend_from_slice(&(rows.len() as u16).to_be_bytes());
    v.extend_from_slice(&[0, 0, 0, 0]);
    v.extend_from_slice(&(if grey { 8u16 } else { 24 }).to_be_bytes());
    for [r, g, b] in rows {
        // A run is one byte long, so a wide row is several packets.
        let mut left = usize::from(width);
        while left > 0 {
            let run = left.min(255);
            v.push(run as u8);
            if grey {
                v.push(*r);
            } else {
                v.extend_from_slice(&[*b, *g, *r]);
            }
            left -= run;
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Colour comes back R, G, B from its B, G, R packets, rows in order, and a row wider than
    /// one run's 255 pixels is stitched from several.
    #[test]
    fn decodes_colour_runs_in_row_order() {
        let file = synth(300, &[[200, 10, 20], [10, 200, 20], [10, 20, 200]], false);
        assert!(looks_like_alias_pix(&file));
        let img = extract(&file).expect("decodes").to_rgb8();
        assert_eq!(img.dimensions(), (300, 3));
        assert_eq!(img.get_pixel(0, 0).0, [200, 10, 20]);
        assert_eq!(
            img.get_pixel(299, 1).0,
            [10, 200, 20],
            "past the first 255-pixel run"
        );
        assert_eq!(img.get_pixel(150, 2).0, [10, 20, 200]);
    }

    /// The 8-bit matte variant.
    #[test]
    fn decodes_a_grey_matte() {
        let file = synth(4, &[[0, 0, 0], [128, 0, 0], [255, 0, 0]], true);
        let img = extract(&file).expect("decodes").to_luma8();
        assert_eq!(img.dimensions(), (4, 3));
        assert_eq!((img.get_pixel(0, 0).0, img.get_pixel(3, 2).0), ([0], [255]));
    }

    /// The exactness that stands in for a signature: one byte more, one run short, a zero
    /// count or another depth, and it is not an Alias PIX file.
    #[test]
    fn only_a_file_whose_runs_land_exactly_is_accepted() {
        let good = synth(8, &[[1, 2, 3]; 4], false);
        assert!(extract(&good).is_some());

        let mut longer = good.clone();
        longer.push(0);
        assert!(extract(&longer).is_none(), "a trailing byte");

        let mut shorter = good.clone();
        shorter.truncate(shorter.len() - 4);
        assert!(extract(&shorter).is_none(), "one run short of the picture");

        let mut zero = good.clone();
        zero[HEADER_LEN] = 0;
        assert!(extract(&zero).is_none(), "a zero-length run");

        let mut depth = good.clone();
        depth[9] = 32;
        assert!(!looks_like_alias_pix(&depth));
        assert!(
            extract(&depth).is_none(),
            "32 bits per pixel is not this format"
        );

        let mut huge = good;
        huge[0..4].copy_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
        assert!(
            extract(&huge).is_none(),
            "a 65535 x 65535 header is refused before any work"
        );
    }

    /// The corpus-driven assertion: FFmpeg's own reference file decodes at its declared size
    /// with a real picture in it. Skips where the sibling corpus is absent (CI).
    #[test]
    fn a_real_alias_pix_file_decodes() {
        let Ok(bytes) = std::fs::read("../test-corpus/real.pix") else {
            return;
        };
        let img = extract(&bytes)
            .expect("a real .pix should decode")
            .to_rgb8();
        assert_eq!(img.dimensions(), (201, 79));
        let (mut lo, mut hi) = (255u8, 0u8);
        for p in img.pixels() {
            lo = lo.min(p[1]);
            hi = hi.max(p[1]);
        }
        assert!(hi - lo > 100, "a picture has contrast; got {lo}..{hi}");
    }
}
