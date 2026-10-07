#![cfg(test)]

//! HDR (PQ) samples render like their SDR twins, twin by twin.

use super::*;

/// How far the PQ twin's ramp may end from its SDR twin's, in 8-bit levels. Measured
/// 2026-09-29 across all eight twin decodes: 5 at most (the scaled AVIF thumbnail).
const RAMP_END_SLACK: u8 = 8;
/// How far the PQ twin's ramp mean may sit from its SDR twin's, as a fraction of it. Measured
/// 1.5% at most; #38's dark render sat 16% or more below, #39's clip about 9% above.
const RAMP_MEAN_SLACK: f64 = 0.05;

/// Issue #38: a JPEG XL whose base image is HDR (PQ transfer, BT.2020 primaries) thumbnailed
/// almost black - grey ramp peak 39 of 255 on 3.0.0 - while its SDR twin was fine. The 16-bit
/// integer samples still carried the PQ curve and were colour-managed as if 10000 nits were
/// white. The tier now routes an HDR file through the PNG `cICP` conversion and the shared
/// float tone map, so it renders like its SDR twin (see [`assert_hdr_twin_matches_its_sdr_twin`]).
#[test]
fn hdr_pq_jxl_renders_as_bright_as_its_sdr_twin() {
    let jxl = |bytes: &[u8]| {
        crate::decode::tiers::decode_jxl(bytes, None, false).expect("decode the jxl twin")
    };
    assert_hdr_twin_matches_its_sdr_twin("jxl tier", &jxl(JXL_PQ2020), &jxl(JXL_SDR709));
}

/// Issue #39, the signal half: an AVIF whose `nclx` names a PQ transfer is an HDR picture
/// whatever its depth or matrix says, and hands the ImageMagick tier the cICP it needs to
/// convert magick's raw signal (the fallback for an AVIF our own decoder cannot read, and every
/// HDR HEIC). Its SDR twin - the same scene, sRGB / BT.709 - hands over nothing.
#[test]
fn hdr_pq_avif_signals_hdr_and_its_sdr_twin_does_not() {
    use crate::decode::avif::isobmff_hdr_cicp;
    let pq = isobmff_hdr_cicp(AVIF_PQ2020).expect("the PQ twin signals an HDR transfer");
    assert_eq!((pq.primaries, pq.transfer, pq.full_range), (9, 16, true));
    assert_eq!(isobmff_hdr_cicp(AVIF_SDR709), None);
    assert_eq!(
        isobmff_hdr_cicp(JXL_PQ2020),
        None,
        "not ISOBMFF, so not this rule's"
    );
}

/// Left edge, right edge and mean of the grey ramp along the top half of a twin-scene render,
/// whatever size it was rendered at.
pub(super) fn scene_ramp(img: &image::DynamicImage) -> (u8, u8, f64) {
    let rgb = img.to_rgb8();
    let (w, h) = rgb.dimensions();
    let y = h / 8;
    let left = rgb.get_pixel(1, y).0[1];
    let right = rgb.get_pixel(w - 2, y).0[1];
    let mean = (0..w)
        .map(|x| f64::from(rgb.get_pixel(x, y).0[1]))
        .sum::<f64>()
        / f64::from(w);
    (left, right, mean)
}

/// The assertion every HDR twin pair makes: the PQ (or scRGB) twin renders like its SDR twin.
/// The twins are ONE scene, and it never goes brighter than reference white, so since the
/// tone map became extended Reinhard (2026-09-29: the brightest sample is white, and a picture
/// with no highlights shows as authored) the PQ ramp ends where the SDR one does. Both shipped
/// bugs still fail it: #38 rendered the ramp dark (39 at white, 148 through magick's raw
/// signal), #39 clipped everything over 80 nits so the ramp read 255 from its midpoint up,
/// which lifts the mean far past the SDR twin's.
pub(super) fn assert_hdr_twin_matches_its_sdr_twin(
    label: &str,
    pq: &image::DynamicImage,
    sdr: &image::DynamicImage,
) {
    let (pq_left, pq_right, pq_mean) = scene_ramp(pq);
    let (sdr_left, sdr_right, sdr_mean) = scene_ramp(sdr);
    assert!(
        sdr_right >= 250,
        "{label}: the SDR control's ramp should end near white, got {sdr_right}"
    );
    assert!(
        pq_right.abs_diff(sdr_right) <= RAMP_END_SLACK,
        "{label}: PQ ramp ends at {pq_right}, its SDR twin at {sdr_right}"
    );
    assert!(
        (pq_mean - sdr_mean).abs() <= RAMP_MEAN_SLACK * sdr_mean,
        "{label}: PQ ramp mean {pq_mean:.1} vs SDR {sdr_mean:.1}: rendered dark, or clipped"
    );
    assert!(
        pq_left <= 24 && sdr_left <= 24,
        "{label}: both ramps start near black ({pq_left}, {sdr_left})"
    );
}

/// Issue #39, the pixels. A PQ / BT.2020 AVIF thumbnailed as a bleached picture: Windows'
/// codec hands an HDR frame back as linear scRGB floats and the 8-bit conversion clipped
/// everything over 80 nits to white, so the grey ramp read 255 from its midpoint up. Where
/// ImageMagick decoded it instead it came out dark and flat (the raw PQ signal shown as sRGB,
/// 148 at white). Both paths now land where every HDR source does. Skips, saying so, on a
/// machine with no AVIF decoder at all (CI has neither the AV1 extension nor a magick with
/// libheif); the routing half above runs everywhere.
#[test]
fn hdr_pq_avif_renders_as_bright_as_its_sdr_twin() {
    use crate::decode::{decode_full, decode_preview_capped};
    com_init();
    let Ok(sdr) = decode_full(AVIF_SDR709) else {
        eprintln!("no AVIF decoder on this machine - skipping the render half of #39");
        return;
    };
    let pq = decode_full(AVIF_PQ2020).expect("the SDR twin decoded, so the PQ twin must too");
    assert_hdr_twin_matches_its_sdr_twin("full decode", &pq, &sdr);
    // The thumbnail path scales inside the codec (Fant hands back PREMULTIPLIED float), and it
    // is the path this bug actually shipped on.
    let sdr = decode_preview_capped(AVIF_SDR709, 160).expect("scaled SDR twin");
    let pq = decode_preview_capped(AVIF_PQ2020, 160).expect("scaled PQ twin");
    assert_hdr_twin_matches_its_sdr_twin("thumbnail decode", &pq, &sdr);
}

/// The ImageMagick half of #39 on its own: magick decodes a PQ AVIF to its raw signal (white at
/// 58% of full scale), and the tiers' finishing step must convert it. Runs wherever a magick
/// with AVIF support is reachable, and says so where one is not.
#[test]
fn hdr_pq_avif_through_magick_lands_at_reference_white() {
    use crate::decode::magick::{decode_via_magick_capped, magick_available, Fidelity};
    if !magick_available() {
        eprintln!("no ImageMagick here - skipping the magick half of #39");
        return;
    }
    let (Ok(pq_raw), Ok(sdr_raw)) = (
        decode_via_magick_capped(AVIF_PQ2020, None, Fidelity::Tile),
        decode_via_magick_capped(AVIF_SDR709, None, Fidelity::Tile),
    ) else {
        eprintln!("this ImageMagick cannot decode AVIF - skipping the magick half of #39");
        return;
    };
    let (_, raw_right, _) = scene_ramp(&pq_raw);
    assert!(
        (140..=156).contains(&raw_right),
        "magick's raw PQ ramp ends at {raw_right}; expected the unconverted signal, ~148"
    );
    let pq = crate::decode::finish_magick_output(pq_raw, AVIF_PQ2020, true);
    let sdr = crate::decode::finish_magick_output(sdr_raw, AVIF_SDR709, true);
    assert_hdr_twin_matches_its_sdr_twin("magick decode", &pq, &sdr);
}

/// The transfer-function axis, as a gate: every container that can carry an HDR picture with
/// integer samples has a PQ (or scRGB) twin and an SDR twin of ONE scene under test, and one
/// assertion shape covers them all. This is the test class 3.0 was missing (Michael,
/// 2026-09-10): every other gate enumerated FORMATS, none enumerated the transfer, so a PQ
/// JPEG XL was never rendered by anything before a user did it (#38), and then a PQ AVIF
/// (#39). A format that cannot be decoded on this machine is skipped BY NAME, never silently.
#[test]
fn every_hdr_capable_format_has_a_twin_pair_under_test() {
    use crate::decode::decode_full;
    com_init();
    let pairs: [(&str, &[u8], &[u8]); 5] = [
        ("jxl", JXL_PQ2020, JXL_SDR709),
        ("avif", AVIF_PQ2020, AVIF_SDR709),
        ("heic", HEIC_PQ2020, HEIC_SDR709),
        ("jxr", JXR_SCRGB, JXR_SDR709),
        ("tif", TIFF_PQ2020, TIFF_SDR709),
    ];
    for (format, hdr, sdr) in pairs {
        assert!(
            hdr.len() > 100 && sdr.len() > 100,
            "{format}: a twin fixture is missing or empty"
        );
        let Ok(sdr) = decode_full(sdr) else {
            eprintln!("{format}: no decoder for it on this machine - twin pair skipped");
            continue;
        };
        let hdr = decode_full(hdr).unwrap_or_else(|e| {
            panic!("{format}: the SDR twin decoded but the HDR twin did not: {e}")
        });
        assert_hdr_twin_matches_its_sdr_twin(format, &hdr, &sdr);
    }
}

/// A 16-bit TIFF wearing a BT.2020 PQ ICC profile is the same picture as the PQ JPEG XL: integer
/// samples still on the PQ curve, described by a profile. Colour-managing it through the
/// profile treats 10 000 nits as white (near black); `icc_hdr_cicp` reads the profile's own
/// `cicp` tag and routes it through the cICP conversion instead. Pure Rust, so it runs on CI.
#[test]
fn hdr_pq_tiff_with_an_icc_profile_is_tone_mapped_not_colour_managed() {
    use crate::decode::decode_full;
    // The profile itself is recognised as PQ / BT.2020 both by its tag and by its curve.
    let icc = include_bytes!("../../../../../../tests/fixtures/tiff/bt2020-pq.icc");
    let profile = moxcms::ColorProfile::new_from_slice(icc).expect("a real ICC profile");
    let cicp = icc_hdr_cicp(&profile).expect("the PQ profile is recognised as HDR");
    assert_eq!((cicp.primaries, cicp.transfer), (9, 16));
    let mut untagged = profile.clone();
    untagged.cicp = None;
    let by_curve = icc_hdr_cicp(&untagged).expect("the PQ curve is recognised without the tag");
    assert_eq!((by_curve.primaries, by_curve.transfer), (9, 16));
    assert!(
        icc_hdr_cicp(&moxcms::ColorProfile::new_srgb()).is_none(),
        "sRGB is not HDR"
    );
    assert!(
        icc_hdr_cicp(&moxcms::ColorProfile::new_display_p3()).is_none(),
        "Display P3 is not HDR"
    );
    // The container read finds the profile the `image` crate's TIFF decoder does not.
    assert_eq!(
        crate::decode::color::tiff_icc(TIFF_PQ2020).as_deref(),
        Some(&icc[..]),
        "tiff_icc must return IFD0's tag 34675 byte for byte"
    );
    assert_eq!(
        crate::decode::color::tiff_icc(TIFF_SDR709),
        None,
        "the SDR twin carries none"
    );
    assert_eq!(
        crate::decode::color::tiff_icc(JXL_PQ2020),
        None,
        "not a TIFF"
    );
    // The image tier must hand the profile up with the pixels: without it, the PQ samples
    // reach the tone map as if they were sRGB and the ramp reads 148 at white.
    let (raw, embedded) =
        crate::decode::decode_with_image_alloc_raw(TIFF_PQ2020, crate::decode::MAX_ALLOC)
            .expect("the image tier decodes the PQ TIFF");
    assert!(
        embedded.as_ref().is_some_and(|p| p.len() == icc.len()),
        "the image tier dropped the TIFF's ICC profile (got {:?} bytes, decoded as {:?})",
        embedded.as_ref().map(Vec::len),
        raw.color()
    );
    // And the picture: the SDR twin unchanged, the PQ twin at reference white.
    let sdr = decode_full(TIFF_SDR709).expect("the SDR TIFF twin decodes anywhere");
    let pq = decode_full(TIFF_PQ2020).expect("the PQ TIFF twin decodes anywhere");
    assert_hdr_twin_matches_its_sdr_twin("tiff + PQ icc", &pq, &sdr);
}

/// A float picture is linear light, never on the PQ curve: with a PQ profile it comes back from
/// the profile step as it went in, for its caller's one tone map. Until 2026-10-07 the profile
/// step tone-mapped it and the caller tone-mapped that 8-bit result again.
#[test]
fn a_float_picture_with_an_hdr_profile_is_tone_mapped_once() {
    let icc = include_bytes!("../../../../../../tests/fixtures/tiff/bt2020-pq.icc");
    let img = image::DynamicImage::ImageRgb32F(image::Rgb32FImage::from_pixel(
        4,
        4,
        image::Rgb([0.25, 0.5, 1.0]),
    ));
    let out = crate::decode::color::apply_icc_to_srgb(img.clone(), Some(icc.to_vec()));
    assert_eq!(out, img, "the float samples, untouched");
}
