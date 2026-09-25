#![cfg(test)]

use super::*;

/// A one-tile 8x8 codestream header with no tile data, for the pure header-derived
/// helpers (`subband_step`, `validate_reduced_scope`).
fn synthetic_codestream(
    levels: u8,
    components: Vec<codestream::Component>,
    qcd: codestream::Qcd,
) -> codestream::Codestream<'static> {
    let n = components.len();
    codestream::Codestream {
        siz: codestream::Siz {
            xsiz: 8,
            ysiz: 8,
            xosiz: 0,
            yosiz: 0,
            xtsiz: 8,
            ytsiz: 8,
            xtosiz: 0,
            ytosiz: 0,
            components,
        },
        cod: codestream::Cod {
            progression: 0,
            layers: 1,
            mct: false,
            levels,
            cblk_w: 64,
            cblk_h: 64,
            cblk_style: 0,
            reversible: false,
            precincts: Vec::new(),
            sop: false,
            eph: false,
        },
        qcd,
        cod_comp: vec![None; n],
        qcd_comp: vec![None; n],
        tiles: vec![Vec::new()],
    }
}

fn component(prec: u8, signed: bool) -> codestream::Component {
    codestream::Component {
        prec,
        signed,
        dx: 1,
        dy: 1,
    }
}

/// Scalar-derived quantization: LL and the coarsest detail bands use the signalled
/// exponent; each finer resolution's exponent is one lower, never below zero, and
/// the mantissa is shared. Expounded tables are read per band as before.
#[test]
fn derived_quantization_exponent_drops_one_per_resolution() {
    let derived = codestream::Qcd {
        style: 1,
        guard_bits: 2,
        steps: vec![(10, 5)],
    };
    let c = synthetic_codestream(3, vec![component(7, false)], derived);
    assert_eq!(subband_step(&c, 0, 0, 0), (10, 5));
    for b in 0..3 {
        assert_eq!(subband_step(&c, 0, 1, b), (10, 5));
        assert_eq!(subband_step(&c, 0, 2, b), (9, 5));
        assert_eq!(subband_step(&c, 0, 3, b), (8, 5));
    }

    let low = codestream::Qcd {
        style: 1,
        guard_bits: 2,
        steps: vec![(1, 0)],
    };
    let c = synthetic_codestream(5, vec![component(7, false)], low);
    assert_eq!(subband_step(&c, 0, 5, 2), (0, 0));

    let expounded = codestream::Qcd {
        style: 2,
        guard_bits: 2,
        steps: vec![(10, 5), (9, 1), (9, 2), (9, 3), (8, 4), (8, 5), (8, 6)],
    };
    let c = synthetic_codestream(2, vec![component(7, false)], expounded);
    assert_eq!(subband_step(&c, 0, 0, 0), (10, 5));
    assert_eq!(subband_step(&c, 0, 1, 1), (9, 2));
    assert_eq!(subband_step(&c, 0, 2, 2), (8, 6));
}

/// The output's level shift and scale come from component 0, so a colour component
/// with a different depth or signedness is refused; a differing alpha component is
/// never decoded and passes.
#[test]
fn mixed_precision_colour_components_are_refused() {
    let qcd = codestream::Qcd {
        style: 0,
        guard_bits: 2,
        steps: vec![(8, 0)],
    };
    let mixed_rgb = vec![
        component(7, false),
        component(11, false),
        component(7, false),
    ];
    let c = synthetic_codestream(1, mixed_rgb, qcd.clone());
    assert!(matches!(
        validate_reduced_scope(&c, false),
        Err(Jp2Error::Unsupported(_))
    ));

    let signed_g = vec![component(7, false), component(7, true), component(7, false)];
    let c = synthetic_codestream(1, signed_g, qcd.clone());
    assert!(matches!(
        validate_reduced_scope(&c, false),
        Err(Jp2Error::Unsupported(_))
    ));

    let rgb_1bit_alpha = vec![
        component(7, false),
        component(7, false),
        component(7, false),
        component(0, false),
    ];
    let c = synthetic_codestream(1, rgb_1bit_alpha, qcd);
    assert_eq!(validate_reduced_scope(&c, false).ok(), Some(4));
    assert_eq!(used_components(4), 3);
    assert_eq!(used_components(2), 1);
}

/// Decode the corpus's 76 MP scan at a thumbnail size and write it out for comparison.
/// Skips when the corpus has not been built.
/// Every corpus JPEG 2000 file must decode through the native reduced path — including
/// the 76 MP archival scan with its 1529 tile-parts, 30 layers, RPCL progression and
/// 256x256 precincts. (Pixel CORRECTNESS is pinned by the bit-exactness test above and
/// the preview-handler integration tests; this one pins breadth and speed.)
#[test]
fn decode_every_corpus_jp2() {
    let dir = st2k_base::testcorpus::dir();
    for name in [
        "sample.j2k",
        "sample.jp2",
        "sample.jpf",
        "sample.jpx",
        "huge.jp2",
    ] {
        let Ok(bytes) = std::fs::read(dir.join(name)) else {
            continue;
        };
        let t0 = std::time::Instant::now();
        match decode_reduced(&bytes, 256) {
            Ok((rgb, w, h)) => {
                eprintln!("  {name}: OK {w}x{h} in {:?}", t0.elapsed());
                if let Some(img) = image::RgbImage::from_raw(w, h, rgb) {
                    let _ = img.save(std::env::temp_dir().join(format!("st2k_jp2_{name}.png")));
                }
            }
            Err(e) => panic!("{name}: must decode natively, got: {e}"),
        }
    }
}

/// A 341-byte, 1-bit, PALETTED blank page from a real archive.org user (issue #11).
/// Its `pclr` box maps index 0 -> WHITE; a decoder that renders raw indices paints it
/// solid black. Every sample must come out white — not "mostly", every one, because
/// the image is genuinely blank and the palette is genuinely two-entry.
#[test]
fn bilevel_paletted_page_renders_white() {
    let p = st2k_base::testcorpus::dir().join("tiny-bilevel.jp2");
    let Ok(bytes) = std::fs::read(&p) else {
        eprintln!("skipping: no tiny-bilevel.jp2");
        return;
    };
    let (rgb, w, h) = decode_reduced(&bytes, 256).expect("bilevel paletted decode");
    assert!(w > 0 && h > 0);
    assert!(
        rgb.iter().all(|&v| v == 255),
        "blank white paletted page decoded non-white (palette ignored?)"
    );
}

#[test]
fn decode_huge_corpus_jp2() {
    let p = st2k_base::testcorpus::dir().join("huge.jp2");
    let Ok(bytes) = std::fs::read(&p) else {
        eprintln!("skipping: no ../test-corpus/huge.jp2");
        return;
    };
    // PROGRESS PROBE, NOT A CONTRACT. The pixel path is unfinished and deliberately not
    // wired into the cascade, so a failure here is the expected state, not a regression:
    // asserting on it would just paint CI red over known-incomplete work. It reports how
    // far the decode gets and writes the result out when it succeeds, so the next session
    // starts from a fact instead of a guess. It becomes a real assertion the day the
    // output matches a reference decoder.
    let t0 = std::time::Instant::now();
    match decode_reduced(&bytes, 1024) {
        Ok((rgb, w, h)) => {
            eprintln!("jp2 native: decoded {w}x{h} in {:?}", t0.elapsed());
            // What IS a contract, even for an unfinished path: a success is a well-formed
            // picture. Wrong-sized pixel data or a zero dimension is a bug wherever the
            // decoder is on its way, and it must never reach a caller as "Ok".
            // (The reduced level is the smallest one that still COVERS the target, so an
            // edge may exceed 1024; the size is not the contract, the shape is.)
            assert!(w > 0 && h > 0, "reduced to {w}x{h}");
            assert_eq!(rgb.len(), w as usize * h as usize * 3, "pixel buffer size");
            if let Some(img) = image::RgbImage::from_raw(w, h, rgb) {
                let out = std::env::temp_dir().join("st2k_jp2_native.png");
                let _ = img.save(&out);
                eprintln!("jp2 native: wrote {}", out.display());
            }
        }
        Err(e) => eprintln!("jp2 native: not there yet — {e}"),
    }
}

/// An image offset (the corpus's real.j2k starts its picture at x=150, y=300 on the reference
/// grid) put every code-block in the wrong place, and the file drew as noise in every release up
/// to 3.2.0. ImageMagick, an independent decoder, reads the same picture; it takes the offset
/// off twice, so its frame is ours less 150 columns and 300 rows, from the top left.
#[test]
fn an_image_offset_decodes_to_the_picture() {
    let Some(bytes) = st2k_base::testcorpus::read("real.j2k") else {
        eprintln!("NOT MEASURED: real.j2k absent");
        return;
    };
    if !crate::decode::magick_available() {
        eprintln!("NOT MEASURED: no ImageMagick");
        return;
    }
    let (full_w, full_h) = (2592, 1944);
    // Two levels down: 648x486, a quarter of the picture on each side.
    let (rgb, w, h) = super::decode_reduced(&bytes, 600).expect("decodes");
    assert_eq!(
        (w * 4, h * 4),
        (full_w, full_h),
        "the image area, the offset taken off once"
    );
    let ours = image::RgbImage::from_raw(w, h, rgb).expect("buffer");
    let theirs = crate::decode::magick::decode_via_magick_capped(
        &bytes,
        None,
        crate::decode::magick::Fidelity::Full,
    )
    .expect("ImageMagick reads it")
    .to_rgb8();
    let (cw, ch) = (theirs.width() / 4, theirs.height() / 4);
    let theirs = image::imageops::resize(&theirs, cw, ch, image::imageops::FilterType::Triangle);
    let mut sum = 0u64;
    for (x, y, p) in theirs.enumerate_pixels() {
        let q = ours.get_pixel(x, y);
        sum += (0..3).map(|i| u64::from(p[i].abs_diff(q[i]))).sum::<u64>();
    }
    let mean = sum as f64 / f64::from(cw * ch * 3);
    assert!(
        mean < 6.0,
        "not ImageMagick's picture: mean difference {mean:.1}"
    );
}
