//! Which files are handed to WIC, and which are kept away from it.
//! Every bucket here was measured rather than assumed: WIC is right for the
//! plain cases and wrong in specific, reproducible ones, and the routing
//! table is only as good as the cases pinned below.

/// The WebP WIC-eligibility sniffer. Every branch is a routing decision with a correctness
/// stake, so every branch is pinned: an animated WebP through WIC could pick a different
/// FRAME, and an ICC WebP through WIC would skip verified colour management.
#[test]
fn webp_wic_routing_excludes_exactly_the_risky_cases() {
    use crate::decode::webp_prefers_wic;

    fn webp(fourcc: &[u8; 4], flags: Option<u8>) -> Vec<u8> {
        let mut b = b"RIFF\x00\x01\x00\x00WEBP".to_vec();
        b.extend_from_slice(fourcc);
        b.extend_from_slice(&10u32.to_le_bytes()); // chunk size
        b.push(flags.unwrap_or(0));
        b.extend_from_slice(&[0u8; 12]); // rest of the VP8X payload / stub data
        b
    }

    // Simple stills: no feature flags exist at all, so nothing to be wrong about.
    assert!(webp_prefers_wic(&webp(b"VP8 ", None)));
    assert!(webp_prefers_wic(&webp(b"VP8L", None)));

    // Extended stills: alpha, EXIF and XMP are fine (alpha survives the shared 32bppRGBA
    // conversion; EXIF orientation is our own pipeline's job either way).
    assert!(webp_prefers_wic(&webp(b"VP8X", Some(0x10)))); // alpha
    assert!(webp_prefers_wic(&webp(b"VP8X", Some(0x0C)))); // EXIF | XMP

    // The two exclusions this sniffer exists for.
    assert!(
        !webp_prefers_wic(&webp(b"VP8X", Some(0x02))),
        "animated WebP must stay on the pure-Rust path: frame choice is pinned there"
    );
    assert!(
        !webp_prefers_wic(&webp(b"VP8X", Some(0x30))),
        "ICC-tagged WebP must stay on the verified colour-management path"
    );

    // Not WebP, unknown first chunk, truncated: decline, keeping the existing tier order.
    assert!(!webp_prefers_wic(b"RIFF\x00\x00\x00\x00WAVEfmt "));
    assert!(!webp_prefers_wic(&webp(b"ANMF", None)));
    assert!(!webp_prefers_wic(&b"RIFF\x00\x01\x00\x00WEBPVP8X"[..]));
    assert!(!webp_prefers_wic(b""));
}

/// The BT.601-AVIF Media Foundation path, minus Media Foundation: the eligibility gates,
/// the YUV maths, and the mini-MP4 all verify without the codec, so CI (which has no AV1
/// extension) still pins everything except the decode itself. The decode is pinned by the
/// corpus fixture `sample-avif-601.avif` + `_expected-colors.txt` on machines that have it.
#[test]
fn avif_mf_eligibility_takes_exactly_the_measured_buckets() {
    use crate::decode::avifmf::eligible_bt601_still;

    fn bx(typ: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let size = u32::try_from(8 + body.len()).unwrap();
        [&size.to_be_bytes()[..], &typ[..], body].concat()
    }
    // A configurable AVIF skeleton: ipco with av1C(s), optional nclx, ispe, optional auxC.
    struct Cfg {
        matrix: Option<u16>,
        primaries: u16,
        av1c_count: usize,
        profile_byte: u8, // av1C byte 1: seq_profile in the top 3 bits
        flags2: u8,       // av1C byte 2: high_bitdepth bit 6, monochrome bit 4
        aux_c: bool,
    }
    fn avif(c: &Cfg) -> Vec<u8> {
        let mut props = Vec::new();
        let mut ispe = vec![0u8; 4];
        ispe.extend_from_slice(&320u32.to_be_bytes());
        ispe.extend_from_slice(&240u32.to_be_bytes());
        props.push(bx(b"ispe", &ispe));
        for _ in 0..c.av1c_count {
            props.push(bx(b"av1C", &[0x81, c.profile_byte, c.flags2, 0x00]));
        }
        if let Some(m) = c.matrix {
            let mut nclx = b"nclx".to_vec();
            nclx.extend_from_slice(&c.primaries.to_be_bytes());
            nclx.extend_from_slice(&2u16.to_be_bytes());
            nclx.extend_from_slice(&m.to_be_bytes());
            nclx.push(0x00);
            props.push(bx(b"colr", &nclx));
        }
        if c.aux_c {
            props.push(bx(
                b"auxC",
                b"urn:mpeg:mpegB:cicp:systems:auxiliary:alpha\0",
            ));
        }
        let iprp = bx(b"iprp", &bx(b"ipco", &props.concat()));
        let meta = bx(b"meta", &[&[0u8; 4][..], &iprp].concat());
        [bx(b"ftyp", b"avif\0\0\0\0mif1"), meta].concat()
    }
    let base = Cfg {
        matrix: Some(6),
        primaries: 1,
        av1c_count: 1,
        profile_byte: 0x00,
        flags2: 0x0c,
        aux_c: false,
    };

    // The three eligible matrices: explicit BT.601 (5/6) and unspecified (2, decoded as 601
    // by the ecosystem reference — measured, worst error 1).
    for m in [2u16, 5, 6] {
        let c = Cfg {
            matrix: Some(m),
            ..base
        };
        assert!(
            eligible_bt601_still(&avif(&c)).is_some(),
            "matrix {m} is a measured BT.601 bucket and must be eligible"
        );
    }
    // The dims come from ispe, verbatim.
    let s = eligible_bt601_still(&avif(&base)).unwrap();
    assert_eq!((s.width, s.height), (320, 240));
    assert!(!s.full_range, "range bit clear must read as limited");

    // Everything below must DECLINE (fall back to ImageMagick, never decode wrongly):
    let cases: &[(&str, Cfg)] = &[
        (
            "BT.709 belongs to the WIC fast path, not here",
            Cfg {
                matrix: Some(1),
                ..base
            },
        ),
        (
            "BT.2020 and friends stay with magick's full CICP handling",
            Cfg {
                matrix: Some(9),
                ..base
            },
        ),
        (
            "no colr box at all is not a licence to guess",
            Cfg {
                matrix: None,
                ..base
            },
        ),
        (
            "wide-gamut primaries stay with magick",
            Cfg {
                primaries: 12,
                ..base
            },
        ),
        (
            "a second av1C means an auxiliary (alpha) item - magick composites those",
            Cfg {
                av1c_count: 2,
                ..base
            },
        ),
        (
            "an auxC property is an alpha plane even with one av1C visible",
            Cfg {
                aux_c: true,
                ..base
            },
        ),
        (
            "High profile (4:4:4) is outside the Main-profile gate",
            Cfg {
                profile_byte: 0x20,
                ..base
            },
        ),
        (
            "high bit depth belongs to the WIC+curve path",
            Cfg {
                flags2: 0x4c,
                ..base
            },
        ),
        (
            "monochrome is untested territory - decline",
            Cfg {
                flags2: 0x1c,
                ..base
            },
        ),
    ];
    for (why, c) in cases {
        assert!(eligible_bt601_still(&avif(c)).is_none(), "{why}");
    }
    // Full-range flag reaches the conversion.
    let mut f = avif(&base);
    let i = f.windows(4).position(|w| w == b"nclx").unwrap();
    f[i + 10] = 0x80;
    assert!(eligible_bt601_still(&f).unwrap().full_range);
}

/// The YUV maths against published BT.601 anchor vectors, both ranges. Wrong coefficients
/// here would ship exactly the colour shift this path exists to eliminate.
#[test]
fn avif_mf_yuv_conversion_matches_bt601_anchors() {
    use crate::decode::avifmf::nv12_to_srgb_bt601;
    use crate::video::Nv12Frame;

    // One 2x2 frame, all four pixels the same YUV triple.
    fn frame(y: u8, cb: u8, cr: u8) -> Nv12Frame {
        Nv12Frame {
            data: vec![y, y, y, y, cb, cr],
            width: 2,
            height: 2,
            stride: 2,
        }
    }
    // (y, cb, cr, full_range, expected rgb, tolerance)
    type Anchor = (u8, u8, u8, bool, (i32, i32, i32), i32);
    let anchors: &[Anchor] = &[
        (16, 128, 128, false, (0, 0, 0), 0),        // limited black
        (235, 128, 128, false, (255, 255, 255), 0), // limited white
        (126, 128, 128, false, (128, 128, 128), 1), // limited mid grey
        (82, 90, 240, false, (255, 0, 0), 2),       // limited saturated red
        (145, 54, 34, false, (0, 255, 0), 2),       // limited saturated green
        (41, 240, 110, false, (0, 0, 255), 2),      // limited saturated blue
        (0, 128, 128, true, (0, 0, 0), 0),          // full black
        (255, 128, 128, true, (255, 255, 255), 0),  // full white
        (200, 128, 128, true, (200, 200, 200), 0),  // full grey passes through untouched
    ];
    for &(y, cb, cr, full, (er, eg, eb), tol) in anchors {
        let img = nv12_to_srgb_bt601(&frame(y, cb, cr), 2, 2, full, None).unwrap();
        let px = img.to_rgba8().get_pixel(0, 0).0;
        for (got, want) in px[..3].iter().zip([er, eg, eb]) {
            assert!(
                (i32::from(*got) - want).abs() <= tol,
                "yuv({y},{cb},{cr}) full={full}: got {:?}, wanted ({er},{eg},{eb}) +/-{tol}",
                &px[..3]
            );
        }
        assert_eq!(px[3], 255, "this path never carries alpha");
    }
}

/// Target-aware subsampling: asking for a small thumbnail must convert FEWER pixels, without
/// changing what those pixels are. This is the fix for the 12 MP AVIF running 2.95x Windows.
#[test]
fn avif_mf_conversion_subsamples_for_a_small_target() {
    use crate::decode::avifmf::nv12_to_srgb_bt601;
    use crate::video::Nv12Frame;

    // A 1200x900 flat mid-grey frame: flat so subsampling cannot change the answer.
    let (w, h) = (1200usize, 900usize);
    let mut data = vec![126u8; w * h];
    data.resize(w * h + w * h / 2, 128u8);
    let frame = Nv12Frame {
        data,
        width: w as u32,
        height: h as u32,
        stride: w as u32,
    };

    // No target: full resolution, as the full-fidelity callers still get.
    let full = nv12_to_srgb_bt601(&frame, w as u32, h as u32, false, None).unwrap();
    assert_eq!((full.width(), full.height()), (1200, 900));

    // A 100 px target wants >= 300 px of intermediate, so step = 1200/300 = 4.
    let small = nv12_to_srgb_bt601(&frame, w as u32, h as u32, false, Some(100)).unwrap();
    assert_eq!(
        (small.width(), small.height()),
        (300, 225),
        "must subsample to >= 3x the target edge, not to the target itself (that would alias)"
    );

    // Never UPSAMPLE, and never subsample when the source is already small enough.
    let big_target = nv12_to_srgb_bt601(&frame, w as u32, h as u32, false, Some(4096)).unwrap();
    assert_eq!((big_target.width(), big_target.height()), (1200, 900));

    // The colour is identical either way - this is a work reduction, not a quality change.
    for img in [&full, &small] {
        let px = img.to_rgba8().get_pixel(1, 1).0;
        assert!(
            px[..3].iter().all(|c| (i32::from(*c) - 128).abs() <= 1),
            "subsampling must not shift colour; got {:?}",
            &px[..3]
        );
    }
}

/// The one-frame MP4 the path builds must be one OUR OWN mp4 parser recognises as av01 —
/// a cheap structural round-trip that needs no codec.
#[test]
fn avif_mf_mini_mp4_roundtrips_through_the_mp4_parser() {
    use crate::decode::avifmf::{build_av01_mp4, Av1Still};
    let still = Av1Still {
        av1c: {
            let body = [0x81u8, 0x00, 0x0c, 0x00];
            let mut b = 12u32.to_be_bytes().to_vec();
            b.extend_from_slice(b"av1C");
            b.extend_from_slice(&body);
            b
        },
        colr: {
            let mut payload = b"nclx".to_vec();
            payload.extend_from_slice(&[0, 1, 0, 2, 0, 6, 0]);
            let mut b = (8 + payload.len() as u32).to_be_bytes().to_vec();
            b.extend_from_slice(b"colr");
            b.extend_from_slice(&payload);
            b
        },
        width: 64,
        height: 48,
        full_range: false,
    };
    let mini = build_av01_mp4(&still, &[0u8; 32]).expect("muxer must accept a plain still");
    let fourcc = crate::mp4::video_codec_fourcc(&mut std::io::Cursor::new(&mini));
    assert_eq!(
        fourcc.as_ref(),
        Some(b"av01"),
        "the mini-MP4 must advertise an av01 track our own parser can read back"
    );
}

/// The GIF WIC-eligibility sniffer. It walks the block chain, so every branch is pinned
/// against a real (if minimal) GIF rather than a header stub.
#[test]
fn gif_wic_routing_takes_only_the_plain_single_frame_case() {
    use crate::decode::gif_prefers_wic;

    /// A complete, structurally valid GIF: header, 2-entry global table, optional extra
    /// blocks, then `frames` image descriptors and the trailer.
    fn gif(w: u16, h: u16, frames: &[(u16, u16, u16, u16)], extension: bool) -> Vec<u8> {
        let mut b = b"GIF89a".to_vec();
        b.extend_from_slice(&w.to_le_bytes());
        b.extend_from_slice(&h.to_le_bytes());
        b.push(0x80); // global colour table, 2 entries
        b.push(0); // background index
        b.push(0); // aspect ratio
        b.extend_from_slice(&[0, 0, 0, 255, 255, 255]);
        if extension {
            b.extend_from_slice(&[0x21, 0xF9, 0x04, 0, 0, 0, 0, 0x00]);
        }
        for (left, top, fw, fh) in frames {
            b.push(0x2C);
            b.extend_from_slice(&left.to_le_bytes());
            b.extend_from_slice(&top.to_le_bytes());
            b.extend_from_slice(&fw.to_le_bytes());
            b.extend_from_slice(&fh.to_le_bytes());
            b.push(0); // no local colour table
            b.push(2); // LZW minimum code size
            b.extend_from_slice(&[2, 0x44, 0x01, 0x00]); // one sub-block, then terminator
        }
        b.push(0x3B);
        b
    }

    let full = [(0u16, 0u16, 64u16, 64u16)];
    assert!(
        gif_prefers_wic(&gif(64, 64, &full, false)),
        "a plain single-frame GIF is what this fast path exists for"
    );
    assert!(
        gif_prefers_wic(&gif(64, 64, &full, true)),
        "a graphic control extension is normal on a still and must not disqualify it"
    );

    // Animation: which frame becomes the thumbnail is the decoder's choice, so it stays on
    // the decoder whose choice the corpus already pins.
    assert!(
        !gif_prefers_wic(&gif(64, 64, &[full[0], full[0]], false)),
        "a two-frame GIF must not change decoder"
    );
    // A frame that does not cover the canvas: the image tier composites it onto the full
    // canvas, WIC returns the frame at its own size. Two different pictures.
    assert!(
        !gif_prefers_wic(&gif(64, 64, &[(0, 0, 32, 32)], false)),
        "an undersized frame renders differently through WIC"
    );
    assert!(
        !gif_prefers_wic(&gif(64, 64, &[(8, 8, 64, 64)], false)),
        "an offset frame renders differently through WIC"
    );

    // Not a GIF, and truncations at each structural step: all ineligible, never a panic.
    assert!(!gif_prefers_wic(b"not a gif at all"));
    assert!(!gif_prefers_wic(&[]));
    let whole = gif(64, 64, &full, true);
    for cut in 0..whole.len() {
        assert!(
            !gif_prefers_wic(&whole[..cut]),
            "a GIF truncated to {cut} bytes must be ineligible, not eligible or a panic"
        );
    }
}

/// The BMP WIC-eligibility sniffer. Every branch is a routing decision that could change what
/// the user SEES, so every branch is pinned.
#[test]
fn bmp_wic_routing_excludes_the_ambiguous_cases() {
    use crate::decode::bmp_prefers_wic;

    /// A BMP head: BITMAPFILEHEADER(14) + BITMAPINFOHEADER(40), enough for the sniffer.
    fn bmp(dib_size: u32, bitcount: u16, compression: u32) -> Vec<u8> {
        let mut b = b"BM".to_vec();
        b.extend_from_slice(&0u32.to_le_bytes()); // file size
        b.extend_from_slice(&0u32.to_le_bytes()); // reserved
        b.extend_from_slice(&54u32.to_le_bytes()); // pixel offset
        b.extend_from_slice(&dib_size.to_le_bytes());
        b.extend_from_slice(&64u32.to_le_bytes()); // width
        b.extend_from_slice(&64u32.to_le_bytes()); // height
        b.extend_from_slice(&1u16.to_le_bytes()); // planes
        b.extend_from_slice(&bitcount.to_le_bytes());
        b.extend_from_slice(&compression.to_le_bytes());
        b.resize(64, 0);
        b
    }

    // The plain memory layouts this optimisation is for.
    for bits in [1u16, 4, 8, 16, 24] {
        assert!(
            bmp_prefers_wic(&bmp(40, bits, 0)),
            "{bits}-bit BI_RGB is a plain layout and must take the fast path"
        );
    }
    assert!(
        bmp_prefers_wic(&bmp(40, 16, 3)),
        "BI_BITFIELDS is still a plain layout"
    );
    // A BITMAPV5HEADER is just a longer header over the same layout.
    assert!(bmp_prefers_wic(&bmp(124, 24, 0)));

    // 32-bit: the alpha byte is alpha in some writers and garbage in others, so the two
    // decoders are entitled to disagree. Stay on the pinned one.
    assert!(
        !bmp_prefers_wic(&bmp(40, 32, 0)),
        "32-bit BMP alpha is ambiguous - it must not change decoder for a speed win"
    );
    // Compressed variants are their own decoders with their own quirks.
    for comp in [1u32, 2, 4, 5] {
        assert!(
            !bmp_prefers_wic(&bmp(40, 8, comp)),
            "compression {comp} is not the plain layout this targets"
        );
    }
    // A BITMAPCOREHEADER (12) has no compression field at all - decline rather than misread.
    assert!(!bmp_prefers_wic(&bmp(12, 24, 0)));
    // Not a BMP, and truncated.
    assert!(!bmp_prefers_wic(b"RIFF\x00\x00\x00\x00WEBPVP8 "));
    assert!(!bmp_prefers_wic(&bmp(40, 24, 0)[..20]));
    assert!(!bmp_prefers_wic(b""));
}
