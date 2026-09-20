//! The mouse: button down, the drag in between and button up, plus the hover state a move updates.

use super::*;

/// `WM_LBUTTONDOWN`: with no region yet, either sample a pixel (Eyedropper) or start the
/// region drag; with a region already up, route through the annotation toolbar/flyouts/
/// canvas via [`on_lbuttondown_selected`].
pub(super) unsafe fn on_lbuttondown(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    activate_overlay(hwnd);
    let s = &mut *shot_ptr(hwnd);
    let p = pt(lparam);
    match s.sel {
        None => {
            if s.tool == Tool::Eyedropper {
                // Pick a colour without dragging a region first (E + click).
                sample_pixel(s, p);
            } else {
                s.sel_dragging = true;
                s.sel_anchor = p;
                s.cur = p;
            }
            let _ = InvalidateRect(Some(hwnd), None, false);
            LRESULT(0)
        }
        Some(sel) => on_lbuttondown_selected(hwnd, s, sel, p),
    }
}

/// A mouse-down while a region is already up: flyouts intercept first, then the
/// toolbar, then the click falls through to whatever the active tool does with it.
pub(super) unsafe fn on_lbuttondown_selected(
    hwnd: HWND,
    s: &mut Shot,
    sel: RECT,
    p: POINT,
) -> LRESULT {
    let dpi = shot_dpi_for_sel(s, sel);
    let buttons = toolbar::layout(sel, s.vw, s.vh, dpi);
    if let Some(r) = try_color_flyout_click(hwnd, s, &buttons, p, dpi) {
        return r;
    }
    if let Some(r) = try_text_flyout_click(hwnd, s, &buttons, p, dpi) {
        return r;
    }
    if let Some(r) = try_toolbar_button_click(hwnd, s, &buttons, p) {
        return r;
    }
    apply_selection_click(s, p);
    let _ = InvalidateRect(Some(hwnd), None, false);
    LRESULT(0)
}

/// Apply a colour-palette choice: pick the colour, or open the native picker for the two
/// cells that mean "make a new custom", then close the palette.
///
/// This body used to live inline in `try_color_flyout_click`, which made the mouse the ONLY
/// thing that could ever choose a colour. `actions::handle_button` is already the shared
/// invoke seam for the toolbar; the two flyouts had no equivalent, so the keyboard invoke
/// would have had to be a copy, and a copy is a second truth that drifts. Extracted instead,
/// with the mouse path now routing through it, so both callers are provably the same action.
pub(in super::super) unsafe fn apply_swatch(hwnd: HWND, s: &mut Shot, swatch: Swatch) {
    match swatch {
        Swatch::Color(c) | Swatch::Custom(Some(c)) => s.cur_color = c,
        Swatch::Custom(None) | Swatch::Picker => pick_custom_color(hwnd, s),
    }
    s.color_flyout = false;
}

/// Apply a text-settings flyout choice. Extracted from `try_text_flyout_click` for the same
/// reason as [`apply_swatch`]: the keyboard invoke has to run this exact code, not a copy of
/// it. `More` deliberately closes the whole flyout, because it hands over to the modal
/// native Font dialog.
pub(in super::super) unsafe fn apply_text_item(hwnd: HWND, s: &mut Shot, item: TextItem) {
    match item {
        TextItem::FontField => s.font_dropdown = !s.font_dropdown,
        TextItem::FontOption(i) => {
            tools::set_face(&mut s.text_font, toolbar::PRESET_FONTS[i]);
            s.font_dropdown = false;
        }
        TextItem::SizeDown => {
            let sz = (-s.text_font.lfHeight - 2).max(tools::TEXT_SIZE_MIN);
            s.text_font.lfHeight = -sz;
        }
        TextItem::SizeUp => {
            let sz = (-s.text_font.lfHeight + 2).min(tools::TEXT_SIZE_MAX);
            s.text_font.lfHeight = -sz;
        }
        TextItem::Bold => {
            s.text_font.lfWeight = if s.text_font.lfWeight >= 700 {
                400
            } else {
                700
            };
        }
        TextItem::Underline => {
            s.text_font.lfUnderline = u8::from(s.text_font.lfUnderline == 0);
        }
        TextItem::More => {
            pick_text_font(hwnd, s);
            s.text_flyout = false;
            s.font_dropdown = false;
        }
    }
}

/// Click routing for the open colour palette flyout: `Some(_)` means the caller must
/// return that `LRESULT` immediately (the click was consumed); `None` means the flyout
/// wasn't open, or it just closed and the click should keep falling through untouched.
pub(super) unsafe fn try_color_flyout_click(
    hwnd: HWND,
    s: &mut Shot,
    buttons: &[(Button, RECT)],
    p: POINT,
    dpi: i32,
) -> Option<LRESULT> {
    if !s.color_flyout {
        return None;
    }
    if let Some((_, cbr)) = buttons.iter().find(|(b, _)| *b == Button::Color) {
        let (_, sw) = toolbar::color_flyout_layout(*cbr, s.vw, s.vh, &s.customs, dpi);
        if let Some((swatch, _)) = sw.iter().find(|(_, r)| pt_in(*r, p)) {
            apply_swatch(hwnd, s, *swatch);
            let _ = InvalidateRect(Some(hwnd), None, false);
            return Some(LRESULT(0));
        }
    }
    // Clicked off the palette → close it; consume if that click was the Colour button
    // itself (else fall through).
    s.color_flyout = false;
    let _ = InvalidateRect(Some(hwnd), None, false);
    if toolbar::hit(buttons, p.x, p.y) == Some(Button::Color) {
        return Some(LRESULT(0));
    }
    None
}

/// Click routing for the open text-settings flyout, same shape as
/// [`try_color_flyout_click`]: `Some(_)` to return immediately, `None` to fall through.
pub(super) unsafe fn try_text_flyout_click(
    hwnd: HWND,
    s: &mut Shot,
    buttons: &[(Button, RECT)],
    p: POINT,
    dpi: i32,
) -> Option<LRESULT> {
    if !s.text_flyout {
        return None;
    }
    if let Some((_, tbr)) = buttons.iter().find(|(b, _)| *b == Button::Tool(Tool::Text)) {
        let (_, its) = toolbar::text_flyout_layout(*tbr, s.vw, s.vh, s.font_dropdown, dpi);
        if let Some((item, _)) = its.iter().find(|(_, r)| pt_in(*r, p)) {
            apply_text_item(hwnd, s, *item);
            let _ = InvalidateRect(Some(hwnd), None, false);
            return Some(LRESULT(0));
        }
    }
    // Clicked off the flyout → close it. Consume if it was the Text button itself; else
    // fall through (a canvas click then drops the text caret and starts typing).
    s.text_flyout = false;
    s.font_dropdown = false;
    let _ = InvalidateRect(Some(hwnd), None, false);
    if toolbar::hit(buttons, p.x, p.y) == Some(Button::Tool(Tool::Text)) {
        return Some(LRESULT(0));
    }
    None
}

/// A click on a toolbar button takes priority over drawing. `None` means the click
/// missed every button.
pub(super) unsafe fn try_toolbar_button_click(
    hwnd: HWND,
    s: &mut Shot,
    buttons: &[(Button, RECT)],
    p: POINT,
) -> Option<LRESULT> {
    let btn = toolbar::hit(buttons, p.x, p.y)?;
    if handle_button(hwnd, s, btn) {
        return Some(LRESULT(0)); // window destroyed — stop touching it
    }
    let _ = InvalidateRect(Some(hwnd), None, false);
    Some(LRESULT(0))
}

/// The click missed every flyout and toolbar button: route it to whatever the active
/// tool does with a fresh mouse-down on the canvas itself.
pub(super) unsafe fn apply_selection_click(s: &mut Shot, p: POINT) {
    let ctrl = (GetKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000) != 0;
    if ctrl && s.typing.is_some() && s.tool == Tool::Text {
        // Ctrl-drag while typing repositions the *active* text box (you stay in edit
        // mode) — place the caption as you write it.
        s.typing_drag = true;
        s.move_from = Some(p);
    } else if ctrl || s.tool == Tool::Move {
        begin_move_grab(s, p);
    } else if s.tool == Tool::Eyedropper {
        sample_pixel(s, p); // grab the pixel's colour; never draws
    } else if s.tool == Tool::Text {
        apply_text_click(s, p);
    } else if s.tool == Tool::Number {
        apply_number_click(s, p);
    } else {
        begin_draw(s, p);
    }
}

/// Move tool — or Ctrl-drag with any tool — grabs the topmost shape under the cursor (if
/// any) and starts a fresh undo record (a total left over from a PREVIOUS drag must not
/// apply to this one).
pub(super) unsafe fn begin_move_grab(s: &mut Shot, p: POINT) {
    s.selected = tools::hit_shape(&s.shapes, p.x, p.y);
    s.move_from = s.selected.map(|_| p);
    MOVE_UNDO.with(|c| c.set(None));
}

/// Text-tool click: while typing, finish & deselect (no new box on this click); when idle,
/// start a fresh box. Predictable "click away to commit" instead of spawning an empty box
/// you then have to Esc out of.
pub(super) unsafe fn apply_text_click(s: &mut Shot, p: POINT) {
    if s.typing.is_some() {
        commit_text(s);
    } else {
        s.typing = Some((p, String::new()));
        s.pending_hi = None; // fresh buffer, no half-typed surrogate
    }
}

/// Number-tool click: stamp the next number as a new shape (the new "last action").
pub(super) unsafe fn apply_number_click(s: &mut Shot, p: POINT) {
    let n = s.number_next;
    s.number_next += 1;
    let color = s.color();
    s.shapes.push(Shape::Number { at: p, n, color });
    s.redo.clear();
    MOVE_UNDO.with(|c| c.set(None));
}

/// Start a freehand/shape draw at `p` (any other tool).
pub(super) unsafe fn begin_draw(s: &mut Shot, p: POINT) {
    s.draw_from = Some(p);
    s.pen_pts.clear();
    s.pen_pts.push(p);
    s.cur = p;
}

/// `WM_MOUSEMOVE`: the loupe, the active drag (if any), the click-a-window hint, and the
/// toolbar hover/tooltip timer are independent concerns, each gets its own pass.
pub(super) unsafe fn on_mousemove(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    let s = &mut *shot_ptr(hwnd);
    let p = pt(lparam);
    update_eyedropper_loupe(hwnd, s, p);
    let old_cur = s.cur;
    s.cur = p;
    update_active_drag(hwnd, s, p, old_cur);
    update_window_hint(hwnd, s, p);
    update_hover_button(hwnd, s, p);
    LRESULT(0)
}

/// The Eyedropper loupe tracks the cursor: clear the "copied" flash and repaint just the
/// old + new loupe areas (not the whole virtual screen, which would be a heavy blit per
/// tick on a multi-monitor desktop).
pub(super) unsafe fn update_eyedropper_loupe(hwnd: HWND, s: &mut Shot, p: POINT) {
    if s.tool != Tool::Eyedropper {
        return;
    }
    let old = loupe_rect(s, s.cur.x, s.cur.y);
    let new = loupe_rect(s, p.x, p.y);
    s.eye_copied = false;
    let _ = InvalidateRect(Some(hwnd), Some(&old), false);
    let _ = InvalidateRect(Some(hwnd), Some(&new), false);
}

/// Extra margin (screen px) around a drag's old/new bounding rect before invalidating it:
/// covers chrome drawn outside the shape's own bbox — the selection's live size
/// badge (drawn just above the rect), the green move-selection frame's +3px halo, arrow
/// heads, and pen/shape stroke thickness (up to 40px, see `on_key_size_or_thickness`) —
/// so the dirty region stays generous rather than pixel-tight while remaining nowhere
/// near a full-virtual-screen invalidate on every mouse-move tick.
pub(super) const DRAG_DIRTY_MARGIN: i32 = 100;

/// `r` grown by `m` on every side.
pub(super) fn inflate(r: RECT, m: i32) -> RECT {
    RECT {
        left: r.left - m,
        top: r.top - m,
        right: r.right + m,
        bottom: r.bottom + m,
    }
}

/// Invalidate just the union of `old` and `new` (each margin-inflated) — the same
/// old-rect + new-rect pattern `update_eyedropper_loupe` already uses, so a drag tick
/// repaints only the area that actually changed instead of the whole virtual screen
/// (the module's own doc comment on `FRAME` claims this already happens; P53 found it
/// didn't for any drawing/selection/move drag, only for the Eyedropper loupe).
pub(super) unsafe fn invalidate_drag_span(hwnd: HWND, old: RECT, new: RECT) {
    let _ = InvalidateRect(Some(hwnd), Some(&inflate(old, DRAG_DIRTY_MARGIN)), false);
    let _ = InvalidateRect(Some(hwnd), Some(&inflate(new, DRAG_DIRTY_MARGIN)), false);
}

/// Whichever gesture is currently active (region drag, text-box reposition, shape move,
/// or freehand pen) advances by this tick's cursor position. At most one of these is
/// ever active at once. `old_cur` is the cursor position as of the LAST tick (before
/// this call's caller overwrote `s.cur`), needed to compute the "old" half of each
/// drag's before/after dirty rect.
pub(super) unsafe fn update_active_drag(hwnd: HWND, s: &mut Shot, p: POINT, old_cur: POINT) {
    if s.sel_dragging {
        drag_selection_region(hwnd, s, p, old_cur);
    } else if s.typing_drag {
        drag_active_text(hwnd, s, p);
    } else if s.move_from.is_some() && s.selected.is_some() {
        drag_selected_shape(hwnd, s, p);
    } else if s.draw_from.is_some() {
        drag_draw(hwnd, s, p, old_cur);
    }
}

/// Advance the region drag to this tick and dirty the old+new bounding rects.
pub(super) unsafe fn drag_selection_region(hwnd: HWND, s: &mut Shot, p: POINT, old_cur: POINT) {
    let old = tools::norm(s.sel_anchor, old_cur);
    let new = tools::norm(s.sel_anchor, p);
    invalidate_drag_span(hwnd, old, new);
}

/// Reposition the active text box by the cursor delta (still editing).
pub(super) unsafe fn drag_active_text(hwnd: HWND, s: &mut Shot, p: POINT) {
    if let Some(from) = s.move_from {
        if let Some((at, buf)) = s.typing.as_mut() {
            let old_r = tools::text_extent(*at, buf, &s.text_font);
            at.x += p.x - from.x;
            at.y += p.y - from.y;
            let new_r = tools::text_extent(*at, buf, &s.text_font);
            invalidate_drag_span(hwnd, old_r, new_r);
        }
        s.move_from = Some(p);
    }
}

/// Drag the grabbed shape by the cursor delta, folding this tick's delta into the drag's
/// running total so Ctrl+Z can undo the WHOLE drag (not just the last tick) by inverting it.
pub(super) unsafe fn drag_selected_shape(hwnd: HWND, s: &mut Shot, p: POINT) {
    let (Some(from), Some(idx)) = (s.move_from, s.selected) else {
        return;
    };
    let (dx, dy) = (p.x - from.x, p.y - from.y);
    if idx < s.shapes.len() {
        let old_bb = tools::shape_bbox(&s.shapes[idx]);
        tools::translate_shape(&mut s.shapes[idx], dx, dy);
        MOVE_UNDO.with(|c| c.set(Some(accumulate_move_undo(c.get(), idx, dx, dy))));
        let new_bb = tools::shape_bbox(&s.shapes[idx]);
        invalidate_drag_span(hwnd, old_bb, new_bb);
    }
    s.move_from = Some(p);
}

/// Advance the active draw gesture to this tick: for the Pen only the newest segment is new
/// geometry (everything before it was already painted correctly on the last tick); for every
/// other tool, dirty the old and new endpoint bounding rects.
pub(super) unsafe fn drag_draw(hwnd: HWND, s: &mut Shot, p: POINT, old_cur: POINT) {
    let Some(a) = s.draw_from else { return };
    if s.tool == Tool::Pen {
        let seg_from = s.pen_pts.last().copied().unwrap_or(old_cur);
        s.pen_pts.push(p);
        let seg = tools::norm(seg_from, p);
        let _ = InvalidateRect(Some(hwnd), Some(&inflate(seg, DRAG_DIRTY_MARGIN)), false);
    } else {
        let shift = shift_active(s);
        let old_b = tools::drag_endpoint(s.tool, a, old_cur, shift);
        let new_b = tools::drag_endpoint(s.tool, a, p, shift);
        invalidate_drag_span(hwnd, tools::norm(a, old_b), tools::norm(a, new_b));
    }
}

/// Minimum interval between full z-order re-scans for the click-a-window hint:
/// [`window_under`] does up to two DWM calls per top-level window it walks past, which
/// is real cost to pay on every single WM_MOUSEMOVE tick while nothing is selected yet.
pub(super) const WINDOW_HINT_THROTTLE_MS: u64 = 50;

/// Before any selection exists, track the WINDOW under the cursor so a bare click can
/// capture it (the hint paints as a live preview). Not while the Eyedropper is armed —
/// there a click means "sample this pixel", and a window highlight would promise
/// something the click won't do. The z-order walk itself is throttled to
/// [`WINDOW_HINT_THROTTLE_MS`] — between scans the existing hint is kept as-is,
/// which is imperceptible at that cadence and far cheaper than a DWM query pair per
/// candidate window on every tick.
pub(super) unsafe fn update_window_hint(hwnd: HWND, s: &mut Shot, p: POINT) {
    if s.sel.is_some() || s.sel_dragging {
        return;
    }
    let now = GetTickCount64();
    if now.saturating_sub(s.win_hint_scan_ms) < WINDOW_HINT_THROTTLE_MS {
        return;
    }
    s.win_hint_scan_ms = now;
    let hint = if s.tool == Tool::Eyedropper || s.automation.is_some() {
        None
    } else {
        window_under(hwnd, s.vx, s.vy, s.vw, s.vh, p)
    };
    let changed = match (s.win_hint, hint) {
        (None, None) => false,
        (Some(a), Some(b)) => {
            a.left != b.left || a.top != b.top || a.right != b.right || a.bottom != b.bottom
        }
        _ => true,
    };
    if changed {
        s.win_hint = hint;
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

/// The toolbar's layout for `sel` at `dpi`, rebuilt only when the selection or DPI
/// actually changed since the last call. WM_MOUSEMOVE (this function's caller)
/// and WM_SETCURSOR (`is_over_toolbar_ui`) both ask for the layout on essentially every
/// tick — Windows sends both messages per mouse move — so until now each rebuilt the
/// same 24-button layout from scratch a second time.
pub(in super::super) unsafe fn toolbar_layout_cached(
    s: &mut Shot,
    sel: RECT,
    dpi: i32,
) -> Vec<(Button, RECT)> {
    let key = (sel.left, sel.top, sel.right, sel.bottom, dpi);
    if s.tb_cache_key != Some(key) {
        s.tb_cache = toolbar::layout(sel, s.vw, s.vh, dpi);
        s.tb_cache_key = Some(key);
    }
    s.tb_cache.clone()
}

/// Track which toolbar button we're hovering (only when idle), and (re)arm the
/// hover-delay timer so the tooltip pops after a beat.
pub(super) unsafe fn update_hover_button(hwnd: HWND, s: &mut Shot, p: POINT) {
    let idle = !s.sel_dragging && s.draw_from.is_none() && s.move_from.is_none();
    let hovered = match (idle, s.sel) {
        (true, Some(sel)) => {
            let dpi = shot_dpi_for_sel(s, sel);
            toolbar::hit(&toolbar_layout_cached(s, sel, dpi), p.x, p.y)
        }
        _ => None,
    };
    if hovered != s.hover_btn {
        s.hover_btn = hovered;
        s.tip_show = false;
        let _ = KillTimer(Some(hwnd), HOVER_TIMER);
        if hovered.is_some() {
            let _ = SetTimer(Some(hwnd), HOVER_TIMER, 450, None);
        }
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

/// `WM_LBUTTONUP`: finish whichever gesture was active (region drag, text reposition,
/// shape move, or a drawn annotation).
pub(super) unsafe fn on_lbuttonup(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    let s = &mut *shot_ptr(hwnd);
    let p = pt(lparam);
    if s.sel_dragging {
        if let Some(r) = finish_selection_drag(hwnd, s, p) {
            return r;
        }
    } else if s.typing_drag {
        s.typing_drag = false;
        s.move_from = None; // done repositioning the active text box
    } else if s.move_from.is_some() {
        s.move_from = None; // finished dragging the selected shape
    } else if let Some(a) = s.draw_from.take() {
        finish_draw_drag(s, a, p);
    }
    let _ = InvalidateRect(Some(hwnd), None, false);
    LRESULT(0)
}

/// The region drag (or click-a-window "drag") just ended: commit the region, and in OCR
/// launch mode recognize + close immediately instead of raising the annotation toolbar.
/// `Some(_)` means the caller must return that `LRESULT` immediately (the window may
/// already be destroyed).
pub(super) unsafe fn finish_selection_drag(hwnd: HWND, s: &mut Shot, p: POINT) -> Option<LRESULT> {
    s.sel_dragging = false;
    let r = tools::norm(s.sel_anchor, p);
    if (r.right - r.left) > 4 && (r.bottom - r.top) > 4 {
        s.sel = Some(r);
        // OCR launch mode: the region IS the whole gesture. Recognize and close instead
        // of raising the annotation toolbar. A too-small drag falls through with no
        // selection, so the overlay stays up for a second try rather than closing on a
        // mis-click.
        if s.ocr_mode {
            finish_ocr(s);
            let _ = DestroyWindow(hwnd);
            return Some(LRESULT(0));
        }
    } else if let Some(w) = s.win_hint.take() {
        // A CLICK (a "drag" under the threshold) with a window highlighted: the window
        // IS the region. Same commit as a drag ending — including OCR mode, where
        // clicking a dialog reads the text out of it.
        s.sel = Some(w);
        if s.ocr_mode {
            finish_ocr(s);
            let _ = DestroyWindow(hwnd);
            return Some(LRESULT(0));
        }
    }
    s.win_hint = None;
    None
}

/// A drawn annotation (rect/arrow/pen/…) just finished: turn it into a shape and, under
/// automation, record it as the drag the next `AutomationDrag` query will report.
pub(super) unsafe fn finish_draw_drag(s: &mut Shot, a: POINT, p: POINT) {
    let shift = shift_active(s);
    let tool = s.tool;
    let final_point = tools::drag_endpoint(tool, a, p, shift);
    if finish_shape(s, a, final_point) {
        if let Some(state) = s.automation.as_mut() {
            state.commit_gen += 1;
            state.last_drag = Some(AutomationDrag {
                tool,
                anchor: a,
                raw: p,
                final_point,
                snapped: shift,
            });
            state.status = "ready";
        }
    }
}
