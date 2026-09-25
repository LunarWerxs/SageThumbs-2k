//! The keyboard focus model: Tab and the arrows walk the toolbar and its flyouts, Space and Enter invoke.

use super::*;

/// The keys the keyboard focus model owns. Everything except Tab is ADDITIONALLY gated on
/// focus already existing, at the one call site in [`on_key_focus`]; this table only says
/// "do not bother laying the toolbar out for a key that could never be a focus key".
///
/// Every one of these was a dead key in this window before the focus model: Tab, Space and
/// the four arrows fell through `handle_key` to `false`, and Esc/Enter are re-checked
/// against focus before anything is taken from them.
pub(super) fn is_focus_key(vk: u16) -> bool {
    [
        VK_TAB, VK_SPACE, VK_RETURN, VK_ESCAPE, VK_LEFT, VK_RIGHT, VK_UP, VK_DOWN,
    ]
    .iter()
    .any(|k| k.0 == vk)
}

/// The laid-out colour-palette items, or `None` when the palette is not open. This is the
/// same call the mouse hit-test makes, so a focus index can never address a cell the mouse
/// could not have clicked.
pub(in super::super) fn color_flyout_items(
    s: &Shot,
    buttons: &[(Button, RECT)],
    dpi: i32,
) -> Option<Vec<(Swatch, RECT)>> {
    if !s.color_flyout {
        return None;
    }
    let (_, cbr) = buttons.iter().find(|(b, _)| *b == Button::Color)?;
    Some(toolbar::color_flyout_layout(*cbr, s.vw, s.vh, &s.customs, dpi).1)
}

/// The laid-out text-settings items, or `None` when that flyout is not open. Note the list
/// LENGTHENS by one row per preset font while the dropdown is expanded, which is why nothing
/// here may assume an index survives a state change.
pub(in super::super) fn text_flyout_items(
    s: &Shot,
    buttons: &[(Button, RECT)],
    dpi: i32,
) -> Option<Vec<(TextItem, RECT)>> {
    if !s.text_flyout {
        return None;
    }
    let (_, tbr) = buttons
        .iter()
        .find(|(b, _)| *b == Button::Tool(Tool::Text))?;
    Some(toolbar::text_flyout_layout(*tbr, s.vw, s.vh, s.font_dropdown, dpi).1)
}

/// Where `btn` sits on the bar, used to hand focus back to the button that OWNS a flyout
/// once that flyout is gone.
pub(in super::super) fn button_index(buttons: &[(Button, RECT)], btn: Button) -> Option<usize> {
    buttons.iter().position(|(b, _)| *b == btn)
}

/// Reconcile `s.focus` with what is actually on screen, before any focus key acts on it.
///
/// Focus is an index into a list other code is free to change underneath it: expanding the
/// font dropdown lengthens the text flyout, picking any tool closes both flyouts outright,
/// and a plain mouse click can do either while focus is sitting in one. Rather than make
/// every one of those call sites remember a focus rule (which is how focus ends up pointing
/// at a cell that is no longer painted), the repair runs here, on the one path that reads
/// focus. A flyout that has closed hands focus back to the button that owns it; an index
/// past the end of its list is pulled back to the start.
pub(super) fn repair_focus(s: &mut Shot, buttons: &[(Button, RECT)], dpi: i32) {
    let Some(focus) = s.focus else { return };
    let repaired = match focus {
        FocusTarget::Toolbar(i) => repair_toolbar_focus(buttons, i),
        FocusTarget::ColorFlyout(i) => repair_color_focus(s, buttons, dpi, i),
        FocusTarget::TextFlyout(i) => repair_text_focus(s, buttons, dpi, i),
    };
    s.focus = repaired;
}

/// A toolbar focus index is valid unless it names a separator; otherwise fall back to the
/// bar's first focusable button. The bar's item table is fixed, so a bad index here is
/// defensive only.
pub(super) fn repair_toolbar_focus(buttons: &[(Button, RECT)], i: usize) -> Option<FocusTarget> {
    match buttons.get(i) {
        Some((b, _)) if !matches!(b, Button::Sep) => Some(FocusTarget::Toolbar(i)),
        _ => toolbar::first_focusable(buttons).map(FocusTarget::Toolbar),
    }
}

/// The shared repair rule for a flyout focus index: kept when in range, pulled to the first
/// item when past the end, or handed back to the button that owns the flyout once it has closed.
fn repair_flyout_focus<I>(
    items: Option<Vec<(I, RECT)>>,
    owner: Button,
    buttons: &[(Button, RECT)],
    i: usize,
    to_focus: fn(usize) -> FocusTarget,
) -> Option<FocusTarget> {
    match items {
        Some(items) if i < items.len() => Some(to_focus(i)),
        Some(_) => Some(to_focus(0)),
        None => button_index(buttons, owner).map(FocusTarget::Toolbar),
    }
}

/// A palette focus index is kept when in range, pulled to the first cell when past the end,
/// or handed back to the button that owns the palette once it has closed.
pub(super) fn repair_color_focus(
    s: &Shot,
    buttons: &[(Button, RECT)],
    dpi: i32,
    i: usize,
) -> Option<FocusTarget> {
    repair_flyout_focus(
        color_flyout_items(s, buttons, dpi),
        Button::Color,
        buttons,
        i,
        FocusTarget::ColorFlyout,
    )
}

/// Same as [`repair_color_focus`], for the text-settings flyout.
pub(super) fn repair_text_focus(
    s: &Shot,
    buttons: &[(Button, RECT)],
    dpi: i32,
    i: usize,
) -> Option<FocusTarget> {
    repair_flyout_focus(
        text_flyout_items(s, buttons, dpi),
        Button::Tool(Tool::Text),
        buttons,
        i,
        FocusTarget::TextFlyout,
    )
}

/// Follow the invoke: a flyout that just OPENED takes focus.
///
/// This is the user-visible point of the whole change. `handle_button` opens the palette or
/// the text settings without knowing anything about focus, and opening a panel that a
/// keyboard user then cannot reach is precisely the hole being closed here, so the move
/// happens once, right after any invoke, rather than being spelled out per button.
pub(super) fn focus_into_open_flyout(s: &mut Shot) {
    if s.color_flyout && !matches!(s.focus, Some(FocusTarget::ColorFlyout(_))) {
        s.focus = Some(FocusTarget::ColorFlyout(0));
    } else if s.text_flyout && !matches!(s.focus, Some(FocusTarget::TextFlyout(_))) {
        s.focus = Some(FocusTarget::TextFlyout(0));
    }
}

/// Space or Enter on the focused item. Returns whether a repaint is needed.
///
/// A toolbar button goes through `actions::handle_button`, the same seam the mouse click
/// uses, INCLUDING its "true means the window is gone" contract: when it returns true this
/// returns immediately and touches neither `s` nor `hwnd` again, because `DestroyWindow`
/// delivers `WM_DESTROY` synchronously and that frees the boxed `Shot` out from under us.
pub(in super::super) unsafe fn invoke_focus(
    hwnd: HWND,
    s: &mut Shot,
    buttons: &[(Button, RECT)],
    dpi: i32,
) -> bool {
    match s.focus {
        Some(FocusTarget::Toolbar(i)) => invoke_toolbar_focus(hwnd, s, buttons, i),
        Some(FocusTarget::ColorFlyout(i)) => invoke_color_focus(hwnd, s, buttons, dpi, i),
        Some(FocusTarget::TextFlyout(i)) => invoke_text_focus(hwnd, s, buttons, dpi, i),
        None => false,
    }
}

/// Space/Enter on a focused toolbar button: run its action through the shared
/// `actions::handle_button` seam, INCLUDING its "true means the window is gone" contract: when
/// it returns true this returns immediately and touches neither `s` nor `hwnd` again, because
/// `DestroyWindow` delivers `WM_DESTROY` synchronously and frees the boxed `Shot`.
pub(super) unsafe fn invoke_toolbar_focus(
    hwnd: HWND,
    s: &mut Shot,
    buttons: &[(Button, RECT)],
    i: usize,
) -> bool {
    let Some((btn, _)) = buttons.get(i).copied() else {
        return false;
    };
    if handle_button(hwnd, s, btn) {
        return false; // window destroyed, `s` and `hwnd` are both dangling now
    }
    focus_into_open_flyout(s);
    true
}

/// Space/Enter on a focused palette cell: pick the swatch, then hand focus back to the button
/// that opened the palette (any pick closes it).
pub(super) unsafe fn invoke_color_focus(
    hwnd: HWND,
    s: &mut Shot,
    buttons: &[(Button, RECT)],
    dpi: i32,
    i: usize,
) -> bool {
    let Some(items) = color_flyout_items(s, buttons, dpi) else {
        return false;
    };
    let Some((swatch, _)) = items.get(i).copied() else {
        return false;
    };
    apply_swatch(hwnd, s, swatch);
    // Any pick closes the palette, so focus returns to the button that opened it instead
    // of pointing into a panel that is no longer painted.
    s.focus = button_index(buttons, Button::Color).map(FocusTarget::Toolbar);
    true
}

/// Space/Enter on a focused text-settings item: apply it, then reconcile focus with whatever
/// the item did to the flyout (closing it, opening the font dropdown, or collapsing it).
pub(super) unsafe fn invoke_text_focus(
    hwnd: HWND,
    s: &mut Shot,
    buttons: &[(Button, RECT)],
    dpi: i32,
    i: usize,
) -> bool {
    let Some(items) = text_flyout_items(s, buttons, dpi) else {
        return false;
    };
    let Some((item, _)) = items.get(i).copied() else {
        return false;
    };
    let was_open = s.font_dropdown;
    apply_text_item(hwnd, s, item);
    if !s.text_flyout {
        // "Font... (more)" hands over to the native dialog and closes the flyout.
        s.focus = button_index(buttons, Button::Tool(Tool::Text)).map(FocusTarget::Toolbar);
    } else if matches!(item, TextItem::FontField) && s.font_dropdown && !was_open {
        // The font list is the second control with no keyboard route at all before
        // this, so opening it from the keyboard has to land INSIDE it. Index 1 is
        // the first option row: the field itself is always index 0, and the options
        // follow it directly (see `toolbar::text_flyout_layout`).
        s.focus = Some(FocusTarget::TextFlyout(1));
    } else if matches!(item, TextItem::FontOption(_)) {
        // Picking a font collapses the list, which shortens it back to the field.
        s.focus = Some(FocusTarget::TextFlyout(0));
    }
    true
}

/// Move focus one place with Tab / Shift+Tab, WITHIN the current group.
///
/// Traversal stays inside a group on purpose. An open flyout is modal to the mouse already
/// (a click anywhere else closes it), so letting Tab wander out of it would leave a panel
/// open with focus somewhere behind it. The ways out are the same two the mouse has:
/// choose something, or press Esc.
pub(super) fn step_focus_target(
    s: &Shot,
    buttons: &[(Button, RECT)],
    dpi: i32,
    forward: bool,
) -> Option<FocusTarget> {
    match s.focus {
        // The first Tab is the entry point into the whole model.
        None => toolbar::first_focusable(buttons).map(FocusTarget::Toolbar),
        Some(FocusTarget::Toolbar(i)) => {
            toolbar::step_focus(buttons, i, forward).map(FocusTarget::Toolbar)
        }
        Some(FocusTarget::ColorFlyout(i)) => step_color_focus(s, buttons, dpi, forward, i),
        Some(FocusTarget::TextFlyout(i)) => step_text_focus(s, buttons, dpi, forward, i),
    }
}

/// The shared Tab step for a flyout: move the index by ±1, wrapping within its laid-out items.
fn step_flyout_focus(
    len: usize,
    i: usize,
    forward: bool,
    to_focus: fn(usize) -> FocusTarget,
) -> Option<FocusTarget> {
    toolbar::wrap_step(len, i, if forward { 1 } else { -1 }).map(to_focus)
}

/// Tab inside the palette: step its focus index by ±1, wrapping within the laid-out cells.
pub(super) fn step_color_focus(
    s: &Shot,
    buttons: &[(Button, RECT)],
    dpi: i32,
    forward: bool,
    i: usize,
) -> Option<FocusTarget> {
    let items = color_flyout_items(s, buttons, dpi)?;
    step_flyout_focus(items.len(), i, forward, FocusTarget::ColorFlyout)
}

/// Tab inside the text flyout: step its focus index by ±1, wrapping within the laid-out items.
pub(super) fn step_text_focus(
    s: &Shot,
    buttons: &[(Button, RECT)],
    dpi: i32,
    forward: bool,
    i: usize,
) -> Option<FocusTarget> {
    let items = text_flyout_items(s, buttons, dpi)?;
    step_flyout_focus(items.len(), i, forward, FocusTarget::TextFlyout)
}

/// Move focus with an arrow key. `vertical` steps by the group's measured column count, so
/// the palette behaves as the grid it is and the text flyout, whose first row is a single
/// field and so measures one column, behaves as the stack it is. The toolbar is a single row
/// and is handled by the caller.
pub(super) fn arrow_focus_target(
    s: &Shot,
    buttons: &[(Button, RECT)],
    dpi: i32,
    forward: bool,
    vertical: bool,
) -> Option<FocusTarget> {
    match s.focus {
        Some(FocusTarget::Toolbar(i)) => {
            toolbar::step_focus(buttons, i, forward).map(FocusTarget::Toolbar)
        }
        Some(FocusTarget::ColorFlyout(i)) => {
            arrow_step_color(s, buttons, dpi, forward, vertical, i)
        }
        Some(FocusTarget::TextFlyout(i)) => arrow_step_text(s, buttons, dpi, forward, vertical, i),
        None => None,
    }
}

/// The arrow step magnitude for a group's laid-out rects: its measured column count when
/// moving vertically, else 1.
pub(super) fn arrow_step_magnitude(rects: &[RECT], vertical: bool) -> isize {
    if vertical {
        toolbar::grid_cols(rects).max(1) as isize
    } else {
        1
    }
}

/// The shared arrow step for a flyout: step by the group's measured column count when moving
/// vertically, else by one, wrapping within its laid-out items.
fn arrow_step_flyout<I>(
    items: Vec<(I, RECT)>,
    forward: bool,
    vertical: bool,
    i: usize,
    to_focus: fn(usize) -> FocusTarget,
) -> Option<FocusTarget> {
    let rects: Vec<RECT> = items.iter().map(|(_, r)| *r).collect();
    let mag = arrow_step_magnitude(&rects, vertical);
    toolbar::wrap_step(items.len(), i, if forward { mag } else { -mag }).map(to_focus)
}

/// Arrow-step the palette focus index within its grid.
pub(super) fn arrow_step_color(
    s: &Shot,
    buttons: &[(Button, RECT)],
    dpi: i32,
    forward: bool,
    vertical: bool,
    i: usize,
) -> Option<FocusTarget> {
    arrow_step_flyout(
        color_flyout_items(s, buttons, dpi)?,
        forward,
        vertical,
        i,
        FocusTarget::ColorFlyout,
    )
}

/// Arrow-step the text-flyout focus index within its grid.
pub(super) fn arrow_step_text(
    s: &Shot,
    buttons: &[(Button, RECT)],
    dpi: i32,
    forward: bool,
    vertical: bool,
    i: usize,
) -> Option<FocusTarget> {
    arrow_step_flyout(
        text_flyout_items(s, buttons, dpi)?,
        forward,
        vertical,
        i,
        FocusTarget::TextFlyout,
    )
}

/// The keyboard focus model's key handling, and the ONLY place the keyboard model sets
/// `s.focus` from nothing.
///
/// `None` means "this key is not mine here" and the caller must carry on exactly as it did
/// before this model existed. That gate is the entire compatibility contract of this change:
/// everything below the Tab branch is reached only when focus is ALREADY set, the keyboard
/// model starts `None`, and only Tab sets it, so a user who never presses Tab cannot observe
/// any of this, not even a swallowed keystroke.
pub(super) unsafe fn on_key_focus(hwnd: HWND, s: &mut Shot, vk: u16, shift: bool) -> Option<bool> {
    // Cheap gates first: this runs on EVERY key-down, so laying the toolbar out for a key
    // that could not be a focus key, or for a user who has never pressed Tab, is pure waste.
    if !is_focus_key(vk) || (s.focus.is_none() && vk != VK_TAB.0) {
        return None;
    }
    // No bar means nothing to focus: before the first region is committed, and in the OCR
    // launch mode, which finishes on the drag and never shows a toolbar at all.
    let sel = s.sel?;
    if s.ocr_mode {
        return None;
    }
    let dpi = shot_dpi_for_sel(s, sel);
    let buttons = toolbar_layout_cached(s, sel, dpi);
    repair_focus(s, &buttons, dpi);

    if vk == VK_TAB.0 {
        // A group with nothing focusable in it leaves the key alone rather than eating it.
        let next = step_focus_target(s, &buttons, dpi, !shift)?;
        s.focus = Some(next);
        return Some(true);
    }

    // Below here focus is already set, or `repair_focus` could not find anywhere to put it.
    s.focus?;

    if vk == VK_ESCAPE.0 {
        // Esc peels focus off FIRST and stops there: the overlay stays open, and the next
        // Esc does exactly what Esc has always done (close a flyout, cancel an in-progress
        // edit, deselect, then close the capture). `handle_key`'s SETTLE_CLOSE_MS guard runs
        // before this, so the launching hotkey's in-flight keystroke is still swallowed.
        s.focus = None;
        return Some(true);
    }

    if vk == VK_SPACE.0 || vk == VK_RETURN.0 {
        return Some(invoke_focus(hwnd, s, &buttons, dpi));
    }

    on_key_focus_arrow(s, &buttons, dpi, vk)
}

/// The four arrow keys under the focus model: step focus within the group, or report
/// `None` for a key the model does not own.
pub(super) unsafe fn on_key_focus_arrow(
    s: &mut Shot,
    buttons: &[(Button, RECT)],
    dpi: i32,
    vk: u16,
) -> Option<bool> {
    let (forward, vertical) = match vk {
        x if x == VK_LEFT.0 => (false, false),
        x if x == VK_RIGHT.0 => (true, false),
        x if x == VK_UP.0 => (false, true),
        x if x == VK_DOWN.0 => (true, true),
        _ => return None,
    };
    if vertical && matches!(s.focus, Some(FocusTarget::Toolbar(_))) {
        return None; // the bar is one row, so Up/Down stay the no-ops they were
    }
    if let Some(next) = arrow_focus_target(s, buttons, dpi, forward, vertical) {
        s.focus = Some(next);
        return Some(true);
    }
    Some(false)
}
