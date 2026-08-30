//! Unit tests for the decode pipeline.
//!
//! One submodule per thing under test. The shared fixtures stay here because
//! every submodule reaches them through `use super::*`, which is also how they
//! reach the decode internals the assertions name.

use super::*;

// The tests exercise internals that now live in the sibling children; glob them in so
// every assertion below still names them exactly as it did pre-split.
use super::readers::*;

// Not imported by the hub any more: only these tests still name the length-only wrapper.
use crate::container::jpeg_span_len;

fn png_bytes(w: u32, h: u32, color: [u8; 4]) -> Vec<u8> {
    let mut img = image::RgbaImage::new(w, h);
    for p in img.pixels_mut() {
        *p = image::Rgba(color);
    }
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .unwrap();
    bytes
}

fn noisy_jpeg_bytes(w: u32, h: u32) -> Vec<u8> {
    let mut img = image::RgbImage::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let r = ((x * 37 + y * 11) & 0xFF) as u8;
            let g = ((x * 13 + y * 53) & 0xFF) as u8;
            let b = ((x * 97 + y * 3) & 0xFF) as u8;
            img.put_pixel(x, y, image::Rgb([r, g, b]));
        }
    }
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgb8(img)
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Jpeg,
        )
        .unwrap();
    assert!(
        bytes.len() >= MIN_RAW_PREVIEW,
        "test JPEG must be large enough to be a RAW preview"
    );
    bytes
}

fn tiny_tga_with_trailing_jpeg() -> Vec<u8> {
    let mut tga = vec![0u8; 18];
    tga[2] = 2; // uncompressed true-color
    tga[12..14].copy_from_slice(&2u16.to_le_bytes());
    tga[14..16].copy_from_slice(&2u16.to_le_bytes());
    tga[16] = 24; // BGR
    tga[17] = 0x20; // top-left origin
    tga.extend_from_slice(&[
        0, 0, 255, // red
        0, 255, 0, // green
        255, 0, 0, // blue
        255, 255, 255, // white
    ]);
    tga.extend_from_slice(&noisy_jpeg_bytes(192, 192));
    tga
}

/// A minimal valid JPEG: SOI + APPn (length-prefixed) + SOS + `entropy` bytes of
/// payload (no 0xFF) + EOI. Used to exercise the RAW-preview carver.
fn mini_jpeg(app_payload: &[u8], entropy: usize) -> Vec<u8> {
    let mut v = vec![0xFF, 0xD8]; // SOI
                                  // APP0 segment carrying `app_payload` (length covers the 2 length bytes too).
    let app_len = (app_payload.len() + 2) as u16;
    v.extend_from_slice(&[0xFF, 0xE0]);
    v.extend_from_slice(&app_len.to_be_bytes());
    v.extend_from_slice(app_payload);
    v.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x02]); // SOS, header length = 2 (none)
    v.extend(std::iter::repeat_n(0x55, entropy)); // entropy data (contains no 0xFF)
    v.extend_from_slice(&[0xFF, 0xD9]); // EOI
    v
}

/// [`mini_jpeg`] carrying an explicit SOFn frame header, so a test can build the difference
/// between a picture and a pile of sensor readings. `sof` is the marker's low byte: 0xC0 is
/// baseline, 0xC3 is LOSSLESS — the encoding Canon CR2 uses for raw sensor data.
fn mini_jpeg_sof(sof: u8, entropy: usize) -> Vec<u8> {
    let mut v = vec![0xFF, 0xD8]; // SOI
                                  // SOFn. The declared length (0x0B = 11) counts its own two bytes, so exactly 9 must
                                  // follow: precision, height, width, component count, then one 3-byte component spec. Get
                                  // that wrong and `jpeg_span` walks off into the next marker and rejects the whole frame —
                                  // a fixture bug that reads exactly like a code bug.
    v.extend_from_slice(&[
        0xFF, sof, 0x00, 0x0B, // marker + segment length
        0x08, 0x00, 0x10, 0x00, 0x10, 0x01, // 8-bit, 16x16, one component
        0x00, 0x11, 0x00, // component 0: id, sampling 1x1, quant table 0
    ]);
    v.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x02]); // SOS, header length = 2 (none)
    v.extend(std::iter::repeat_n(0x55, entropy)); // entropy data (contains no 0xFF)
    v.extend_from_slice(&[0xFF, 0xD9]); // EOI
    v
}

/// A JPEG XL whose colour encoding is **AdobeRGB**, encoded with the exact cjxl 0.12
/// parameters from issue #9 (`-d 1.0 -m 1 -e 9 -p --faster_decoding 2 --brotli_effort 11`).
/// 256x256, in-gamut patches only, so every correct decoder must agree on the answer and
/// the assertion below can't be a gamut-mapping coin flip. Regenerate with:
///   cjxl g_adobe.png adobergb_modular.jxl -d 1.0 -m 1 -e 9 -p --faster_decoding 2 --brotli_effort 11
const JXL_ADOBERGB: &[u8] = include_bytes!("../../tests/fixtures/jxl/adobergb_modular.jxl");

/// Wrap a JPEG's bytes with an EXIF APP1 declaring `orientation` (1..=8).
///
/// Hand-assembled rather than pulled from a corpus file so the test states exactly what it
/// depends on: one IFD0 entry, tag 0x0112, little-endian TIFF.
fn with_exif_orientation(jpeg: &[u8], orientation: u16) -> Vec<u8> {
    assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "fixture must be a JPEG");
    let mut app1: Vec<u8> = Vec::new();
    app1.extend_from_slice(b"Exif\0\0");
    app1.extend_from_slice(b"II\x2A\x00"); // little-endian TIFF magic
    app1.extend_from_slice(&8u32.to_le_bytes()); // IFD0 begins at offset 8
    app1.extend_from_slice(&1u16.to_le_bytes()); // one entry
    app1.extend_from_slice(&0x0112u16.to_le_bytes()); // Orientation
    app1.extend_from_slice(&3u16.to_le_bytes()); // type SHORT
    app1.extend_from_slice(&1u32.to_le_bytes()); // count
    app1.extend_from_slice(&(orientation as u32).to_le_bytes()); // value, left-packed
    app1.extend_from_slice(&0u32.to_le_bytes()); // no next IFD

    let mut out = Vec::with_capacity(jpeg.len() + app1.len() + 4);
    out.extend_from_slice(&jpeg[..2]); // SOI
    out.extend_from_slice(&[0xFF, 0xE1]);
    out.extend_from_slice(&((app1.len() + 2) as u16).to_be_bytes());
    out.extend_from_slice(&app1);
    out.extend_from_slice(&jpeg[2..]);
    out
}

/// A genuine, minimal PSX TIM: the 8-byte header (magic `0x10`, mode 2 = 16-bit
/// direct colour, no CLUT) followed by one image block. Built in code rather than
/// committed as a binary so the assertion runs on every machine, and verified
/// against ImageMagick itself before it was written down: `magick identify` reports
/// `TIM 4x4` and every pixel decodes to pure red.
///
/// TIM is the sharp end of the name-selected-coder problem. It carries no signature
/// ImageMagick can sniff, so a nameless stream is undecodable — and unlike RLA/MDC
/// it also refuses a forced `tim:-` coder prefix ("insufficient image data"), which
/// is why [`decode_by_extension`] stages a real file instead.
fn synthetic_tim() -> Vec<u8> {
    const W: u16 = 4;
    const H: u16 = 4;
    const RED_BGR555: u16 = 0x001F;
    let mut px = Vec::new();
    for _ in 0..(W as usize * H as usize) {
        px.extend_from_slice(&RED_BGR555.to_le_bytes());
    }
    let mut out = Vec::new();
    out.extend_from_slice(&0x10u32.to_le_bytes()); // TIM magic
    out.extend_from_slice(&2u32.to_le_bytes()); // flags: 16-bit direct
    out.extend_from_slice(&(12 + px.len() as u32).to_le_bytes()); // block length
    out.extend_from_slice(&0u16.to_le_bytes()); // frame-buffer x
    out.extend_from_slice(&0u16.to_le_bytes()); // frame-buffer y
    out.extend_from_slice(&W.to_le_bytes());
    out.extend_from_slice(&H.to_le_bytes());
    out.extend_from_slice(&px);
    out
}

/// A positional-ramp EXR: red encodes the column, green the row.
pub(crate) fn ramp_exr_bytes(w: u32, h: u32) -> Vec<u8> {
    let buf =
        image::Rgba32FImage::from_fn(w, h, |x, y| image::Rgba([x as f32, y as f32, 0.25, 1.0]));
    let mut out = Vec::new();
    DynamicImage::ImageRgba32F(buf)
        .write_to(
            &mut std::io::Cursor::new(&mut out),
            image::ImageFormat::OpenExr,
        )
        .expect("write ramp exr");
    out
}

mod alpha;
mod colour;
mod decode_tiers;
mod embedded_previews;
mod named_coders;
mod policy;
mod preview_capping;
mod sweep;
mod vector_and_animation;
mod wic_routing;
