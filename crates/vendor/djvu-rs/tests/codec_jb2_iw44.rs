//! Integration tests for `jb2` and `iw44` decoders.
//!
//! Single-page fixture files use the IFF parser to reach raw Sjbz/BG44 chunks
//! and feed them directly to the codec layer. Multi-page corpus files use the
//! high-level `Document` API (which internally drives the same codecs).

use djvu_rs::Document;
use djvu_rs::error::{Iw44Error, Jb2Error};
use djvu_rs::iff::parse_form;
use djvu_rs::iw44::Iw44Image;
use djvu_rs::jb2 as jb2_new;

// ── JB2 ───────────────────────────────────────────────────────────────────────

/// Decode the Sjbz chunk from boy_jb2.djvu — a small bilevel fixture page.
#[test]
fn jb2_decode_boy_bilevel() {
    let data = std::fs::read("tests/fixtures/boy_jb2.djvu").unwrap();
    let form = parse_form(&data).unwrap();
    assert_eq!(
        &form.form_type, b"DJVU",
        "boy_jb2.djvu must be a single-page DJVU"
    );

    let sjbz = form
        .chunks
        .iter()
        .find(|c| &c.id == b"Sjbz")
        .expect("must contain Sjbz chunk");

    let bitmap = jb2_new::decode(sjbz.data, None).expect("Sjbz must decode without error");
    assert!(
        bitmap.width > 0 && bitmap.height > 0,
        "decoded bitmap must have non-zero dimensions"
    );
}

/// Decoded JB2 bitmap dimensions must match the INFO chunk for boy_jb2.djvu.
#[test]
fn jb2_decode_dimensions_match_info() {
    let data = std::fs::read("tests/fixtures/boy_jb2.djvu").unwrap();
    let form = parse_form(&data).unwrap();

    let info = form.chunks.iter().find(|c| &c.id == b"INFO").unwrap();
    let w = u16::from_be_bytes([info.data[0], info.data[1]]) as u32;
    let h = u16::from_be_bytes([info.data[2], info.data[3]]) as u32;

    let sjbz = form.chunks.iter().find(|c| &c.id == b"Sjbz").unwrap();
    let bitmap = jb2_new::decode(sjbz.data, None).unwrap();

    assert_eq!(bitmap.width, w, "bitmap width must match INFO");
    assert_eq!(bitmap.height, h, "bitmap height must match INFO");
}

/// Render a bilevel corpus page — exercises JB2 decode end-to-end.
#[test]
fn jb2_decode_corpus_bilevel_via_render() {
    let doc = Document::open("tests/corpus/cable_1973_100133.djvu").unwrap();
    let page = doc.page(0).unwrap();
    let pixmap = page
        .render()
        .expect("bilevel page must render without error");
    assert!(pixmap.width > 0 && pixmap.height > 0);
    // A bilevel page renders to RGBA — verify data length
    assert_eq!(
        pixmap.data.len(),
        (pixmap.width * pixmap.height * 4) as usize
    );
}

/// Empty input returns Err, not a panic.
#[test]
fn jb2_decode_empty_returns_error() {
    let result = jb2_new::decode(&[], None);
    assert!(result.is_err(), "empty Sjbz data must return Err");
}

/// Garbage input returns Err, not a panic.
#[test]
fn jb2_decode_garbage_no_panic() {
    let _ = jb2_new::decode(b"not a JB2 stream!!", None);
}

/// Jb2Error implements std::error::Error.
#[test]
fn jb2_error_implements_error_trait() {
    fn requires_error<E: std::error::Error>() {}
    requires_error::<Jb2Error>();
}

// ── IW44 ──────────────────────────────────────────────────────────────────────

/// Decode the first BG44 chunk from boy.djvu (color IW44 fixture page).
#[test]
fn iw44_decode_first_chunk_boy() {
    let data = std::fs::read("tests/fixtures/boy.djvu").unwrap();
    let form = parse_form(&data).unwrap();
    assert_eq!(
        &form.form_type, b"DJVU",
        "boy.djvu must be a single-page DJVU"
    );

    let bg44 = form
        .chunks
        .iter()
        .find(|c| &c.id == b"BG44")
        .expect("boy.djvu must contain BG44");

    let mut img = Iw44Image::new();
    img.decode_chunk(bg44.data)
        .expect("first BG44 must decode without error");
    assert!(img.width > 0 && img.height > 0);
}

/// Decoding all BG44 chunks and calling to_rgb() must produce a valid RGBA pixmap.
#[test]
fn iw44_to_rgb_after_all_chunks_boy() {
    let data = std::fs::read("tests/fixtures/boy.djvu").unwrap();
    let form = parse_form(&data).unwrap();

    let mut img = Iw44Image::new();
    for chunk in form.chunks.iter().filter(|c| &c.id == b"BG44") {
        img.decode_chunk(chunk.data).unwrap();
    }

    let pixmap = img.to_rgb().expect("to_rgb must succeed after all chunks");
    assert!(pixmap.width > 0 && pixmap.height > 0);
    assert_eq!(
        pixmap.data.len(),
        (pixmap.width * pixmap.height * 4) as usize,
        "RGBA pixmap data size must equal width*height*4"
    );
}

/// to_rgb_subsample(2) produces approximately half-size output.
#[test]
fn iw44_to_rgb_subsample_halves_dimensions() {
    let data = std::fs::read("tests/fixtures/boy.djvu").unwrap();
    let form = parse_form(&data).unwrap();

    let mut img = Iw44Image::new();
    for chunk in form.chunks.iter().filter(|c| &c.id == b"BG44") {
        img.decode_chunk(chunk.data).unwrap();
    }

    let full = img.to_rgb().unwrap();
    let half = img.to_rgb_subsample(2).unwrap();

    assert!(half.width <= full.width / 2 + 1);
    assert!(half.height <= full.height / 2 + 1);
}

/// Calling to_rgb() on a freshly-constructed (empty) Iw44Image returns an error.
#[test]
fn iw44_to_rgb_before_any_chunk_returns_error() {
    let img = Iw44Image::new();
    let result = img.to_rgb();
    assert!(result.is_err(), "to_rgb on empty image must return Err");
}

/// decode_chunk with empty data returns Err, not a panic.
#[test]
fn iw44_decode_chunk_empty_returns_error() {
    let mut img = Iw44Image::new();
    let result = img.decode_chunk(&[]);
    assert!(result.is_err());
}

/// decode_chunk with garbage data returns Err, not a panic.
#[test]
fn iw44_decode_chunk_garbage_no_panic() {
    let mut img = Iw44Image::new();
    let _ = img.decode_chunk(b"garbage data!!!");
}

/// Render a color IW44 corpus page — exercises IW44 decode end-to-end.
#[test]
fn iw44_decode_corpus_color_via_render() {
    let doc = Document::open("tests/corpus/watchmaker.djvu").unwrap();
    let page = doc.page(0).unwrap();
    let pixmap = page.render().expect("color page must render without error");
    assert!(pixmap.width > 0 && pixmap.height > 0);
    assert_eq!(
        pixmap.data.len(),
        (pixmap.width * pixmap.height * 4) as usize
    );
}

/// Iw44Error implements std::error::Error.
#[test]
fn iw44_error_implements_error_trait() {
    fn requires_error<E: std::error::Error>() {}
    requires_error::<Iw44Error>();
}
