#![cfg(test)]

//! Layer stacks under a budget: which layers survive, and a source that trickles bytes.

use super::*;

/// One layer of a [`synthetic_xcf_stack`] fixture.
pub(super) struct Spec {
    pub(super) rgba: [u8; 4],
    pub(super) visible: bool,
    pub(super) opacity: u8,
}

impl Spec {
    /// An ordinary opaque layer of one flat colour.
    pub(super) fn solid(rgb: [u8; 3]) -> Self {
        Spec {
            rgba: [rgb[0], rgb[1], rgb[2], 255],
            visible: true,
            opacity: 255,
        }
    }

    pub(super) fn hidden(mut self) -> Self {
        self.visible = false;
        self
    }

    /// Present and enabled, but every pixel fully transparent — the shape a real file
    /// takes when its lower layers are erased regions rather than background.
    pub(super) fn clear(mut self) -> Self {
        self.rgba[3] = 0;
        self
    }
}

/// Build a structurally VALID multi-layer XCF: v011 (64-bit pointers), RGBA layers,
/// uncompressed tiles, every layer filling the whole canvas.
///
/// `specs` is BOTTOM-first, so its LAST entry is the one a correct composite puts on top
/// and therefore the colour the thumbnail must show. GIMP writes the layer pointer list
/// top-first, so the list is emitted in reverse — the same orientation a real file has,
/// which is the detail the layer-selection order turns on.
///
/// Offsets are computed rather than hand-counted because every pointer in this format is
/// absolute, so one inserted field silently invalidates a literal table.
pub(super) fn synthetic_xcf_stack(w: u32, h: u32, specs: &[Spec]) -> Vec<u8> {
    assert!(
        w <= TILE && h <= TILE,
        "the fixture writes ONE tile per layer, so it cannot exceed the tile grid"
    );
    let header = 14 + 4 * 4 + (4 + 4 + 1) + (4 + 4);
    let ptr_list = 8 * specs.len() + 8;
    // dims + type + name + PROP_OPACITY + PROP_VISIBLE + PROP_END + hierarchy + mask
    let layer_rec = 4 + 4 + 4 + 4 + 1 + (4 + 4 + 4) * 2 + (4 + 4) + 8 + 8;
    let hier_rec = 4 + 4 + 4 + 8;
    let level_rec = 4 + 4 + 8;
    let tile_len = (w * h * 4) as usize;
    let per_layer = layer_rec + hier_rec + level_rec + tile_len;
    let first_layer = header + ptr_list;
    let layer_off = |i: usize| first_layer + i * per_layer;

    let mut b: Vec<u8> = Vec::new();
    push_xcf_header(&mut b, w, h);
    assert_eq!(
        b.len(),
        header,
        "header layout drifted from its computed size"
    );

    for i in (0..specs.len()).rev() {
        b.extend_from_slice(&u64b(layer_off(i) as u64));
    }
    b.extend_from_slice(&u64b(0)); // end of layer list
    assert_eq!(b.len(), first_layer);

    for (i, spec) in specs.iter().enumerate() {
        assert_eq!(b.len(), layer_off(i));
        push_layer_start(&mut b, w, h, 1); // RGBA, 4 channels
        push_property(&mut b, 6, &u32b(u32::from(spec.opacity))); // PROP_OPACITY
        push_property(&mut b, 8, &u32b(u32::from(spec.visible))); // PROP_VISIBLE
        b.extend_from_slice(&u32b(0)); // PROP_END
        b.extend_from_slice(&u32b(0));
        b.extend_from_slice(&u64b((layer_off(i) + layer_rec) as u64));
        b.extend_from_slice(&u64b(0)); // no layer mask

        b.extend_from_slice(&u32b(w)); // hierarchy
        b.extend_from_slice(&u32b(h));
        b.extend_from_slice(&u32b(4)); // bpp = 4 channels x 1 byte
        b.extend_from_slice(&u64b((layer_off(i) + layer_rec + hier_rec) as u64));

        b.extend_from_slice(&u32b(w)); // level
        b.extend_from_slice(&u32b(h));
        b.extend_from_slice(&u64b(
            (layer_off(i) + layer_rec + hier_rec + level_rec) as u64,
        ));

        for _ in 0..(w * h) {
            b.extend_from_slice(&spec.rgba);
        }
    }
    b
}

/// A budget that cannot buy every layer must buy the TOP ones.
///
/// THE regression test for a bug that shipped in 2.0.0 and was reported by a user on
/// 2026-08-17 ("xcf don't work anymore with new versions for big files"). The budget was
/// spent in layer-list order, which is bottom-up, so a file with more layer area than the
/// allowance rendered its LOWER layers and silently discarded everything above them — a
/// thumbnail of a half-finished picture, indistinguishable to the viewer from the real one.
///
/// It is driven through the real front door with real bytes, because that is the only
/// place this is visible: the arithmetic test above passed the entire time the bug was
/// live. The budget is an argument so the case can be posed at 2x2 instead of at the
/// 16384-square scale where it costs gigabytes to reproduce.
#[test]
fn a_budget_short_of_every_layer_keeps_the_top_ones_not_the_bottom_ones() {
    let (red, green, blue) = ([200, 30, 30], [30, 190, 30], [30, 60, 210]);
    let stack = synthetic_xcf_stack(
        2,
        2,
        &[Spec::solid(red), Spec::solid(green), Spec::solid(blue)],
    );

    // The control: with room for all three, the top layer covers the other two.
    let full = extract(&stack).expect("three opaque layers must composite");
    assert_eq!(full.to_rgba8().get_pixel(0, 0).0, [30, 60, 210, 255]);

    // Room for exactly ONE 2x2 layer. The answer must still be the top layer; the shipped
    // bug returned `red` here, the bottom of the stack.
    let starved = extract_within(&stack, 4).expect("a starved budget must still draw");
    assert_eq!(
        starved.to_rgba8().get_pixel(0, 0).0,
        [30, 60, 210, 255],
        "an exhausted budget must give up the layers UNDERNEATH, not the visible top"
    );
}

/// The user's actual symptom: no thumbnail at all, from a file that has one.
///
/// Lower layers that are fully transparent are ordinary (erased regions, empty
/// backgrounds). Spending the budget bottom-up on those left a canvas where nothing had
/// been drawn, and `extract`'s own blank-composite check then correctly turned that into
/// `None` — so a perfectly good image produced the default icon in Explorer. The failure
/// needs no exotic file, only a stack too big for the allowance.
#[test]
fn transparent_lower_layers_cannot_starve_the_visible_top_layer_into_nothing() {
    let stack = synthetic_xcf_stack(
        2,
        2,
        &[
            Spec::solid([200, 30, 30]).clear(),
            Spec::solid([30, 190, 30]).clear(),
            Spec::solid([30, 60, 210]),
        ],
    );
    let img = extract_within(&stack, 4)
        .expect("the opaque top layer must be drawn, not skipped for two transparent ones");
    assert_eq!(img.to_rgba8().get_pixel(0, 0).0, [30, 60, 210, 255]);
}

/// Layers that cannot draw must not be charged for the privilege.
///
/// Hiding a layer is how GIMP users set one aside, so files carry piles of them. Charging
/// them spends the allowance on pixels that are decoded, composited nowhere, and dropped —
/// and on a tight budget it spends the whole allowance before reaching anything visible.
#[test]
fn hidden_and_off_canvas_layers_are_free() {
    let hidden_below: Vec<Spec> = (0..4)
        .map(|_| Spec::solid([200, 30, 30]).hidden())
        .chain(std::iter::once(Spec::solid([30, 60, 210])))
        .collect();
    let img = extract_within(&synthetic_xcf_stack(2, 2, &hidden_below), 4)
        .expect("four hidden layers must not consume a budget the visible one needs");
    assert_eq!(img.to_rgba8().get_pixel(0, 0).0, [30, 60, 210, 255]);

    // The same allowance, and the same rule, for a layer parked outside the canvas.
    let head = |ox: i32| LayerHead {
        lw: 2,
        lh: 2,
        ltype: 1,
        ox,
        oy: 0,
        opacity: 1.0,
        visible: true,
        hptr: 0,
    };
    assert!(head(0).draws_on(2, 2), "a layer on the canvas draws");
    assert!(
        !head(2).draws_on(2, 2),
        "a layer past the right edge cannot"
    );
    assert!(!head(-2).draws_on(2, 2), "nor one past the left edge");
    assert!(head(-1).draws_on(2, 2), "but a straddling one still does");
}

/// A source that hands back only a few bytes per `read` must decode identically.
///
/// This is the half of the streaming rescue with teeth. `read_at` loops until it has the
/// window it asked for, and a `Cursor` always fills a buffer in one call, so every other
/// test in this file exercises that loop exactly zero times. A COM `IStream` from the shell
/// has no such obligation and returns what it feels like, which is precisely the source
/// this path exists to serve: a partial read treated as the whole window would decode
/// garbage from a file that is perfectly fine.
#[test]
fn a_source_that_only_ever_returns_a_few_bytes_at_a_time_decodes_the_same_picture() {
    /// Never returns more than 7 bytes, however much is asked for.
    struct Dribble(std::io::Cursor<Vec<u8>>);

    impl Read for Dribble {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = buf.len().min(7);
            self.0.read(&mut buf[..n])
        }
    }
    impl Seek for Dribble {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            self.0.seek(pos)
        }
    }

    let stack = synthetic_xcf_stack(
        8,
        8,
        &[Spec::solid([200, 30, 30]), Spec::solid([30, 60, 210])],
    );
    let whole = extract(&stack).expect("control: the fixture decodes");
    let dribbled = extract_seek(Dribble(std::io::Cursor::new(stack)), None)
        .expect("a short-reading source must not lose the image");
    assert_eq!(
        whole.to_rgba8().into_raw(),
        dribbled.to_rgba8().into_raw(),
        "a source that dribbles bytes must produce the identical picture"
    );
}

/// Running out mid-stack STOPS; it does not skip the expensive layer and keep going.
///
/// Skipping would let a small layer be drawn while a larger one ABOVE it is missing, so
/// the output would be neither the top of the image nor a plainly truncated version of it,
/// but an arbitrary subset — the one failure shape harder to recognise as wrong than a
/// missing layer.
#[test]
fn selection_stops_at_the_first_unaffordable_layer() {
    let head = |lw: u32| {
        Some(LayerHead {
            lw,
            lh: 1,
            ltype: 1,
            ox: 0,
            oy: 0,
            opacity: 1.0,
            visible: true,
            hptr: 0,
        })
    };
    // Top-first: a 1px layer, then a 100px one, then another 1px. A budget of 2 buys the
    // first, cannot buy the second, and must NOT skip ahead to the third.
    let keep = select_layers(2, &[head(1), head(100), head(1)], 200, 1);
    assert_eq!(keep, vec![true, false, false]);
}
