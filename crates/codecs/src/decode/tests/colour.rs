#![cfg(test)]

//! Getting the colour right, which is where thumbnails visibly go wrong.
//! ICC profiles reassembled out of chunks, the colr box walked out of an
//! ISOBMFF container, and the high-depth curve that undoes what WIC does.

use super::*;
mod hdr;
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

/// A CMYK JPEG tagged with Photoshop's own U.S. Web Coated (SWOP) v2 profile, flat in the ink
/// Photoshop stored for its red (build-corpus.ps1, 9z5b): Photoshop shows it as (220, 42, 40).
/// Until 2026-10-06 it came out (234, 7, 4), the naive conversion, because moxcms refused the
/// CMYK-plus-alpha transform `decode_cmyk_jpeg` asked it for. Through the thumbnail route, so
/// a tier taking the JPEG ahead of the colour-managed one fails here too.
#[test]
fn a_cmyk_jpeg_is_shown_through_its_profile() {
    let Some(bytes) = st2k_base::testcorpus::read("sample-jpeg-cmyk-swop.jpg") else {
        return;
    };
    let img = crate::decode::decode_preview_capped(&bytes, 256)
        .expect("a CMYK JPEG thumbnails")
        .to_rgb8();
    let px = img.get_pixel(img.width() / 2, img.height() / 2).0;
    assert!(
        px.iter()
            .zip([220u8, 42, 40])
            .all(|(&got, want)| got.abs_diff(want) <= 6),
        "{px:?}, where Photoshop shows [220, 42, 40]"
    );
}

#[test]
fn jxl_applies_its_embedded_color_profile() {
    // Issue #9: the jxl tier decoded correctly but never colour-managed, unlike the `image`
    // and WIC tiers. A wide-gamut jxl therefore reached Explorer with its raw AdobeRGB
    // numbers treated as sRGB, which is a visible shift on every saturated colour.
    let img = crate::decode::tiers::decode_jxl(JXL_ADOBERGB, None, false)
        .expect("decode the AdobeRGB jxl");
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
    use crate::decode::avif::isobmff_hdr_cicp;
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
    assert_eq!(isobmff_hdr_cicp(&sdr_primary), None);
    let pq_primary = two_items(2);
    let cicp = isobmff_hdr_cicp(&pq_primary).expect("the primary item is the PQ one");
    assert_eq!((cicp.primaries, cicp.transfer), (9, 16));
}
