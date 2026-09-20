use super::*;

/// Left/Right steps one place and wraps at both ends of the CURRENT bar, and never
/// crosses into the other bar (that is Up/Down's job, tested separately below).
#[test]
fn arrow_step_wraps_within_the_bar_and_never_crosses_bars() {
    assert_eq!(
        arrow_step(FocusTarget::Caption(0), 3, 7, true),
        Some(FocusTarget::Caption(1))
    );
    // Right off the last caption button wraps to the first, not into the transport strip.
    assert_eq!(
        arrow_step(FocusTarget::Caption(2), 3, 7, true),
        Some(FocusTarget::Caption(0))
    );
    // Left off the first wraps to the last.
    assert_eq!(
        arrow_step(FocusTarget::Caption(0), 3, 7, false),
        Some(FocusTarget::Caption(2))
    );
    assert_eq!(
        arrow_step(FocusTarget::Transport(6), 3, 7, true),
        Some(FocusTarget::Transport(0))
    );
    // A single-item bar stays put rather than looping forever or panicking.
    assert_eq!(
        arrow_step(FocusTarget::Caption(0), 1, 0, true),
        Some(FocusTarget::Caption(0))
    );
}

/// Down/Up jump straight to the other bar, landing on the same index (clamped), and are a
/// no-op — `None`, so the caller leaves the key unhandled — when there is nowhere to go.
#[test]
fn switch_bar_moves_between_bars_and_stays_put_with_no_transport() {
    assert_eq!(
        switch_bar(FocusTarget::Caption(2), 5, 7, true),
        Some(FocusTarget::Transport(2))
    );
    assert_eq!(
        switch_bar(FocusTarget::Transport(1), 5, 7, false),
        Some(FocusTarget::Caption(1))
    );
    // Caption index is clamped into a shorter transport strip.
    assert_eq!(
        switch_bar(FocusTarget::Caption(4), 5, 2, true),
        Some(FocusTarget::Transport(1))
    );
    // No transport strip showing: Down from the caption bar goes nowhere.
    assert_eq!(switch_bar(FocusTarget::Caption(0), 5, 0, true), None);
    // Already on the caption bar: Up has nowhere further to go.
    assert_eq!(switch_bar(FocusTarget::Transport(0), 0, 7, false), None);
}

/// Tab/Shift+Tab treat the two bars as ONE sequence: stepping off either end of one bar
/// lands on the start/end of the other — the "or Tab again" half of the caption-to-transport
/// transition (the direct jump is `switch_bar`, tested above).
#[test]
fn tab_step_crosses_from_the_caption_bar_into_the_transport_strip_and_back() {
    // Last caption button, Tab forward -> first transport control.
    assert_eq!(
        tab_step(FocusTarget::Caption(2), 3, 7, true),
        Some(FocusTarget::Transport(0))
    );
    // First transport control, Shift+Tab -> last caption button.
    assert_eq!(
        tab_step(FocusTarget::Transport(0), 3, 7, false),
        Some(FocusTarget::Caption(2))
    );
    // Last transport control, Tab forward wraps back to the first caption button.
    assert_eq!(
        tab_step(FocusTarget::Transport(6), 3, 7, true),
        Some(FocusTarget::Caption(0))
    );
    // No transport strip showing: Tab off the last caption button wraps within the caption
    // bar instead of landing on a strip that isn't there.
    assert_eq!(
        tab_step(FocusTarget::Caption(2), 3, 0, true),
        Some(FocusTarget::Caption(0))
    );
}

/// Focus resets when the VISIBLE SET CHANGES: an index that is still in range survives a
/// repaint untouched, but one a shrunk bar has left out of bounds is dropped rather than
/// silently clamped onto a different button.
#[test]
fn repair_focus_clears_when_the_index_falls_out_of_bounds() {
    assert_eq!(
        repair_focus(Some(FocusTarget::Caption(4)), 5, 7),
        Some(FocusTarget::Caption(4))
    );
    // The bar shrank (e.g. a content-kind change hid some buttons) — index 4 no longer
    // exists, so focus drops rather than landing on whatever is now at 4.
    assert_eq!(repair_focus(Some(FocusTarget::Caption(4)), 3, 7), None);
    assert_eq!(
        repair_focus(Some(FocusTarget::Transport(1)), 5, 2),
        Some(FocusTarget::Transport(1))
    );
    assert_eq!(repair_focus(Some(FocusTarget::Transport(1)), 5, 1), None);
    assert_eq!(repair_focus(None, 5, 7), None);
}

/// The other half of "focus resets ... on a content change": a focus set under an OLD load
/// generation is dropped even though nothing explicitly cleared it, the moment the current
/// generation has moved on (a new file, or an in-place content-kind change).
#[test]
fn live_focus_drops_a_focus_from_a_stale_load_generation() {
    let f = Some(FocusTarget::Caption(0));
    assert_eq!(live_focus(f, 5, 5), f);
    assert_eq!(live_focus(f, 5, 6), None);
    assert_eq!(live_focus(None, 5, 5), None);
}

/// Every caption button must have a REAL translated tooltip. `i18n::t` returns a
/// `⟨?⟩` sentinel when a key is missing from both the active locale and `en`, so this
/// catches the easy half of adding a button: wiring the enum + paint + click and then
/// forgetting the string. Distinctness catches the copy-paste variant (two buttons
/// pointing at one key), which reads as a duplicated tooltip on hover.
#[test]
fn every_toolbar_button_has_its_own_real_tooltip() {
    // Run the whole bar under BOTH skins. The theme button's tooltip depends on which one
    // is active (it names the mode it switches TO), so a single pass leaves one of its two
    // strings unchecked — and WHICH one gets checked would depend on how the machine
    // running the test happens to be themed. That is a test whose result turns on
    // something other than the code.
    // Every combination of the two caption toggles whose tip depends on them, under both
    // skins: eight passes, so all of Theme's, Pin's and Source's strings are reached. A
    // single pass leaves one string of each pair unchecked, and WHICH one would depend on
    // how the machine running the test happens to be themed.
    for dark in [false, true] {
        crate::dark::set_theme_override(Some(dark));
        for pinned in [false, true] {
            for src_view in [false, true] {
                let mut seen: Vec<&str> = Vec::new();
                for &b in BTNS.iter() {
                    let tip = btn_tip(b, pinned, src_view);
                    assert!(
                        !tip.is_empty() && !tip.starts_with('\u{27e8}'),
                        "a preview toolbar button has no translated tooltip \
                         (missing locale key) at dark={dark} pinned={pinned} src={src_view}"
                    );
                    assert!(
                        !seen.contains(&tip),
                        "two preview toolbar buttons share the tooltip {tip:?}"
                    );
                    seen.push(tip);
                }
            }
        }
    }
    crate::dark::set_theme_override(None);
}

/// The transport strip's two toggles must have both of their strings, and they must differ
/// from each other — same gate as the caption bar's, which the strip never had.
#[test]
fn every_transport_toggle_has_a_string_for_both_states() {
    use super::super::transport::{tbtn_tip, TBTNS};
    for muted in [false, true] {
        for looping in [false, true] {
            let mut seen: Vec<&str> = Vec::new();
            for &t in TBTNS.iter() {
                let tip = tbtn_tip(t, muted, looping);
                assert!(
                    !tip.is_empty() && !tip.starts_with('\u{27e8}'),
                    "a transport control has no translated tooltip \
                     (missing locale key) at muted={muted} looping={looping}"
                );
                assert!(
                    !seen.contains(&tip),
                    "two transport controls share the tooltip {tip:?}"
                );
                seen.push(tip);
            }
        }
    }
}

/// A toggle's tooltip must actually CHANGE when the toggle flips, or re-sending it is
/// pointless and the shipped bug is still there in a different shape.
///
/// This is the regression test for the reported one: the moon icon kept saying
/// "Light background" after the first click of the theme button.
#[test]
fn a_flipped_toggle_reports_a_different_tooltip() {
    crate::dark::set_theme_override(Some(true));
    let theme_dark_skin = btn_tip(Btn::Theme, false, false);
    crate::dark::set_theme_override(Some(false));
    let theme_light_skin = btn_tip(Btn::Theme, false, false);
    crate::dark::set_theme_override(None);
    assert_ne!(
        theme_dark_skin, theme_light_skin,
        "the light/dark button must name the theme it switches TO, so its tip has to flip"
    );
    assert_ne!(
        btn_tip(Btn::Pin, false, false),
        btn_tip(Btn::Pin, true, false),
        "the pin's tip must say `unpin` once pinned, not repeat the state you are in"
    );
    assert_ne!(
        btn_tip(Btn::Source, false, false),
        btn_tip(Btn::Source, false, true),
        "view-source must not still offer `view source` while you are looking at source"
    );
    // …and the change has to be VISIBLE to the tooltip control, which only re-reads what we
    // send it. This is the comparison `update_tooltips` gates the re-send on.
    assert!(tooltip_text_changed(
        &[btn_tip(Btn::Pin, false, false)],
        &[btn_tip(Btn::Pin, true, false)]
    ));
    assert!(!tooltip_text_changed(
        &[btn_tip(Btn::Pin, true, false)],
        &[btn_tip(Btn::Pin, true, false)]
    ));
    // Length mismatch is the first-paint case: nothing registered yet.
    assert!(tooltip_text_changed(&[], &["x"]));
}

/// The crowded caption must fit, and the ordinary one must not change.
///
/// Twelve buttons is the real worst case (a Markdown document with headings, a web image and
/// a source view), and 400 px is the viewer's minimum width. At 38 px each that is 456 px of
/// buttons in a 400 px caption, and the old fixed-width layout ran the leftmost ones off the
/// left edge — invisible and unclickable. The second half of this test is the more important
/// half: with room to spare the answer has to be exactly `BTN_W`, or this "fix" would have
/// silently re-laid-out every preview window in the product.
#[test]
fn cells_narrow_only_when_the_caption_cannot_fit_them() {
    const FULL: i32 = 38;
    const MIN: i32 = 22;
    // A roomy caption: untouched, whatever the button count.
    assert_eq!(cell_width(FULL, MIN, 634, 10), FULL);
    assert_eq!(cell_width(FULL, MIN, 994, 12), FULL);
    // Exactly enough room is still "enough" — no premature shrinking.
    assert_eq!(cell_width(FULL, MIN, FULL * 12, 12), FULL);
    // The crowded case: shrink, and the whole set must then fit.
    let bw = cell_width(FULL, MIN, 394, 12);
    assert!(bw < FULL, "should have narrowed, got {bw}");
    assert!(
        bw * 12 <= 394,
        "narrowed to {bw} but 12 of them still overflow"
    );
    // Absurdly small: clamped at the floor rather than collapsing to slivers.
    assert_eq!(cell_width(FULL, MIN, 60, 12), MIN);
    // Degenerate inputs must not divide by zero or go negative.
    assert_eq!(cell_width(FULL, MIN, 634, 0), FULL);
    assert_eq!(cell_width(FULL, MIN, -20, 8), MIN);
}

fn rc(left: i32, right: i32) -> RECT {
    RECT {
        left,
        top: 0,
        right,
        bottom: 36,
    }
}

/// The guard in front of the tooltip re-point must fire on exactly the layout shift that
/// shipped broken, and stay quiet on a plain repaint.
///
/// The shipped bug: the tips were registered while the viewer was still `Loading`, where
/// `Btn::Ocr` is hidden, so Pin and Copy sat one 38 px slot further right than they would
/// once the decode landed and the OCR button appeared between Copy and Info. Nothing
/// re-pointed them afterwards, so hovering Copy showed the PIN's tooltip ("Keep on top")
/// and the pin itself had none. Buttons are right-packed, so only the entries LEFT of the
/// one that appeared move — which is why this has to compare the whole list rather than,
/// say, the count.
#[test]
fn tooltip_layout_change_is_detected_when_a_button_appears_mid_load() {
    // …Copy, [Ocr hidden], Info… — Pin and Copy sit one slot right of their final home.
    let loading = [rc(330, 368), rc(368, 406), rc(0, 0), rc(406, 444)];
    // The decode landed: Ocr took a slot and pushed Copy and Pin left.
    let decoded = [rc(292, 330), rc(330, 368), rc(368, 406), rc(406, 444)];
    assert!(
        tooltip_layout_changed(&loading, &decoded),
        "a button appearing must re-point the tips, or they describe the wrong buttons"
    );
    // A repaint that changed nothing (scroll notch, hover) must NOT send window messages.
    assert!(!tooltip_layout_changed(&decoded, &decoded));
    // A resize moves every right-anchored button, and must also be caught.
    let wider: Vec<RECT> = decoded
        .iter()
        .map(|r| rc(r.left + 80, r.right + 80))
        .collect();
    assert!(tooltip_layout_changed(&decoded, &wider));
    // First paint: nothing registered yet.
    assert!(tooltip_layout_changed(&[], &decoded));
}
