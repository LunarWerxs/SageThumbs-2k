//! The vestigial vertical-resize reflow system (v2-era; see A048/A261 — the v3 nav-rail
//! shell is fixed-size, so `on_resize`'s first call captures the design layout and it
//! never actually reflows). Split out of `mod.rs`; `on_getminmaxinfo` (which reads the
//! captured design size) stays in the wndproc hub.

use super::*;

// ---- Vertical resize (v2-era; harmless leftover, see A048) ------------------------
// Originally: the window grew in HEIGHT only (width locked in WM_GETMINMAXINFO); on
// WM_SIZE the bottom-anchored controls slid down / the stretchy ones grew, and the
// left scroll viewport recomputed — so a taller window simply showed more options.
// The v3 nav-rail shell (`navrail::apply_v3_layout`) dropped WS_THICKFRAME (see
// main.rs), so the window is fixed-size now and WM_SIZE only ever fires once, at
// creation — the "first call just captures the design layout" branch below, which
// never falls through to an actual reflow. The left-column scroll subsystem this
// used to drive was removed on 2026-09-22 (the v3 layout had hidden its scrollbar and
// mask since it shipped); REFLOW_CTLS below is trimmed to drop the controls the v3
// layout keeps permanently hidden (A048/A261).

pub(super) struct ReflowCtl {
    id: i32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    stretchy: bool, // true = grow height (top fixed); false = bottom chrome (slide y down)
}
pub(super) struct ResizeState {
    pub(super) win_w: i32,  // locked window width (device px)
    pub(super) win_h0: i32, // minimum window height (device px) = the design size
    client_h0: i32,         // original client height (device px), for the resize delta
    ctrls: Vec<ReflowCtl>,
}
thread_local! {
    pub(super) static RESIZE: core::cell::RefCell<Option<ResizeState>> = const { core::cell::RefCell::new(None) };
}

/// Controls reflowed on resize: the right file-types list GROWs in height; the
/// footer buttons slide down with the bottom. Does NOT list `ID_SCROLLBAR` /
/// `ID_LEFT_MASK` / `ID_BANNER` — `navrail::V3_ALWAYS_HIDDEN` hides those on every
/// page with no page that ever un-hides them, so reflowing them would just move
/// invisible controls (A048/A261; `reflow_ctls_never_targets_a_permanently_hidden_control`
/// below locks this against a future entry re-adding one of them).
const REFLOW_CTLS: &[(i32, bool)] = &[
    (ID_LIST, true),
    (ID_ABOUT, false),
    (ID_PROMO_LINK, false),
    (IDCANCEL, false),
    (IDOK, false),
];

/// Measure the current geometry of every `REFLOW_CTLS` entry (and the window rect) and
/// store it as the design layout in `RESIZE`.
unsafe fn capture_design_layout(hwnd: HWND, client_h: i32) {
    let mut wr = RECT::default();
    let _ = GetWindowRect(hwnd, &mut wr);
    let mut ctrls = Vec::new();
    for &(id, stretchy) in REFLOW_CTLS {
        if let Ok(h) = GetDlgItem(Some(hwnd), id) {
            let mut r = RECT::default();
            if GetWindowRect(h, &mut r).is_ok() {
                let mut tl = POINT {
                    x: r.left,
                    y: r.top,
                };
                let _ = ScreenToClient(hwnd, &mut tl);
                ctrls.push(ReflowCtl {
                    id,
                    x: tl.x,
                    y: tl.y,
                    w: r.right - r.left,
                    h: r.bottom - r.top,
                    stretchy,
                });
            }
        }
    }
    RESIZE.with(|s| {
        *s.borrow_mut() = Some(ResizeState {
            win_w: wr.right - wr.left,
            win_h0: wr.bottom - wr.top,
            client_h0: client_h,
            ctrls,
        });
    });
}

/// Reflow the bottom-anchored controls for the new client height + recompute the left
/// scroll viewport. The first call (during creation) just captures the design layout.
pub(super) unsafe fn on_resize(hwnd: HWND, client_h: i32) {
    let first = RESIZE.with(|s| s.borrow().is_none());
    if first {
        capture_design_layout(hwnd, client_h);
        return; // the first size IS the design layout — nothing to reflow yet
    }
    RESIZE.with(|s| {
        let s = s.borrow();
        let Some(st) = s.as_ref() else { return };
        let delta = client_h - st.client_h0;
        for c in &st.ctrls {
            let Ok(h) = GetDlgItem(Some(hwnd), c.id) else {
                continue;
            };
            if c.stretchy {
                let _ = SetWindowPos(
                    h,
                    None,
                    0,
                    0,
                    c.w,
                    (c.h + delta).max(1),
                    SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
            } else {
                let _ = SetWindowPos(
                    h,
                    None,
                    c.x,
                    c.y + delta,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
        }
    });
    // The moved chrome needs a repaint (the dividers).
    let _ = InvalidateRect(Some(hwnd), None, true);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A048/A261: `REFLOW_CTLS` must never target a control `navrail::V3_ALWAYS_HIDDEN`
    /// keeps permanently hidden — reflowing an invisible control on resize is pure
    /// waste and is exactly what got trimmed here (ID_SCROLLBAR/ID_LEFT_MASK/
    /// ID_BANNER). Fails if a future edit re-adds one of those ids to REFLOW_CTLS
    /// without noticing the v3 layout hides it unconditionally.
    #[test]
    fn reflow_ctls_never_targets_a_permanently_hidden_control() {
        for &(id, _stretchy) in REFLOW_CTLS {
            assert!(
                !V3_ALWAYS_HIDDEN.contains(&id),
                "REFLOW_CTLS reflows id {id}, but navrail::V3_ALWAYS_HIDDEN hides it on \
                 every page with no page that un-hides it"
            );
        }
    }
}
