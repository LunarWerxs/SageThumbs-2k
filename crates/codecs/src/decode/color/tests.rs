#![cfg(test)]

use super::*;

/// A moderately-sized CMYK JPEG (well within MAX_DIM/MAX_PIXELS) must still be refused
/// once what the decode holds would exceed MAX_ALLOC (512 MiB) — the gap A025/A010 found
/// between the dimension caps and the actual allocation budget.
#[test]
fn cmyk_transient_budget_catches_what_dimension_caps_miss() {
    let coefficients = CMYK_COEFFICIENT_BYTES_PER_PIXEL;
    // 8000x8000 is far under MAX_DIM (16384) and MAX_PIXELS (16384^2), but a progressive
    // decode's 12 bytes/px is ~730 MiB there, comfortably past the 512 MiB MAX_ALLOC budget.
    assert!(cmyk_transient_bytes_exceed_budget(
        8000,
        8000,
        coefficients,
        MAX_ALLOC
    ));
    // A small, ordinary CMYK JPEG must sail through unaffected.
    assert!(!cmyk_transient_bytes_exceed_budget(
        800,
        600,
        coefficients,
        MAX_ALLOC
    ));
    // Right at the boundary: exactly MAX_ALLOC bytes is not "exceeds".
    let (w, h) = (200u32, 200u32);
    let budget = (w as u64) * (h as u64) * coefficients;
    assert!(!cmyk_transient_bytes_exceed_budget(
        w,
        h,
        coefficients,
        budget
    ));
    assert!(cmyk_transient_bytes_exceed_budget(
        w,
        h,
        coefficients,
        budget - 1
    ));
}

/// A one-scan baseline CMYK JPEG of 7000x10000 (an A1 poster at 300 dpi, 70 MP) holds 4-5
/// bytes a pixel, so it is within budget and keeps its profile; it used to be charged 13 and
/// fell to the image crate's naive CMYK. A progressive one of that size still is not.
#[test]
fn a_baseline_cmyk_poster_fits_the_budget_and_a_progressive_one_does_not() {
    let jpeg = |sof: u8, scans: usize| {
        let mut b = vec![0xFF, 0xD8, 0xFF, sof, 0x00, 0x14, 8];
        b.extend_from_slice(&[0x27, 0x10, 0x1B, 0x58, 4]); // 10000 high, 7000 wide, 4 components
        (1..=4u8).for_each(|id| b.extend_from_slice(&[id, 0x11, 0]));
        (0..scans).for_each(|_| b.extend_from_slice(&[0xFF, 0xDA, 0, 2, 1, 2, 3]));
        b
    };
    let fits = |b: &[u8]| cmyk_output_within_limits(7000, 10_000, cmyk_jpeg_bytes_per_pixel(b));
    assert!(fits(&jpeg(0xC0, 1)));
    assert!(!fits(&jpeg(0xC2, 1)), "progressive keeps coefficients");
    assert!(!fits(&jpeg(0xC0, 2)), "several scans keep coefficients");
}

/// The in-place band conversion must give exactly what one whole-image transform gives, over
/// a height that is not a whole number of bands: a band's RGB lands over inks already read.
#[test]
fn cmyk_bands_to_rgb_matches_one_whole_transform() {
    use moxcms::{ColorProfile, Layout, TransformOptions};
    let srgb = ColorProfile::new_srgb();
    let transform = srgb
        .create_transform_8bit(
            Layout::Rgba,
            &srgb,
            Layout::Rgb,
            TransformOptions::default(),
        )
        .unwrap();
    let (w, h) = (7usize, CMYK_BAND_ROWS * 2 + 5);
    let inks: Vec<u8> = (0..w * h * 4).map(|i| (i * 31 % 251) as u8).collect();
    let mut whole = vec![0u8; w * h * 3];
    transform.transform(&inks, &mut whole).unwrap();
    let mut banded = inks;
    cmyk_bands_to_rgb(&*transform, &mut banded, w, h).unwrap();
    assert_eq!(banded, whole);
}

/// Bogus/non-CMYK input must still decline cleanly through the normal early-outs — the
/// budget check sits between the dimension gate and the ICC-profile read, so it must
/// never itself be reachable with attacker data that isn't already a plausible CMYK JPEG.
#[test]
fn decode_cmyk_jpeg_declines_non_jpeg_bytes() {
    assert!(decode_cmyk_jpeg(b"not a jpeg at all").is_none());
    assert!(decode_cmyk_jpeg(&[]).is_none());
}

/// `tone_map_float` must not allocate a second `to_rgba32f()` copy for the two variants
/// every call site actually passes it (Rgb32F/Rgba32F) — pinned by checking both variants
/// still tone-map correctly after the rewrite, matching the pre-existing Rgba32F coverage
/// in `decode::tests::tone_map_rescues_all_zero_alpha_float`.
#[test]
fn tone_map_float_matches_concrete_variant_directly() {
    // Rgb32F has no alpha channel: output must be fully opaque, matching the old
    // to_rgba32f()-synthesized a=1.0 behaviour.
    let mut buf = image::Rgb32FImage::new(2, 2);
    for p in buf.pixels_mut() {
        *p = image::Rgb([0.5f32, 0.25, 1.5]);
    }
    let out = tone_map_float(&DynamicImage::ImageRgb32F(buf)).to_rgba8();
    assert!(out.pixels().all(|p| p.0[3] == 255), "Rgb32F must be opaque");
    assert!(out.pixels().all(|p| p.0[0] > 0), "RGB content must survive");

    // Rgba32F: partial alpha must still be preserved verbatim (not rescued to opaque).
    let mut buf = image::Rgba32FImage::new(2, 1);
    buf.put_pixel(0, 0, image::Rgba([1.0f32, 1.0, 1.0, 1.0]));
    buf.put_pixel(1, 0, image::Rgba([1.0f32, 1.0, 1.0, 0.0]));
    let out = tone_map_float(&DynamicImage::ImageRgba32F(buf)).to_rgba8();
    assert_eq!(out.get_pixel(0, 0).0[3], 255);
    assert_eq!(out.get_pixel(1, 0).0[3], 0, "partial alpha must survive");
}

/// Float pixels are tone-mapped against the image's own brightest sample (extended Reinhard):
/// an EXR that never goes past reference white shows as authored - white at 255, mid-grey
/// 0.5 at sRGB 188 - instead of dimmed to 73% (white at 188), which is how every such EXR
/// looked until 2026-09-29. A real HDR image keeps reference white where it always was.
#[test]
fn a_float_image_within_reference_white_shows_as_authored_and_real_hdr_keeps_its_headroom() {
    let mut sdr = image::Rgb32FImage::new(3, 1);
    sdr.put_pixel(0, 0, image::Rgb([1.0f32, 1.0, 1.0]));
    sdr.put_pixel(1, 0, image::Rgb([0.5f32, 0.5, 0.5]));
    sdr.put_pixel(2, 0, image::Rgb([0.0f32, 1.0, 0.0]));
    let out = tone_map_float(&DynamicImage::ImageRgb32F(sdr)).to_rgba8();
    assert_eq!(out.get_pixel(0, 0).0, [255, 255, 255, 255]);
    assert_eq!(out.get_pixel(1, 0).0, [188, 188, 188, 255]);
    assert_eq!(out.get_pixel(2, 0).0, [0, 255, 0, 255]);

    let mut hdr = image::Rgb32FImage::new(2, 1);
    hdr.put_pixel(0, 0, image::Rgb([1.0f32, 1.0, 1.0]));
    hdr.put_pixel(1, 0, image::Rgb([39.0f32, 39.0, 39.0]));
    let out = tone_map_float(&DynamicImage::ImageRgb32F(hdr)).to_rgba8();
    // 188: where plain Reinhard put reference white too (sRGB of 0.5).
    assert_eq!(
        out.get_pixel(0, 0).0[0],
        188,
        "reference white under real highlights"
    );
    assert_eq!(
        out.get_pixel(1, 0).0[0],
        255,
        "the brightest sample is white"
    );
}

/// A genuinely-sRGB embedded profile must short-circuit `apply_icc_to_srgb` to a pure
/// pass-through (bit-identical output, not merely close) — the whole point of the check
/// is to skip the moxcms transform entirely for the common case, not just make it cheap.
#[test]
fn srgb_profile_short_circuits_to_a_pass_through() {
    let icc = moxcms::ColorProfile::new_srgb()
        .encode()
        .expect("encode sRGB");
    let img = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(2, 2, image::Rgb([30, 150, 80])));
    let out = apply_icc_to_srgb(img.clone(), Some(icc));
    assert_eq!(
        out.to_rgb8(),
        img.to_rgb8(),
        "a real sRGB profile must pass through byte-for-byte, not merely close"
    );
}

/// A Display-P3 profile must NOT be mistaken for sRGB — same rough shape of transfer
/// curve, different primaries — or wide-gamut files would stop being colour-managed.
#[test]
fn display_p3_is_not_mistaken_for_srgb() {
    let icc = moxcms::ColorProfile::new_display_p3()
        .encode()
        .expect("encode P3");
    let src = moxcms::ColorProfile::new_from_slice(&icc).expect("parse P3");
    assert!(
        !icc_profile_is_srgb(&src),
        "Display-P3 primaries must not read as sRGB"
    );
}
