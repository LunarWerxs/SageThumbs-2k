//! The keyboard map: Escape, Enter, Delete, undo and redo, the clipboard chords and the tool letters.

use super::*;

/// `VK_ESCAPE` while the overlay is open: peel back transient editor state before closing
/// the whole capture. This makes Esc useful for correcting a mis-click instead of
/// immediately losing the screenshot underneath it; only once nothing is left to unwind
/// does it fall through to closing the window.
pub(super) unsafe fn on_key_escape(hwnd: HWND, s: &mut Shot) -> bool {
    if s.sel_dragging {
        s.sel_dragging = false; // cancel the initial region drag, keep the overlay
        return true;
    }
    if s.color_flyout || s.text_flyout || s.font_dropdown {
        s.color_flyout = false;
        s.text_flyout = false;
        s.font_dropdown = false;
        return true;
    }
    if s.typing.is_some() {
        s.typing = None; // cancel the in-progress text only
        s.typing_drag = false; // and any active reposition drag
        s.move_from = None;
        s.pending_hi = None; // drop any half-typed surrogate
        return true;
    }
    if s.draw_from.take().is_some() {
        s.pen_pts.clear(); // cancel the active annotation, keep the capture
        return true;
    }
    if s.selected.take().is_some() {
        s.move_from = None; // deselect first; a second Esc closes
        return true;
    }
    let _ = DestroyWindow(hwnd);
    false
}

/// `VK_RETURN`: accept to the clipboard (the quick "I'm done, it's copied" gesture) and
/// close, unless an annotation is mid-typing — then the newline falls through to WM_CHAR
/// instead of committing the whole capture.
pub(super) unsafe fn on_key_enter(hwnd: HWND, s: &mut Shot) -> bool {
    if s.typing.is_some() {
        // Enter while an annotation is being typed must insert a newline, not
        // commit + close the whole capture — otherwise multi-line annotations are
        // structurally impossible. Don't consume it here: falling through lets the
        // WM_CHAR(0x0D) that follows land the literal newline (see WM_CHAR above).
        return false;
    }
    if block_automation_output(s, "blocked-copy") {
        return true;
    }
    // Saving to a file is the explicit Ctrl+S / Save-button action.
    commit_text(s);
    // A clipboard failure (finish_copy already showed a toast) must not still destroy the
    // window — that discarded the capture with nothing copied and no way to retry it
    // No selection at all still closes as before: there is nothing to fail at.
    if s.sel.is_some() && !finish_copy(s) {
        return false;
    }
    let _ = DestroyWindow(hwnd);
    false
}

/// `VK_DELETE` under the Move tool: removes the grabbed shape and records it, so the next
/// Ctrl+Z (or the toolbar's Undo) puts it back at the same place in the stack. It used to
/// be pushed onto `redo`, while Ctrl+Z popped the NEWEST shape: one Delete and one Ctrl+Z
/// then removed two annotations.
pub(super) fn on_key_delete(s: &mut Shot) -> bool {
    if let Some(idx) = s.selected.take() {
        if idx < s.shapes.len() {
            let sh = s.shapes.remove(idx);
            remember_delete(idx, sh);
        }
    }
    s.move_from = None;
    true
}

/// Ctrl+Z (undo) / Ctrl+Y or Ctrl+Shift+Z (redo). `None` if `vk` is neither.
pub(super) fn on_key_undo_redo(s: &mut Shot, ctrl: bool, shift: bool, vk: u16) -> Option<bool> {
    if ctrl && !shift && vk == b'Z' as u16 {
        undo_last(s);
        return Some(true);
    }
    if ctrl && (vk == b'Y' as u16 || (shift && vk == b'Z' as u16)) {
        redo_last(s);
        return Some(true);
    }
    None
}

/// Ctrl+C: accept + close, but only once a region exists and the copy actually succeeds —
/// a clipboard failure (toast already shown by `finish_copy`) must leave the overlay open
/// so the capture isn't lost.
pub(super) unsafe fn on_ctrl_c(hwnd: HWND, s: &mut Shot) -> bool {
    if block_automation_output(s, "blocked-copy") {
        return true;
    }
    if s.sel.is_some() {
        commit_text(s);
        if finish_copy(s) {
            let _ = DestroyWindow(hwnd);
        }
    }
    false
}

/// Ctrl+T: accept + close via OCR (only once a region exists).
pub(super) unsafe fn on_ctrl_t(hwnd: HWND, s: &mut Shot) -> bool {
    if block_automation_output(s, "blocked-ocr") {
        return true;
    }
    if s.sel.is_some() {
        commit_text(s);
        close_if_handed_off(hwnd, finish_ocr(s));
    }
    false
}

/// Ctrl+S: accept + close via Save-As, which keeps the overlay open if that prompt is
/// cancelled.
pub(super) unsafe fn on_ctrl_s(hwnd: HWND, s: &mut Shot) -> bool {
    if block_automation_output(s, "blocked-save") {
        return true;
    }
    if s.sel.is_some() {
        commit_text(s);
        if finish_save(hwnd, s) {
            let _ = DestroyWindow(hwnd);
        }
    }
    false
}

/// Ctrl+U: upload & copy the link (G199b) — the toolbar's Upload button previously had no
/// keyboard equivalent at all, unlike every other action on the bar.
pub(super) unsafe fn on_ctrl_u(hwnd: HWND, s: &mut Shot) -> bool {
    if block_automation_output(s, "blocked-upload") {
        return true;
    }
    if s.sel.is_some() {
        commit_text(s);
        close_if_handed_off(hwnd, compose_and_spawn(s, "--upload"));
    }
    false
}

/// Ctrl+C (copy) / Ctrl+T (OCR) / Ctrl+S (save) / Ctrl+U (upload): each accepts + closes
/// (only once a region exists), checked before the plain-letter tool shortcuts below so
/// 'C'/'T' alone stay tool picks. `None` if `vk` is none of the four, or `ctrl` is not held.
pub(super) unsafe fn on_key_clipboard_action(
    hwnd: HWND,
    s: &mut Shot,
    ctrl: bool,
    vk: u16,
) -> Option<bool> {
    if !ctrl {
        return None;
    }
    match vk {
        x if x == b'C' as u16 => Some(on_ctrl_c(hwnd, s)),
        x if x == b'T' as u16 => Some(on_ctrl_t(hwnd, s)),
        x if x == b'S' as u16 => Some(on_ctrl_s(hwnd, s)),
        x if x == b'U' as u16 => Some(on_ctrl_u(hwnd, s)),
        _ => None,
    }
}

/// The plain-letter tool shortcut `vk` names, or `None` if it names no tool. Callers gate
/// out Ctrl/Alt combinations first — this is purely the letter -> `Tool` table.
pub(super) fn tool_shortcut_for(vk: u16) -> Option<Tool> {
    TOOL_SHORTCUTS
        .iter()
        .find(|(k, _)| *k == vk)
        .map(|(_, t)| *t)
}

/// The plain-letter tool shortcuts as a flat `(key, tool)` table, searched in order.
pub(super) const TOOL_SHORTCUTS: &[(u16, Tool)] = &[
    (b'R' as u16, Tool::Rect),
    (b'O' as u16, Tool::Ellipse),
    (b'C' as u16, Tool::Ellipse),
    (b'A' as u16, Tool::Arrow),
    (b'L' as u16, Tool::Line),
    (b'P' as u16, Tool::Pen),
    (b'T' as u16, Tool::Text),
    (b'N' as u16, Tool::Number),
    (b'H' as u16, Tool::Highlight),
    (b'B' as u16, Tool::Pixelate), // B = blur/blockify
    (b'I' as u16, Tool::Invert),
    (b'E' as u16, Tool::Eyedropper),
    (b'M' as u16, Tool::Move),
];

/// VK_OEM_4 '[' / VK_OEM_6 ']': text size while the Text tool is active, else line
/// thickness. Returns `false` if `vk` is neither key. Text size shares
/// [`tools::TEXT_SIZE_MIN`]/[`tools::TEXT_SIZE_MAX`] with the text-settings flyout's
/// own +/- buttons — a size the flyout allowed used to silently shrink the moment the
/// user next pressed `]`, because this path clamped to a smaller, different max.
pub(super) fn on_key_size_or_thickness(s: &mut Shot, vk: u16) -> bool {
    if vk == 0xDB {
        if s.tool == Tool::Text {
            let sz = (-s.text_font.lfHeight - 2).max(tools::TEXT_SIZE_MIN);
            s.text_font.lfHeight = -sz;
        } else {
            s.thickness = (s.thickness - 1).max(1);
        }
        return true;
    }
    if vk == 0xDD {
        if s.tool == Tool::Text {
            let sz = (-s.text_font.lfHeight + 2).min(tools::TEXT_SIZE_MAX);
            s.text_font.lfHeight = -sz;
        } else {
            s.thickness = (s.thickness + 1).min(40);
        }
        return true;
    }
    false
}

/// Keyboard: tool shortcuts, colour/thickness, undo/redo, accept (Enter → copy),
/// save (Ctrl+S), cancel/close (Esc). Returns true if a repaint is needed.
pub(in super::super) unsafe fn handle_key(hwnd: HWND, vk: u16) -> bool {
    let s = &mut *shot_ptr(hwnd);
    let ctrl = (GetKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000) != 0;
    let shift = (GetKeyState(VK_SHIFT.0 as i32) as u16 & 0x8000) != 0;
    let alt = (GetKeyState(VK_MENU.0 as i32) as u16 & 0x8000) != 0;

    // Windows automation can send a drag or a key chord, but cannot hold a
    // modifier across a drag. This automation-only latch lets a real mouse-message
    // drag exercise the exact same Shift-snap preview/commit path.
    if handle_key_automation(s, vk) {
        return true;
    }

    // Ignore the close keys for a moment after the overlay opens, so the keystroke
    // that fired the launching hotkey can't instantly cancel/accept the capture.
    if (vk == VK_ESCAPE.0 || vk == VK_RETURN.0)
        && GetTickCount64().saturating_sub(s.born) < SETTLE_CLOSE_MS
    {
        return false;
    }

    // Keyboard focus traversal: the only route a keyboard-only user has to the colour
    // palette and the font dropdown, both of which are coordinate-hit-tested flyouts with no
    // child controls for Windows to move focus between.
    //
    // Placed AFTER the settle guard so the launching hotkey's in-flight Esc/Enter is still
    // swallowed, and skipped entirely while text is being typed, because those keys belong
    // to the text (the `s.typing` early return below says the same thing for every other
    // key). `on_key_focus` returns `None` for anything it does not own, and everything it
    // does own except Tab is gated on `s.focus` already being `Some`.
    if s.typing.is_none() {
        if let Some(handled) = on_key_focus(hwnd, s, vk, shift) {
            return handled;
        }
    }

    if vk == VK_ESCAPE.0 {
        return on_key_escape(hwnd, s);
    }
    if vk == VK_RETURN.0 {
        return on_key_enter(hwnd, s);
    }
    // While typing text, swallow every other key here (the characters are inserted
    // by WM_CHAR) so letters go into the text instead of triggering tool shortcuts.
    if s.typing.is_some() {
        return false;
    }

    if vk == VK_DELETE.0 {
        return on_key_delete(s);
    }
    if let Some(handled) = on_key_undo_redo(s, ctrl, shift, vk) {
        return handled;
    }
    if let Some(handled) = on_key_clipboard_action(hwnd, s, ctrl, vk) {
        return handled;
    }

    // OCR launch mode never reaches the annotation pass, so the tool / colour / thickness
    // shortcuts below have nothing to act on — and the Eyedropper's pick-without-a-region
    // click would hijack the one drag the mode exists for. Swallow them.
    if s.ocr_mode {
        return false;
    }

    apply_tool_or_adjust_key(s, ctrl, alt, vk)
}

/// The F8 automation latch: toggle forced Shift on the attached automation state. Returns
/// whether it was handled (false when there is no automation state, so the key falls through).
pub(super) fn handle_key_automation(s: &mut Shot, vk: u16) -> bool {
    if vk != VK_F8.0 {
        return false;
    }
    if let Some(state) = s.automation.as_mut() {
        state.forced_shift = !state.forced_shift;
        state.status = "ready";
        return true;
    }
    false
}

/// The tool-letter / colour-cycle / size-or-thickness shortcuts, reached once every earlier
/// key group has declined. Ctrl/Alt+letter must never fall through to a plain-letter tool
/// shortcut — Ctrl+Z/Y/C/T/S are already intercepted explicitly by `handle_key`, but anything
/// else (Ctrl+A, Alt+R, …) used to silently switch tools instead of doing nothing.
pub(super) fn apply_tool_or_adjust_key(s: &mut Shot, ctrl: bool, alt: bool, vk: u16) -> bool {
    let new_tool = if ctrl || alt {
        None
    } else {
        tool_shortcut_for(vk)
    };
    if let Some(t) = new_tool {
        commit_text(s);
        s.tool = t;
        s.selected = None; // dropping the move selection when switching tools
        s.move_from = None;
        s.typing_drag = false;
        return true;
    }
    if vk == b'K' as u16 {
        s.cycle_color();
        return true;
    }
    on_key_size_or_thickness(s, vk)
}
