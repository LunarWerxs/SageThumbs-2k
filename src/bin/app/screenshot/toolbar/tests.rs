#![cfg(test)]

use super::*;

fn sel() -> RECT {
    RECT {
        left: 300,
        top: 200,
        right: 700,
        bottom: 500,
    }
}

/// Every non-separator item must be reachable by a click. A button that lays out but
/// can't be hit is invisible to the user even though it paints, so this pins the
/// layout/hit-test pair for the whole bar (the OCR button included).
#[test]
fn every_button_is_hittable_and_no_two_overlap() {
    let buttons = layout(sel(), 1920, 1080, 96);
    assert_eq!(buttons.len(), items().len());
    for (btn, r) in &buttons {
        let (cx, cy) = ((r.left + r.right) / 2, (r.top + r.bottom) / 2);
        if matches!(btn, Button::Sep) {
            assert!(hit(&buttons, cx, cy).is_none(), "a divider is clickable");
            continue;
        }
        assert!(
            hit(&buttons, cx, cy) == Some(*btn),
            "a laid-out button isn't hit-testable at its own centre"
        );
    }
    for pair in buttons.windows(2) {
        assert!(
            pair[0].1.right <= pair[1].1.left,
            "toolbar cells overlap — a click would land on the wrong action"
        );
    }
}

/// The OCR button ships in the action group next to Copy (copy pixels / copy words),
/// and every button carries a tooltip — `button_tip` returning "" would show an empty
/// bubble on hover.
///
/// `button_tip` is now locale-dependent (audit F29), so this forces English explicitly
/// rather than trusting whatever the test machine's Windows UI language happens to be -
/// the assertion below checks a literal English substring.
#[test]
fn ocr_button_sits_next_to_copy_and_is_described() {
    sagethumbs2k_core::i18n::apply_override_or_system(Some("en"));
    let order: Vec<Button> = items().iter().map(|(b, _)| *b).collect();
    let copy = order
        .iter()
        .position(|b| *b == Button::Copy)
        .expect("Copy button");
    let ocr = order
        .iter()
        .position(|b| *b == Button::Ocr)
        .expect("OCR button");
    assert_eq!(
        ocr,
        copy + 1,
        "OCR must stay immediately after Copy in the action group"
    );
    for (btn, _) in items() {
        if matches!(btn, Button::Sep) {
            continue;
        }
        assert!(!button_tip(btn).is_empty());
    }
    assert!(button_tip(Button::Ocr).contains("Ctrl+T"));
}

/// Audit F29: every tooltip must come from the locale table, not a hardcoded literal.
/// Compares `button_tip` against `t(key)` (with the tool letter substituted the same way
/// `button_tip` itself does it) for every button - a hardcoded `&'static str` could not
/// track a key it never looks up. Also proves the `{key}` placeholder actually gets filled:
/// a missing `.replace()` would leave the literal text `{key}` in the tooltip.
#[test]
fn button_tip_reads_the_locale_table_and_fills_the_key_placeholder() {
    sagethumbs2k_core::i18n::apply_override_or_system(Some("en"));
    let pairs = [
        (Button::Tool(Tool::Rect), "shot_tip_rect", "R"),
        (Button::Tool(Tool::Ellipse), "shot_tip_ellipse", "O"),
        (Button::Tool(Tool::Arrow), "shot_tip_arrow", "A"),
        (Button::Tool(Tool::Line), "shot_tip_line", "L"),
        (Button::Tool(Tool::Pen), "shot_tip_pen", "P"),
        (Button::Tool(Tool::Text), "shot_tip_text", "T"),
        (Button::Tool(Tool::Number), "shot_tip_number", "N"),
        (Button::Tool(Tool::Highlight), "shot_tip_highlight", "H"),
        (Button::Tool(Tool::Pixelate), "shot_tip_pixelate", "B"),
        (Button::Tool(Tool::Invert), "shot_tip_invert", "I"),
        (Button::Tool(Tool::Eyedropper), "shot_tip_eyedropper", "E"),
        (Button::Tool(Tool::Move), "shot_tip_move", "M"),
        (Button::Color, "shot_tip_color", "K"),
    ];
    for (btn, key, letter) in pairs {
        let tip = button_tip(btn);
        assert_eq!(tip, crate::win::t(key).replace("{key}", letter));
        assert!(
            !tip.contains("{key}"),
            "the {{key}} placeholder was never substituted in {key}"
        );
        assert!(tip.contains(letter), "{key} lost its shortcut letter");
    }
    for (btn, key) in [
        (Button::Undo, "shot_tip_undo"),
        (Button::Redo, "shot_tip_redo"),
        (Button::Copy, "shot_tip_copy"),
        (Button::Ocr, "shot_tip_ocr"),
        (Button::Save, "shot_tip_save"),
        (Button::Upload, "shot_tip_upload"),
        (Button::Close, "shot_tip_close"),
    ] {
        assert_eq!(button_tip(btn), crate::win::t(key));
    }
}

/// Keyboard focus has to reach every button the mouse can click, in both directions,
/// and has to step OVER the dividers rather than parking on one: a focus ring around a
/// painted line, with Space doing nothing, reads as a broken toolbar.
#[test]
fn keyboard_focus_walks_the_bar_both_ways_and_never_lands_on_a_divider() {
    let bar = layout(sel(), 1920, 1080, 96);
    let focusable: Vec<usize> = bar
        .iter()
        .enumerate()
        .filter(|(_, (b, _))| !matches!(b, Button::Sep))
        .map(|(i, _)| i)
        .collect();
    assert!(
        focusable.len() > 1,
        "this bar is supposed to have real buttons"
    );
    assert_eq!(first_focusable(&bar), focusable.first().copied());

    // From every focusable seat, one step forward lands on the next focusable seat and
    // one step back lands on the previous one, with the dividers between them skipped
    // and both ends wrapping.
    for (n, &i) in focusable.iter().enumerate() {
        let ahead = focusable[(n + 1) % focusable.len()];
        let behind = focusable[(n + focusable.len() - 1) % focusable.len()];
        assert_eq!(step_focus(&bar, i, true), Some(ahead));
        assert_eq!(step_focus(&bar, i, false), Some(behind));
    }

    // A full lap must visit every focusable button exactly once and close back on the
    // first, which is the property a user actually feels: hold Tab and you get round the
    // whole bar without a repeat and without a dead stop.
    let mut seen = Vec::new();
    let mut cur = first_focusable(&bar).expect("a focusable button");
    for _ in 0..focusable.len() {
        seen.push(cur);
        cur = step_focus(&bar, cur, true).expect("the walk must always find a seat");
    }
    seen.sort_unstable();
    assert_eq!(seen, focusable);
    assert_eq!(
        cur, focusable[0],
        "a full lap must close back on the first button"
    );
}

/// The degenerate lists. A `loop { i = next(i) }` written the obvious way spins forever
/// on an empty bar or one holding nothing but dividers, and this binary is built with
/// `panic = "abort"`, so a hang here freezes a fullscreen topmost window with no way out.
/// The walk must ANSWER "nowhere to go" instead.
#[test]
fn keyboard_focus_cannot_panic_or_spin_on_a_degenerate_bar() {
    let empty: Vec<(Button, RECT)> = Vec::new();
    assert_eq!(first_focusable(&empty), None);
    assert_eq!(step_focus(&empty, 0, true), None);
    assert_eq!(step_focus(&empty, 7, false), None); // a stale index must clamp, not index

    let one = vec![(Button::Copy, RECT::default())];
    assert_eq!(first_focusable(&one), Some(0));
    assert_eq!(step_focus(&one, 0, true), Some(0));
    assert_eq!(step_focus(&one, 0, false), Some(0));

    let dividers = vec![
        (Button::Sep, RECT::default()),
        (Button::Sep, RECT::default()),
    ];
    assert_eq!(first_focusable(&dividers), None);
    assert_eq!(step_focus(&dividers, 0, true), None);
    assert_eq!(step_focus(&dividers, 1, false), None);
}

/// The flyouts have no dividers, so they step by index, but they still have to wrap at
/// both ends and survive a list length that changed underneath a stale index (the text
/// flyout grows by eight rows the moment the font dropdown expands).
#[test]
fn wrap_step_moves_through_a_flyout_list_and_wraps_at_both_ends() {
    assert_eq!(wrap_step(11, 0, 1), Some(1));
    assert_eq!(wrap_step(11, 10, 1), Some(0)); // off the end, back to the start
    assert_eq!(wrap_step(11, 0, -1), Some(10)); // off the start, round to the end

    // A vertical arrow in the 6 wide palette steps a whole row. The last row is ragged
    // (11 cells in a 6 wide grid), and the wrap there is through the flat list on
    // purpose: a column-preserving wrap would leave the top row's last cell with a Down
    // key that does nothing at all.
    assert_eq!(wrap_step(11, 0, 6), Some(6));
    assert_eq!(wrap_step(11, 5, 6), Some(0));
    assert_eq!(wrap_step(11, 6, -6), Some(0));

    assert_eq!(wrap_step(0, 0, 1), None, "an empty list has nowhere to go");
    assert_eq!(wrap_step(1, 0, 1), Some(0));
    assert_eq!(wrap_step(1, 0, -1), Some(0));
    assert_eq!(wrap_step(4, 99, 1), Some(0)); // stale index clamps to the last, then steps
}

/// The arrow keys measure the grid off the laid-out rects rather than assuming a shape,
/// so this pins what that measurement actually returns for the two real flyouts. Get it
/// wrong for the palette and Up/Down move one swatch instead of one row; get it wrong for
/// the text flyout and they jump over most of the settings.
#[test]
fn grid_cols_is_measured_from_the_real_flyout_layouts() {
    let bar = layout(sel(), 1920, 1080, 96);
    let (_, color_cell) = bar
        .iter()
        .find(|(b, _)| *b == Button::Color)
        .copied()
        .expect("the Colour button");
    let (_, swatches) = color_flyout_layout(color_cell, 1920, 1080, &[], 96);
    let rects: Vec<RECT> = swatches.iter().map(|(_, r)| *r).collect();
    assert_eq!(grid_cols(&rects), 6, "the palette is a 6 wide grid");

    let (_, text_cell) = bar
        .iter()
        .find(|(b, _)| *b == Button::Tool(Tool::Text))
        .copied()
        .expect("the Text button");
    let (_, items) = text_flyout_layout(text_cell, 1920, 1080, true, 96);
    let rects: Vec<RECT> = items.iter().map(|(_, r)| *r).collect();
    assert_eq!(
        grid_cols(&rects),
        1,
        "the text flyout is a stack of rows, so a vertical step is one row"
    );

    assert_eq!(grid_cols(&[]), 0);
}

/// The bar has to fit on-screen even on a small display, or the rightmost actions
/// (Save / Upload / Close, and now OCR) hang off the edge unreachable.
#[test]
fn bar_stays_inside_a_small_virtual_screen() {
    let (vw, vh) = (1024, 768);
    let buttons = layout(
        RECT {
            left: 900,
            top: 700,
            right: 1000,
            bottom: 760,
        },
        vw,
        vh,
        96,
    );
    let bar = bar_rect(&buttons, 96);
    assert!(
        bar.left >= 0 && bar.top >= 0,
        "bar clipped off the top-left"
    );
    assert!(bar.right <= vw, "bar runs off the right edge");
    assert!(bar.bottom <= vh, "bar runs off the bottom edge");
}
