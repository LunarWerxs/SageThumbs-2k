//! Getting the colour right, which is where thumbnails visibly go wrong.
//! ICC profiles reassembled out of chunks, the colr box walked out of an
//! ISOBMFF container, and the high-depth curve that undoes what WIC does.

use super::*;

#[test]
fn icc_color_management_to_srgb() {
    use image::{DynamicImage, GenericImageView, Rgb, RgbImage};
    // No embedded profile → the image must come back byte-for-byte unchanged.
    let img = DynamicImage::ImageRgb8(RgbImage::from_pixel(2, 2, Rgb([30, 150, 80])));
    assert_eq!(
        apply_icc_to_srgb(img.clone(), None).to_rgb8(),
        img.to_rgb8(),
        "no profile must pass through untouched"
    );
    // A real Display-P3 profile (encoded via moxcms) must color-manage a saturated
    // color toward sRGB — values change, dimensions preserved, never blanked.
    let p3 = moxcms::ColorProfile::new_display_p3()
        .encode()
        .expect("encode P3");
    let managed = apply_icc_to_srgb(img.clone(), Some(p3));
    assert_eq!(managed.dimensions(), (2, 2));
    assert_ne!(
        managed.to_rgb8(),
        img.to_rgb8(),
        "a Display-P3 pixel must be transformed, not passed through"
    );
    // A CMYK-space profile must be left alone (we only handle RGB profiles).
    let cmyk_unhandled = apply_icc_to_srgb(img.clone(), Some(vec![0u8; 4])); // junk ICC
    assert_eq!(
        cmyk_unhandled.to_rgb8(),
        img.to_rgb8(),
        "bad ICC → unchanged"
    );
}

#[test]
fn colr_box_profile_extraction() {
    // Embedded ICC: `prof` / `rICC` colour types → the raw profile bytes.
    assert_eq!(
        colr_profile(&[&b"prof"[..], &[1, 2, 3, 4]].concat()),
        Some(vec![1, 2, 3, 4])
    );
    assert_eq!(
        colr_profile(&[&b"rICC"[..], &[9, 9]].concat()),
        Some(vec![9, 9])
    );
    // CICP nclx Display-P3 (primaries = 12, sRGB transfer = 13) → built-in profile.
    assert!(
        colr_profile(&[b'n', b'c', b'l', b'x', 0, 12, 0, 13, 0, 1, 0])
            .is_some_and(|v| !v.is_empty()),
        "nclx Display-P3 maps to a profile"
    );
    // P3 primaries alone are insufficient: a different transfer curve must never be
    // interpreted through the sRGB curve baked into the Display-P3 ICC profile.
    assert_eq!(
        colr_profile(&[b'n', b'c', b'l', b'x', 0, 12, 0, 1, 0, 1, 0]),
        None,
        "P3 primaries with BT.709 transfer are not Display P3"
    );
    assert_eq!(
        colr_profile(&[b'n', b'c', b'l', b'x', 0, 12, 0, 16, 0, 9, 0x80]),
        None,
        "P3 primaries with PQ transfer are not Display P3"
    );
    assert_eq!(
        colr_profile(&[b'n', b'c', b'l', b'x', 0, 12]),
        None,
        "truncated nclx is ignored"
    );
    // nclx BT.709/sRGB (primaries = 1) is a no-op; junk / empty → None.
    assert_eq!(
        colr_profile(&[b'n', b'c', b'l', b'x', 0, 1, 0, 13, 0, 1, 0]),
        None
    );
    assert_eq!(colr_profile(b"prof"), None, "empty profile");
    assert_eq!(colr_profile(b"xxxxyyyy"), None, "unknown colour_type");
}

#[test]
fn isobmff_colr_box_walk() {
    // Minimal AVIF-ish tree: ftyp + meta(FullBox){ iprp{ ipco{ colr(prof + ICC) }}}.
    fn bx(typ: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let size = (8 + body.len()) as u32;
        [&size.to_be_bytes()[..], &typ[..], body].concat()
    }
    let icc = vec![7u8; 32];
    let colr = bx(b"colr", &[&b"prof"[..], &icc].concat());
    let ipco = bx(b"ipco", &colr);
    let iprp = bx(b"iprp", &ipco);
    let meta = bx(b"meta", &[&[0u8; 4][..], &iprp].concat()); // meta FullBox: 4-byte ver/flags
    let file = [bx(b"ftyp", b"avif"), meta].concat();
    assert_eq!(
        isobmff_color_icc(&file),
        Some(icc),
        "ICC pulled from the nested colr box"
    );
    // A non-ISOBMFF buffer (no leading `ftyp`) is never walked.
    assert_eq!(isobmff_color_icc(&[0xFFu8; 64]), None);
}

#[test]
fn heic_auxiliary_alpha_box_walk() {
    fn bx(typ: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let size = u32::try_from(8 + body.len()).unwrap();
        [&size.to_be_bytes()[..], &typ[..], body].concat()
    }
    fn heic_with_auxc(aux_type: &[u8], associated_item: u16, auxl_target: Option<u16>) -> Vec<u8> {
        let mut auxc_body = vec![0u8; 4]; // FullBox version + flags
        auxc_body.extend_from_slice(aux_type);
        let auxc = bx(b"auxC", &auxc_body);
        // auxC is property #2. Item 2 is the auxiliary image, and item 1 is
        // the primary — the same topology as the pinned libheif HEIC fixture.
        let ipco = bx(b"ipco", &[bx(b"ispe", &[0u8; 12]), auxc].concat());
        let ipma = bx(
            b"ipma",
            &[
                &[0u8; 4][..], // FullBox version + flags
                &1u32.to_be_bytes(),
                &associated_item.to_be_bytes(),
                &[1, 0x82], // one essential association to property #2
            ]
            .concat(),
        );
        let iprp = bx(b"iprp", &[ipco, ipma].concat());
        let pitm = bx(b"pitm", &[&[0u8; 4][..], &1u16.to_be_bytes()].concat());
        let iref = auxl_target.map(|target| {
            let auxl = bx(
                b"auxl",
                &[
                    &2u16.to_be_bytes()[..],
                    &1u16.to_be_bytes(),
                    &target.to_be_bytes(),
                ]
                .concat(),
            );
            bx(b"iref", &[&[0u8; 4][..], &auxl].concat())
        });
        let mut meta_body = [&[0u8; 4][..], &pitm, &iprp].concat();
        if let Some(iref) = iref {
            meta_body.extend(iref);
        }
        let meta = bx(b"meta", &meta_body);
        [bx(b"ftyp", b"heic\0\0\0\0mif1"), meta].concat()
    }

    let alpha = heic_with_auxc(b"urn:mpeg:hevc:2015:auxid:1\0", 2, Some(1));
    assert!(
        isobmff_has_hevc_aux_alpha(&alpha),
        "an HEVC alpha auxC property associated with an auxl item is detected"
    );
    assert!(
        !isobmff_has_hevc_aux_alpha(&heic_with_auxc(b"urn:mpeg:hevc:2015:auxid:2\0", 2, Some(1))),
        "a non-alpha HEVC auxiliary type is ignored"
    );
    assert!(
        !isobmff_has_hevc_aux_alpha(&heic_with_auxc(b"urn:mpeg:hevc:2015:auxid:1", 2, Some(1))),
        "the aux type must be NUL-terminated"
    );
    assert!(
        !isobmff_has_hevc_aux_alpha(&heic_with_auxc(b"urn:mpeg:hevc:2015:auxid:1\0", 1, Some(1))),
        "an auxC property assigned to the wrong item cannot affect routing"
    );
    assert!(
        !isobmff_has_hevc_aux_alpha(&heic_with_auxc(b"urn:mpeg:hevc:2015:auxid:1\0", 2, None)),
        "an associated auxC without an auxl relationship cannot affect routing"
    );
    assert!(
        !isobmff_has_hevc_aux_alpha(&heic_with_auxc(b"urn:mpeg:hevc:2015:auxid:1\0", 2, Some(3))),
        "an auxl relationship to a non-primary item cannot affect routing"
    );

    let loose = [
        bx(b"ftyp", b"heic\0\0\0\0mif1"),
        bx(b"free", b"urn:mpeg:hevc:2015:auxid:1\0"),
    ]
    .concat();
    assert!(
        !isobmff_has_hevc_aux_alpha(&loose),
        "the identifier outside meta/iprp/ipco/auxC cannot affect routing"
    );

    let mut truncated = alpha;
    truncated.pop();
    assert!(
        !isobmff_has_hevc_aux_alpha(&truncated),
        "truncated declared boxes are rejected"
    );
}

/// Issue #9: which AVIFs must bypass WIC because its AV1 codec misreads their colour.
///
/// The expectations here are not a guess about the spec — each one is a case measured
/// against libavif AND ImageMagick, worst-channel error out of 255, by
/// `scripts/repro-avif-color.ps1`. WIC was correct in only ONE configuration, so this is a
/// whitelist: anything not proven good is routed to ImageMagick, and anything unparseable
/// is too, so a future WIC that behaves differently cannot silently reintroduce the shift.
#[test]
fn avif_colour_routing_matches_what_wic_actually_gets_wrong() {
    use crate::decode::color::{avif_wic_verdict, AvifWicVerdict};

    fn bx(typ: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let size = u32::try_from(8 + body.len()).unwrap();
        [&size.to_be_bytes()[..], &typ[..], body].concat()
    }

    /// `high_bitdepth` sets the av1C bit that marks 10/12-bit; `matrix` writes an nclx
    /// `colr` box with that CICP matrix coefficient (None writes no `colr` at all).
    fn avif(high_bitdepth: bool, matrix: Option<u16>) -> Vec<u8> {
        // av1C: marker+version, profile/level, then the flags byte whose bit 6 is
        // high_bitdepth. Trailing byte is the (unused here) config OBU space.
        let av1c = bx(
            b"av1C",
            &[0x81, 0x00, if high_bitdepth { 0x4c } else { 0x0c }, 0x00],
        );
        let mut props = vec![bx(b"ispe", &[0u8; 12]), av1c];
        if let Some(m) = matrix {
            let mut nclx = b"nclx".to_vec();
            nclx.extend_from_slice(&1u16.to_be_bytes()); // colour_primaries: BT.709
            nclx.extend_from_slice(&13u16.to_be_bytes()); // transfer: sRGB
            nclx.extend_from_slice(&m.to_be_bytes()); // matrix_coefficients
            nclx.push(0x80); // full_range_flag
            props.push(bx(b"colr", &nclx));
        }
        let iprp = bx(b"iprp", &bx(b"ipco", &props.concat()));
        let meta = bx(b"meta", &[&[0u8; 4][..], &iprp].concat());
        [bx(b"ftyp", b"avif\0\0\0\0mif1"), meta].concat()
    }

    // The ONE measured-correct case, and the only one that keeps the cheap WIC path:
    // ordinary 8-bit BT.709, which is what Chrome and Squoosh emit. Measured error 1-3.
    assert_eq!(
        avif_wic_verdict(&avif(false, Some(1))),
        AvifWicVerdict::Trusted,
        "8-bit BT.709 is measurably correct through WIC and must stay on the fast path"
    );
    // Identity leaves RGB alone, so there is no conversion for WIC to get wrong.
    assert_eq!(
        avif_wic_verdict(&avif(false, Some(0))),
        AvifWicVerdict::Trusted
    );

    // avifenc's DEFAULT matrix. Greys hold, saturated colour shifts. Measured error 19.
    assert_eq!(
        avif_wic_verdict(&avif(false, Some(6))),
        AvifWicVerdict::Untrusted,
        "8-bit BT.601 (avifenc's default) must route to ImageMagick: WIC clips while converting          with the wrong matrix, so the error is NOT recoverable after the fact"
    );
    // High bit depth WITH an nclx box is wrong for every matrix in the same way — a transfer
    // curve, not a matrix error (mid grey 128 reads as 138). That is invertible in-process, so
    // it must NOT cost a subprocess.
    for matrix in [0u16, 1, 6, 9] {
        assert_eq!(
            avif_wic_verdict(&avif(true, Some(matrix))),
            AvifWicVerdict::NeedsHighDepthCurve,
            "10/12-bit AVIF with colour signalling is curve-correctable, not magick-bound              (matrix {matrix})"
        );
    }
    // ...but high bit depth with NO colour box at all fails a DIFFERENT way (a full-vs-limited
    // range error, 0 -> 15 and 255 -> 233) which this curve does not fix. It stays on magick.
    assert_eq!(
        avif_wic_verdict(&avif(true, None)),
        AvifWicVerdict::Untrusted,
        "high-bit-depth AVIF with no nclx fails on RANGE, not transfer - the curve must not claim it"
    );
    // No colour signalling at all: WIC assumes BT.709 where libaom encoded BT.601.
    // Measured error 19 at 8-bit, so an absent nclx is NOT a licence to trust WIC.
    assert_eq!(
        avif_wic_verdict(&avif(false, None)),
        AvifWicVerdict::Untrusted,
        "an 8-bit AVIF with no nclx box must route to ImageMagick"
    );

    // HEIC carries hvcC, not av1C, and is routed by the auxiliary-alpha rule instead.
    // Give it an nclx with a matrix that WOULD trip the AVIF rule, to prove the av1C gate
    // is what decides rather than the colour box.
    let heic = {
        let mut nclx = b"nclx".to_vec();
        nclx.extend_from_slice(&1u16.to_be_bytes());
        nclx.extend_from_slice(&13u16.to_be_bytes());
        nclx.extend_from_slice(&6u16.to_be_bytes());
        nclx.push(0x80);
        let ipco = bx(
            b"ipco",
            &[bx(b"hvcC", &[0u8; 4]), bx(b"colr", &nclx)].concat(),
        );
        let meta = bx(b"meta", &[&[0u8; 4][..], &bx(b"iprp", &ipco)].concat());
        [bx(b"ftyp", b"heic\0\0\0\0mif1"), meta].concat()
    };
    assert_eq!(
        avif_wic_verdict(&heic),
        AvifWicVerdict::Trusted,
        "HEIC is not an AVIF and must not be routed by this rule"
    );
    // Not ISOBMFF at all, and a truncated container: decline rather than chew through it.
    assert_eq!(
        avif_wic_verdict(b"not an isobmff file at all"),
        AvifWicVerdict::Trusted
    );
    let mut truncated = avif(true, Some(6));
    truncated.truncate(12);
    assert_eq!(avif_wic_verdict(&truncated), AvifWicVerdict::Trusted);
}

/// The inverse of the transfer WIC applies to high-bit-depth AV1. Pinned against the MEASURED
/// curve, not against itself: the right-hand column is what Microsoft's AV1 codec 2.0.24.0
/// actually returned for a 17-step grey ramp encoded at 10-bit, so this test fails if the
/// correction stops undoing the thing it was built to undo.
#[test]
fn high_depth_curve_undoes_what_wic_measurably_does() {
    use crate::decode::color::undo_wic_high_depth_curve;
    use image::{DynamicImage, Rgba, RgbaImage};

    // (true value, what WIC handed back for it). Measured on a 10-bit AVIF grey ramp.
    const MEASURED: [(u8, u8); 17] = [
        (0, 0),
        (16, 29),
        (32, 46),
        (48, 62),
        (64, 77),
        (80, 93),
        (96, 108),
        (112, 123),
        (128, 138),
        (143, 153),
        (159, 167),
        (175, 182),
        (191, 197),
        (207, 211),
        (223, 225),
        (239, 240),
        (255, 254),
    ];

    let mut img = RgbaImage::new(MEASURED.len() as u32, 1);
    for (x, (_, wic)) in MEASURED.iter().enumerate() {
        // Alpha deliberately mid-range: the curve must leave it ALONE, or every semi-
        // transparent pixel silently changes opacity.
        img.put_pixel(x as u32, 0, Rgba([*wic, *wic, *wic, 128]));
    }
    let fixed = undo_wic_high_depth_curve(DynamicImage::ImageRgba8(img)).to_rgba8();

    let mut worst = 0i32;
    for (x, (truth, _)) in MEASURED.iter().enumerate() {
        let px = fixed.get_pixel(x as u32, 0).0;
        assert_eq!(
            px[3], 128,
            "alpha must pass through the colour curve untouched"
        );
        assert_eq!(
            px[0], px[1],
            "the curve must be per-channel identical on a grey"
        );
        worst = worst.max((i32::from(px[0]) - i32::from(*truth)).abs());
    }
    // Uncorrected, this ramp is off by up to 14. The analytic inverse tracks the measured
    // curve to within 2, so anything above that means the correction has drifted.
    assert!(
        worst <= 2,
        "high-bit-depth correction left a worst-channel error of {worst} (expected <= 2)"
    );
}

/// The curve must be monotonic and keep the endpoints, or it would crush highlights/shadows
/// and shift the black/white points of every corrected thumbnail.
#[test]
fn high_depth_curve_is_monotonic_and_keeps_endpoints() {
    use crate::decode::color::undo_wic_high_depth_curve;
    use image::{DynamicImage, Rgba, RgbaImage};

    let mut img = RgbaImage::new(256, 1);
    for v in 0u32..256 {
        let b = v as u8;
        img.put_pixel(v, 0, Rgba([b, b, b, 255]));
    }
    let out = undo_wic_high_depth_curve(DynamicImage::ImageRgba8(img)).to_rgba8();
    assert_eq!(out.get_pixel(0, 0).0[0], 0, "black must stay black");
    assert_eq!(out.get_pixel(255, 0).0[0], 255, "white must stay white");
    for v in 1u32..256 {
        assert!(
            out.get_pixel(v, 0).0[0] >= out.get_pixel(v - 1, 0).0[0],
            "curve must be monotonic; it is not at {v}"
        );
    }
}

/// The JPEG APP2 ICC reassembler. A real profile usually arrives in ONE chunk, so the corpus
/// fixture exercises only the easy path; the multi-chunk cases below are the ones that decide
/// whether a big profile comes back whole, in order, or not at all. Getting this wrong is not a
/// crash, it is a wrong-coloured thumbnail, which is the failure this whole area keeps having.
#[test]
fn jpeg_icc_reassembles_every_chunk_or_returns_nothing() {
    use crate::decode::color::jpeg_icc;

    /// A JPEG made only of the APP2 segments given, then SOS. `chunks` is (seq, total, body).
    fn jpeg_with_icc(chunks: &[(u8, u8, &[u8])]) -> Vec<u8> {
        let mut b = vec![0xFF, 0xD8];
        for (seq, total, body) in chunks {
            let len = 2 + 12 + 2 + body.len();
            b.extend_from_slice(&[0xFF, 0xE2]);
            b.extend_from_slice(&(len as u16).to_be_bytes());
            b.extend_from_slice(b"ICC_PROFILE\0");
            b.push(*seq);
            b.push(*total);
            b.extend_from_slice(body);
        }
        b.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x02]); // SOS, ends the marker walk
        b
    }

    // One chunk: the ordinary case, and the one the corpus fixture covers.
    assert_eq!(
        jpeg_icc(&jpeg_with_icc(&[(1, 1, b"profile-bytes")])).as_deref(),
        Some(&b"profile-bytes"[..])
    );

    // Several chunks are concatenated IN SEQUENCE ORDER, not in the order they appear. A
    // writer is entitled to emit them in any order and some do.
    assert_eq!(
        jpeg_icc(&jpeg_with_icc(&[
            (2, 3, b"-two"),
            (1, 3, b"one"),
            (3, 3, b"-three")
        ]))
        .as_deref(),
        Some(&b"one-two-three"[..])
    );

    // A MISSING chunk yields nothing at all. Returning the parts we happened to have would
    // hand moxcms a corrupt profile, and a corrupt profile is a wrong picture rather than a
    // skipped correction - which is exactly the failure mode to avoid. This is also what
    // protects the callers that pass a bounded head rather than the whole file.
    assert!(jpeg_icc(&jpeg_with_icc(&[(1, 3, b"one"), (2, 3, b"-two")])).is_none());

    // No APP2 at all, not a JPEG, and empty input: all None, never a panic.
    assert!(jpeg_icc(&jpeg_with_icc(&[])).is_none());
    assert!(jpeg_icc(b"\x89PNG\r\n\x1a\n").is_none());
    assert!(jpeg_icc(&[]).is_none());

    // Every truncation of a valid two-chunk file yields either NOTHING or the WHOLE profile,
    // never a partial one, and never a panic. (A cut that lands after the last APP2 but before
    // the scan legitimately still has every chunk, so "always None" would be the wrong
    // assertion - the invariant is all-or-nothing, not nothing.)
    let whole = jpeg_with_icc(&[(1, 2, b"first-half"), (2, 2, b"second-half")]);
    for cut in 0..whole.len() {
        match jpeg_icc(&whole[..cut]) {
            None => {}
            Some(got) => assert_eq!(
                got, b"first-halfsecond-half",
                "a file truncated to {cut} bytes returned a PARTIAL profile"
            ),
        }
    }
}

#[test]
fn detects_cmyk_jpeg_by_component_count() {
    // Minimal JPEG: SOI + SOF0 declaring `nf` components + EOI. CMYK/YCCK are 4-component.
    fn jpeg_with_components(nf: u8) -> Vec<u8> {
        let len = 8 + 3 * nf as usize; // SOF0 length field
        let mut b = vec![0xFF, 0xD8]; // SOI
        b.extend_from_slice(&[0xFF, 0xC0, (len >> 8) as u8, len as u8, 8, 0, 1, 0, 1, nf]);
        b.extend(std::iter::repeat_n(0u8, 3 * nf as usize)); // component specs
        b.extend_from_slice(&[0xFF, 0xD9]); // EOI
        b
    }
    assert!(
        is_cmyk_jpeg(&jpeg_with_components(4)),
        "4-component JPEG = CMYK/YCCK"
    );
    assert!(
        !is_cmyk_jpeg(&jpeg_with_components(3)),
        "3-component = YCbCr/RGB"
    );
    assert!(
        !is_cmyk_jpeg(&jpeg_with_components(1)),
        "1-component = grayscale"
    );
    assert!(
        !is_cmyk_jpeg(&[0x89, b'P', b'N', b'G', 0, 0, 0, 0]),
        "PNG is not a CMYK JPEG"
    );
    assert!(!is_cmyk_jpeg(&[]), "empty input");
}

#[test]
fn jxl_applies_its_embedded_color_profile() {
    // Issue #9: the jxl tier decoded correctly but never colour-managed, unlike the `image`
    // and WIC tiers. A wide-gamut jxl therefore reached Explorer with its raw AdobeRGB
    // numbers treated as sRGB, which is a visible shift on every saturated colour.
    let img =
        crate::decode::tiers::decode_jxl(JXL_ADOBERGB, None).expect("decode the AdobeRGB jxl");
    let rgb = img.to_rgb8();
    let px = rgb.get_pixel(16, 16).0;

    // The file's raw stored value. Seeing THIS is the bug: it means no profile was applied.
    assert_ne!(
        [px[0], px[1], px[2]],
        [180, 80, 80],
        "jxl decoded to its raw AdobeRGB numbers - the embedded profile was ignored"
    );
    // AdobeRGB(180,80,80) converted to sRGB. Cross-checked against djxl + LittleCMS, which
    // land on (206,79,79); allow a small delta for a different CMS's rounding.
    for (got, want) in px.iter().zip([206u8, 79, 79]) {
        assert!(
            (i32::from(*got) - i32::from(want)).abs() <= 4,
            "colour-managed jxl pixel {px:?} is not close to the expected [206,79,79]"
        );
    }
}
