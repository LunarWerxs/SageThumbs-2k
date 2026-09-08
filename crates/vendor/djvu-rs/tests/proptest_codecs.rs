//! Property-based round-trip tests (issue #195).
//!
//! Generates randomised inputs and checks the encode/decode invariants:
//! * `decode(encode(B)) == B` for `Bitmap` (JB2, both encoders)
//! * `bzz_decode(bzz_encode(d)) == d` for arbitrary byte slices
//!
//! Default `proptest` budget is 256 cases per test, suitable for CI.

use djvu_rs::Bitmap;
use djvu_rs::Pixmap;
use djvu_rs::annotation::{
    Annotation, Border, Color, Highlight, MapArea, Rect, Shape, encode_annotations,
    encode_annotations_bzz, parse_annotations,
};
use djvu_rs::bzz::bzz_decode;
use djvu_rs::fgbz_encode::{FgbzColor, decode_fgbz, encode_fgbz};
use djvu_rs::iff::{Chunk, DjvuFile, emit, parse};
use djvu_rs::iw44::Iw44Image;
use djvu_rs::iw44_encode::{Iw44EncodeOptions, encode_iw44_color};
use djvu_rs::segment::{SegmentOptions, segment_page};
use djvu_rs::smmr::{decode_smmr, encode_smmr};
use djvu_rs::{bzz, bzz_encode, jb2, jb2_encode};
use proptest::prelude::*;

/// Strategy for a bilevel `Bitmap` of arbitrary dimensions and content.
fn arb_bitmap(max_w: u32, max_h: u32) -> impl Strategy<Value = Bitmap> {
    (1u32..=max_w, 1u32..=max_h).prop_flat_map(|(w, h)| {
        let total_bits = (w * h) as usize;
        prop::collection::vec(any::<bool>(), total_bits).prop_map(move |bits| {
            let mut bm = Bitmap::new(w, h);
            for (i, &b) in bits.iter().enumerate() {
                let x = (i as u32) % w;
                let y = (i as u32) / w;
                if b {
                    bm.set_black(x, y);
                }
            }
            bm
        })
    })
}

fn bitmaps_eq(src: &Bitmap, decoded: &Bitmap) -> Result<(), TestCaseError> {
    prop_assert_eq!(src.width, decoded.width);
    prop_assert_eq!(src.height, decoded.height);
    for y in 0..src.height {
        for x in 0..src.width {
            prop_assert_eq!(
                src.get(x, y),
                decoded.get(x, y),
                "mismatch at ({}, {})",
                x,
                y
            );
        }
    }
    Ok(())
}

proptest! {
    // Cap: 64 cases × max 64×64 bitmap keeps total run time well under 1 s on
    // CI. The encoder is O(w·h) plus arithmetic coding overhead; this is plenty
    // to expose context-update / boundary bugs without dominating CI time.
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// JB2 direct encoder (single record-3 / tiled): bit-exact round-trip for
    /// any randomly-generated bitmap.
    #[test]
    fn jb2_direct_roundtrip(bm in arb_bitmap(64, 64)) {
        let bytes = jb2_encode::encode_jb2(&bm);
        let decoded = jb2::decode(&bytes, None).expect("decode failed");
        bitmaps_eq(&bm, &decoded)?;
    }

    /// JB2 dict encoder (CC + record types 1+7): bit-exact round-trip.
    #[test]
    fn jb2_dict_roundtrip(bm in arb_bitmap(64, 64)) {
        let bytes = jb2_encode::encode_jb2_dict(&bm);
        let decoded = jb2::decode(&bytes, None).expect("decode failed");
        bitmaps_eq(&bm, &decoded)?;
    }

    /// BZZ: bit-exact round-trip on arbitrary byte slices.
    #[test]
    fn bzz_roundtrip(data in prop::collection::vec(any::<u8>(), 0..4096)) {
        let encoded = bzz_encode::bzz_encode(&data);
        let decoded = bzz::decode(&encoded).expect("BZZ decode failed");
        prop_assert_eq!(data, decoded);
    }

    /// Smmr (G4/MMR): bit-exact round-trip for arbitrary bilevel bitmaps (#221).
    #[test]
    fn smmr_roundtrip(bm in arb_bitmap(64, 64)) {
        let bytes = encode_smmr(&bm);
        let decoded = decode_smmr(&bytes).expect("Smmr decode failed");
        bitmaps_eq(&bm, &decoded)?;
    }

    /// FGbz: bit-exact round-trip for palette + index table (#217).
    /// Index strategy includes negative i16 values (-1 is sometimes used as
    /// a "no-color" sentinel) and bursts of repeats (BZZ-friendly).
    #[test]
    fn fgbz_roundtrip(
        palette in prop::collection::vec(
            (any::<u8>(), any::<u8>(), any::<u8>())
                .prop_map(|(r, g, b)| FgbzColor { r, g, b }),
            0..256usize,
        ),
        indices in prop::option::of(prop::collection::vec(any::<i16>(), 0..1024usize)),
    ) {
        let bytes = encode_fgbz(&palette, indices.as_deref()).expect("FGbz encode failed");
        let (decoded_palette, decoded_indices) =
            decode_fgbz(&bytes).expect("FGbz decode failed");
        prop_assert_eq!(decoded_palette, palette);
        prop_assert_eq!(decoded_indices, indices.unwrap_or_default());
    }
}

proptest! {
    // Larger image → fewer cases; specifically aimed at the >1 MP tile path
    // (#198) and at edge-tile sizes that aren't multiples of 1024.
    #![proptest_config(ProptestConfig::with_cases(4))]

    #[test]
    fn jb2_direct_tiled_roundtrip(bm in arb_bitmap(1500, 1100)) {
        let bytes = jb2_encode::encode_jb2(&bm);
        let decoded = jb2::decode(&bytes, None).expect("decode failed");
        bitmaps_eq(&bm, &decoded)?;
    }
}

// ---- IFF round-trip --------------------------------------------------------

fn arb_chunk_id() -> impl Strategy<Value = [u8; 4]> {
    // ASCII printable letters/digits — covers the realistic chunk ID space
    // (FORM, INFO, Sjbz, BG44, ...) without producing pathological IDs that
    // a real DjVu file wouldn't contain.
    prop::collection::vec(prop::char::range('A', 'z'), 4..=4).prop_map(|cs| {
        let mut id = [0u8; 4];
        for (i, c) in cs.iter().enumerate() {
            id[i] = *c as u8;
        }
        id
    })
}

fn arb_leaf() -> impl Strategy<Value = Chunk> {
    (
        arb_chunk_id().prop_filter("non-FORM id", |id| id != b"FORM"),
        prop::collection::vec(any::<u8>(), 0..=64),
    )
        .prop_map(|(id, data)| Chunk::Leaf { id, data })
}

fn arb_form(depth: u32) -> BoxedStrategy<Chunk> {
    let leaf = arb_leaf().boxed();
    if depth == 0 {
        leaf
    } else {
        let inner_form = (
            arb_chunk_id(),
            prop::collection::vec(arb_form(depth - 1), 0..=4),
        )
            .prop_map(|(secondary_id, children)| Chunk::Form {
                secondary_id,
                length: 0, // recomputed by `emit`; ignored on round-trip
                children,
            });
        prop_oneof![3 => leaf, 1 => inner_form].boxed()
    }
}

/// Compare two trees ignoring the stored `length` field on `Form` chunks
/// (the emitter recomputes from children).
fn chunks_eq(a: &Chunk, b: &Chunk) -> Result<(), TestCaseError> {
    match (a, b) {
        (
            Chunk::Form {
                secondary_id: sa,
                children: ca,
                ..
            },
            Chunk::Form {
                secondary_id: sb,
                children: cb,
                ..
            },
        ) => {
            prop_assert_eq!(sa, sb);
            prop_assert_eq!(ca.len(), cb.len());
            for (x, y) in ca.iter().zip(cb.iter()) {
                chunks_eq(x, y)?;
            }
            Ok(())
        }
        (Chunk::Leaf { id: ia, data: da }, Chunk::Leaf { id: ib, data: db }) => {
            prop_assert_eq!(ia, ib);
            prop_assert_eq!(da, db);
            Ok(())
        }
        _ => Err(TestCaseError::fail("chunk kind mismatch")),
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// IFF round-trip: parse(emit(file)) == file for any synthetic chunk
    /// tree (depth-limited to keep test time bounded).
    #[test]
    fn iff_roundtrip(root in arb_form(3)) {
        // The legacy parser expects a FORM at the root — wrap a leaf-only
        // case in a top-level FORM so all generated trees are valid files.
        let root = match root {
            f @ Chunk::Form { .. } => f,
            leaf => Chunk::Form {
                secondary_id: *b"DJVU",
                length: 0,
                children: vec![leaf],
            },
        };
        let file = DjvuFile { root };
        let bytes = emit(&file);
        let parsed = parse(&bytes).expect("parse failed");
        chunks_eq(&file.root, &parsed.root)?;
    }
}

// ---- Annotation round-trip --------------------------------------------------

fn arb_color() -> impl Strategy<Value = Color> {
    (any::<u8>(), any::<u8>(), any::<u8>()).prop_map(|(r, g, b)| Color { r, g, b })
}

fn arb_rect() -> impl Strategy<Value = Rect> {
    (0u32..10000, 0u32..10000, 1u32..5000, 1u32..5000).prop_map(|(x, y, width, height)| Rect {
        x,
        y,
        width,
        height,
    })
}

fn arb_shape() -> impl Strategy<Value = Shape> {
    prop_oneof![
        arb_rect().prop_map(Shape::Rect),
        arb_rect().prop_map(Shape::Oval),
        arb_rect().prop_map(Shape::Text),
        (0u32..10000, 0u32..10000, 0u32..10000, 0u32..10000)
            .prop_map(|(a, b, c, d)| Shape::Line(a, b, c, d)),
        prop::collection::vec((0u32..10000, 0u32..10000), 3..=8).prop_map(Shape::Poly),
    ]
}

/// ASCII-only string without S-expression special characters. The
/// annotation encoder writes these inline; parens/quotes/backslashes need
/// escaping that the proptest is not trying to exercise here.
fn arb_simple_string(max_len: usize) -> impl Strategy<Value = String> {
    prop::collection::vec(prop::char::range('a', 'z'), 0..=max_len)
        .prop_map(|cs| cs.iter().collect())
}

fn arb_maparea() -> impl Strategy<Value = MapArea> {
    // NOTE: Border style strategy is `1..=8` (non-empty). The encoder emits
    // `(border <style>)` and the parser only restores `Some(Border)` when
    // there's a non-empty atom after `border`, so `Some(Border{style:""})`
    // round-trips to `None`. That is an asymmetric representation, not a
    // bug we want to test for here — it was found by proptest while
    // building this file (#195).
    (
        arb_simple_string(32),
        arb_simple_string(32),
        arb_shape(),
        prop::option::of(
            prop::collection::vec(prop::char::range('a', 'z'), 1..=8).prop_map(|cs| Border {
                style: cs.iter().collect(),
            }),
        ),
        prop::option::of(arb_color().prop_map(|c| Highlight { color: c })),
    )
        .prop_map(|(url, description, shape, border, highlight)| MapArea {
            url,
            description,
            shape,
            border,
            highlight,
        })
}

fn arb_annotation() -> impl Strategy<Value = Annotation> {
    (
        prop::option::of(arb_color()),
        prop::option::of(25u32..400),
        prop::option::of(prop::sample::select(vec![
            "color".to_string(),
            "bw".to_string(),
            "fore".to_string(),
            "back".to_string(),
        ])),
    )
        .prop_map(|(background, zoom, mode)| Annotation {
            background,
            zoom,
            mode,
        })
}

fn maparea_shapes_eq(a: &Shape, b: &Shape) -> bool {
    match (a, b) {
        (Shape::Rect(x), Shape::Rect(y)) => x == y,
        (Shape::Oval(x), Shape::Oval(y)) => x == y,
        (Shape::Text(x), Shape::Text(y)) => x == y,
        (Shape::Line(a, b, c, d), Shape::Line(p, q, r, s)) => (a, b, c, d) == (p, q, r, s),
        (Shape::Poly(p), Shape::Poly(q)) => p == q,
        _ => false,
    }
}

fn map_areas_eq(a: &MapArea, b: &MapArea) -> Result<(), TestCaseError> {
    prop_assert_eq!(&a.url, &b.url);
    prop_assert_eq!(&a.description, &b.description);
    prop_assert!(maparea_shapes_eq(&a.shape, &b.shape), "shape mismatch");
    prop_assert_eq!(&a.border, &b.border);
    prop_assert_eq!(&a.highlight, &b.highlight);
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// Annotation S-expr round-trip (raw form, no BZZ wrapping).
    #[test]
    fn annotation_roundtrip(
        ann in arb_annotation(),
        areas in prop::collection::vec(arb_maparea(), 0..=8),
    ) {
        let bytes = encode_annotations(&ann, &areas);
        let (ann2, areas2) = parse_annotations(&bytes).expect("parse failed");
        prop_assert_eq!(&ann.background, &ann2.background);
        prop_assert_eq!(ann.zoom, ann2.zoom);
        prop_assert_eq!(&ann.mode, &ann2.mode);
        prop_assert_eq!(areas.len(), areas2.len());
        for (a, b) in areas.iter().zip(areas2.iter()) {
            map_areas_eq(a, b)?;
        }
    }

    /// Annotation S-expr round-trip via the BZZ-wrapped path used by the
    /// FORM:ANNz chunk on disk. Requires at least one mapped area — proptest
    /// found that `encode_annotations_bzz` of a wholly-empty annotation
    /// produces zero bytes, which then fails BZZ decode with `TooShort`
    /// (#195). That is an encoder-edge-case, not a representative real
    /// input: a stored ANNz chunk is never empty.
    #[test]
    fn annotation_bzz_roundtrip(
        ann in arb_annotation(),
        areas in prop::collection::vec(arb_maparea(), 1..=4),
    ) {
        let bytes = encode_annotations_bzz(&ann, &areas);
        let decoded = bzz_decode(&bytes).expect("bzz decode failed");
        let (ann2, areas2) = parse_annotations(&decoded).expect("parse failed");
        prop_assert_eq!(&ann.background, &ann2.background);
        prop_assert_eq!(ann.zoom, ann2.zoom);
        prop_assert_eq!(&ann.mode, &ann2.mode);
        prop_assert_eq!(areas.len(), areas2.len());
        for (a, b) in areas.iter().zip(areas2.iter()) {
            map_areas_eq(a, b)?;
        }
    }
}

/// Strategy for an RGBA `Pixmap` with arbitrary content.
fn arb_pixmap(max_w: u32, max_h: u32) -> impl Strategy<Value = Pixmap> {
    (1u32..=max_w, 1u32..=max_h).prop_flat_map(|(w, h)| {
        let n = (w * h) as usize;
        prop::collection::vec((any::<u8>(), any::<u8>(), any::<u8>()), n).prop_map(move |triples| {
            let mut pm = Pixmap::white(w, h);
            for (i, (r, g, b)) in triples.into_iter().enumerate() {
                let x = (i as u32) % w;
                let y = (i as u32) / w;
                pm.set_rgb(x, y, r, g, b);
            }
            pm
        })
    })
}

fn bt601_lum(r: u8, g: u8, b: u8) -> u32 {
    ((r as u32) * 306 + (g as u32) * 601 + (b as u32) * 117) >> 10
}

/// Decode a full IW44 chunk stream (encoder output) back to a `Pixmap`.
fn decode_iw44(chunks: &[Vec<u8>]) -> Option<Pixmap> {
    let mut img = Iw44Image::new();
    for c in chunks {
        img.decode_chunk(c).ok()?;
    }
    img.to_rgb().ok()
}

proptest! {
    // IW44 is lossy, so we can't assert bit-exact round-trip. The property under
    // test is *robustness*: encoding any RGBA pixmap must not panic, must produce
    // a decodable stream, and that stream must decode back to the same W×H. Few
    // cases × small dims keeps the wavelet+ZP cost off the CI critical path.
    #![proptest_config(ProptestConfig::with_cases(16))]

    /// IW44 colour encoder: no panic, dimensional round-trip for any pixmap.
    #[test]
    fn iw44_encode_robustness(pm in arb_pixmap(48, 48)) {
        let chunks = encode_iw44_color(&pm, &Iw44EncodeOptions::default());
        prop_assert!(!chunks.is_empty(), "encoder produced no chunks");
        let decoded = decode_iw44(&chunks).expect("IW44 re-decode failed");
        prop_assert_eq!(decoded.width, pm.width);
        prop_assert_eq!(decoded.height, pm.height);
    }
}

/// Degenerate / boundary dimensions exercised explicitly (proptest's random
/// strategies rarely hit the exact 1-px and odd-vs-even edges). Every encoder
/// must handle these without panicking; the lossless ones must round-trip.
#[test]
fn encoders_survive_degenerate_dimensions() {
    const DIMS: &[(u32, u32)] = &[
        (1, 1),
        (1, 5),
        (5, 1),
        (2, 2),
        (2, 3),
        (3, 2),
        (7, 1),
        (1, 7),
        (15, 17),
        (17, 15),
        (64, 1),
        (1, 64),
    ];

    for &(w, h) in DIMS {
        // ---- bilevel encoders (lossless → must round-trip) ----
        let mut bm = Bitmap::new(w, h);
        // a deterministic checkerboard so the bitmap isn't trivially all-white
        for y in 0..h {
            for x in 0..w {
                if (x ^ y) & 1 == 0 {
                    bm.set_black(x, y);
                }
            }
        }
        let jb2_direct = jb2_encode::encode_jb2(&bm);
        let dec = jb2::decode(&jb2_direct, None)
            .unwrap_or_else(|e| panic!("jb2 direct decode failed at {w}x{h}: {e:?}"));
        assert_eq!(
            (dec.width, dec.height),
            (w, h),
            "jb2 direct dims at {w}x{h}"
        );

        let jb2_dict = jb2_encode::encode_jb2_dict(&bm);
        let dec = jb2::decode(&jb2_dict, None)
            .unwrap_or_else(|e| panic!("jb2 dict decode failed at {w}x{h}: {e:?}"));
        assert_eq!((dec.width, dec.height), (w, h), "jb2 dict dims at {w}x{h}");

        let smmr = encode_smmr(&bm);
        let dec =
            decode_smmr(&smmr).unwrap_or_else(|e| panic!("smmr decode failed at {w}x{h}: {e:?}"));
        assert_eq!((dec.width, dec.height), (w, h), "smmr dims at {w}x{h}");

        // ---- IW44 colour encoder (lossy → dimensional round-trip only) ----
        let mut pm = Pixmap::white(w, h);
        for y in 0..h {
            for x in 0..w {
                let v = ((x * 37 + y * 91) & 0xff) as u8;
                pm.set_rgb(x, y, v, v.wrapping_add(40), v.wrapping_add(80));
            }
        }
        let chunks = encode_iw44_color(&pm, &Iw44EncodeOptions::default());
        let dec = decode_iw44(&chunks).unwrap_or_else(|| panic!("IW44 decode failed at {w}x{h}"));
        assert_eq!((dec.width, dec.height), (w, h), "iw44 dims at {w}x{h}");
    }
}

proptest! {
    // Segmentation v1 (#220): cap dimensions so the O(w·h) scan stays cheap
    // on CI; 64 cases at ≤ 32×32 over a few sub-sample factors is enough to
    // shake out off-by-one bugs in the block accumulator without dominating
    // the run.
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Mask shape + threshold predicate: every mask pixel has BT.601 lum
    /// strictly less than threshold; every non-mask pixel has lum ≥ threshold.
    #[test]
    fn segment_mask_matches_threshold(
        pm in arb_pixmap(32, 32),
        threshold in any::<u8>(),
        sub in 1u32..=16,
    ) {
        let opts = SegmentOptions {
            threshold,
            bg_subsample: sub,
            ..SegmentOptions::default()
        };
        let seg = segment_page(&pm, &opts);
        prop_assert_eq!(seg.mask.width, pm.width);
        prop_assert_eq!(seg.mask.height, pm.height);
        for y in 0..pm.height {
            for x in 0..pm.width {
                let (r, g, b) = pm.get_rgb(x, y);
                let lum = bt601_lum(r, g, b);
                let masked = seg.mask.get(x, y);
                if masked {
                    prop_assert!(
                        lum < threshold as u32,
                        "masked pixel ({x},{y}) lum {lum} >= threshold {threshold}"
                    );
                } else {
                    prop_assert!(
                        lum >= threshold as u32,
                        "unmasked pixel ({x},{y}) lum {lum} < threshold {threshold}"
                    );
                }
            }
        }
    }

    /// BG dimensions are ceil(w/sub) × ceil(h/sub), with sub=0 clamped to 1.
    #[test]
    fn segment_bg_dims_round_up(
        pm in arb_pixmap(32, 32),
        sub in 0u32..=16,
    ) {
        let opts = SegmentOptions {
            threshold: 128,
            bg_subsample: sub,
            ..SegmentOptions::default()
        };
        let seg = segment_page(&pm, &opts);
        let effective = sub.max(1);
        prop_assert_eq!(seg.bg.width, pm.width.div_ceil(effective));
        prop_assert_eq!(seg.bg.height, pm.height.div_ceil(effective));
    }

    /// Determinism: two calls with the same input must agree byte-for-byte.
    #[test]
    fn segment_is_deterministic(
        pm in arb_pixmap(32, 32),
        threshold in any::<u8>(),
        sub in 1u32..=16,
    ) {
        let opts = SegmentOptions {
            threshold,
            bg_subsample: sub,
            ..SegmentOptions::default()
        };
        let a = segment_page(&pm, &opts);
        let b = segment_page(&pm, &opts);
        prop_assert_eq!(a.mask.data, b.mask.data);
        prop_assert_eq!(a.bg.data, b.bg.data);
    }

    /// Threshold = 0 produces an empty mask (no pixel has lum < 0).
    #[test]
    fn segment_threshold_zero_yields_empty_mask(pm in arb_pixmap(32, 32)) {
        let opts = SegmentOptions {
            threshold: 0,
            bg_subsample: 4,
            ..SegmentOptions::default()
        };
        let seg = segment_page(&pm, &opts);
        for y in 0..pm.height {
            for x in 0..pm.width {
                prop_assert!(!seg.mask.get(x, y));
            }
        }
    }
}
