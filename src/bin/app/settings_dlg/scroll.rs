//! The left options live in a fixed viewport; content taller than it scrolls via
//! a vertical scrollbar + the mouse wheel. The controls stay direct children of
//! the dialog (so the GetDlgItem-based load/save stays untouched) — scrolling
//! moves them and an opaque mask hides whatever slides below the viewport.

use super::*;

/// The opaque mask below the left viewport: a flat dark fill (hides scrolled-out
/// controls + their field panels) with a hairline rule across its top separating
/// the scroll content from the banner.
pub(super) unsafe fn draw_left_mask(hwnd: HWND, d: &DRAWITEMSTRUCT) {
    fill(d.hDC, &d.rcItem, DARK_BG());
    // Divider a few px below the fold (so it clears the row straddling the edge).
    let y = d.rcItem.top + s(hwnd, 6);
    let line = RECT { left: s(hwnd, 14), top: y, right: s(hwnd, 706), bottom: y + s(hwnd, 1).max(1) };
    fill(d.hDC, &line, BORDER());
}

#[derive(Default)]
pub(super) struct ScrollData {
    items: Vec<HWND>,
    scrollbar: HWND,
    pub(super) pos: i32, // read by the parent's WM_MOUSEWHEEL handler
    range: i32,          // max scroll offset (device px)
    viewport_h: i32,     // device px
}

thread_local! {
    pub(super) static SCROLL: core::cell::RefCell<ScrollData> = core::cell::RefCell::new(ScrollData::default());
}

/// Subclass id for the wheel-forwarder attached to each left-column child so the
/// mouse wheel scrolls the page even when the cursor is over a control.
const WHEEL_FWD_SUBCLASS: usize = 1140;

struct CollectCtx {
    dialog: HWND,
    exclude: [HWND; 5],
    items: Vec<HWND>,
    max_bottom: i32,
    right_edge: i32,
}

unsafe extern "system" fn collect_scrollables(child: HWND, lparam: LPARAM) -> BOOL {
    let ctx = &mut *(lparam.0 as *mut CollectCtx);
    if ctx.exclude.contains(&child) {
        return BOOL(1);
    }
    let mut r = RECT::default();
    if GetWindowRect(child, &mut r).is_err() {
        return BOOL(1);
    }
    let mut tl = POINT { x: r.left, y: r.top };
    let mut br = POINT { x: r.right, y: r.bottom };
    let _ = ScreenToClient(ctx.dialog, &mut tl);
    let _ = ScreenToClient(ctx.dialog, &mut br);
    if tl.x < ctx.right_edge {
        ctx.items.push(child);
        ctx.max_bottom = ctx.max_bottom.max(br.y);
    }
    BOOL(1)
}

/// The viewport's bottom edge in client px — the TOP of the fold mask, which the resize
/// reflow (`on_resize`) moves down when the window is dragged taller. So the scroll math
/// follows the live window height without hardcoding the design bottom. Falls back to the
/// design bottom if the mask isn't there yet.
pub(super) unsafe fn view_bottom_dev(dialog: HWND) -> i32 {
    if let Ok(mask) = GetDlgItem(Some(dialog), ID_LEFT_MASK) {
        let mut r = RECT::default();
        if GetWindowRect(mask, &mut r).is_ok() {
            let mut tl = POINT { x: r.left, y: r.top };
            let _ = ScreenToClient(dialog, &mut tl);
            return tl.y;
        }
    }
    dpi_scale(dialog, LEFT_VIEW_BOTTOM)
}

/// Collect the left-column controls and size the scrollbar. Call once, after all
/// controls (incl. the scrollbar + mask) are built.
pub(super) unsafe fn init_scroll(dialog: HWND) {
    let view_top = dpi_scale(dialog, LEFT_VIEW_TOP);
    let viewport_h = (view_bottom_dev(dialog) - view_top).max(1);
    let gi = |id| GetDlgItem(Some(dialog), id).unwrap_or_default();
    let sb = gi(ID_SCROLLBAR);
    // Child z-order here is "earlier-created sits higher", so set it explicitly:
    // scrollbar + mask above the scrolling controls (so the mask hides the row
    // straddling the fold), banner above the mask (so it shows over it).
    let zfix = |h: HWND| {
        let _ = SetWindowPos(h, Some(HWND_TOP), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
    };
    zfix(sb);
    zfix(gi(ID_LEFT_MASK));
    zfix(gi(ID_BANNER));
    // The footer (About / credit / Cancel / Save) lives in the bottom chrome zone.
    // With no sponsor banner it rises into the banner's old slot, which the full-
    // width mask also covers — so raise it above the mask (the mask's opaque fill is
    // the dialog bg, so the controls read cleanly on top). Harmless with a sponsor
    // present (the footer is below the mask then anyway).
    for id in [ID_ABOUT, ID_PROMO_LINK, IDCANCEL, IDOK] {
        zfix(gi(id));
    }
    let mut ctx = CollectCtx {
        dialog,
        exclude: [sb, gi(ID_LEFT_MASK), gi(ID_BANNER), gi(ID_ABOUT), gi(ID_PROMO_LINK)],
        items: Vec::new(),
        max_bottom: 0,
        right_edge: dpi_scale(dialog, LEFT_RIGHT_EDGE),
    };
    let _ = EnumChildWindows(Some(dialog), Some(collect_scrollables), LPARAM(&mut ctx as *mut _ as isize));
    // Standard child controls (checkbox/static/edit) swallow WM_MOUSEWHEEL instead of
    // bubbling it to the dialog, so the column wouldn't scroll while the cursor is over
    // a control (i.e. most of it). Subclass each collected child to forward the wheel up.
    for &h in &ctx.items {
        let _ = SetWindowSubclass(h, Some(restyle::wheel_forward_subclass), WHEEL_FWD_SUBCLASS, 0);
    }
    let content_h = (ctx.max_bottom - view_top + dpi_scale(dialog, 10)).max(viewport_h);
    let range = (content_h - viewport_h).max(0);
    let si = SCROLLINFO {
        cbSize: std::mem::size_of::<SCROLLINFO>() as u32,
        fMask: SIF_RANGE | SIF_PAGE | SIF_POS,
        nMin: 0,
        nMax: content_h - 1,
        nPage: viewport_h as u32,
        nPos: 0,
        nTrackPos: 0,
    };
    SetScrollInfo(sb, SB_CTL, &si, true);
    if range == 0 {
        let _ = ShowWindow(sb, SW_HIDE);
    }
    SCROLL.with(|s| {
        let mut s = s.borrow_mut();
        s.items = ctx.items;
        s.scrollbar = sb;
        s.pos = 0;
        s.range = range;
        s.viewport_h = viewport_h;
    });
    update_visibility(dialog);
}

/// Hide controls fully outside the viewport (so they can't paint over the
/// banner/footer); show those at least partly inside. The opaque mask + clip-
/// siblings handle the one row that straddles the bottom edge.
unsafe fn update_visibility(dialog: HWND) {
    let top = dpi_scale(dialog, LEFT_VIEW_TOP);
    let bot = view_bottom_dev(dialog);
    SCROLL.with(|s| {
        for &h in &s.borrow().items {
            let mut r = RECT::default();
            if GetWindowRect(h, &mut r).is_err() {
                continue;
            }
            let mut a = POINT { x: r.left, y: r.top };
            let mut b = POINT { x: r.left, y: r.bottom };
            let _ = ScreenToClient(dialog, &mut a);
            let _ = ScreenToClient(dialog, &mut b);
            let visible = b.y > top && a.y < bot;
            let _ = ShowWindow(h, if visible { SW_SHOW } else { SW_HIDE });
        }
    });
}

/// Scroll the left column to `new_pos` (clamped): move its controls + the
/// scrollbar thumb, then repaint the viewport.
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
    SCROLL.with(|s| {
        let s = s.borrow();
        // Reposition each scrolled child with an individual SetWindowPos. A single
        // batched DeferWindowPos pass was tried here (for one combined repaint), but
        // the batch silently failed to COMMIT — every Defer/EndDeferWindowPos call
        // returned success yet the controls never moved, so the column "scrolled" the
        // thumb only while the options stayed put. Per-control SetWindowPos repositions
        // reliably.
        //
        // SWP_NOREDRAW is the key for smooth scrolling: it moves each control WITHOUT
        // the default bit-copy + per-control repaint. Without it, each move blits the
        // control's pixels to the new spot one-at-a-time and the screen composites
        // between them — on a window with WS_CLIPCHILDREN (so the parent can't erase the
        // vacated strips) that reads as ghosting/tearing text. Moving everything silently
        // here, then doing ONE RedrawWindow over the whole band below, lands all the
        // controls at their new offset in a single coherent frame.
        for &h in &s.items {
            let mut r = RECT::default();
            if GetWindowRect(h, &mut r).is_ok() {
                let mut tl = POINT { x: r.left, y: r.top };
                let _ = ScreenToClient(dialog, &mut tl);
                let _ = SetWindowPos(
                    h, None, tl.x, tl.y - delta, 0, 0,
                    SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOREDRAW,
                );
            }
        }
    });
    let si = SCROLLINFO {
        cbSize: std::mem::size_of::<SCROLLINFO>() as u32,
        fMask: SIF_POS,
        nPos: np,
        ..Default::default()
    };
    SetScrollInfo(sb, SB_CTL, &si, true);
    update_visibility(dialog);
    let rc = RECT {
        left: 0,
        top: 0,
        right: dpi_scale(dialog, LEFT_RIGHT_EDGE + 6),
        bottom: view_bottom_dev(dialog),
    };
    // Repaint the band AND all the (silently-moved) child controls in one pass.
    // RDW_ALLCHILDREN is required because the SWP_NOREDRAW moves above suppressed the
    // controls' own repaint — a plain InvalidateRect on the dialog wouldn't reach them,
    // leaving them blank/stale. The double-buffered WM_PAINT fills the bg + chrome and
    // each child repaints itself at its final position, so the frame is coherent (no
    // bit-copy ghosts, no per-control tearing).
    //
    // RDW_UPDATENOW forces that repaint SYNCHRONOUSLY (WM_PAINT before this returns). It
    // is essential, not optional: a fast wheel floods the queue with WM_MOUSEWHEEL, so a
    // deferred (coalesced) paint never gets serviced until the scroll stops — the controls
    // keep moving with SWP_NOREDRAW but the screen doesn't repaint, leaving big blank
    // bands and a "draggy" lag. Painting each step now keeps the screen locked to the
    // wheel. The band is small + double-buffered, so the synchronous repaint is cheap.
    let _ = RedrawWindow(
        Some(dialog),
        Some(&rc),
        None,
        RDW_INVALIDATE | RDW_ALLCHILDREN | RDW_UPDATENOW,
    );
}

/// Recompute the scroll range for the CURRENT viewport (after the window was resized
/// taller/shorter), preserving the scroll position (clamped). Cheaper than `init_scroll`
/// — no re-enumeration/re-subclassing, and no jump to the top.
pub(super) unsafe fn recompute_scroll(dialog: HWND) {
    let view_top = dpi_scale(dialog, LEFT_VIEW_TOP);
    let new_vp = (view_bottom_dev(dialog) - view_top).max(1);
    // Total content height is invariant across resizes: range + viewport == content_h.
    let (content_h, sb, old_pos) =
        SCROLL.with(|s| { let s = s.borrow(); (s.range + s.viewport_h, s.scrollbar, s.pos) });
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
    // Slide the scrolled controls to the (clamped) position, then repaint the viewport.
    scroll_to(dialog, pos);
    update_visibility(dialog);
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
