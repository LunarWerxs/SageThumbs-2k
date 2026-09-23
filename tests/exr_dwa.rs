//! Downstream contract for the vendored `exr` DWA implementation.
//!
//! The focused tests in `vendor/exr` prove the codec itself. These two cases
//! additionally drive the exact public SageThumbs decode path used by Explorer,
//! so a future feature-resolution or image-crate integration change cannot leave
//! DWAA/DWAB green upstream but broken in the application.

use std::io::Cursor;

use exr::prelude::{pixel_vec::PixelVec, *};
use st2k_codecs::decode;

fn dwa_image(compression: Compression) -> Vec<u8> {
    let pixels = vec![
        (
            f16::from_f32(0.10),
            f16::from_f32(0.20),
            f16::from_f32(0.30),
        ),
        (
            f16::from_f32(0.80),
            f16::from_f32(0.25),
            f16::from_f32(0.05),
        ),
        (
            f16::from_f32(0.05),
            f16::from_f32(0.70),
            f16::from_f32(0.40),
        ),
        (
            f16::from_f32(0.35),
            f16::from_f32(0.10),
            f16::from_f32(0.90),
        ),
    ];
    let image = Image::from_encoded_channels(
        (2, 2),
        Encoding {
            compression,
            ..Encoding::default()
        },
        SpecificChannels::rgb(PixelVec::new(Vec2(2, 2), pixels)),
    );

    let mut bytes = Vec::new();
    image
        .write()
        .non_parallel()
        .to_buffered(Cursor::new(&mut bytes))
        .expect("encode tiny DWA fixture");
    bytes
}

fn assert_app_decodes(compression: Compression) {
    let bytes = dwa_image(compression);
    let decoded = decode::decode_full(&bytes).expect("SageThumbs must decode DWA EXR");

    assert_eq!((decoded.width(), decoded.height()), (2, 2));
    assert!(
        decoded.to_rgb8().pixels().any(|pixel| pixel.0 != [0, 0, 0]),
        "decoded DWA fixture must contain visible pixels"
    );
}

#[test]
fn app_decode_accepts_dwaa() {
    assert_app_decodes(Compression::DWAA(Some(45.0)));
}

#[test]
fn app_decode_accepts_dwab() {
    assert_app_decodes(Compression::DWAB(Some(45.0)));
}
