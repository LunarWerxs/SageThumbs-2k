//! Everything that turns user input into state changes: the window procedure, the
//! keyboard map, and the two commit paths (a finished drag becomes a shape, a finished
//! text buffer becomes a placed item).
//!
//! Kept apart from `paint` on purpose - this module decides WHAT is true, `paint` only
//! decides how it looks.

use super::*;
mod pick;
use pick::*;
mod mouse;
use mouse::*;
mod typing;
use typing::*;
mod focus;
use focus::*;
mod keys;
pub(super) use focus::{button_index, color_flyout_items, invoke_focus, text_flyout_items};
pub(super) use keys::handle_key;
#[cfg(test)]
use keys::*;
pub(super) use mouse::{apply_swatch, apply_text_item, toolbar_layout_cached};
// Not in overlay.rs's own `KeyboardAndMouse` import list (nothing there needed Alt before
// this file's ctrl/alt gate on the tool-letter shortcuts), so pulled in directly here.
use std::cell::Cell;
// VK_MENU: the ctrl/alt gate on the tool-letter shortcuts. The rest are the keyboard focus
// model's own keys (`on_key_focus`); none of them were read by this window before it existed,
// which is exactly why they were free to take.
use windows::Win32::UI::Input::KeyboardAndMouse::{
    VK_DOWN, VK_LEFT, VK_MENU, VK_RIGHT, VK_SPACE, VK_TAB, VK_UP,
};
// The one UI Automation name this file needs: the WM_GETOBJECT object id that means "a
// client wants your UIA provider". Everything else about that layer lives in `uia`.
use windows::Win32::UI::Accessibility::UiaRootObjectId;

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

pub(super) extern "system" fn shot_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        // Layer 2 (UI Automation) brackets the dispatch instead of reaching into the handlers.
        //
        // The snapshot is what a screen reader has to be TOLD about (where focus is, which
        // panels are open) rather than asked for. Taking it around the whole dispatch means
        // mouse, keyboard and automation all report through one seam instead of every handler
        // that moves focus having to remember to announce it. It costs nothing while no
        // assistive technology is listening: `uia::watch_before` checks that first, and ignores
        // every message that could not change the answer anyway.
        //
        // Deliberately OUTSIDE `uia`'s borrow guard (see `shot_dispatch`): raising an event can
        // make UIA turn straight round and ask this window for the new element's properties on
        // this very thread, and nothing is holding `Shot` at this point, so that question must
        // be answerable rather than refused as re-entrant.
        let before = uia::watch_before(hwnd, msg);
        let r = shot_dispatch(hwnd, msg, wparam, lparam);
        uia::watch_after(hwnd, before);
        r
    }
}

/// The window procedure proper. Split out of [`shot_wndproc`] so the accessibility seam above
/// wraps every route through it, including the early returns.
unsafe fn shot_dispatch(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // UI Automation asks for the provider with WM_GETOBJECT, and it is answered AHEAD of the
    // null-state guard below on purpose: the provider is addressed by HWND alone and every
    // read it performs already copes with the state not being attached, so there is no reason
    // to make an assistive technology re-ask later just because the query arrived early.
    //
    // Also ahead of the borrow guard, because handing UIA the root can make it ask for
    // properties re-entrantly and this arm borrows nothing.
    if msg == WM_GETOBJECT && lparam.0 as i32 == UiaRootObjectId {
        return uia::on_get_object(hwnd, wparam, lparam);
    }
    // The Shot state is attached only after CreateWindowExW returns; any message
    // during creation has no state yet, so pass it through and let the deref'ing arms
    // always see a valid pointer. (WM_DESTROY guards its own null.)
    if shot_ptr(hwnd).is_null() && msg != WM_DESTROY {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    // Everything below this line borrows `Shot`, and this counter is how `uia` knows it.
    // A modal dialog opened from one of these arms (the colour picker, the font picker, the
    // Save dialog) pumps messages while the arm that opened it still holds `&mut Shot`, so a
    // UIA message arriving during that pump sees a depth above one and declines rather than
    // handing out a second `&mut` to the same state.
    let _borrow = uia::DispatchGuard::enter();
    shot_dispatch_msg(hwnd, msg, wparam, lparam)
}

/// The message table proper, reached only once a `Shot` exists and the borrow guard is held.
/// Split into two groups so neither match carries every arm.
unsafe fn shot_dispatch_msg(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_ERASEBKGND => LRESULT(1), // the snapshot covers every pixel
        WM_PAINT => {
            shot_paint(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => on_destroy(hwnd),
        _ => shot_dispatch_input(hwnd, msg, wparam, lparam),
    }
}

/// The mouse, keyboard and UI Automation message arms (plus the default fall-through).
unsafe fn shot_dispatch_input(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_LBUTTONDOWN => on_lbuttondown(hwnd, lparam),
        WM_MOUSEMOVE => on_mousemove(hwnd, lparam),
        WM_LBUTTONUP => on_lbuttonup(hwnd, lparam),
        WM_CHAR => on_char(hwnd, wparam),
        WM_KEYDOWN => on_keydown(hwnd, wparam, lparam),
        WM_KEYUP => on_keyup(hwnd, wparam),
        WM_TIMER => on_timer(hwnd, wparam),
        WM_SETCURSOR => on_setcursor(hwnd, wparam, lparam),
        // The three private messages the UI Automation provider marshals its work through.
        // Nothing outside this process can produce them: they are plain WM_APP ids on a
        // window class only this module registers.
        uia::WM_UIA_JOB => uia::run_job(hwnd, lparam),
        uia::WM_UIA_INVOKE => uia::run_invoke(hwnd, wparam, lparam),
        uia::WM_UIA_FOCUS => uia::run_set_focus(hwnd, wparam, lparam),
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// `WM_KEYDOWN`: tool shortcuts and undo/redo go through [`handle_key`]; this wrapper
/// only handles the F8 auto-repeat guard and the live Shift-snap preview repaint.
unsafe fn on_keydown(hwnd: HWND, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let vk = wparam.0 as u16;
    // Bit 30 means the key was already down. A held F8 must toggle the automation latch
    // once, not on every auto-repeat WM_KEYDOWN.
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

/// `WM_KEYUP`: Shift can be pressed or released without moving the mouse. Repaint an
/// active line/arrow so its preview toggles immediately in either direction.
unsafe fn on_keyup(hwnd: HWND, wparam: WPARAM) -> LRESULT {
    if wparam.0 as u16 == VK_SHIFT.0 {
        let s = &*shot_ptr(hwnd);
        if s.draw_from.is_some() && matches!(s.tool, Tool::Line | Tool::Arrow) {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }
    LRESULT(0)
}

/// `WM_TIMER`: only the hover-tooltip timer is ever armed on this window.
unsafe fn on_timer(hwnd: HWND, wparam: WPARAM) -> LRESULT {
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

/// `WM_SETCURSOR`: pick the cursor shape for whatever gesture/tool is active over the
/// client area; default handling covers non-client hit-tests (resize borders etc).
unsafe fn on_setcursor(hwnd: HWND, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // Only override the client area; let the default handle the rest.
    if (lparam.0 & 0xffff) as u32 != HTCLIENT {
        return DefWindowProcW(hwnd, WM_SETCURSOR, wparam, lparam);
    }
    let s = &mut *shot_ptr(hwnd);
    let ctrl = (GetKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000) != 0;
    let id = cursor_for_state(s, ctrl);
    if let Ok(cur) = LoadCursorW(None, id) {
        SetCursor(Some(cur));
    }
    LRESULT(1)
}

/// Pick the cursor shape for whatever gesture/tool is active over the client area. An
/// already-active gesture wins over modifier changes and toolbar hover; when idle, match
/// the pointer to what the next click would do. `s.cur` is the last client-space mouse
/// position (WM_SETCURSOR precedes the move).
unsafe fn cursor_for_state(s: &mut Shot, ctrl: bool) -> PCWSTR {
    if s.typing_drag || s.move_from.is_some() {
        IDC_SIZEALL
    } else if s.sel_dragging || s.draw_from.is_some() {
        IDC_CROSS
    } else {
        idle_cursor_for_state(s, ctrl)
    }
}

/// Choose the cursor shape when no drag/draw gesture is currently in progress.
unsafe fn idle_cursor_for_state(s: &mut Shot, ctrl: bool) -> PCWSTR {
    let p = s.cur;
    let over_ui = is_over_toolbar_ui(s, p);
    let moving = ctrl || s.tool == Tool::Move;
    let over_shape = moving && tools::hit_shape(&s.shapes, p.x, p.y).is_some();
    let active_typing_move = ctrl && s.tool == Tool::Text && s.typing.is_some();
    if over_ui {
        IDC_ARROW
    } else if active_typing_move || over_shape {
        IDC_SIZEALL
    } else if moving {
        IDC_ARROW
    } else if s.tool == Tool::Text {
        IDC_IBEAM
    } else {
        IDC_CROSS
    }
}

/// Whether client point `p` is over the toolbar itself or an open flyout panel: the
/// cursor stays the default arrow there instead of the tool's normal crosshair/I-beam.
unsafe fn is_over_toolbar_ui(s: &mut Shot, p: POINT) -> bool {
    let Some(sel) = s.sel else { return false };
    let dpi = shot_dpi_for_sel(s, sel);
    let buttons = toolbar_layout_cached(s, sel, dpi);
    if toolbar::hit(&buttons, p.x, p.y).is_some() {
        return true;
    }
    if s.color_flyout {
        if let Some((_, cbr)) = buttons.iter().find(|(b, _)| *b == Button::Color) {
            let (panel, _) = toolbar::color_flyout_layout(*cbr, s.vw, s.vh, &s.customs, dpi);
            return pt_in(panel, p);
        }
    }
    if s.text_flyout {
        if let Some((_, tbr)) = buttons.iter().find(|(b, _)| *b == Button::Tool(Tool::Text)) {
            let (panel, _) = toolbar::text_flyout_layout(*tbr, s.vw, s.vh, s.font_dropdown, dpi);
            return pt_in(panel, p);
        }
    }
    false
}

/// `WM_DESTROY`: retire the automation providers, free the boxed `Shot` and its GDI objects,
/// then quit the message loop.
unsafe fn on_destroy(hwnd: HWND) -> LRESULT {
    // FIRST, before the box below is freed. An assistive technology can still be holding a
    // provider object for this window, and every one of those objects answers by reaching for
    // the `Shot` that is about to stop existing. See `uia::on_destroy` for why this is a
    // correctness requirement and not a tidy-up.
    uia::on_destroy(hwnd);
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
mod tests;
