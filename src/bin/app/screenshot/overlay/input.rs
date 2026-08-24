//! Everything that turns user input into state changes: the window procedure, the
//! keyboard map, and the two commit paths (a finished drag becomes a shape, a finished
//! text buffer becomes a placed item).
//!
//! Kept apart from `paint` on purpose - this module decides WHAT is true, `paint` only
//! decides how it looks.

use super::*;
// Not in overlay.rs's own `KeyboardAndMouse` import list (nothing there needed Alt before
// this file's ctrl/alt gate on the tool-letter shortcuts), so pulled in directly here.
use std::cell::Cell;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_MENU;

thread_local! {
    /// The Move tool's current/most-recently-finished drag: `(shape index, total dx,
    /// total dy)` accumulated since the last grab, so Ctrl+Z can undo a finished move by
    /// inverting the total translation. Move-dragging used to mutate a shape's position
    /// directly with no undo entry recorded anywhere.
    ///
    /// `Shot` can't carry this itself — it's defined in the parent `overlay.rs` hub, out
    /// of scope for this fix — and a thread-local is safe here because only one capture
    /// overlay is ever alive on this thread at a time (`screenshot::run_capture*` owns
    /// the whole lifecycle as a singleton). Reset to `None` on every new grab
    /// (`WM_LBUTTONDOWN`'s Move branch) and by every OTHER action that mutates `shapes`
    /// (a new shape, a delete, a redo) — see the `set(None)` beside each — so a stale
    /// entry, with an index that may no longer point at the same shape, can never apply.
    static MOVE_UNDO: Cell<Option<(usize, i32, i32)>> = const { Cell::new(None) };
}

pub(super) fn pt(lparam: LPARAM) -> POINT {
    POINT {
        x: (lparam.0 & 0xffff) as u16 as i16 as i32,
        y: ((lparam.0 >> 16) & 0xffff) as u16 as i16 as i32,
    }
}

/// The top-level window under client point `p`, as a client-space rect clamped to the
/// overlay — or `None` over the bare desktop. Drives the click-a-window capture: hovering
/// previews this rect, a sub-threshold "drag" (a click) selects it.
///
/// Walks the REAL z-order (`GetTopWindow` + `GW_HWNDNEXT`) rather than `WindowFromPoint`,
/// which would always answer with the fullscreen overlay itself. The windows behind the
/// overlay still exist and still answer geometry queries; only their pixels are frozen in
/// our snapshot — which is exactly what makes the preview truthful: the rect is where the
/// window WAS at freeze time, and background windows cannot move while a topmost overlay
/// owns the foreground.
///
/// Skips, in the order they bite: our own overlay, invisible windows, minimized windows
/// (their rect is a parked -32000 fiction), DWM-cloaked windows (UWP apps suspended on
/// another virtual desktop LOOK visible to `IsWindowVisible` but draw nothing — a hint
/// that selects an invisible window would capture whatever is behind it), and the desktop
/// shell pair (Progman/WorkerW — "the desktop" is not a window pick, drag instead).
/// The rect is `DWMWA_EXTENDED_FRAME_BOUNDS` — the visual bounds — because `GetWindowRect`
/// includes the invisible resize borders Windows 10+ draws the drop shadow in, which reads
/// as "the capture grabbed a margin of the window behind it".
unsafe fn window_under(
    overlay: HWND,
    vx: i32,
    vy: i32,
    vw: i32,
    vh: i32,
    p: POINT,
) -> Option<RECT> {
    use windows::Win32::Graphics::Dwm::{
        DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetClassNameW, GetTopWindow, GetWindow, IsIconic, IsWindowVisible, GW_HWNDNEXT,
    };
    let screen = POINT {
        x: p.x + vx,
        y: p.y + vy,
    };
    let mut h = GetTopWindow(None).ok()?;
    loop {
        let next = || GetWindow(h, GW_HWNDNEXT).ok();
        if h == overlay || !IsWindowVisible(h).as_bool() || IsIconic(h).as_bool() {
            h = next()?;
            continue;
        }
        let mut cloaked: u32 = 0;
        let _ = DwmGetWindowAttribute(
            h,
            DWMWA_CLOAKED,
            &mut cloaked as *mut _ as *mut core::ffi::c_void,
            core::mem::size_of::<u32>() as u32,
        );
        if cloaked != 0 {
            h = next()?;
            continue;
        }
        let mut r = RECT::default();
        if DwmGetWindowAttribute(
            h,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut r as *mut _ as *mut core::ffi::c_void,
            core::mem::size_of::<RECT>() as u32,
        )
        .is_err()
            && GetWindowRect(h, &mut r).is_err()
        {
            h = next()?;
            continue;
        }
        if screen.x < r.left || screen.x >= r.right || screen.y < r.top || screen.y >= r.bottom {
            h = next()?;
            continue;
        }
        // First HIT in z-order decides — either it's a real window (answer) or the desktop
        // shell (no hint at all; everything below it is covered by it anyway).
        let mut cls = [0u16; 16];
        let n = GetClassNameW(h, &mut cls) as usize;
        let name = String::from_utf16_lossy(&cls[..n.min(cls.len())]);
        if name == "Progman" || name == "WorkerW" {
            return None;
        }
        // Back to client space, clamped to the overlay (a window can hang off-screen).
        let c = RECT {
            left: (r.left - vx).clamp(0, vw),
            top: (r.top - vy).clamp(0, vh),
            right: (r.right - vx).clamp(0, vw),
            bottom: (r.bottom - vy).clamp(0, vh),
        };
        return (c.right > c.left && c.bottom > c.top).then_some(c);
    }
}

pub(super) extern "system" fn shot_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        // The Shot state is attached only after CreateWindowExW returns; any message
        // during creation has no state yet — pass it through so the deref'ing arms
        // always see a valid pointer. (WM_DESTROY guards its own null.)
        if shot_ptr(hwnd).is_null() && msg != WM_DESTROY {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        match msg {
            WM_ERASEBKGND => LRESULT(1), // the snapshot covers every pixel
            WM_LBUTTONDOWN => {
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
                    }
                    Some(sel) => {
                        let dpi = shot_dpi_for_sel(s, sel);
                        let buttons = toolbar::layout(sel, s.vw, s.vh, dpi);
                        // The open colour palette intercepts clicks first.
                        if s.color_flyout {
                            if let Some((_, cbr)) =
                                buttons.iter().find(|(b, _)| *b == Button::Color)
                            {
                                let (_, sw) =
                                    toolbar::color_flyout_layout(*cbr, s.vw, s.vh, &s.customs, dpi);
                                if let Some((swatch, _)) = sw.iter().find(|(_, r)| pt_in(*r, p)) {
                                    match *swatch {
                                        Swatch::Color(c) | Swatch::Custom(Some(c)) => {
                                            s.cur_color = c
                                        }
                                        Swatch::Custom(None) | Swatch::Picker => {
                                            pick_custom_color(hwnd, s)
                                        }
                                    }
                                    s.color_flyout = false;
                                    let _ = InvalidateRect(Some(hwnd), None, false);
                                    return LRESULT(0);
                                }
                            }
                            // Clicked off the palette → close it; consume if that click
                            // was the Colour button itself (else fall through).
                            s.color_flyout = false;
                            let _ = InvalidateRect(Some(hwnd), None, false);
                            if toolbar::hit(&buttons, p.x, p.y) == Some(Button::Color) {
                                return LRESULT(0);
                            }
                        }
                        // The open text settings flyout intercepts clicks too.
                        if s.text_flyout {
                            if let Some((_, tbr)) =
                                buttons.iter().find(|(b, _)| *b == Button::Tool(Tool::Text))
                            {
                                let (_, its) = toolbar::text_flyout_layout(
                                    *tbr,
                                    s.vw,
                                    s.vh,
                                    s.font_dropdown,
                                    dpi,
                                );
                                if let Some((item, _)) = its.iter().find(|(_, r)| pt_in(*r, p)) {
                                    match *item {
                                        TextItem::FontField => s.font_dropdown = !s.font_dropdown,
                                        TextItem::FontOption(i) => {
                                            tools::set_face(
                                                &mut s.text_font,
                                                toolbar::PRESET_FONTS[i],
                                            );
                                            s.font_dropdown = false;
                                        }
                                        TextItem::SizeDown => {
                                            let sz = (-s.text_font.lfHeight - 2).max(8);
                                            s.text_font.lfHeight = -sz;
                                        }
                                        TextItem::SizeUp => {
                                            let sz = (-s.text_font.lfHeight + 2).min(120);
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
                                            s.text_font.lfUnderline =
                                                u8::from(s.text_font.lfUnderline == 0);
                                        }
                                        TextItem::More => {
                                            pick_text_font(hwnd, s);
                                            s.text_flyout = false;
                                            s.font_dropdown = false;
                                        }
                                    }
                                    let _ = InvalidateRect(Some(hwnd), None, false);
                                    return LRESULT(0);
                                }
                            }
                            // Clicked off the flyout → close it. Consume if it was the
                            // Text button itself; else fall through (a canvas click
                            // then drops the text caret and starts typing).
                            s.text_flyout = false;
                            s.font_dropdown = false;
                            let _ = InvalidateRect(Some(hwnd), None, false);
                            if toolbar::hit(&buttons, p.x, p.y) == Some(Button::Tool(Tool::Text)) {
                                return LRESULT(0);
                            }
                        }
                        // A click on a toolbar button takes priority over drawing.
                        if let Some(btn) = toolbar::hit(&buttons, p.x, p.y) {
                            if handle_button(hwnd, s, btn) {
                                return LRESULT(0); // window destroyed — stop touching it
                            }
                            let _ = InvalidateRect(Some(hwnd), None, false);
                            return LRESULT(0);
                        }
                        let ctrl = (GetKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000) != 0;
                        if ctrl && s.typing.is_some() && s.tool == Tool::Text {
                            // Ctrl-drag while typing repositions the *active* text box
                            // (you stay in edit mode) — place the caption as you write it.
                            s.typing_drag = true;
                            s.move_from = Some(p);
                        } else if ctrl || s.tool == Tool::Move {
                            // Move tool — or Ctrl-drag with any tool — grabs the
                            // topmost shape under the cursor (if any).
                            s.selected = tools::hit_shape(&s.shapes, p.x, p.y);
                            s.move_from = s.selected.map(|_| p);
                            // A fresh grab starts a fresh undo record — any total left over
                            // from a PREVIOUS drag must not apply to this one.
                            MOVE_UNDO.with(|c| c.set(None));
                        } else if s.tool == Tool::Eyedropper {
                            sample_pixel(s, p); // grab the pixel's colour; never draws
                        } else if s.tool == Tool::Text {
                            // Click while typing = finish & deselect (no new box on this
                            // click); a click when idle starts a fresh box. Predictable
                            // "click away to commit" instead of spawning an empty box you
                            // then have to Esc out of.
                            if s.typing.is_some() {
                                commit_text(s);
                            } else {
                                s.typing = Some((p, String::new()));
                                s.pending_hi = None; // fresh buffer, no half-typed surrogate
                            }
                        } else if s.tool == Tool::Number {
                            let n = s.number_next;
                            s.number_next += 1;
                            let color = s.color();
                            s.shapes.push(Shape::Number { at: p, n, color });
                            s.redo.clear();
                            MOVE_UNDO.with(|c| c.set(None)); // a new shape is the new "last action"
                        } else {
                            s.draw_from = Some(p);
                            s.pen_pts.clear();
                            s.pen_pts.push(p);
                            s.cur = p;
                        }
                    }
                }
                let _ = InvalidateRect(Some(hwnd), None, false);
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let s = &mut *shot_ptr(hwnd);
                let p = pt(lparam);
                // The Eyedropper loupe tracks the cursor: clear the "copied" flash and
                // repaint just the old + new loupe areas (not the whole virtual screen,
                // which would be a heavy blit per tick on a multi-monitor desktop).
                if s.tool == Tool::Eyedropper {
                    let old = loupe_rect(s, s.cur.x, s.cur.y);
                    let new = loupe_rect(s, p.x, p.y);
                    s.eye_copied = false;
                    let _ = InvalidateRect(Some(hwnd), Some(&old), false);
                    let _ = InvalidateRect(Some(hwnd), Some(&new), false);
                }
                s.cur = p;
                if s.sel_dragging {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                } else if s.typing_drag {
                    // Reposition the active text box by the cursor delta (still editing).
                    if let Some(from) = s.move_from {
                        if let Some((at, _)) = s.typing.as_mut() {
                            at.x += p.x - from.x;
                            at.y += p.y - from.y;
                        }
                        s.move_from = Some(p);
                    }
                    let _ = InvalidateRect(Some(hwnd), None, false);
                } else if let (Some(from), Some(idx)) = (s.move_from, s.selected) {
                    // Drag the grabbed shape by the cursor delta.
                    let (dx, dy) = (p.x - from.x, p.y - from.y);
                    if idx < s.shapes.len() {
                        tools::translate_shape(&mut s.shapes[idx], dx, dy);
                        // Fold this tick's delta into the drag's running total, so Ctrl+Z
                        // can undo the WHOLE drag (not just the last tick) by inverting it.
                        MOVE_UNDO.with(|c| c.set(Some(accumulate_move_undo(c.get(), idx, dx, dy))));
                    }
                    s.move_from = Some(p);
                    let _ = InvalidateRect(Some(hwnd), None, false);
                } else if s.draw_from.is_some() {
                    if s.tool == Tool::Pen {
                        s.pen_pts.push(p);
                    }
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                // Before any selection exists, track the WINDOW under the cursor so a bare
                // click can capture it (the hint paints as a live preview). Not while the
                // Eyedropper is armed — there a click means "sample this pixel", and a
                // window highlight would promise something the click won't do.
                if s.sel.is_none() && !s.sel_dragging {
                    let hint = if s.tool == Tool::Eyedropper || s.automation.is_some() {
                        None
                    } else {
                        window_under(hwnd, s.vx, s.vy, s.vw, s.vh, p)
                    };
                    let changed = match (s.win_hint, hint) {
                        (None, None) => false,
                        (Some(a), Some(b)) => {
                            a.left != b.left
                                || a.top != b.top
                                || a.right != b.right
                                || a.bottom != b.bottom
                        }
                        _ => true,
                    };
                    if changed {
                        s.win_hint = hint;
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                }
                // Track which toolbar button we're hovering (only when idle), and
                // (re)arm the hover-delay timer so the tooltip pops after a beat.
                let idle = !s.sel_dragging && s.draw_from.is_none() && s.move_from.is_none();
                let hovered = match (idle, s.sel) {
                    (true, Some(sel)) => toolbar::hit(
                        &toolbar::layout(sel, s.vw, s.vh, shot_dpi_for_sel(s, sel)),
                        p.x,
                        p.y,
                    ),
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
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let s = &mut *shot_ptr(hwnd);
                let p = pt(lparam);
                if s.sel_dragging {
                    s.sel_dragging = false;
                    let r = tools::norm(s.sel_anchor, p);
                    if (r.right - r.left) > 4 && (r.bottom - r.top) > 4 {
                        s.sel = Some(r);
                        // OCR launch mode: the region IS the whole gesture. Recognize and
                        // close instead of raising the annotation toolbar. A too-small drag
                        // falls through with no selection, so the overlay stays up for a
                        // second try rather than closing on a mis-click.
                        if s.ocr_mode {
                            finish_ocr(s);
                            let _ = DestroyWindow(hwnd);
                            return LRESULT(0);
                        }
                    } else if let Some(w) = s.win_hint.take() {
                        // A CLICK (a "drag" under the threshold) with a window highlighted:
                        // the window IS the region. Same commit as a drag ending — including
                        // OCR mode, where clicking a dialog reads the text out of it.
                        s.sel = Some(w);
                        if s.ocr_mode {
                            finish_ocr(s);
                            let _ = DestroyWindow(hwnd);
                            return LRESULT(0);
                        }
                    }
                    s.win_hint = None;
                } else if s.typing_drag {
                    s.typing_drag = false;
                    s.move_from = None; // done repositioning the active text box
                } else if s.move_from.is_some() {
                    s.move_from = None; // finished dragging the selected shape
                } else if let Some(a) = s.draw_from.take() {
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
                let _ = InvalidateRect(Some(hwnd), None, false);
                LRESULT(0)
            }
            WM_CHAR => {
                let s = &mut *shot_ptr(hwnd);
                // WM_CHAR carries one UTF-16 code unit — decode it (not a single ASCII
                // byte) so accented and other Unicode characters type correctly. A
                // non-BMP character arrives as a high+low surrogate pair across two
                // messages; buffer the high half until its low half lands.
                if s.typing.is_some() {
                    let u = (wparam.0 & 0xFFFF) as u16;
                    if let Some(hi) = s.pending_hi.take() {
                        // Expecting the low half of a surrogate pair.
                        if (0xDC00..=0xDFFF).contains(&u) {
                            if let Some(ch) =
                                char::decode_utf16([hi, u]).next().and_then(|r| r.ok())
                            {
                                if let Some((_, buf)) = s.typing.as_mut() {
                                    buf.push(ch);
                                }
                            }
                            let _ = InvalidateRect(Some(hwnd), None, false);
                            return LRESULT(0);
                        }
                        // Stray high surrogate without a matching low half — drop it and
                        // fall through to process `u` on its own.
                    }
                    if (0xD800..=0xDBFF).contains(&u) {
                        s.pending_hi = Some(u); // high surrogate — wait for its low half
                    } else if u == 0x08 {
                        if let Some((_, buf)) = s.typing.as_mut() {
                            buf.pop();
                        }
                    } else if u == 0x0D {
                        // Enter mid-annotation: handle_key's VK_RETURN branch defers to
                        // here instead of committing/closing while typing (see there), so
                        // this is where the literal newline actually lands.
                        if let Some((_, buf)) = s.typing.as_mut() {
                            buf.push('\n');
                        }
                    } else if u >= 0x20 && u != 0x7F {
                        // A BMP character (lone surrogates were handled above), excluding
                        // DEL (0x7F, sent by Ctrl+Backspace on some layouts) — it renders
                        // as a tofu glyph instead of doing anything useful, so drop it
                        // rather than insert it. Lossy path so an unexpected unpaired
                        // surrogate can't panic.
                        if let Some((_, buf)) = s.typing.as_mut() {
                            buf.push_str(&String::from_utf16_lossy(&[u]));
                        }
                    }
                    let _ = InvalidateRect(Some(hwnd), None, false);
                    return LRESULT(0);
                }
                LRESULT(0)
            }
            WM_KEYDOWN => {
                let vk = wparam.0 as u16;
                // Bit 30 means the key was already down. A held F8 must toggle the
                // automation latch once, not on every auto-repeat WM_KEYDOWN.
                let repeated_f8 = vk == VK_F8.0 && (lparam.0 & (1isize << 30)) != 0;
                let shift_preview = if vk == VK_SHIFT.0 {
                    let s = &*shot_ptr(hwnd);
                    s.draw_from.is_some() && matches!(s.tool, Tool::Line | Tool::Arrow)
                } else {
                    false
                };
                if (!repeated_f8 && handle_key(hwnd, vk)) || shift_preview {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                LRESULT(0)
            }
            WM_KEYUP => {
                // Shift can be pressed or released without moving the mouse. Repaint an
                // active line/arrow so its preview toggles immediately in either direction.
                if wparam.0 as u16 == VK_SHIFT.0 {
                    let s = &*shot_ptr(hwnd);
                    if s.draw_from.is_some() && matches!(s.tool, Tool::Line | Tool::Arrow) {
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                }
                LRESULT(0)
            }
            WM_TIMER => {
                let s = &mut *shot_ptr(hwnd);
                if wparam.0 == HOVER_TIMER {
                    let _ = KillTimer(Some(hwnd), HOVER_TIMER);
                    if s.hover_btn.is_some() && !s.tip_show {
                        s.tip_show = true;
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                }
                LRESULT(0)
            }
            WM_SETCURSOR => {
                // Only override the client area; let the default handle the rest.
                if (lparam.0 & 0xffff) as u32 != HTCLIENT {
                    return DefWindowProcW(hwnd, msg, wparam, lparam);
                }
                let s = &*shot_ptr(hwnd);
                let p = s.cur; // last client-space mouse pos (WM_SETCURSOR precedes the move)
                let ctrl = (GetKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000) != 0;
                // Over the toolbar, or over an open flyout panel?
                let over_ui = s.sel.is_some_and(|sel| {
                    let dpi = shot_dpi_for_sel(s, sel);
                    let buttons = toolbar::layout(sel, s.vw, s.vh, dpi);
                    if toolbar::hit(&buttons, p.x, p.y).is_some() {
                        return true;
                    }
                    if s.color_flyout {
                        if let Some((_, cbr)) = buttons.iter().find(|(b, _)| *b == Button::Color) {
                            let (panel, _) =
                                toolbar::color_flyout_layout(*cbr, s.vw, s.vh, &s.customs, dpi);
                            return pt_in(panel, p);
                        }
                    }
                    if s.text_flyout {
                        if let Some((_, tbr)) =
                            buttons.iter().find(|(b, _)| *b == Button::Tool(Tool::Text))
                        {
                            let (panel, _) =
                                toolbar::text_flyout_layout(*tbr, s.vw, s.vh, s.font_dropdown, dpi);
                            return pt_in(panel, p);
                        }
                    }
                    false
                });
                let moving = ctrl || s.tool == Tool::Move;
                let over_shape = moving && tools::hit_shape(&s.shapes, p.x, p.y).is_some();
                let active_typing_move = ctrl && s.tool == Tool::Text && s.typing.is_some();
                // An already-active gesture wins over modifier changes and toolbar
                // hover. When idle, match the pointer to what the next click would do.
                let id = if s.typing_drag || s.move_from.is_some() {
                    IDC_SIZEALL
                } else if s.sel_dragging || s.draw_from.is_some() {
                    IDC_CROSS
                } else if over_ui {
                    IDC_ARROW
                } else if active_typing_move || over_shape {
                    IDC_SIZEALL
                } else if moving {
                    IDC_ARROW
                } else if s.tool == Tool::Text {
                    IDC_IBEAM
                } else {
                    IDC_CROSS
                };
                if let Ok(cur) = LoadCursorW(None, id) {
                    SetCursor(Some(cur));
                }
                LRESULT(1)
            }
            WM_PAINT => {
                shot_paint(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                let ptr = shot_ptr(hwnd);
                if !ptr.is_null() {
                    let s = Box::from_raw(ptr);
                    let _ = DeleteDC(s.shot);
                    let _ = DeleteObject(HGDIOBJ(s.shot_bmp.0));
                    let _ = DeleteDC(s.dimmed);
                    let _ = DeleteObject(HGDIOBJ(s.dimmed_bmp.0));
                }
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// Keyboard: tool shortcuts, colour/thickness, undo/redo, accept (Enter → copy),
/// save (Ctrl+S), cancel/close (Esc). Returns true if a repaint is needed.
pub(super) unsafe fn handle_key(hwnd: HWND, vk: u16) -> bool {
    let s = &mut *shot_ptr(hwnd);
    let ctrl = (GetKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000) != 0;
    let shift = (GetKeyState(VK_SHIFT.0 as i32) as u16 & 0x8000) != 0;
    let alt = (GetKeyState(VK_MENU.0 as i32) as u16 & 0x8000) != 0;

    // Windows automation can send a drag or a key chord, but cannot hold a
    // modifier across a drag. This automation-only latch lets a real mouse-message
    // drag exercise the exact same Shift-snap preview/commit path.
    if vk == VK_F8.0 {
        if let Some(state) = s.automation.as_mut() {
            state.forced_shift = !state.forced_shift;
            state.status = "ready";
            return true;
        }
    }

    // Ignore the close keys for a moment after the overlay opens, so the keystroke
    // that fired the launching hotkey can't instantly cancel/accept the capture.
    if (vk == VK_ESCAPE.0 || vk == VK_RETURN.0)
        && GetTickCount64().saturating_sub(s.born) < SETTLE_CLOSE_MS
    {
        return false;
    }

    if vk == VK_ESCAPE.0 {
        // Peel back transient editor state before closing the whole capture. This
        // makes Esc useful for correcting a mis-click instead of immediately losing
        // the screenshot underneath it.
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
        return false;
    }
    if vk == VK_RETURN.0 {
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
        // Enter = accept to the clipboard (the quick "I'm done, it's copied" gesture).
        // Saving to a file is the explicit Ctrl+S / Save-button action.
        commit_text(s);
        if s.sel.is_some() {
            finish_copy(s);
        }
        let _ = DestroyWindow(hwnd);
        return false;
    }
    // While typing text, swallow every other key here (the characters are inserted
    // by WM_CHAR) so letters go into the text instead of triggering tool shortcuts.
    if s.typing.is_some() {
        return false;
    }

    // Move tool: Delete removes the grabbed shape. Pushed onto `redo` (not cleared) so
    // Ctrl+Y restores it, same as any other undo entry — it used to discard the shape
    // outright, so a delete could never be taken back.
    if vk == VK_DELETE.0 {
        if let Some(idx) = s.selected.take() {
            if idx < s.shapes.len() {
                let sh = s.shapes.remove(idx);
                s.redo.push(sh);
                // Indices shift on a remove; a leftover move-undo could now point at the
                // wrong shape (or the one just deleted).
                MOVE_UNDO.with(|c| c.set(None));
            }
        }
        s.move_from = None;
        return true;
    }

    if ctrl && !shift && vk == b'Z' as u16 {
        undo_step(s, MOVE_UNDO.with(|c| c.take()));
        return true;
    }
    if ctrl && (vk == b'Y' as u16 || (shift && vk == b'Z' as u16)) {
        if let Some(sh) = s.redo.pop() {
            s.shapes.push(sh);
        }
        s.selected = None;
        s.move_from = None;
        // A redo is a new "last action" — an old move-undo record no longer applies.
        MOVE_UNDO.with(|c| c.set(None));
        return true;
    }
    // Ctrl+C = copy to clipboard; Ctrl+S = save. Both accept + close (only once a
    // region exists). Ctrl+S keeps the overlay open if the Save-As prompt is cancelled.
    // Checked before the plain-letter tool shortcuts below, so 'C' alone is still Ellipse.
    if ctrl && vk == b'C' as u16 {
        if block_automation_output(s, "blocked-copy") {
            return true;
        }
        if s.sel.is_some() {
            commit_text(s);
            finish_copy(s);
            let _ = DestroyWindow(hwnd);
        }
        return false;
    }
    // Ctrl+T = read the region's text (OCR) and close. Also checked ahead of the
    // plain-letter shortcuts, so 'T' alone stays the Text tool.
    if ctrl && vk == b'T' as u16 {
        if block_automation_output(s, "blocked-ocr") {
            return true;
        }
        if s.sel.is_some() {
            commit_text(s);
            finish_ocr(s);
            let _ = DestroyWindow(hwnd);
        }
        return false;
    }
    if ctrl && vk == b'S' as u16 {
        if block_automation_output(s, "blocked-save") {
            return true;
        }
        if s.sel.is_some() {
            commit_text(s);
            if finish_save(hwnd, s) {
                let _ = DestroyWindow(hwnd);
            }
        }
        return false;
    }

    // OCR launch mode never reaches the annotation pass, so the tool / colour / thickness
    // shortcuts below have nothing to act on — and the Eyedropper's pick-without-a-region
    // click would hijack the one drag the mode exists for. Swallow them.
    if s.ocr_mode {
        return false;
    }

    // Ctrl/Alt+letter must never fall through to a plain-letter tool shortcut — Ctrl+Z/Y/C/T/S
    // are already intercepted explicitly above, but anything else (Ctrl+A, Alt+R, …) used to
    // reach this match unfiltered and silently switch tools instead of doing nothing.
    let new_tool = if ctrl || alt {
        None
    } else {
        match vk {
            x if x == b'R' as u16 => Some(Tool::Rect),
            x if x == b'O' as u16 || x == b'C' as u16 => Some(Tool::Ellipse),
            x if x == b'A' as u16 => Some(Tool::Arrow),
            x if x == b'L' as u16 => Some(Tool::Line),
            x if x == b'P' as u16 => Some(Tool::Pen),
            x if x == b'T' as u16 => Some(Tool::Text),
            x if x == b'N' as u16 => Some(Tool::Number),
            x if x == b'H' as u16 => Some(Tool::Highlight),
            x if x == b'B' as u16 => Some(Tool::Pixelate), // B = blur/blockify
            x if x == b'I' as u16 => Some(Tool::Invert),
            x if x == b'E' as u16 => Some(Tool::Eyedropper),
            x if x == b'M' as u16 => Some(Tool::Move),
            _ => None,
        }
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
    if vk == 0xDB {
        // VK_OEM_4 '[' — text size while the Text tool is active, else line thickness.
        if s.tool == Tool::Text {
            let sz = (-s.text_font.lfHeight - 2).max(10);
            s.text_font.lfHeight = -sz;
        } else {
            s.thickness = (s.thickness - 1).max(1);
        }
        return true;
    }
    if vk == 0xDD {
        // VK_OEM_6 ']'
        if s.tool == Tool::Text {
            let sz = (-s.text_font.lfHeight + 2).min(96);
            s.text_font.lfHeight = -sz;
        } else {
            s.thickness = (s.thickness + 1).min(40);
        }
        return true;
    }
    false
}

/// Fold one `WM_MOUSEMOVE` tick's `(dx, dy)` into a Move drag's running total. Restarts
/// at zero if `prev` belongs to a DIFFERENT shape index — a defensive fallback (grabs
/// already reset [`MOVE_UNDO`] to `None`) so a stale total can never apply to the wrong
/// shape even if some future call site forgets to clear it. Pure so the accumulation is
/// unit-testable without a real drag.
fn accumulate_move_undo(
    prev: Option<(usize, i32, i32)>,
    idx: usize,
    dx: i32,
    dy: i32,
) -> (usize, i32, i32) {
    let (_, adx, ady) = prev.filter(|(pi, _, _)| *pi == idx).unwrap_or((idx, 0, 0));
    (idx, adx + dx, ady + dy)
}

/// What Ctrl+Z does: revert a just-finished Move if one is pending and had a NONZERO
/// delta (`pending_move`, taken from [`MOVE_UNDO`]) — otherwise fall back to the
/// original behaviour of popping the most recently created shape onto `redo`. A grab
/// that never actually dragged (zero accumulated delta — the user just clicked a shape
/// with the Move tool) deliberately falls through too: treating it as a real undo step
/// would eat the next Ctrl+Z as a no-op instead of undoing the last created shape.
///
/// Pulled out of [`handle_key`] so it is unit-testable without faking the Ctrl chord —
/// `handle_key` reads the REAL keyboard state via `GetKeyState`, which a test parameter
/// can't override.
fn undo_step(s: &mut Shot, pending_move: Option<(usize, i32, i32)>) {
    match pending_move.filter(|(_, dx, dy)| *dx != 0 || *dy != 0) {
        Some((idx, dx, dy)) if idx < s.shapes.len() => {
            tools::translate_shape(&mut s.shapes[idx], -dx, -dy);
        }
        _ => {
            if let Some(sh) = s.shapes.pop() {
                s.redo.push(sh);
            }
        }
    }
    s.selected = None; // indices may have shifted
    s.move_from = None;
}

/// Turn the finished drag (anchor `a` → release `b`) into a [`Shape`]. Returns
/// whether a shape was actually committed (tiny/unsupported gestures return false).
pub(super) fn finish_shape(s: &mut Shot, a: POINT, b: POINT) -> bool {
    let color = s.color();
    let w = s.thickness;
    let shape = match s.tool {
        Tool::Rect => Shape::Rect {
            r: tools::norm(a, b),
            color,
            w,
        },
        Tool::Ellipse => Shape::Ellipse {
            r: tools::norm(a, b),
            color,
            w,
        },
        Tool::Arrow => Shape::Arrow { a, b, color, w },
        Tool::Line => Shape::Line { a, b, color, w },
        Tool::Pen => Shape::Pen {
            pts: std::mem::take(&mut s.pen_pts),
            color,
            w,
        },
        Tool::Highlight => Shape::Highlight {
            r: tools::norm(a, b),
            color,
        },
        Tool::Pixelate => Shape::Pixelate {
            r: tools::norm(a, b),
        },
        Tool::Invert => Shape::Invert {
            r: tools::norm(a, b),
        },
        Tool::Text | Tool::Number | Tool::Eyedropper | Tool::Move => return false,
    };
    // Skip a tiny accidental drag for any rect-based shape.
    if matches!(&shape,
        Shape::Rect { r, .. } | Shape::Ellipse { r, .. } | Shape::Highlight { r, .. }
            | Shape::Pixelate { r } | Shape::Invert { r }
        if (r.right - r.left).abs() < 3 && (r.bottom - r.top).abs() < 3)
    {
        return false;
    }
    s.shapes.push(shape);
    s.redo.clear();
    MOVE_UNDO.with(|c| c.set(None)); // a new shape is the new "last action"
    true
}

/// Commit a non-empty active text buffer into a placed Text shape.
pub(super) fn commit_text(s: &mut Shot) {
    s.pending_hi = None; // any half-typed surrogate is abandoned when the buffer closes
    if let Some((at, buf)) = s.typing.take() {
        if !buf.is_empty() {
            let color = s.color();
            let font = s.text_font;
            s.shapes.push(Shape::Text {
                at,
                s: buf,
                color,
                font,
            });
            s.redo.clear();
            MOVE_UNDO.with(|c| c.set(None)); // a new shape is the new "last action"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;

    /// A minimal, never-shown `Shot` — enough state for the keyboard paths these tests
    /// exercise. `shot`/`dimmed` are throwaway 1x1 compatible bitmaps, not the real
    /// full-screen snapshot (nothing here paints).
    unsafe fn test_shot() -> Box<Shot> {
        let screen = GetDC(None);
        let shot = CreateCompatibleDC(Some(screen));
        let shot_bmp = CreateCompatibleBitmap(screen, 1, 1);
        let dimmed = CreateCompatibleDC(Some(screen));
        let dimmed_bmp = CreateCompatibleBitmap(screen, 1, 1);
        let _ = ReleaseDC(None, screen);
        Box::new(Shot {
            shot,
            shot_bmp,
            dimmed,
            dimmed_bmp,
            vx: 0,
            vy: 0,
            vw: 100,
            vh: 100,
            // None, matching the real overlay's initial state (before any region drag) —
            // these tests only exercise the keyboard/typing paths, none of which read
            // `sel`, and leaving it `None` means `handle_key`'s VK_RETURN path takes the
            // "nothing to copy" branch instead of a real `finish_copy` clipboard write,
            // which would otherwise clobber whatever is on the machine's clipboard.
            sel: None,
            sel_dragging: false,
            sel_anchor: POINT::default(),
            tool: Tool::Text,
            cur_color: {
                let (r, g, b) = PALETTE[0];
                rgb(r, g, b)
            },
            thickness: 3,
            shapes: Vec::new(),
            redo: Vec::new(),
            draw_from: None,
            pen_pts: Vec::new(),
            cur: POINT::default(),
            typing: None,
            typing_drag: false,
            eye_copied: false,
            pending_hi: None,
            number_next: 1,
            selected: None,
            move_from: None,
            text_font: tools::default_text_font(18),
            color_flyout: false,
            customs: Vec::new(),
            cust_colors: [COLORREF(0); 16],
            text_flyout: false,
            font_dropdown: false,
            hover_btn: None,
            tip_show: false,
            born: 0, // 0, not "now" — tests must not trip the just-opened SETTLE_CLOSE_MS guard
            automation: None,
            ocr_mode: false,
            win_hint: None,
        })
    }

    /// A never-shown window carrying a live `test_shot()` in `GWLP_USERDATA`, so a test
    /// can drive `handle_key`/`shot_wndproc` directly instead of reimplementing their
    /// logic. `DestroyWindow` (called by every test before it returns) delivers a real
    /// synchronous `WM_DESTROY` to `shot_wndproc`, which frees the box and the GDI
    /// objects exactly the way the production teardown does — no separate cleanup path
    /// for tests to drift from.
    unsafe fn test_window() -> HWND {
        let class = w!("st2k_test_shot_input");
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(shot_wndproc),
            hInstance: HINSTANCE(GetModuleHandleW(None).unwrap_or_default().0),
            lpszClassName: class,
            ..Default::default()
        };
        let _ = RegisterClassW(&wc); // a 2nd test's registration of the same class is a no-op
        #[allow(clippy::unwrap_used)]
        // test-only: an unusable HWND means abort the run, not skip it
        let hwnd = CreateWindowExW(
            Default::default(),
            class,
            w!(""),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let state = test_shot();
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(state) as isize);
        hwnd
    }

    /// DEL (0x7F, sent by Ctrl+Backspace on some layouts) must never land in the
    /// annotation buffer as a literal character — it used to render as a tofu glyph.
    #[test]
    fn wm_char_rejects_del_without_inserting_a_tofu_glyph() {
        unsafe {
            let hwnd = test_window();
            {
                let s = &mut *shot_ptr(hwnd);
                s.typing = Some((POINT::default(), String::new()));
            }
            shot_wndproc(hwnd, WM_CHAR, WPARAM(0x7F), LPARAM(0));
            let buf = {
                let s = &*shot_ptr(hwnd);
                s.typing.as_ref().map(|(_, b)| b.clone())
            };
            assert_eq!(
                buf,
                Some(String::new()),
                "DEL (0x7F) must not be inserted into the annotation text"
            );
            let _ = DestroyWindow(hwnd);
        }
    }

    /// A normal printable character is unaffected by the DEL exclusion — the guard must
    /// be specific to 0x7F, not an accidental narrowing of the whole `u >= 0x20` range.
    #[test]
    fn wm_char_still_accepts_an_ordinary_character() {
        unsafe {
            let hwnd = test_window();
            {
                let s = &mut *shot_ptr(hwnd);
                s.typing = Some((POINT::default(), String::new()));
            }
            shot_wndproc(hwnd, WM_CHAR, WPARAM(b'A' as usize), LPARAM(0));
            let buf = {
                let s = &*shot_ptr(hwnd);
                s.typing.as_ref().map(|(_, b)| b.clone())
            };
            assert_eq!(buf.as_deref(), Some("A"));
            let _ = DestroyWindow(hwnd);
        }
    }

    /// Enter mid-annotation must insert a newline (via the WM_CHAR that follows a real
    /// keypress), never commit + close the capture — that made multi-line annotations
    /// structurally impossible.
    #[test]
    fn enter_while_typing_inserts_a_newline_instead_of_closing_the_capture() {
        unsafe {
            let hwnd = test_window();
            {
                let s = &mut *shot_ptr(hwnd);
                s.typing = Some((POINT::default(), "line one".to_string()));
            }
            let consumed = handle_key(hwnd, VK_RETURN.0);
            assert!(
                !consumed,
                "VK_RETURN while typing must not report itself as handled by handle_key \
                 (WM_CHAR does the actual insert)"
            );
            {
                let s = &*shot_ptr(hwnd);
                assert!(
                    s.typing.is_some(),
                    "Enter mid-annotation must not commit/close the text"
                );
            }
            // The WM_CHAR(0x0D) a real Enter keypress generates must land the newline.
            shot_wndproc(hwnd, WM_CHAR, WPARAM(0x0D), LPARAM(0));
            let buf = {
                let s = &*shot_ptr(hwnd);
                s.typing.as_ref().map(|(_, b)| b.clone())
            };
            assert_eq!(buf.as_deref(), Some("line one\n"));
            let _ = DestroyWindow(hwnd);
        }
    }

    /// Enter when NOT typing keeps its normal accept-and-close behaviour (the fix must
    /// only change the mid-typing case, not disable Enter generally).
    #[test]
    fn enter_while_not_typing_still_closes_the_capture() {
        unsafe {
            let hwnd = test_window();
            {
                let s = &mut *shot_ptr(hwnd);
                s.typing = None;
            }
            let _ = handle_key(hwnd, VK_RETURN.0);
            assert!(
                !IsWindow(Some(hwnd)).as_bool(),
                "Enter with no active typing must still accept + close the capture"
            );
        }
    }

    /// Deleting a shape must leave it restorable via Ctrl+Y, not discard it outright —
    /// the bug this replaces cleared `redo` instead of pushing the removed shape onto it.
    #[test]
    fn deleting_a_shape_pushes_it_onto_redo_instead_of_discarding_it() {
        unsafe {
            let hwnd = test_window();
            {
                let s = &mut *shot_ptr(hwnd);
                s.shapes.push(Shape::Number {
                    at: POINT { x: 5, y: 5 },
                    n: 7,
                    color: s.cur_color,
                });
                s.selected = Some(0);
            }
            let consumed = handle_key(hwnd, VK_DELETE.0);
            assert!(consumed);
            {
                let s = &*shot_ptr(hwnd);
                assert!(
                    s.shapes.is_empty(),
                    "the shape must be removed from the live list"
                );
                assert_eq!(
                    s.redo.len(),
                    1,
                    "the deleted shape must be preserved for Ctrl+Y, not discarded"
                );
                match &s.redo[0] {
                    Shape::Number { n, .. } => assert_eq!(*n, 7, "wrong shape landed in redo"),
                    _ => panic!("expected the deleted Number shape in redo, got a different kind"),
                }
            }
            let _ = DestroyWindow(hwnd);
        }
    }

    /// `accumulate_move_undo` sums consecutive ticks for the SAME shape, so Ctrl+Z can
    /// invert the whole drag (not just its last tick).
    #[test]
    fn accumulate_move_undo_sums_deltas_for_the_same_shape() {
        let first = accumulate_move_undo(None, 3, 5, -2);
        assert_eq!(first, (3, 5, -2));
        let second = accumulate_move_undo(Some(first), 3, 1, 4);
        assert_eq!(
            second,
            (3, 6, 2),
            "the second tick must add onto the first, not replace it"
        );
    }

    /// A grab that changes WHICH shape is selected must restart the total at zero rather
    /// than inheriting a previous drag's accumulated delta — the defensive fallback in
    /// `accumulate_move_undo` in case some caller forgets to reset `MOVE_UNDO` on grab.
    #[test]
    fn accumulate_move_undo_restarts_when_the_grabbed_shape_changes() {
        let stale = Some((3, 5, -2));
        let fresh = accumulate_move_undo(stale, 7, 1, 1);
        assert_eq!(
            fresh,
            (7, 1, 1),
            "a different shape index must not inherit the old total"
        );
    }

    /// The bug this replaces: Move-dragging mutated a shape's position with no undo
    /// entry recorded at all, so Ctrl+Z after a move either did nothing useful or
    /// deleted an unrelated shape. `undo_step` must invert the recorded drag in place
    /// instead of falling back to popping the shape off entirely.
    #[test]
    fn undo_step_reverts_a_pending_move_instead_of_deleting_the_shape() {
        unsafe {
            let mut s = test_shot();
            s.shapes.push(Shape::Number {
                at: POINT { x: 50, y: 50 },
                n: 1,
                color: s.cur_color,
            });
            undo_step(&mut s, Some((0, 10, -4)));
            assert_eq!(s.shapes.len(), 1, "a move-undo must not remove the shape");
            match &s.shapes[0] {
                Shape::Number { at, .. } => assert_eq!(
                    (at.x, at.y),
                    (40, 54),
                    "the total drag delta must be inverted exactly"
                ),
                _ => panic!("expected the Number shape to remain, got a different kind"),
            }
            assert!(s.redo.is_empty(), "a move-undo is not a deletion");
        }
    }

    /// A grab that never actually dragged (zero accumulated delta — the user merely
    /// clicked a shape with the Move tool) must NOT swallow the next Ctrl+Z as a no-op:
    /// it has to fall through to the normal "undo the last created shape" behaviour.
    #[test]
    fn undo_step_falls_back_to_popping_when_no_real_move_happened() {
        unsafe {
            let mut s = test_shot();
            s.shapes.push(Shape::Number {
                at: POINT { x: 1, y: 1 },
                n: 1,
                color: s.cur_color,
            });
            undo_step(&mut s, Some((0, 0, 0)));
            assert!(
                s.shapes.is_empty(),
                "a zero-delta grab must fall through to popping the shape"
            );
            assert_eq!(s.redo.len(), 1);
        }
    }
}
