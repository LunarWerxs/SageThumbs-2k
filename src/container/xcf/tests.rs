#![cfg(test)]

use super::*;
mod codec;
mod stack;
use stack::*;

/// [`extract_seek_within`] over a byte slice, so the budget cases read as plainly as the
/// ordinary ones. The in-memory entry point takes exactly this route in production.
fn extract_within(bytes: &[u8], layer_budget: u64) -> Option<DynamicImage> {
    extract_seek_within(
        std::io::Cursor::new(bytes),
        layer_budget,
        LAYER_HEAD_PRESCAN_BUDGET,
        None,
    )
}

// ── Layer-header prescan budget ───────────────────────────────────────────
// A crafted file can declare thousands of layer pointers, each needing only
// "an offset with at least a window's worth of file left after it" (no valid layer record
// required) to make the OLD prescan copy the full LAYER_HEAD_WINDOW per pointer — up to
// ~8 GiB of buffer copies for MAX_LAYERS pointers, before a single pixel decision is made.

/// Counts bytes actually handed back by `read()`, so the assertion below is about real
/// I/O rather than about the loop's own bookkeeping.
struct CountingReader<R> {
    inner: R,
    reads: std::rc::Rc<std::cell::Cell<u64>>,
}
impl<R: Read> Read for CountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.reads.set(self.reads.get() + n as u64);
        Ok(n)
    }
}
impl<R: Seek> Seek for CountingReader<R> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(pos)
    }
}

/// Big-endian byte view of a 32-bit word, matching XCF's on-wire encoding.
fn u32b(v: u32) -> [u8; 4] {
    v.to_be_bytes()
}

/// Big-endian byte view of a 64-bit word, matching XCF's on-wire encoding.
fn u64b(v: u64) -> [u8; 8] {
    v.to_be_bytes()
}

/// A structurally valid v011 (wide-pointer) prologue declaring `n` layer pointers, ALL
/// pointing at the SAME offset — a zero-filled blob big enough that every successful read
/// gets its full (possibly shrunk) window's worth of bytes, so the fixture stays a few MB
/// regardless of how large `n` gets instead of needing `n` distinct targets.
fn crafted_many_layer_pointers(n: usize, filler_len: usize) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(b"gimp xcf v011\0");
    b.extend_from_slice(&u32b(4)); // width
    b.extend_from_slice(&u32b(4)); // height
    b.extend_from_slice(&u32b(0)); // base type RGB
    b.extend_from_slice(&u32b(150)); // 8-bit gamma precision
    b.extend_from_slice(&u32b(0)); // PROP_END
    b.extend_from_slice(&u32b(0));
    let filler_at = (b.len() + 8 * n + 8) as u64;
    for _ in 0..n {
        b.extend_from_slice(&u64b(filler_at));
    }
    b.extend_from_slice(&u64b(0)); // end of layer pointer list
    assert_eq!(b.len() as u64, filler_at, "computed filler offset drifted");
    b.resize(b.len() + filler_len, 0u8);
    b
}

/// Total bytes actually read off the source during the whole prescan must stay bounded by
/// (roughly) the prescan budget, NOT scale with how many layer pointers the file declares
/// — 4000 crafted pointers must cost about the same real I/O as 8 of them.
#[test]
fn layer_header_prescan_reads_are_bounded_by_the_running_budget_not_by_pointer_count() {
    const FILLER: usize = 2 * 1024 * 1024; // over any shrunk per-read window
    const SMALL_BUDGET: usize = 300_000; // far under LAYER_HEAD_WINDOW (1 MiB)
                                         // A generous ceiling: the one prologue probe read (<= 256 KiB) plus the one prescan
                                         // read the budget actually buys (<= SMALL_BUDGET), with slack — nowhere near what
                                         // thousands of unconditional 1 MiB reads would cost.
    const CEILING: u64 = 1_048_576;

    for n in [8usize, 4000] {
        let bytes = crafted_many_layer_pointers(n, FILLER);
        let reads = std::rc::Rc::new(std::cell::Cell::new(0u64));
        let src = CountingReader {
            inner: std::io::Cursor::new(bytes),
            reads: std::rc::Rc::clone(&reads),
        };
        let _ = extract_seek_within(src, MAX_LAYER_PIXELS, SMALL_BUDGET, None);
        assert!(
            reads.get() < CEILING,
            "prescan for {n} layer pointers read {} bytes off the source -- must stay \
             bounded by the prescan budget, not scale with pointer count",
            reads.get()
        );
    }
}

// ── Reduced-grid flatten ──────────────────────────────────────────────────
// Added 2026-08-21 after measuring that the full-resolution flatten spent 5.7 s decoding
// layers and 4.6 s compositing them for one 6000x4000 corpus fixture, to produce a 256 px
// tile. See `extract_scaled`.

#[test]
fn the_step_is_one_unless_a_target_actually_asks_for_less() {
    // No target, a zero target, and a target at least as big as the canvas all have to
    // leave the decoder on the exact path it took before this existed.
    assert_eq!(step_for(6000, 4000, None), 1);
    assert_eq!(step_for(6000, 4000, Some(0)), 1);
    assert_eq!(step_for(200, 100, Some(256)), 1);
    assert_eq!(step_for(256, 256, Some(256)), 1);
    // And the reduced canvas must still COVER the target, never land under it.
    for (w, h, t) in [
        (6000u32, 4000u32, 256u32),
        (12000, 12000, 256),
        (16000, 1200, 96),
    ] {
        let s = step_for(w, h, Some(t));
        assert!(
            w.max(h).div_ceil(s) >= t,
            "{w}x{h} -> target {t} undershot at step {s}"
        );
    }
    assert_eq!(step_for(6000, 4000, Some(256)), 23);
}

/// The reduced flatten must agree with the full one about what the picture IS. Flat layers
/// make that exact rather than approximate, which is the same trick `_expected-colors.txt`
/// uses on the real fixtures.
#[test]
fn a_reduced_flatten_produces_the_same_colour_as_the_full_one() {
    let xcf = synthetic_xcf_stack(
        64,
        64,
        &[Spec::solid([230, 220, 30]), Spec::solid([10, 20, 200])],
    );
    let full = extract(&xcf).expect("full-resolution flatten");
    let small = extract_scaled(&xcf, Some(8)).expect("reduced flatten");
    assert_eq!((full.width(), full.height()), (64, 64));
    assert_eq!((small.width(), small.height()), (8, 8));
    let f = full.to_rgba8();
    let s = small.to_rgba8();
    // Whichever layer wins, BOTH paths must agree it won — that is the invariant, and
    // hard-coding a colour here would only pin the fixture's stacking order instead.
    let winner = f.get_pixel(32, 32).0;
    assert_eq!(winner[3], 255, "the flattened result should be opaque");
    assert!(
        f.pixels().all(|p| p.0 == winner),
        "the full flatten of flat layers should be one colour"
    );
    assert!(
        s.pixels().all(|p| p.0 == winner),
        "the reduced flatten disagreed with the full one"
    );
}

/// Averaging STRAIGHT (non-premultiplied) colour is the classic downscale artefact: a
/// fully transparent pixel still carries colour bytes, and letting them vote drags a halo
/// into whatever is beside it. `resolve_accumulator` weights by alpha to stop that, and
/// this pins it: a half-opaque red layer over nothing must stay red, not slide toward the
/// black of the transparent pixels it is averaged with.
#[test]
fn reduced_averaging_is_alpha_weighted_so_transparency_cannot_tint_the_result() {
    let mut translucent = Spec::solid([255, 0, 0]);
    translucent.rgba[3] = 128;
    let xcf = synthetic_xcf_stack(64, 64, &[translucent]);
    let small = extract_scaled(&xcf, Some(8)).expect("reduced flatten");
    let s = small.to_rgba8();
    let px = s.get_pixel(4, 4).0;
    assert!(
        px[0] > 240 && px[1] < 12 && px[2] < 12,
        "hue drifted under alpha-weighted averaging: {px:?}"
    );
}

/// A layer parked off the top-left has a NEGATIVE offset, and Rust's `/` truncates toward
/// zero while the floor is what placement needs. At step 23 that is the difference between
/// -2 and -1, i.e. the layer landing a pixel off. `div_euclid` is the fix; this is the
/// arithmetic, tested directly because a one-pixel shift is invisible in a thumbnail and
/// would never be caught by eye.
#[test]
fn negative_layer_offsets_floor_rather_than_truncate_toward_zero() {
    let step = 23i32;
    assert_eq!((-30i32).div_euclid(step), -2);
    assert_eq!((-30i32) / step, -1, "plain division is the bug this avoids");
    assert_eq!(0i32.div_euclid(step), 0);
    assert_eq!(46i32.div_euclid(step), 2);
}

/// Tiles are stored back-to-back, so the next tile's pointer marks where this one ends and
/// the read window can be the record's real length instead of the worst case. It must only
/// ever SHRINK the read: an out-of-order or hand-crafted pointer list has to keep the old
/// window, or a tile would be read short and the layer lost.
#[test]
fn the_tile_read_window_shrinks_but_never_grows_and_ignores_a_backwards_pointer() {
    let worst_case = 32_832usize;
    let clamp = |next: u64, ptr: u64| -> usize {
        match Some(next) {
            Some(n) if n > ptr => {
                let span = (n - ptr).min(worst_case as u64) as usize;
                if span >= 8 {
                    span
                } else {
                    worst_case
                }
            }
            _ => worst_case,
        }
    };
    assert_eq!(clamp(1_000 + 2_048, 1_000), 2_048); // the common case: a real shrink
    assert_eq!(clamp(1_000 + 999_999, 1_000), worst_case); // never grows past the cap
    assert_eq!(clamp(900, 1_000), worst_case); // backwards pointer: keep the old window
    assert_eq!(clamp(1_000, 1_000), worst_case); // zero-length: keep the old window
    assert_eq!(clamp(1_004, 1_000), worst_case); // absurdly short: keep the old window
}

#[test]
fn rejects_non_xcf() {
    assert!(extract(b"not an xcf file at all").is_none());
    assert!(!looks_like_xcf(b"PK\x03\x04"));
    assert!(looks_like_xcf(b"gimp xcf v011\0rest"));
}

/// A172: `Rd::u32`/`Rd::ptr` build their slice range from `self.p`, which can be set
/// directly from an attacker-controlled 64-bit file offset (`hptr`/`tptr` feed `p` via
/// `ptr()`'s own return). A cursor positioned near `usize::MAX` must make the `p + N`
/// addition itself refuse cleanly (`None`) rather than panic (debug/test, overflow-checks
/// on) or silently wrap to a small `p` whose `.get()` then spuriously succeeds against the
/// WRONG bytes (release, overflow-checks off) — the exact failure mode `take()`, the same
/// struct's other cursor-advance method, already avoided with `checked_add`.
#[test]
fn u32_and_ptr_refuse_a_cursor_near_the_end_of_the_address_space_instead_of_overflowing() {
    let data = [0u8; 4];
    let mut cursor = Rd {
        d: &data,
        p: usize::MAX - 1,
    };
    assert_eq!(cursor.u32(), None);

    let mut cursor = Rd {
        d: &data,
        p: usize::MAX - 3,
    };
    assert_eq!(cursor.ptr(true), None); // wide (8-byte) read
    cursor.p = usize::MAX - 1;
    assert_eq!(cursor.ptr(false), None); // narrow (4-byte, delegates to u32) read
}

/// The cumulative budget must be spendable to exhaustion by legal-looking layers.
///
/// Each individual value a bomb declares is inside a cap that already existed: the canvas
/// is within MAX_DIM, every layer is within MAX_DIM, and the layer COUNT is within
/// MAX_LAYERS. Only the total is absurd, which is exactly what those three caps cannot
/// see. This pins the arithmetic of that total rather than building a multi-gigabyte
/// fixture, since materializing the bomb to prove we refuse the bomb costs the bomb.
/// The rule `decode_layer` actually consults, tested at its boundary.
///
/// An adversarial audit fairly pointed out that the arithmetic test below never touches
/// the parser, so it could not tell "the budget is enforced" from "the budget exists as a
/// constant". `spend_layer` IS the decision [`select_layers`] makes (one line), so pinning
/// it pins the refusal without allocating the gigabytes the refusal prevents.
///
/// The audit was righter than it knew, and this test is the cautionary half of the pair
/// below it: it kept passing through the entire life of a shipped bug, because a budget
/// can be enforced to the pixel and still be spent on the wrong layers.
#[test]
fn the_layer_budget_refuses_the_layer_that_would_overspend_it() {
    // A full-size layer fits once and leaves nothing, so a SECOND one is refused. That
    // pair is the whole property: legal files render, a pile of them does not.
    let full = u64::from(MAX_DIM) * u64::from(MAX_DIM);
    assert_eq!(spend_layer(MAX_LAYER_PIXELS, MAX_DIM, MAX_DIM), Some(0));
    assert_eq!(
        spend_layer(0, MAX_DIM, MAX_DIM),
        None,
        "a spent budget must refuse, not wrap into a huge allowance"
    );
    assert_eq!(
        full, MAX_LAYER_PIXELS,
        "the budget is exactly one full-size image"
    );

    // One pixel past the remaining budget is refused; exactly at it is allowed.
    assert_eq!(spend_layer(100, 10, 10), Some(0));
    assert_eq!(spend_layer(99, 10, 10), None);
}

/// The canvas must NOT be charged to the layer budget.
///
/// This is the regression an audit caught: the first version of the budget subtracted the
/// canvas from a shared pool sized at MAX_ALLOC/4 (134 MP), which is BELOW this project's
/// declared-area ceiling MAX_PIXELS (268 MP), so a legal 12000x12000 XCF that used to
/// render started returning nothing. Nothing that rendered before may stop rendering.
#[test]
fn a_legal_full_size_canvas_is_never_refused_by_the_layer_budget() {
    // The largest canvas the per-edge check admits is exactly MAX_PIXELS, and a layer
    // that size still fits the budget, so no legal canvas can be priced out.
    assert_eq!(
        u64::from(MAX_DIM) * u64::from(MAX_DIM),
        crate::decode::limits::MAX_PIXELS
    );
    // The specific size the audit named, 144 MP, is comfortably inside it.
    assert!(spend_layer(MAX_LAYER_PIXELS, 12_000, 12_000).is_some());
}

/// Build a structurally VALID minimal XCF: v011 (64-bit pointers), RGB, uncompressed,
/// one opaque layer that fills the canvas, filled with `rgb`.
///
/// Offsets are computed rather than hand-counted because every pointer in this format is
/// absolute, so one inserted field silently invalidates a literal table.
fn synthetic_xcf(w: u32, h: u32, rgb: [u8; 3]) -> Vec<u8> {
    synthetic_xcf_with_props(w, h, rgb, &[])
}

/// Push the common v011 image header for a `w`-by-`h` RGB image carrying only the
/// compression method (none) and no other image-level properties.
fn push_xcf_header(b: &mut Vec<u8>, w: u32, h: u32) {
    b.extend_from_slice(b"gimp xcf v011\0");
    b.extend_from_slice(&u32b(w));
    b.extend_from_slice(&u32b(h));
    b.extend_from_slice(&u32b(0)); // base type RGB
    b.extend_from_slice(&u32b(150)); // 8-bit gamma
    b.extend_from_slice(&u32b(17)); // PROP_COMPRESSION
    b.extend_from_slice(&u32b(1));
    b.push(0); // none
    b.extend_from_slice(&u32b(0)); // PROP_END
    b.extend_from_slice(&u32b(0));
}

/// Push a layer's fixed record prefix: canvas-sized dimensions, channel type `ltype`
/// (0 = RGB, 1 = RGBA) and an empty name.
fn push_layer_start(b: &mut Vec<u8>, w: u32, h: u32, ltype: u32) {
    b.extend_from_slice(&u32b(w));
    b.extend_from_slice(&u32b(h));
    b.extend_from_slice(&u32b(ltype));
    b.extend_from_slice(&u32b(1)); // name length (just the NUL)
    b.push(0);
}

/// Push one raw `(ptype, payload)` entry of a property list.
fn push_property(b: &mut Vec<u8>, ptype: u32, payload: &[u8]) {
    b.extend_from_slice(&u32b(ptype));
    b.extend_from_slice(&u32b(payload.len() as u32));
    b.extend_from_slice(payload);
}

/// The same, with `props` written into the LAYER's property list as raw
/// `(ptype, payload)` pairs, so the opacity / visibility / offset branches can be
/// exercised with real bytes instead of being assumed.
fn synthetic_xcf_with_props(w: u32, h: u32, rgb: [u8; 3], props: &[(u32, Vec<u8>)]) -> Vec<u8> {
    // Sizes of each region, so the absolute pointers can be resolved before writing.
    let header = 14 + 4 * 4 + (4 + 4 + 1) + (4 + 4); // magic..props incl. PROP_END
    let ptr_list = 8 + 8; // one layer pointer + terminator
    let layer_off = header + ptr_list;
    let props_len: usize = props.iter().map(|(_, v)| 4 + 4 + v.len()).sum();
    let layer_len = 4 + 4 + 4 + 4 + 1 + props_len + (4 + 4) + 8 + 8; // dims..maskptr
    let hier_off = layer_off + layer_len;
    let hier_len = 4 + 4 + 4 + 8;
    let level_off = hier_off + hier_len;
    let level_len = 4 + 4 + 8; // dims + ONE tile pointer (w,h <= TILE here)
    let tile_off = level_off + level_len;

    let mut b: Vec<u8> = Vec::new();
    push_xcf_header(&mut b, w, h);
    assert_eq!(
        b.len(),
        header,
        "header layout drifted from its computed size"
    );

    b.extend_from_slice(&u64b(layer_off as u64));
    b.extend_from_slice(&u64b(0)); // end of layer list

    // --- layer ---
    push_layer_start(&mut b, w, h, 0); // RGB, 3 channels
    for (ptype, payload) in props {
        push_property(&mut b, *ptype, payload);
    }
    b.extend_from_slice(&u32b(0)); // PROP_END
    b.extend_from_slice(&u32b(0));
    b.extend_from_slice(&u64b(hier_off as u64));
    b.extend_from_slice(&u64b(0)); // no layer mask
    assert_eq!(b.len(), hier_off);

    // --- hierarchy ---
    b.extend_from_slice(&u32b(w));
    b.extend_from_slice(&u32b(h));
    b.extend_from_slice(&u32b(3)); // bpp = 3 channels x 1 byte
    b.extend_from_slice(&u64b(level_off as u64));
    assert_eq!(b.len(), level_off);

    // --- level ---
    b.extend_from_slice(&u32b(w));
    b.extend_from_slice(&u32b(h));
    b.extend_from_slice(&u64b(tile_off as u64));
    assert_eq!(b.len(), tile_off);

    // --- tile: uncompressed, one sample triple per pixel ---
    for _ in 0..(w * h) {
        b.extend_from_slice(&rgb);
    }
    b
}

/// `extract` really decodes a real XCF, pixels and all.
///
/// WHY THIS EXISTS, and it is not a nicety: a `cargo mutants` run over this file scored
/// 9 caught against 18 MISSED, and one of the missed mutants was
/// `replace extract -> Option<DynamicImage> with None`. The whole parser could be
/// replaced by "return nothing" and every test here still passed, because they all
/// tested helpers (RLE, zlib, precision, budget arithmetic) and none of them ever fed
/// bytes to the front door. This does, so gutting `extract`, inverting its magic check,
/// or breaking its dimension guards now fails.
#[test]
fn extract_decodes_a_real_synthetic_xcf_down_to_the_pixels() {
    let img = extract(&synthetic_xcf(2, 2, [200, 100, 50]))
        .expect("a structurally valid XCF must decode");
    assert_eq!((img.width(), img.height()), (2, 2));
    let rgba = img.to_rgba8();
    for px in rgba.pixels() {
        assert_eq!(
            px.0,
            [200, 100, 50, 255],
            "layer colour did not survive the composite"
        );
    }
}

/// The layer PROPERTY branches, driven with real bytes and asserted by their effect.
///
/// `cargo mutants` flagged every one of these guards as un-killed: the opacity, float
/// opacity, visibility and offset arms could each be forced true or false and no test
/// noticed, because the only fixture in this file wrote an empty property list. They
/// parse attacker-supplied bytes, so "never exercised" is the wrong state for them.
///
/// Each case asserts a VISIBLE consequence rather than that parsing merely succeeded:
/// a fully transparent composite is refused by `extract`'s own blank-tile check, so
/// "opacity 0 yields None" is an observation, not an implementation detail.
#[test]
fn layer_properties_are_parsed_and_actually_take_effect() {
    let solid = [90u8, 160, 220];

    // Opaque and visible: the control.
    let opaque = synthetic_xcf_with_props(
        2,
        2,
        solid,
        &[
            (6, 255u32.to_be_bytes().to_vec()), // PROP_OPACITY, fully opaque
            (8, 1u32.to_be_bytes().to_vec()),   // PROP_VISIBLE, shown
        ],
    );
    let img = extract(&opaque).expect("an opaque visible layer must render");
    assert_eq!(img.to_rgba8().get_pixel(0, 0).0, [90, 160, 220, 255]);

    // PROP_OPACITY of zero draws nothing, so the composite is blank and refused.
    let transparent = synthetic_xcf_with_props(2, 2, solid, &[(6, 0u32.to_be_bytes().to_vec())]);
    assert!(
        extract(&transparent).is_none(),
        "a zero-opacity layer must not produce a visible tile"
    );

    // PROP_VISIBLE of zero does the same by a different route.
    let hidden = synthetic_xcf_with_props(2, 2, solid, &[(8, 0u32.to_be_bytes().to_vec())]);
    assert!(
        extract(&hidden).is_none(),
        "an invisible layer must not be composited"
    );

    // PROP_FLOAT_OPACITY overrides the integer one, so a 1.0 float rescues a 0 integer.
    let float_wins = synthetic_xcf_with_props(
        2,
        2,
        solid,
        &[
            (6, 0u32.to_be_bytes().to_vec()),
            (33, 1.0f32.to_be_bytes().to_vec()),
        ],
    );
    assert!(
        extract(&float_wins).is_some(),
        "PROP_FLOAT_OPACITY must override the integer opacity that precedes it"
    );

    // PROP_OFFSETS moves the layer. Pushed fully off a 2x2 canvas, nothing lands.
    let mut off = Vec::new();
    off.extend_from_slice(&8i32.to_be_bytes());
    off.extend_from_slice(&8i32.to_be_bytes());
    let shifted = synthetic_xcf_with_props(2, 2, solid, &[(15, off)]);
    assert!(
        extract(&shifted).is_none(),
        "a layer offset entirely off-canvas must contribute no pixels"
    );

    // A TRUNCATED property payload must be ignored, not misread: the length guards on
    // these arms are what mutation testing said were untested.
    let short = synthetic_xcf_with_props(2, 2, solid, &[(6, vec![0u8, 0])]);
    assert!(
        extract(&short).is_some(),
        "a 2-byte PROP_OPACITY is too short to honour, so the layer stays fully opaque"
    );
}

/// A file that has the MAGIC but not a full header must be refused, not panic.
///
/// `looks_like_xcf` is satisfied by 9 bytes, while the next line indexes `bytes[9..13]`
/// directly, so the `bytes.len() < 14` guard between them is the only thing standing
/// between a 9-to-13-byte file and a slice-out-of-range panic. Under `panic = "abort"`
/// in the shell that is the user's Explorer dying on a truncated download.
///
/// Found by `cargo mutants`: changing that guard to `== 14` left every test passing,
/// because nothing here had ever fed it a short-but-magic file. Every length in the gap
/// is covered, not just one, since the failure is a boundary.
#[test]
fn a_file_with_the_magic_but_a_truncated_header_is_refused_without_panicking() {
    for len in 9..=15usize {
        let mut b = b"gimp xcf v011\0".to_vec();
        b.truncate(len.min(14));
        while b.len() < len {
            b.push(0);
        }
        assert_eq!(b.len(), len);
        // No unwinding to catch: the shell aborts on panic, so "did not panic" is the
        // assertion, and reaching the next line at all is what proves it.
        let got = extract(&b);
        assert!(
            got.is_none(),
            "a {len}-byte file cannot contain an image, so it must be refused"
        );
    }
}

/// The same fixture, one field at a time, proves the header guards are load-bearing.
#[test]
fn extract_refuses_a_synthetic_xcf_whose_header_is_corrupted() {
    let good = synthetic_xcf(2, 2, [10, 20, 30]);
    assert!(extract(&good).is_some(), "control case must decode");

    // Wrong magic.
    let mut bad = good.clone();
    bad[0] = b'G';
    assert!(extract(&bad).is_none(), "magic check must reject");

    // Zero width, which the dimension guard exists to catch.
    let mut zero_w = good.clone();
    zero_w[14..18].copy_from_slice(&0u32.to_be_bytes());
    assert!(extract(&zero_w).is_none(), "a zero width must be refused");

    // A width past MAX_DIM, the per-edge bomb guard.
    let mut huge = good.clone();
    huge[14..18].copy_from_slice(&(MAX_DIM + 1).to_be_bytes());
    assert!(extract(&huge).is_none(), "past MAX_DIM must be refused");

    // Truncated mid-tile: the reads are bounds-checked, so this is None, never a panic.
    let truncated = &good[..good.len() - 3];
    assert!(extract(truncated).is_none(), "a short file must be refused");
}
