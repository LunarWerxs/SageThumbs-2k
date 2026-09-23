#![cfg(test)]

use super::*;

/// A moderately-sized CMYK JPEG (well within MAX_DIM/MAX_PIXELS) must still be refused
/// once its transient CMYK/Cmyka/RGBA buffers would exceed MAX_ALLOC (512 MiB) — the gap
/// A025/A010 found between the dimension caps and the actual allocation budget.
#[test]
fn cmyk_transient_budget_catches_what_dimension_caps_miss() {
    // 8000x8000 is far under MAX_DIM (16384) and MAX_PIXELS (16384^2), but at 13 transient
    // bytes/px that's ~830 MiB, comfortably past the 512 MiB MAX_ALLOC budget.
    assert!(cmyk_transient_bytes_exceed_budget(8000, 8000, MAX_ALLOC));
    // A small, ordinary CMYK JPEG must sail through unaffected.
    assert!(!cmyk_transient_bytes_exceed_budget(800, 600, MAX_ALLOC));
    // Right at the boundary: exactly MAX_ALLOC bytes is not "exceeds".
    let (w, h) = (200u32, 200u32);
    let budget = (w as u64) * (h as u64) * 13;
    assert!(!cmyk_transient_bytes_exceed_budget(w, h, budget));
    assert!(cmyk_transient_bytes_exceed_budget(w, h, budget - 1));
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
