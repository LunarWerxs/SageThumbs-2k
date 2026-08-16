//! Pathological IMAGE DIMENSIONS through the real thumbnail path.
//!
//! WHY THIS EXISTS. The regression corpus (`..\test-corpus`) is one sample per FORMAT, and
//! almost every one of those samples is the same 512x384 base image: a survey of it found
//! six distinct sizes across 329 files (512x384, its 384x512 rotation, three tiny squares,
//! and one contact sheet). So the corpus proves "we can open a PSD" and proves nothing at
//! all about "we can open a PSD whose width is odd", which is a completely different class
//! of bug. Dimension bugs do not live in the container parsing, they live in the arithmetic
//! underneath it: chroma subsampling wants even edges, JPEG codes in 8x8 blocks and scales
//! by eighths, the JPEG 2000 5/3 wavelet splits odd spans unevenly, and fit-to-box divides.
//!
//! It is an INTEGRATION test on purpose, for two reasons. It needs no corpus, so unlike
//! `container::fuzzseed`'s neighbours it is just as strong on a CI runner as on this desk
//! (the sibling corpus is not in git and CI never checks it out). And it drives the same
//! public entry point the shell does, so it exercises the tier dispatch rather than one
//! decoder in isolation.
//!
//! Every case asserts the two things a thumbnail must never get wrong regardless of size:
//! it must not panic (the shell builds `panic = "abort"`, so a panic here is the user's
//! Explorer dying), and it must come back inside the requested box with the source aspect
//! ratio preserved. "It returned an error" is an acceptable answer for a hostile size; "it
//! panicked" and "it returned a mis-shaped tile" are not.

use sagethumbs2k_core::decode;

/// A deterministic PNG at exactly `w` x `h`.
///
/// Content is a position-dependent pattern, not a flat fill: a flat image compresses to
/// almost nothing and, more importantly, hides an off-by-one in a resize behind pixels that
/// all look alike. This makes a wrong row stride visible in principle and keeps the encoded
/// size proportional to the real pixel count.
fn png(w: u32, h: u32) -> Vec<u8> {
    let mut img = image::RgbImage::new(w, h);
    for y in 0..h {
        for x in 0..w {
            img.put_pixel(
                x,
                y,
                image::Rgb([
                    ((x * 7 + y * 13) & 0xFF) as u8,
                    ((x * 31 + y * 3) & 0xFF) as u8,
                    ((x ^ y) & 0xFF) as u8,
                ]),
            );
        }
    }
    let mut out = Vec::new();
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .expect("encode png");
    out
}

/// The same, as a JPEG. Separate because JPEG is the format whose arithmetic is most
/// dimension-sensitive: 8x8 DCT blocks, 16x16 MCUs once chroma is subsampled, and a
/// codec-scaled decode path that only offers eighths.
fn jpeg(w: u32, h: u32) -> Vec<u8> {
    let mut img = image::RgbImage::new(w, h);
    for y in 0..h {
        for x in 0..w {
            img.put_pixel(
                x,
                y,
                image::Rgb([
                    ((x * 7 + y * 13) & 0xFF) as u8,
                    ((x * 31 + y * 3) & 0xFF) as u8,
                    ((x ^ y) & 0xFF) as u8,
                ]),
            );
        }
    }
    let mut out = Vec::new();
    image::DynamicImage::ImageRgb8(img)
        .write_to(
            &mut std::io::Cursor::new(&mut out),
            image::ImageFormat::Jpeg,
        )
        .expect("encode jpeg");
    out
}

/// Sizes chosen for the arithmetic they break, not for looking varied.
///
/// Each entry names the specific hazard, because a bare list of numbers rots into
/// "someone's favourite sizes" and then gets pruned by whoever finds it noisy.
const FRINGE: &[(u32, u32, &str)] = &[
    (
        1,
        1,
        "degenerate single pixel: every ratio and divisor is 1",
    ),
    (
        1,
        512,
        "one pixel wide: aspect ratio 1:512, fit-to-box can round a side to 0",
    ),
    (512, 1, "one pixel tall: the transpose of the above"),
    (2, 2, "smaller than one 8x8 DCT block"),
    (
        7,
        7,
        "odd, prime, and under a block: no dimension divides evenly",
    ),
    (9, 9, "one past a block boundary in both axes"),
    (15, 15, "one short of an MCU in both axes"),
    (17, 17, "one past an MCU in both axes"),
    (31, 33, "odd by odd, straddling a power of two"),
    (63, 65, "either side of 64, the XCF tile edge"),
    (
        100,
        101,
        "even by odd: the chroma-subsampling asymmetry case",
    ),
    (
        101,
        100,
        "odd by even: the transpose, which is NOT the same code path",
    ),
    (
        255,
        257,
        "either side of 256, a common thumbnail request size",
    ),
    (
        511,
        513,
        "either side of 512, the DCT-scaled fast path's size floor neighbourhood",
    ),
    (
        1023,
        1025,
        "either side of 1024, the preview pane's target edge",
    ),
    (
        4096,
        3,
        "extremely wide and short: 12288 px total but a 1365:1 ratio",
    ),
    (3, 4096, "extremely tall and narrow: the transpose"),
];

/// Thumbnail requests worth trying against each size. 1 and 2 are absurd on purpose: a
/// caller CAN ask for them, and a divide-by-something-that-rounds-to-zero is exactly the
/// bug this file is looking for.
const REQUESTS: &[u32] = &[1, 2, 16, 96, 256, 1024];

/// A thumbnail must fit its box and keep its aspect ratio, at EVERY source size.
///
/// The aspect check tolerates one pixel of rounding per side, because an integer fit of an
/// odd source into an even box genuinely cannot be exact. It does not tolerate more: a
/// squashed or stretched tile is the visible symptom of the arithmetic being wrong.
fn assert_sane(tag: &str, d: &decode::Decoded, sw: u32, sh: u32, cx: u32) {
    assert!(
        d.width > 0 && d.height > 0,
        "{tag}: produced a {}x{} tile",
        d.width,
        d.height
    );
    assert!(
        d.width <= cx && d.height <= cx,
        "{tag}: {}x{} escapes the {cx}px box",
        d.width,
        d.height
    );
    assert_eq!(
        d.rgba.len(),
        (d.width as usize) * (d.height as usize) * 4,
        "{tag}: buffer length disagrees with the reported {}x{}",
        d.width,
        d.height
    );
    // Aspect, compared as a cross-product so there is no float division by a tiny side.
    let (a, b) = (
        (d.width as u64) * (sh as u64),
        (d.height as u64) * (sw as u64),
    );
    // Slack is DERIVED, not picked: one pixel of integer rounding in `width` moves the
    // cross-product by `sh`, and one pixel in `height` moves it by `sw`, so `sw + sh` is
    // exactly "both sides rounded once" and nothing looser. An earlier version used
    // `max(sw, sh)` scaled by the output, which allowed 3328 of drift on the 4096x3 case
    // and would have waved through a tile of completely the wrong shape.
    let slack = (sw as u64) + (sh as u64);
    assert!(
        a.abs_diff(b) <= slack,
        "{tag}: {}x{} is the wrong shape for a {sw}x{sh} source",
        d.width,
        d.height
    );
}

/// "A refusal is acceptable" is the right rule for ONE hostile size and a trap for a whole
/// sweep: a decoder mutated to fail unconditionally refuses every case, and a sweep that only
/// ever checks the `Ok` arm would call that a pass. So each sweep counts its successes and
/// requires at least one. The floor is one rather than a picked number because one is the only
/// value that is DERIVED — it is exactly the boundary between "declined some hostile sizes"
/// and "produced nothing at all" — and any higher floor would be an invented quota that goes
/// stale the moment a size is added to `FRINGE`. `the_sweep_harness_is_not_vacuous` carries
/// the stronger claim separately, against sizes that have no excuse to fail.
fn assert_not_vacuous(sweep: &str, decoded: usize, cases: usize) {
    assert!(
        decoded > 0,
        "{sweep}: all {cases} cases declined — that is a broken decoder, not a strict one"
    );
}

#[test]
fn png_thumbnails_survive_every_fringe_dimension() {
    let (mut decoded, mut cases) = (0usize, 0usize);
    for &(w, h, why) in FRINGE {
        let bytes = png(w, h);
        for &cx in REQUESTS {
            let tag = format!("png {w}x{h} -> {cx} ({why})");
            cases += 1;
            match decode::decode_thumbnail_opts(&bytes, cx, false) {
                Ok(d) => {
                    assert_sane(&tag, &d, w, h, cx);
                    decoded += 1;
                }
                // A refusal is allowed; a panic is not, and getting here proves there was none.
                Err(e) => println!("{tag}: declined ({e:?}), which is acceptable"),
            }
        }
    }
    assert_not_vacuous("png sweep", decoded, cases);
}

#[test]
fn jpeg_thumbnails_survive_every_fringe_dimension() {
    let (mut decoded, mut cases) = (0usize, 0usize);
    for &(w, h, why) in FRINGE {
        // A 1-px side is not expressible in some JPEG samplings; skip only those two.
        if w < 2 || h < 2 {
            continue;
        }
        let bytes = jpeg(w, h);
        for &cx in REQUESTS {
            let tag = format!("jpeg {w}x{h} -> {cx} ({why})");
            cases += 1;
            match decode::decode_thumbnail_opts(&bytes, cx, false) {
                Ok(d) => {
                    assert_sane(&tag, &d, w, h, cx);
                    decoded += 1;
                }
                Err(e) => println!("{tag}: declined ({e:?}), which is acceptable"),
            }
        }
    }
    assert_not_vacuous("jpeg sweep", decoded, cases);
}

/// The embedded-thumbnail shortcut is a SECOND path with its own arithmetic, and it is the
/// one Explorer takes for small requests, so it needs the same sweep rather than being
/// assumed equivalent to the full decode.
#[test]
fn the_embedded_thumbnail_shortcut_survives_every_fringe_dimension() {
    let (mut decoded, mut cases) = (0usize, 0usize);
    for &(w, h, why) in FRINGE {
        if w < 2 || h < 2 {
            continue;
        }
        let bytes = jpeg(w, h);
        for &cx in REQUESTS {
            let tag = format!("jpeg(embedded) {w}x{h} -> {cx} ({why})");
            cases += 1;
            match decode::decode_thumbnail_opts(&bytes, cx, true) {
                Ok(d) => {
                    assert_sane(&tag, &d, w, h, cx);
                    decoded += 1;
                }
                Err(e) => println!("{tag}: declined ({e:?}), which is acceptable"),
            }
        }
    }
    assert_not_vacuous("embedded-thumbnail sweep", decoded, cases);
}

/// `decode_preview_capped` is what the preview pane and the property probe call, and it
/// caps by longest edge rather than fitting a box, so it gets its own assertion.
#[test]
fn the_capped_preview_decode_survives_every_fringe_dimension() {
    let (mut decoded, mut cases) = (0usize, 0usize);
    for &(w, h, why) in FRINGE {
        let bytes = png(w, h);
        for &cap in &[1u32, 32, 256, 2048] {
            let tag = format!("preview_capped {w}x{h} -> {cap} ({why})");
            cases += 1;
            match decode::decode_preview_capped(&bytes, cap) {
                Ok(img) => {
                    use image::GenericImageView;
                    let (dw, dh) = img.dimensions();
                    decoded += 1;
                    assert!(dw > 0 && dh > 0, "{tag}: produced {dw}x{dh}");
                    // Never ENLARGES past the source: a preview may decline to shrink, but
                    // inventing pixels beyond the original is always wrong here.
                    assert!(
                        dw <= w.max(cap) && dh <= h.max(cap),
                        "{tag}: {dw}x{dh} exceeds both the source and the cap"
                    );
                }
                Err(e) => println!("{tag}: declined ({e:?}), which is acceptable"),
            }
        }
    }
    assert_not_vacuous("capped-preview sweep", decoded, cases);
}

/// The sweeps above tolerate a refusal per case, so on their own they can only prove "nothing
/// panicked and nothing came back mis-shaped". This is the test that says the harness is
/// measuring a working decoder at all: ordinary, unremarkable sizes through all three entry
/// points, where a refusal has no excuse and IS the failure. Without it, gutting a decoder to
/// return `Err` unconditionally would leave the file printing "declined, which is acceptable"
/// several hundred times and still exiting green.
#[test]
fn the_sweep_harness_is_not_vacuous() {
    for &(w, h) in &[(64u32, 64u32), (100, 40), (40, 100)] {
        let p = png(w, h);
        let j = jpeg(w, h);
        decode::decode_thumbnail_opts(&p, 64, false)
            .unwrap_or_else(|e| panic!("a plain {w}x{h} PNG must thumbnail, got {e:?}"));
        decode::decode_thumbnail_opts(&j, 64, false)
            .unwrap_or_else(|e| panic!("a plain {w}x{h} JPEG must thumbnail, got {e:?}"));
        decode::decode_thumbnail_opts(&j, 64, true).unwrap_or_else(|e| {
            panic!("a plain {w}x{h} JPEG must thumbnail via the shortcut, got {e:?}")
        });
        decode::decode_preview_capped(&p, 256)
            .unwrap_or_else(|e| panic!("a plain {w}x{h} PNG must preview, got {e:?}"));
    }
}
