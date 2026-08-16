//! The left viewport's scrollbar + mouse-wheel plumbing (thumb position, range,
//! WM_VSCROLL). The v3 nav-rail layout (`navrail::apply_v3_layout`) hides both the
//! scrollbar and the fold mask this module was built to drive unconditionally at
//! dialog init, so nothing here is visible today, but `mod.rs` still routes
//! WM_VSCROLL/WM_MOUSEWHEEL/WM_DRAWITEM through it, so the plumbing stays live and
//! correct rather than silently no-op. The original per-control reposition/hide-show
//! pass (an `items: Vec<HWND>` populated by a since-removed `init_scroll`) was deleted
//! here: it read a field nothing in the tree ever pushed to, so it always iterated
//! zero controls. Removing it changes nothing observable: it never did anything.

use super::*;

/// The opaque mask below the left viewport: a flat dark fill (hides scrolled-out
/// controls + their field panels) with a hairline rule across its top separating
/// the scroll content from the banner.
pub(super) unsafe fn draw_left_mask(hwnd: HWND, d: &DRAWITEMSTRUCT) {
    fill(d.hDC, &d.rcItem, DARK_BG());
    // Divider a few px below the fold (so it clears the row straddling the edge).
    let y = d.rcItem.top + s(hwnd, 6);
    let line = RECT {
        left: s(hwnd, 14),
        top: y,
        right: s(hwnd, 706),
        bottom: y + s(hwnd, 1).max(1),
    };
    fill(d.hDC, &line, BORDER());
}

#[derive(Default)]
pub(super) struct ScrollData {
    scrollbar: HWND,
    pub(super) pos: i32, // read by the parent's WM_MOUSEWHEEL handler
    range: i32,          // max scroll offset (device px)
    viewport_h: i32,     // device px
}

thread_local! {
    pub(super) static SCROLL: core::cell::RefCell<ScrollData> = core::cell::RefCell::new(ScrollData::default());
}

/// The viewport's bottom edge in client px — the TOP of the fold mask, which the resize
/// reflow (`on_resize`) moves down when the window is dragged taller. So the scroll math
/// follows the live window height without hardcoding the design bottom. Falls back to the
/// design bottom if the mask isn't there yet.
pub(super) unsafe fn view_bottom_dev(dialog: HWND) -> i32 {
    if let Ok(mask) = GetDlgItem(Some(dialog), ID_LEFT_MASK) {
        let mut r = RECT::default();
        if GetWindowRect(mask, &mut r).is_ok() {
            let mut tl = POINT {
                x: r.left,
                y: r.top,
            };
            let _ = ScreenToClient(dialog, &mut tl);
            return tl.y;
        }
    }
    dpi_scale(dialog, LEFT_VIEW_BOTTOM)
}

/// Scroll the left column to `new_pos` (clamped): move the scrollbar thumb, then
/// repaint the viewport band. (This used to also reposition a tracked list of child
/// controls; that list was never populated anywhere in the tree (see the module
/// doc), so the reposition pass was deleted as a proven no-op, not a behavior change.)
pub(super) unsafe fn scroll_to(dialog: HWND, new_pos: i32) {
    let (delta, np, sb) = SCROLL.with(|s| {
        let mut s = s.borrow_mut();
        let np = new_pos.clamp(0, s.range);
        let delta = np - s.pos;
        s.pos = np;
        (delta, np, s.scrollbar)
    });
    if delta == 0 {
        return;
    }
    let si = SCROLLINFO {
        cbSize: std::mem::size_of::<SCROLLINFO>() as u32,
        fMask: SIF_POS,
        nPos: np,
        ..Default::default()
    };
    SetScrollInfo(sb, SB_CTL, &si, true);
    let rc = RECT {
        left: 0,
        top: 0,
        right: dpi_scale(dialog, LEFT_RIGHT_EDGE + 6),
        bottom: view_bottom_dev(dialog),
    };
    // Repaint the viewport band (mask + scrollbar chrome). No child controls are moved
    // here (see the module doc), so this is a plain repaint, not the coalesced
    // "move-then-flush" it used to be when a reposition pass ran first.
    let _ = RedrawWindow(
        Some(dialog),
        Some(&rc),
        None,
        RDW_INVALIDATE | RDW_ALLCHILDREN | RDW_UPDATENOW,
    );
}

/// Recompute the scroll range for the CURRENT viewport (after the window was resized
/// taller/shorter), preserving the scroll position (clamped).
pub(super) unsafe fn recompute_scroll(dialog: HWND) {
    let view_top = dpi_scale(dialog, LEFT_VIEW_TOP);
    let new_vp = (view_bottom_dev(dialog) - view_top).max(1);
    // Total content height is invariant across resizes: range + viewport == content_h.
    let (content_h, sb, old_pos) = SCROLL.with(|s| {
        let s = s.borrow();
        (s.range + s.viewport_h, s.scrollbar, s.pos)
    });
    let new_range = (content_h - new_vp).max(0);
    SCROLL.with(|s| {
        let mut s = s.borrow_mut();
        s.viewport_h = new_vp;
        s.range = new_range;
    });
    let pos = old_pos.min(new_range);
    let si = SCROLLINFO {
        cbSize: std::mem::size_of::<SCROLLINFO>() as u32,
        fMask: SIF_RANGE | SIF_PAGE | SIF_POS,
        nMin: 0,
        nMax: content_h - 1,
        nPage: new_vp as u32,
        nPos: pos,
        nTrackPos: 0,
    };
    SetScrollInfo(sb, SB_CTL, &si, true);
    let _ = ShowWindow(sb, if new_range == 0 { SW_HIDE } else { SW_SHOW });
    // Land the thumb at the (clamped) position and repaint the viewport band.
    scroll_to(dialog, pos);
}

/// Handle a WM_VSCROLL from the left scrollbar.
pub(super) unsafe fn on_vscroll(dialog: HWND, wparam: WPARAM, lparam: LPARAM) {
    let sb = SCROLL.with(|s| s.borrow().scrollbar);
    if HWND(lparam.0 as *mut c_void) != sb {
        return;
    }
    let (pos, range, vp) = SCROLL.with(|s| {
        let s = s.borrow();
        (s.pos, s.range, s.viewport_h)
    });
    let line = dpi_scale(dialog, 28);
    let new = match SCROLLBAR_COMMAND((wparam.0 & 0xFFFF) as i32) {
        SB_LINEUP => pos - line,
        SB_LINEDOWN => pos + line,
        SB_PAGEUP => pos - vp,
        SB_PAGEDOWN => pos + vp,
        SB_TOP => 0,
        SB_BOTTOM => range,
        SB_THUMBTRACK | SB_THUMBPOSITION => {
            let mut si = SCROLLINFO {
                cbSize: std::mem::size_of::<SCROLLINFO>() as u32,
                fMask: SIF_TRACKPOS,
                ..Default::default()
            };
            let _ = GetScrollInfo(sb, SB_CTL, &mut si);
            si.nTrackPos
        }
        _ => pos,
    };
    scroll_to(dialog, new);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The per-control reposition pass this module used to run (driven by an `items:
    /// Vec<HWND>` field) was removed as dead code: nothing anywhere in the tree ever
    /// pushed a handle into it, so it always iterated zero controls (see the module doc).
    /// This locks in that `scroll_to` still clamps and records the position correctly
    /// with NO per-control list at all: `HWND::default()` (no real window, same pattern
    /// as `mod.rs`'s `set_shot_status_records_green_state_independent_of_the_label_text`)
    /// exercises the state-write half independent of GDI, so a future edit that quietly
    /// reintroduces a hidden dependency on such a list fails here instead of only
    /// breaking silently in the UI.
    #[test]
    fn scroll_to_clamps_and_records_position_without_any_control_list() {
        unsafe {
            SCROLL.with(|s| {
                let mut s = s.borrow_mut();
                s.range = 500;
                s.pos = 0;
            });

            scroll_to(HWND::default(), 120);
            assert_eq!(SCROLL.with(|s| s.borrow().pos), 120);

            // Clamped to the range ceiling, not the raw (out-of-range) request.
            scroll_to(HWND::default(), 9999);
            assert_eq!(SCROLL.with(|s| s.borrow().pos), 500);

            // Clamped to zero, not negative.
            scroll_to(HWND::default(), -50);
            assert_eq!(SCROLL.with(|s| s.borrow().pos), 0);
        }
    }
}
