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

/// Issue #9: an AVIF's colour signalling must reach the PROBE that covers it.
///
/// This test used to assert the verdicts themselves, from a table measured by hand against AV1
/// Video Extension 2.0.24.0. That component updates itself, 2.0.30.0 changed two of the rows,
/// and because the test pinned the old answers it went on passing while the shipped behaviour
/// was wrong — a green suite describing a codec that no longer existed. The verdicts now come
/// from `decode/wicprobe.rs` measuring the codec that is actually installed, and what is left
/// to test here is ours: that each shape is classified into the right probe, that an
/// unmeasured shape borrows nobody's answer, and that a non-AVIF is not routed by this rule
/// at all. Every assertion below runs on a machine with no AV1 codec whatsoever.
#[test]
fn avif_colour_routing_matches_what_wic_actually_gets_wrong() {
    use crate::decode::color::{avif_wic_class_of, avif_wic_verdict, AvifWicVerdict};
    use crate::decode::wicprobe::WicClass;

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

    /// A 10-bit MONOCHROME AVIF: av1C bit 6 (high_bitdepth) and bit 4 (monochrome), with an
    /// nclx box present so the test proves the monochrome flag wins over the matrix rather
    /// than merely being reached when no matrix is written.
    fn avif_mono() -> Vec<u8> {
        let av1c = bx(b"av1C", &[0x81, 0x00, 0x5c, 0x00]);
        let mut nclx = b"nclx".to_vec();
        nclx.extend_from_slice(&1u16.to_be_bytes());
        nclx.extend_from_slice(&13u16.to_be_bytes());
        nclx.extend_from_slice(&1u16.to_be_bytes());
        nclx.push(0x80);
        let props = [bx(b"ispe", &[0u8; 12]), av1c, bx(b"colr", &nclx)].concat();
        let iprp = bx(b"iprp", &bx(b"ipco", &props));
        let meta = bx(b"meta", &[&[0u8; 4][..], &iprp].concat());
        [bx(b"ftyp", b"avif\0\0\0\0mif1"), meta].concat()
    }

    // Each shape must land in the class whose PROBE covers it. The verdict itself belongs to
    // whatever AV1 codec is installed on the machine running this (see `decode/wicprobe.rs`)
    // and is deliberately not asserted here — pinning it is what let a five-week-old
    // measurement of a self-updating Store component keep deciding, long after it stopped
    // being true.
    for (matrix, want) in [
        // 0 is the identity matrix (lossless RGB), 1 is BT.709.
        (Some(0), WicClass::EightBt709),
        (Some(1), WicClass::EightBt709),
        // 5 and 6 are the two spellings of BT.601; 6 is what `avifenc` writes by default.
        (Some(5), WicClass::EightBt601),
        (Some(6), WicClass::EightBt601),
        // No `colr` box: the decoder is guessing, and which way it guesses has changed.
        (None, WicClass::EightNoColr),
    ] {
        assert_eq!(
            avif_wic_class_of(&avif(false, matrix)),
            Some(want),
            "8-bit matrix {matrix:?} must be probed as {want:?}"
        );
    }
    for (matrix, want) in [
        (Some(0), WicClass::HighBt709),
        (Some(1), WicClass::HighBt709),
        (Some(5), WicClass::HighBt601),
        (Some(6), WicClass::HighBt601),
    ] {
        assert_eq!(
            avif_wic_class_of(&avif(true, matrix)),
            Some(want),
            "10/12-bit matrix {matrix:?} must be probed as {want:?}"
        );
    }
    // Monochrome has no chroma planes and so no matrix to misread; it gets its own probe, and
    // must NOT inherit a colour class. Applying the colour classes' transfer correction to it
    // took a correct decode 15/255 away from right (2026-09-08).
    assert_eq!(
        avif_wic_class_of(&avif_mono()),
        Some(WicClass::HighMono),
        "high-bit-depth monochrome AVIF has its own probe"
    );
    // BT.2020 (9), "unspecified" (2), or anything from a newer spec than this build knows:
    // no probe covers it, so there is no measurement to route on.
    for matrix in [2u16, 9, 14, 4095] {
        assert_eq!(
            avif_wic_class_of(&avif(true, Some(matrix))),
            None,
            "matrix {matrix} is unmeasured and must not borrow another class's verdict"
        );
        assert_eq!(
            avif_wic_verdict(&avif(true, Some(matrix))),
            AvifWicVerdict::Untrusted,
            "an unmeasured shape routes to ImageMagick rather than guessing"
        );
    }
    // High bit depth with no `colr` box at all used to be "no probe, ask ImageMagick" on the
    // strength of one measurement (a full-vs-limited RANGE error). It has its own probe now,
    // so it is classed and MEASURED like the rest; the verdict is the codec's to give.
    assert_eq!(
        avif_wic_class_of(&avif(true, None)),
        Some(WicClass::HighNoColr),
        "high-bit-depth AVIF with no colour box has its own probe"
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

/// Issue #38: a JPEG XL whose base image is HDR (PQ transfer, BT.2020 primaries) thumbnailed
/// almost black - grey ramp peak 39 of 255 on 3.0.0 - while its SDR twin was fine. The 16-bit
/// integer samples still carried the PQ curve and were colour-managed as if 10000 nits were
/// white. The tier now routes an HDR file through the PNG `cICP` conversion and the shared
/// float tone map, so it lands where an EXR or an HDR PNG does: reference white at Reinhard's
/// 1.0, which is 187 of 255 in sRGB, not 255. That is the house convention for every HDR
/// source, and the SDR twin is the untouched control at 255.
#[test]
fn hdr_pq_jxl_renders_as_bright_as_its_sdr_twin() {
    fn ramp(bytes: &[u8]) -> (u8, u8, f64) {
        let img = crate::decode::tiers::decode_jxl(bytes, None).expect("decode the jxl twin");
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
    let (pq_left, pq_right, pq_mean) = ramp(JXL_PQ2020);
    let (sdr_left, sdr_right, sdr_mean) = ramp(JXL_SDR709);
    assert!(
        sdr_right >= 250,
        "the SDR control's ramp should end near white, got {sdr_right}"
    );
    // 187 = sRGB(Reinhard(1.0)); the bug rendered this at 39.
    assert!(
        (180..=200).contains(&pq_right),
        "PQ ramp ends at {pq_right} (SDR twin {sdr_right}): expected reference white at ~187"
    );
    assert!(
        pq_mean >= 0.7 * sdr_mean,
        "PQ ramp mean {pq_mean:.1} vs SDR {sdr_mean:.1}: the HDR jxl is still rendered dark"
    );
    assert!(
        pq_left <= 24 && sdr_left <= 24,
        "both ramps start near black ({pq_left}, {sdr_left})"
    );
}

/// Issue #39, the routing half, which needs no AV1 codec: an AVIF whose `nclx` names a PQ
/// transfer is an HDR picture whatever its depth or matrix says, gets the HDR probe class
/// rather than "unmeasured, ask ImageMagick", and hands the magick tiers the cICP they need to
/// convert magick's raw signal. Its SDR twin - the same scene, sRGB / BT.709 - is none of those.
#[test]
fn hdr_pq_avif_is_routed_as_hdr_and_its_sdr_twin_is_not() {
    use crate::decode::color::{avif_wic_class_of, isobmff_hdr_cicp};
    use crate::decode::wicprobe::WicClass;
    let pq = isobmff_hdr_cicp(AVIF_PQ2020).expect("the PQ twin signals an HDR transfer");
    assert_eq!((pq.primaries, pq.transfer, pq.full_range), (9, 16, true));
    assert_eq!(avif_wic_class_of(AVIF_PQ2020), Some(WicClass::HighHdr));
    assert_eq!(isobmff_hdr_cicp(AVIF_SDR709), None);
    assert_eq!(avif_wic_class_of(AVIF_SDR709), Some(WicClass::HighBt709));
    assert_eq!(
        isobmff_hdr_cicp(JXL_PQ2020),
        None,
        "not ISOBMFF, so not this rule's"
    );
}

/// Left edge, right edge and mean of the grey ramp along the top half of a twin-scene render,
/// whatever size it was rendered at.
fn scene_ramp(img: &image::DynamicImage) -> (u8, u8, f64) {
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

/// The assertion the JPEG XL twins already pin (#38), for one decode path of the AVIF twins:
/// reference white at Reinhard's 1.0 - 187 of 255 - with the SDR control untouched at 255.
fn assert_hdr_twin_lands_at_reference_white(
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
    // The clipped WIC path read 255 here; the raw-signal magick path read 148.
    assert!(
        (180..=200).contains(&pq_right),
        "{label}: PQ ramp ends at {pq_right} (SDR twin {sdr_right}): expected reference white at ~187"
    );
    assert!(
        pq_mean >= 0.7 * sdr_mean && pq_mean <= sdr_mean,
        "{label}: PQ ramp mean {pq_mean:.1} vs SDR {sdr_mean:.1}: still dark, or still clipped"
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
    assert_hdr_twin_lands_at_reference_white("full decode", &pq, &sdr);
    // The thumbnail path scales inside the codec (Fant hands back PREMULTIPLIED float), and it
    // is the path this bug actually shipped on.
    let sdr = decode_preview_capped(AVIF_SDR709, 160).expect("scaled SDR twin");
    let pq = decode_preview_capped(AVIF_PQ2020, 160).expect("scaled PQ twin");
    assert_hdr_twin_lands_at_reference_white("thumbnail decode", &pq, &sdr);
}

/// The ImageMagick half of #39 on its own: magick decodes a PQ AVIF to its raw signal (white at
/// 58% of full scale), and the tiers' finishing step must convert it. Runs wherever a magick
/// with AVIF support is reachable, and says so where one is not.
#[test]
fn hdr_pq_avif_through_magick_lands_at_reference_white() {
    use crate::decode::magick::{decode_via_magick_capped, magick_available};
    if !magick_available() {
        eprintln!("no ImageMagick here - skipping the magick half of #39");
        return;
    }
    let (Ok(pq_raw), Ok(sdr_raw)) = (
        decode_via_magick_capped(AVIF_PQ2020, None),
        decode_via_magick_capped(AVIF_SDR709, None),
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
    assert_hdr_twin_lands_at_reference_white("magick decode", &pq, &sdr);
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
        assert_hdr_twin_lands_at_reference_white(format, &hdr, &sdr);
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
    let icc = include_bytes!("../../../tests/fixtures/tiff/bt2020-pq.icc");
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
    assert_hdr_twin_lands_at_reference_white("tiff + PQ icc", &pq, &sdr);
}

/// The WIC tiers need COM on the calling thread; the shell host has it, a test thread does
/// not. Idempotent, and a failure (already initialised on another model) is fine to ignore.
fn com_init() {
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_MULTITHREADED,
        );
    }
}

/// A multi-item AVIF (a gain map, an alpha plane) carries a `colr` box per item. The signals
/// must be the PRIMARY item's, resolved through `pitm` -> `ipma` -> `ipco` index, not
/// whichever box the encoder wrote first or last: here the SDR `colr` comes first and the PQ
/// one second, and the primary item alone decides.
#[test]
fn avif_colour_signals_follow_the_primary_item_not_box_order() {
    use crate::decode::color::{avif_wic_class_of, isobmff_hdr_cicp};
    use crate::decode::wicprobe::WicClass;
    fn bx(typ: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let size = u32::try_from(8 + body.len()).unwrap();
        [&size.to_be_bytes()[..], &typ[..], body].concat()
    }
    fn nclx(primaries: u16, transfer: u16, matrix: u16) -> Vec<u8> {
        let mut b = b"nclx".to_vec();
        b.extend_from_slice(&primaries.to_be_bytes());
        b.extend_from_slice(&transfer.to_be_bytes());
        b.extend_from_slice(&matrix.to_be_bytes());
        b.push(0x80);
        bx(b"colr", &b)
    }
    // ipco: 1 = ispe, 2 = av1C (10-bit), 3 = SDR colr (BT.709/sRGB), 4 = PQ colr (BT.2020).
    let two_items = |primary: u16| -> Vec<u8> {
        let ipco = bx(
            b"ipco",
            &[
                bx(b"ispe", &[0u8; 12]),
                bx(b"av1C", &[0x81, 0x00, 0x4c, 0x00]),
                nclx(1, 13, 1),
                nclx(9, 16, 9),
            ]
            .concat(),
        );
        // ipma v0, flags 0: item 1 -> {1, 2, 3}; item 2 -> {1, 2, 4}.
        let ipma = bx(
            b"ipma",
            &[
                &[0u8; 4][..],
                &2u32.to_be_bytes(),
                &1u16.to_be_bytes(),
                &[3, 1, 2, 3],
                &2u16.to_be_bytes(),
                &[3, 1, 2, 4],
            ]
            .concat(),
        );
        let pitm = bx(b"pitm", &[&[0u8; 4][..], &primary.to_be_bytes()].concat());
        let meta = bx(
            b"meta",
            &[&[0u8; 4][..], &pitm, &bx(b"iprp", &[ipco, ipma].concat())].concat(),
        );
        [bx(b"ftyp", b"avif\0\0\0\0mif1"), meta].concat()
    };
    let sdr_primary = two_items(1);
    assert_eq!(avif_wic_class_of(&sdr_primary), Some(WicClass::HighBt709));
    assert_eq!(isobmff_hdr_cicp(&sdr_primary), None);
    let pq_primary = two_items(2);
    assert_eq!(avif_wic_class_of(&pq_primary), Some(WicClass::HighHdr));
    let cicp = isobmff_hdr_cicp(&pq_primary).expect("the primary item is the PQ one");
    assert_eq!((cicp.primaries, cicp.transfer), (9, 16));
}

/// An AVIF with no `colr` box still carries a colour description in its sequence header,
/// which is what every decoder falls back to; the HDR read takes it too. Two shapes: libavif
/// puts the header in `av1C`'s configOBUs (built by hand here, with PQ / BT.2020 declared),
/// and ffmpeg's muxer leaves a bare `av1C` and the header at the start of the item's data
/// (the real no-colr probes, whose libaom headers declare only the matrix - "unspecified"
/// transfer - so they must stay SDR). The routing class stays the no-colr one either way.
#[test]
fn a_colr_less_avif_takes_its_hdr_signal_from_the_sequence_header() {
    use crate::decode::avifmf::primary_av1_payload;
    use crate::decode::color::{av1_obus_color_config, avif_wic_class_of, isobmff_hdr_cicp};
    use crate::decode::wicprobe::WicClass;
    const P10_NOCOLR: &[u8] = include_bytes!("../../../assets/wicprobe/avif-10bit-nocolr.avif");
    const P8_NOCOLR: &[u8] = include_bytes!("../../../assets/wicprobe/avif-8bit-nocolr.avif");

    // A reduced still-picture sequence header, profile 1 (4:4:4), 32x32, 10-bit, with
    // color_description_present = 1 naming BT.2020 / PQ / BT.2020nc, full range.
    let bits = concat!(
        "001", "1", "1", "00000", // seq_profile, still_picture, reduced, seq_level_idx
        "0100", "0100", "11111", "11111", // width/height bits-1 = 4 -> 5-bit sizes of 31 (+1)
        "0", "0", "0", // 128x128 superblock, filter intra, intra edge filter
        "0", "0", "0", // superres, cdef, restoration
        "1", // high_bitdepth
        "1", // color_description_present_flag
        "00001001", "00010000", "00001001", // cp 9, tc 16, mc 9
        "1",        // color_range
        "0",        // separate_uv_delta_q
        "0"         // film_grain_params_present
    );
    let mut payload = Vec::new();
    for chunk in bits.as_bytes().chunks(8) {
        let s = std::str::from_utf8(chunk).unwrap();
        let padded = format!("{s:0<8}");
        payload.push(u8::from_str_radix(&padded, 2).unwrap());
    }
    let mut obu = vec![0x0A, payload.len() as u8]; // OBU_SEQUENCE_HEADER, has_size
    obu.extend_from_slice(&payload);
    let config = av1_obus_color_config(&obu).expect("the hand-built header parses");
    assert_eq!(
        (
            config.primaries,
            config.transfer,
            config.matrix,
            config.full_range
        ),
        (9, 16, 9, true)
    );

    fn bx(typ: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let size = u32::try_from(8 + body.len()).unwrap();
        [&size.to_be_bytes()[..], &typ[..], body].concat()
    }
    let av1c = bx(b"av1C", &[&[0x81, 0x20, 0x4c, 0x00][..], &obu].concat());
    let ipco = bx(b"ipco", &[bx(b"ispe", &[0u8; 12]), av1c].concat());
    let meta = bx(b"meta", &[&[0u8; 4][..], &bx(b"iprp", &ipco)].concat());
    let colr_less = [bx(b"ftyp", b"avif\0\0\0\0mif1"), meta].concat();
    let cicp = isobmff_hdr_cicp(&colr_less).expect("PQ read from the sequence header in av1C");
    assert_eq!(
        (cicp.primaries, cicp.transfer, cicp.full_range),
        (9, 16, true)
    );
    assert_eq!(avif_wic_class_of(&colr_less), Some(WicClass::HighHdr));

    // The ffmpeg shape: the header is in the item data, and libaom declared no transfer.
    let from_payload = primary_av1_payload(P10_NOCOLR)
        .and_then(av1_obus_color_config)
        .expect("the header at the start of the item data parses");
    assert_eq!((from_payload.transfer, from_payload.matrix), (2, 1));
    assert_eq!(
        isobmff_hdr_cicp(P10_NOCOLR),
        None,
        "an unspecified transfer is not HDR"
    );
    assert_eq!(avif_wic_class_of(P10_NOCOLR), Some(WicClass::HighNoColr));
    assert_eq!(avif_wic_class_of(P8_NOCOLR), Some(WicClass::EightNoColr));
}
