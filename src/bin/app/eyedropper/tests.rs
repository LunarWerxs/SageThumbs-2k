use super::*;

/// The four formats a pick can copy as. Exactness on the primaries matters: a designer
/// checking a pure colour reads "100%", and "99%" reads as a broken converter.
#[test]
fn formats_render_the_primaries_exactly() {
    let red = (255u8, 0u8, 0u8);
    assert_eq!(fmt_color(0, red), "#FF0000");
    assert_eq!(fmt_color(1, red), "rgb(255, 0, 0)");
    assert_eq!(fmt_color(2, red), "hsl(0, 100%, 50%)");
    assert_eq!(fmt_color(3, red), "hsv(0, 100%, 100%)");
    // Greys have no hue and zero saturation in both models.
    let grey = (128u8, 128u8, 128u8);
    assert_eq!(fmt_color(2, grey), "hsl(0, 0%, 50%)");
    assert_eq!(fmt_color(3, grey), "hsv(0, 0%, 50%)");
}

/// An out-of-range format index must fall back to hex, never panic — the value comes
/// from the registry, which anything can have scribbled on.
#[test]
fn unknown_format_is_hex() {
    assert_eq!(fmt_color(17, (1, 2, 3)), "#010203");
    assert_eq!(fmt_color(-1, (1, 2, 3)), "#010203");
}

/// Hue must land in the right sextant for each channel-dominant colour (the `rem_euclid`
/// in the red branch is what keeps a red with a touch of blue from going negative).
#[test]
fn hue_sextants() {
    assert_eq!(rgb_to_hsl((255, 255, 0)).0.round() as i32, 60); // yellow
    assert_eq!(rgb_to_hsl((0, 255, 0)).0.round() as i32, 120); // green
    assert_eq!(rgb_to_hsl((0, 255, 255)).0.round() as i32, 180); // cyan
    assert_eq!(rgb_to_hsl((0, 0, 255)).0.round() as i32, 240); // blue
    assert_eq!(rgb_to_hsl((255, 0, 255)).0.round() as i32, 300); // magenta
    let h = rgb_to_hsl((255, 0, 10)).0;
    assert!(h > 350.0, "red-with-blue hue wrapped wrong: {h}");
}

/// Regression for the unclamped `StretchBlt` source: near any edge of the
/// snapshot, `sx`/`sy` must stay in `[0, v - EYE_SPAN]` so the source rect never
/// extends past the snapshot bounds, and `kx`/`ky` must still land on the cursor's
/// own cell inside that (possibly shifted) window.
#[test]
fn eye_sample_window_stays_inside_the_snapshot_near_every_edge() {
    let (vw, vh) = (1920, 1080);
    for (cx, cy) in [
        (0, 0),           // top-left corner
        (vw - 1, vh - 1), // bottom-right corner
        (0, vh / 2),      // left edge
        (vw - 1, vh / 2), // right edge
        (vw / 2, 0),      // top edge
        (vw / 2, vh - 1), // bottom edge
        (vw / 2, vh / 2), // interior — no clamp should be needed
    ] {
        let (sx, sy, kx, ky) = eye_sample_window(cx, cy, vw, vh);
        assert!(
            sx >= 0 && sx + EYE_SPAN <= vw,
            "sx {sx} puts the source rect outside [0, {vw}) at cursor ({cx}, {cy})"
        );
        assert!(
            sy >= 0 && sy + EYE_SPAN <= vh,
            "sy {sy} puts the source rect outside [0, {vh}) at cursor ({cx}, {cy})"
        );
        // The cursor's own pixel, mapped into the (possibly shifted) window, must
        // still be the cell that gets the crosshair.
        assert_eq!(sx + kx, cx, "kx must map back to the cursor's own column");
        assert_eq!(sy + ky, cy, "ky must map back to the cursor's own row");
    }
}

/// A degenerate/tiny snapshot (smaller than the sample span) must not panic or
/// produce a negative-width source rect — `(vw - EYE_SPAN).max(0)` is what
/// prevents that.
#[test]
fn eye_sample_window_handles_a_snapshot_smaller_than_the_span() {
    let (sx, sy, kx, ky) = eye_sample_window(2, 2, 4, 4);
    assert_eq!((sx, sy), (0, 0));
    assert!(kx < EYE_SPAN && ky < EYE_SPAN);
}

/// Issue #95: a second snapshot must free the previous one's DC/bitmap instead
/// of leaking it — the old `OnceLock` could not be re-set, so it silently kept
/// pointing at whatever was captured FIRST, even after that snapshot was
/// deleted by `on_destroy` (a stale, freed handle read by the second run).
#[test]
fn set_snapshot_frees_the_previous_one() {
    unsafe {
        let screen = GetDC(None);
        let dc1 = CreateCompatibleDC(Some(screen));
        let bmp1 = CreateCompatibleBitmap(screen, 4, 4);
        let dc2 = CreateCompatibleDC(Some(screen));
        let bmp2 = CreateCompatibleBitmap(screen, 4, 4);
        ReleaseDC(None, screen);

        set_snapshot(dc1, bmp1);
        assert_eq!(
            *EYE_SHOT.lock().unwrap(),
            Some((dc1.0 as usize, bmp1.0 as usize))
        );

        // Must free dc1/bmp1 before storing dc2/bmp2, not just clobber the slot.
        set_snapshot(dc2, bmp2);
        assert_eq!(
            *EYE_SHOT.lock().unwrap(),
            Some((dc2.0 as usize, bmp2.0 as usize)),
            "the slot must hold the SECOND snapshot, not still the first"
        );

        free_snapshot();
        assert_eq!(
            *EYE_SHOT.lock().unwrap(),
            None,
            "free_snapshot must clear the slot"
        );
    }
}
