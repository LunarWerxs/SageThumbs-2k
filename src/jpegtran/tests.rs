#![cfg(test)]

use super::*;
use image::GenericImageView;

fn img_apply(img: &image::DynamicImage, op: Op) -> image::DynamicImage {
    match op {
        Op::Rot90 => img.rotate90(),
        Op::Rot180 => img.rotate180(),
        Op::Rot270 => img.rotate270(),
        Op::FlipH => img.fliph(),
        Op::FlipV => img.flipv(),
        // rotate90 = transpose then flip-H, so undoing the flip leaves the transpose.
        Op::Transpose => img.rotate90().fliph(),
        Op::Transverse => img.rotate90().flipv(),
    }
}

const ALL: [Op; 7] = [
    Op::Rot90,
    Op::Rot180,
    Op::Rot270,
    Op::FlipH,
    Op::FlipV,
    Op::Transpose,
    Op::Transverse,
];

/// Encode a gray image as a baseline JPEG in memory.
fn encode_gray_jpeg(g: image::GrayImage) -> Vec<u8> {
    let mut jpeg = Vec::new();
    image::DynamicImage::ImageLuma8(g)
        .write_to(
            &mut std::io::Cursor::new(&mut jpeg),
            image::ImageFormat::Jpeg,
        )
        .unwrap();
    jpeg
}

fn gray_jpeg() -> Vec<u8> {
    let mut g = image::GrayImage::new(32, 24); // 8-aligned, single component
    for (x, y, p) in g.enumerate_pixels_mut() {
        *p = image::Luma([((x * 9 + y * 17 + (x ^ y)) % 256) as u8]);
    }
    encode_gray_jpeg(g)
}

/// Hostile / truncated input must return None, never panic — `panic = "abort"`
/// in release would take the host process (Explorer) down. Exercises the
/// bounds-checked segment parsing against every truncation of a valid JPEG plus
/// hand-built malformed segment headers. (A panic here fails the test.)
#[test]
fn malformed_input_never_panics() {
    let good = gray_jpeg();
    for cut in 0..good.len() {
        for op in ALL {
            let _ = transform(&good[..cut], op);
        }
    }
    let cases: &[&[u8]] = &[
        &[],
        &[0xFF, 0xD8],
        &[0xFF, 0xD8, 0xFF, 0xC0, 0xFF, 0xFF], // SOF0 len 0xFFFF, no body
        &[0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x03, 0x08], // SOF0 len too small for header
        &[0xFF, 0xD8, 0xFF, 0xC4, 0x00, 0x02], // DHT len 2, truncated table
        &[0xFF, 0xD8, 0xFF, 0xDB, 0xFF, 0xFF], // DQT huge len past EOF
        &[0xFF, 0xD8, 0xFF, 0xDA, 0x00],       // SOS, ns byte missing
        &[0xFF, 0xD8, 0xFF, 0xDD, 0x00],       // DRI truncated
        &[0xFF, 0xD8, 0xFF, 0xE0, 0xFF, 0xF0], // APP0 len past EOF
    ];
    for c in cases {
        for op in ALL {
            let _ = transform(c, op);
        }
    }
    let mut junk = vec![0xFF, 0xD8];
    (0..3000u32).for_each(|i| junk.push(((i.wrapping_mul(31).wrapping_add(7)) % 256) as u8));
    for op in ALL {
        let _ = transform(&junk, op);
    }
}

/// Non-transpose ops (flip-H/V, rot-180) keep coefficient *positions*, so the
/// decoder's integer IDCT commutes with them — `decode(transform)` must equal
/// `op(decode)` EXACTLY. This pins down the entropy codec, the within-block
/// signs, and the block-grid mapping.
#[test]
fn flips_and_rot180_are_pixel_exact() {
    let jpeg = gray_jpeg();
    let orig = image::load_from_memory(&jpeg).unwrap();
    for op in [Op::FlipH, Op::FlipV, Op::Rot180] {
        let out = transform(&jpeg, op).expect("in scope");
        let got = image::load_from_memory(&out)
            .expect("decodes")
            .to_luma8()
            .into_raw();
        let want = img_apply(&orig, op).to_luma8().into_raw();
        assert_eq!(got, want, "non-transpose op must be pixel-exact");
    }
}

/// Losslessly transform `jpeg`, then decode both the result and the
/// pixel-level reference `orig` rotated by `op`, asserting equal sizes.
/// `why` is the panic message when `op` is out of scope.
fn lossless_and_reference(
    jpeg: &[u8],
    orig: &image::DynamicImage,
    op: Op,
    why: &str,
) -> (image::DynamicImage, image::DynamicImage) {
    let out = transform(jpeg, op).expect(why);
    let got = image::load_from_memory(&out).expect("decodes");
    let want = img_apply(orig, op);
    assert_eq!(got.dimensions(), want.dimensions());
    (got, want)
}

/// Transpose ops (rot-90/270) move coefficient positions, and the decoder's
/// integer IDCT isn't transpose-symmetric — so vs a pixel-rotate it can differ
/// by ±1 (jpegtran has the same artifact). Require the right DIRECTION and that
/// tiny bound — proving it's a real rotation, not a coefficient bug.
#[test]
fn rot90_270_match_pixel_rotate_within_one() {
    let jpeg = gray_jpeg();
    let orig = image::load_from_memory(&jpeg).unwrap();
    for op in [Op::Rot90, Op::Rot270, Op::Transpose, Op::Transverse] {
        let (got, want) = lossless_and_reference(&jpeg, &orig, op, "in scope");
        let (g, w) = (got.to_luma8().into_raw(), want.to_luma8().into_raw());
        let maxd = g
            .iter()
            .zip(&w)
            .map(|(a, b)| (*a as i32 - *b as i32).abs())
            .max()
            .unwrap();
        assert!(
            maxd <= 1,
            "rot transpose should match a pixel-rotate within 1, got {maxd}"
        );
    }
}

/// Coefficient-level proof the transform is exact + reversible: rot-90 four
/// times is the identity (no net transpose → no IDCT asymmetry), so the result
/// must decode bit-for-bit identically to the original.
#[test]
fn rot90_four_times_is_identity() {
    let jpeg = gray_jpeg();
    let mut cur = jpeg.clone();
    for _ in 0..4 {
        cur = transform(&cur, Op::Rot90).expect("in scope");
    }
    let a = image::load_from_memory(&jpeg)
        .unwrap()
        .to_luma8()
        .into_raw();
    let b = image::load_from_memory(&cur).unwrap().to_luma8().into_raw();
    assert_eq!(a, b, "rot90×4 must round-trip to the identical image");
}

/// Transpose and transverse are their own inverses: applied twice there is no net
/// transpose, so the result must decode bit-for-bit identically to the original.
#[test]
fn transpose_and_transverse_twice_are_identity() {
    let jpeg = gray_jpeg();
    let a = image::load_from_memory(&jpeg)
        .unwrap()
        .to_luma8()
        .into_raw();
    for op in [Op::Transpose, Op::Transverse] {
        let once = transform(&jpeg, op).expect("in scope");
        let twice = transform(&once, op).expect("in scope");
        let b = image::load_from_memory(&twice)
            .unwrap()
            .to_luma8()
            .into_raw();
        assert_eq!(a, b, "{op:?} twice must round-trip to the identical image");
    }
}

/// A DHT whose counts over-subscribe the code space (three 1-bit codes, where only two
/// can exist) used to build a table whose `code - mincode[l]` went negative on the
/// first decoded symbol: a wrap in release, a corrupted "lossless" rotate written in
/// place. Such a table is refused at parse time now.
#[test]
fn over_subscribed_huffman_table_is_rejected() {
    let mut bits = [0u8; 16];
    bits[0] = 3;
    assert!(
        build_dec(&bits, &[1, 2, 3]).is_none(),
        "3 one-bit codes cannot exist"
    );
    // Kraft-exact tables still build: two 1-bit codes, and the standard AC luma table.
    bits[0] = 2;
    assert!(build_dec(&bits, &[1, 2]).is_some());
    assert!(build_dec(&AC_LUMA_BITS, &AC_LUMA_VALS).is_some());
    // A value list that disagrees with the counts is refused too.
    assert!(build_dec(&bits, &[1]).is_none());

    // End to end: patch a real JPEG's first DHT count byte to over-subscribe it and the
    // whole transform must decline rather than decode garbage.
    let good = gray_jpeg();
    let dht = good
        .windows(2)
        .position(|w| w == [0xFF, 0xC4])
        .expect("a DHT segment");
    let mut bad = good.clone();
    bad[dht + 5] = 200; // bits[0]: 200 one-bit codes
    assert!(transform(&bad, Op::FlipH).is_none());
}

/// `decode_huff` must never index below `valptr[l]`: a code smaller than `mincode[l]`
/// while still `<= maxcode[l]` (only possible with a malformed table) is a miss, not a
/// wrapped subtraction that lands on an unrelated symbol.
#[test]
fn decode_huff_rejects_a_code_below_mincode() {
    let mut h = HuffDec {
        mincode: [0; 17],
        maxcode: [-1; 17],
        valptr: [0; 17],
        vals: vec![0xAA, 0xBB],
    };
    h.mincode[1] = 1;
    h.maxcode[1] = 1;
    h.valptr[1] = 1;
    let data = [0x00u8]; // first bit 0: code 0 < mincode[1]
    let mut br = BitReader::new(&data, 0);
    assert!(decode_huff(&mut br, &h).is_none());
}

/// A JPEG carrying a Multi-Picture Format index (APP2 `MPF\0`) or an XMP GContainer
/// directory is declined outright: the rebuilt file would keep the index while the
/// bytes it points at are re-coded or dropped past EOI.
#[test]
fn multi_picture_jpegs_are_declined() {
    let good = gray_jpeg();
    assert!(!has_multi_picture_index(&good));
    let splice = |marker: u8, payload: &[u8]| {
        let mut v = good[..2].to_vec();
        v.extend_from_slice(&[0xFF, marker]);
        v.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
        v.extend_from_slice(payload);
        v.extend_from_slice(&good[2..]);
        v
    };
    let mpf = splice(0xE2, b"MPF\0II*\0\x08\0\0\0secondary-image-index");
    assert!(has_multi_picture_index(&mpf));
    assert!(transform(&mpf, Op::Rot90).is_none(), "MPF must decline");
    let xmp = splice(
        0xE1,
        b"http://ns.adobe.com/xap/1.0/\0<x:xmpmeta><Container:Directory/></x:xmpmeta>",
    );
    assert!(has_multi_picture_index(&xmp));
    assert!(
        transform(&xmp, Op::FlipH).is_none(),
        "GContainer must decline"
    );
    // An ICC APP2 or a plain XMP packet is not an index and stays in scope.
    let icc = splice(0xE2, b"ICC_PROFILE\0\x01\x01profile-bytes");
    assert!(!has_multi_picture_index(&icc));
    assert!(transform(&icc, Op::Rot90).is_some());
    let plain_xmp = splice(0xE1, b"http://ns.adobe.com/xap/1.0/\0<x:xmpmeta/>");
    assert!(!has_multi_picture_index(&plain_xmp));
}

/// Color: chroma upsampling may not commute with rotation at block edges, so
/// only require a *small* mean difference — proving it's a real rotation, not
/// garbage — plus correct dimensions and a clean decode.
#[test]
fn color_transform_is_valid_and_close() {
    let mut c = image::RgbImage::new(32, 32); // 16-aligned (handles 4:2:0)
    for (x, y, p) in c.enumerate_pixels_mut() {
        *p = image::Rgb([
            ((x * 8) % 256) as u8,
            ((y * 8) % 256) as u8,
            (((x + y) * 4) % 256) as u8,
        ]);
    }
    let mut jpeg = Vec::new();
    image::DynamicImage::ImageRgb8(c)
        .write_to(
            &mut std::io::Cursor::new(&mut jpeg),
            image::ImageFormat::Jpeg,
        )
        .unwrap();
    let orig = image::load_from_memory(&jpeg).unwrap();

    for op in ALL {
        let (got, want) =
            lossless_and_reference(&jpeg, &orig, op, "color transform should be in scope");
        let (g, w) = (got.to_rgb8().into_raw(), want.to_rgb8().into_raw());
        let mad: f64 = g
            .iter()
            .zip(&w)
            .map(|(a, b)| (*a as i32 - *b as i32).unsigned_abs() as f64)
            .sum::<f64>()
            / g.len() as f64;
        assert!(
            mad < 3.0,
            "mean abs diff {mad} too high — not a real rotation"
        );
    }
}

// Verifies the restart-marker + 4:2:0-subsampling decode path on a REAL
// ImageMagick-encoded JPEG (the synthetic image-crate JPEGs have neither — this
// is the only coverage of the `br.restart` + multi-block-per-component loop).
// The fixture is COMMITTED so this runs on a plain `cargo test` with no magick
// on PATH. Regenerate with:
//   magick -size 48x32 -seed 7 plasma:fractal -sampling-factor 4:2:0 \
//     -define jpeg:restart-interval=2 tests/fixtures/jpegtran/restart_420.jpg
#[test]
fn handles_real_jpeg_with_restart_markers() {
    let bytes = include_bytes!("../../tests/fixtures/jpegtran/restart_420.jpg");
    // 48×32 is MCU-aligned for 4:2:0, so it MUST be in scope for the lossless
    // transform (a None here would mean the restart-marker path is being skipped,
    // silently losing this coverage — assert it's actually exercised).
    let mut cur = transform(bytes, Op::Rot90)
        .expect("block-aligned restart-marker JPEG must be lossless-transformable");
    // rot90×4 must be coefficient-identity even with restart markers + subsampled chroma.
    for _ in 0..3 {
        cur = transform(&cur, Op::Rot90).expect("subsequent rot90");
    }
    let a = image::load_from_memory(bytes).unwrap().to_rgb8().into_raw();
    let b = image::load_from_memory(&cur).unwrap().to_rgb8().into_raw();
    assert_eq!(
        a, b,
        "rot90×4 of a real restart-marker JPEG must be identity"
    );
}

/// Out-of-scope inputs (non-block-aligned dims) return None so the caller
/// falls back to a lossy re-encode rather than mangle the edge.
#[test]
fn non_aligned_returns_none() {
    let mut g = image::GrayImage::new(30, 20); // not a multiple of 8
    for (x, y, p) in g.enumerate_pixels_mut() {
        *p = image::Luma([((x + y) % 256) as u8]);
    }
    let jpeg = encode_gray_jpeg(g);
    assert!(
        transform(&jpeg, Op::Rot90).is_none(),
        "non-aligned dims should bail"
    );
}

/// An AC run-length that pushes the coefficient index past 63 (a corrupt or crafted Huffman
/// table/data) must decline the block entirely, not return a truncated one. Before the fix,
/// `decode_block` silently `break`d out of the loop and returned `Some(blk)` with the tail
/// left zeroed — a wrong-but-"successful" lossless transform. Drives `decode_block` directly
/// with a single-code DC table (t=0, diff=0) and a single-code AC table mapping to (run=15,
/// size=1): each successful iteration advances k by 16 (15-run + the coefficient itself),
/// so k goes 1 -> 17 -> 33 -> 49, then the fourth symbol's run alone (+15) reaches 64 before
/// a coefficient is stored, tripping the guard.
#[test]
fn ac_run_length_past_63_declines_instead_of_truncating() {
    let dc = build_dec(&[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], &[0]).unwrap();
    let ac = build_dec(&[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], &[0xF1]).unwrap();
    // Every code AND every data bit here is 1-bit-"0", so 9 zero bits cover the DC symbol
    // plus 4 AC iterations (huffman bit + 1 data bit each); the trailing zero bits are
    // never reached once the guard fires.
    let data = [0x00u8, 0x00u8];
    let mut br = BitReader::new(&data, 0);
    let mut pred = 0i32;
    assert!(
        decode_block(&mut br, &dc, &ac, &mut pred).is_none(),
        "a run-length overflowing the 64-coefficient block must decline (None)"
    );
}
