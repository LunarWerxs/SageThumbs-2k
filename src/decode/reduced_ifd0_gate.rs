//! The picture-quality decision in [`super::reduced_ifd0_serves`], pinned.
//!
//! This repo has been bitten by a threshold before, so the tests below assert BOTH sides of
//! it and the corpus ones name the exact files the numbers came from. A change that makes
//! the black Kodak tile ship again fails here, loudly, by name.

use super::{reduced_ifd0_serves, DynamicImage};
use image::{Rgb, RgbImage};

/// A flat rectangle: exactly what a placeholder IFD0 is.
fn flat(w: u32, h: u32) -> DynamicImage {
    DynamicImage::ImageRgb8(RgbImage::from_pixel(w, h, Rgb([18, 18, 18])))
}

/// Something with real detail in it, at a comparable size.
fn detailed(w: u32, h: u32) -> DynamicImage {
    let mut img = RgbImage::new(w, h);
    for (x, y, p) in img.enumerate_pixels_mut() {
        let v = if (x / 7 + y / 5) % 2 == 0 { 15u8 } else { 240 };
        *p = Rgb([v, v.wrapping_add(x as u8), v]);
    }
    DynamicImage::ImageRgb8(img)
}

/// The bug this guards. A blank IFD0 that is BIGGER than the tile must still be refused;
/// accepting it is the black square a Kodak `.dcr` used to thumbnail as.
#[test]
fn a_blank_placeholder_is_refused_however_big_it_is() {
    for cx in [96u32, 256, 768] {
        assert!(
            !reduced_ifd0_serves(&flat(380, 252), Some(cx)),
            "a flat 380x252 placeholder must never answer a {cx} px tile"
        );
    }
    assert!(!reduced_ifd0_serves(&flat(4000, 3000), Some(256)));
}

#[test]
fn a_real_preview_answers_a_tile_it_covers() {
    assert!(reduced_ifd0_serves(&detailed(320, 240), Some(96)));
    assert!(reduced_ifd0_serves(&detailed(320, 240), Some(256)));
}

/// Never enlarge. Serving a 320 px preview into a 768 px tile is the 2.3.1 bug.
#[test]
fn a_preview_smaller_than_the_tile_is_refused() {
    assert!(!reduced_ifd0_serves(&detailed(320, 240), Some(768)));
    assert!(!reduced_ifd0_serves(&detailed(320, 240), Some(321)));
    assert!(
        reduced_ifd0_serves(&detailed(320, 240), Some(320)),
        "exactly covering the tile is covered, not short"
    );
}

/// Convert, Resize and Image-info pass `None` and must always get the real decode.
#[test]
fn a_full_fidelity_caller_is_never_served_a_preview() {
    assert!(!reduced_ifd0_serves(&detailed(4000, 3000), None));
}

/// The real files, by name. Skipped when the corpus is absent (CI never checks it out).
#[test]
fn the_corpus_raws_land_on_the_side_the_measurement_says() {
    let corpus = crate::testcorpus::dir();
    // (file, tile, must this be served from IFD0?)
    let cases = [
        // The placeholder. Bigger than both small tiles and still worthless.
        ("sample.dcr", 96u32, false),
        ("sample.dcr", 256, false),
        // Hasselblad: 1217x913 of real preview covers every size Explorer asks for.
        ("sample.fff", 96, true),
        ("sample.fff", 256, true),
        ("sample.fff", 768, true),
        // Hasselblad 3FR: 320x240 covers the two small views, not the large one.
        ("sample.3fr", 96, true),
        ("sample.3fr", 256, true),
        ("sample.3fr", 768, false),
    ];
    for (name, cx, want) in cases {
        let Ok(bytes) = std::fs::read(corpus.join(name)) else {
            continue; // no corpus here
        };
        assert!(
            crate::streamsrc::tiff_ifd0_is_reduced(&bytes),
            "{name} no longer reports a reduced-resolution IFD0; this test is now blind"
        );
        let img = super::decode_with_image(&bytes)
            .unwrap_or_else(|e| panic!("{name} IFD0 did not decode: {e}"));
        let got = reduced_ifd0_serves(&img, Some(cx));
        assert_eq!(
            got,
            want,
            "{name} at {cx} px: served-from-IFD0 was {got}, expected {want} (luma sd {:.2}, {}x{})",
            super::luma_sd(&img),
            image::GenericImageView::width(&img),
            image::GenericImageView::height(&img)
        );
    }
}
