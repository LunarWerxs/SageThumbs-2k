//! Sizing and placing the window for what it is about to show, and remembering the size the user chose.

use super::*;

/// Whether entering the `Loading` state should call the full `ensure_shown` (which can
/// `SetWindowPos` an already-shown window down to the Loading box) rather than just repaint the
/// current window in place (2026-09-05 audit, F10 review). Pure so the rule is testable without
/// a window: a window that is not shown yet (the very first load) wants the full show-and-size
/// call, since nothing else will show it; a window that IS already shown (an arrow-key step
/// through a folder, or a daemon selection switch) must never resize to the Loading box only to
/// snap back once content lands moments later: `on_render`/`apply_resolved`/
/// `dispatch_image_kind` all already resize/repaint appropriately once real content is ready.
pub(in super::super) fn should_size_for_loading(shown: bool) -> bool {
    !shown
}

/// Show the window at the right size (first time) or resize to fit the current content
/// (subsequent switches keep the current position, per QuickLook's keep-anchored rule).
pub(in super::super) unsafe fn ensure_shown(hwnd: HWND) {
    let st = &*state(hwnd);
    if st.shot {
        return;
    }
    // While full-screen (F11), a content switch must NOT resize the window back to fit-size — that
    // would leave a small borderless window at the old full-screen spot with the `fullscreen` flag
    // still set (desynced). Keep the full-screen geometry; the new content just repaints into it.
    if st.fullscreen.get().is_some() {
        let _ = InvalidateRect(Some(hwnd), None, false);
        return;
    }
    let (cw, ch) = client_size(hwnd);
    if st.shown.get() {
        place(hwnd, cw, ch, None); // keep position, just resize
    } else {
        let _ = KillTimer(Some(hwnd), SHOW_TIMER_ID);
        place(hwnd, cw, ch, center_on_cursor_monitor(cw, ch));
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE); // never steals focus (plan §3)
                                                     // Bring the window to the front of the z-order WITHOUT activating it — Explorer stays the
                                                     // foreground window so its arrow-key selection keeps driving the follow-poll.
                                                     //   * pinned (toolbar pin): genuinely always-on-top.
                                                     //   * open-front (default): a plain HWND_TOP from this *background* process does NOT reliably
                                                     //     beat Explorer's foreground window (it opened BEHIND it), so "bounce" through TOPMOST —
                                                     //     which forces us above everything even from the background — then immediately drop back
                                                     //     to non-topmost so the window can still be covered when you click elsewhere.
                                                     //   * both off: leave it wherever it naturally landed.
        if st.pinned.get() || st.open_front.get() {
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
            if !st.pinned.get() {
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_NOTOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
        }
        st.shown.set(true);
        // Follow the Explorer selection (arrows / clicks) — daemon mode only. A manual
        // `--preview <path>` shows that exact file and must not be hijacked by the selection.
        if !st.manual && !st.poll_started.get() {
            st.poll_started.set(true);
            start_poll(hwnd);
        }
    }
    let _ = InvalidateRect(Some(hwnd), None, false);
}

/// The follow-selection poll: a dedicated thread (NEVER a `WM_TIMER` on the UI thread — the
/// `IShellWindows` automation marshals into explorer.exe and can stall) that re-resolves the
/// foreground selection every 500 ms and posts a switch when it changes. Exits when the viewer
/// window is gone. Mirrors QuickLook's `FocusMonitor`.
pub(in super::super) fn start_poll(hwnd: HWND) {
    let hwnd_raw = hwnd.0 as isize;
    std::thread::spawn(move || unsafe {
        let hwnd = HWND(hwnd_raw as *mut core::ffi::c_void);
        let mut last: Option<String> = None;
        while poll_once(hwnd, &mut last) {}
    });
}

/// Run one 500 ms follow-poll tick: sleep, then post a switch when the selection changed; returns false when polling must stop (viewer closed, or window vanished mid-post).
unsafe fn poll_once(hwnd: HWND, last: &mut Option<String>) -> bool {
    std::thread::sleep(std::time::Duration::from_millis(500));
    if !IsWindow(Some(hwnd)).as_bool() {
        return false; // viewer closed — stop polling
    }
    if let crate::explorer_selection::PreviewTarget::Path(path) =
        crate::explorer_selection::preview_target()
    {
        // inits its own COM STA; post only when the selection actually changed
        if last.as_deref() != Some(path.as_str()) {
            *last = Some(path.clone());
            let boxed = Box::into_raw(Box::new(path));
            if PostMessageW(Some(hwnd), WM_APP_SWITCH, WPARAM(0), LPARAM(boxed as isize)).is_err() {
                drop(Box::from_raw(boxed)); // window vanished mid-post — don't leak
                return false;
            }
        }
    }
    true
}

/// Clamp a REMEMBERED client size to something that actually fits: never below the window's
/// minimum, never larger than the monitor's work area (a size dragged out on a 4K screen must not
/// open off the edge of a laptop panel). Pure math, so it is unit-testable without a window.
pub(in super::super) fn clamp_remembered_size(
    (w, h): (i32, i32),
    (min_w, min_h): (i32, i32),
    (work_w, work_h): (i32, i32),
) -> (i32, i32) {
    (
        w.clamp(min_w, work_w.max(min_w)),
        h.clamp(min_h, work_h.max(min_h)),
    )
}

/// Compute the desired CLIENT size (device px) for the current content.
///
/// A size the user dragged out beats the per-content default, for every content kind and every
/// file after it — that IS the "remember it" behaviour (`settings::preview_window_size`); a
/// caption double-click forgets it again. Two exceptions, in order:
///   * a resize drag that is still in progress wins over both, so a follow-selection switch
///     landing mid-drag can't yank the frame out from under the cursor;
///   * `--shot` ignores the remembered size entirely, so a headless capture never depends on
///     whatever size the developer happened to leave their own viewer at.
pub(in super::super) unsafe fn client_size(hwnd: HWND) -> (i32, i32) {
    let st = &*state(hwnd);
    let sc = |v: i32| crate::win::dpi_scale(hwnd, v);
    let cap = sc(CAPTION_H);
    if !st.shot {
        if let Some(size) = user_chosen_size(hwnd, st) {
            return size;
        }
    }
    match st.kind.get() {
        ContentKind::Image => match image_dims(st) {
            Some((rdw, rdh)) => fit_to_work_area(hwnd, rdw, rdh, cap),
            None => (sc(LOADING_W), sc(LOADING_H)),
        },
        ContentKind::InfoCard => (sc(CARD_W), sc(CARD_H) + cap),
        ContentKind::Text | ContentKind::Markdown => (sc(TEXT_W), sc(TEXT_H)),
        ContentKind::Video => match st.video_dims.get() {
            // Real clip dimensions (rotation applied), known once MF has read the metadata. Fit
            // them the same way an image is fitted, then add the chrome. Without this every clip
            // opened into the same 16:9 shell and portrait phone video sat letterboxed inside it.
            Some((vw, vh)) if vw > 0 && vh > 0 => fit_to_work_area(hwnd, vw, vh, cap + sc(SCRUB_H)),
            // Audio, or metadata not in yet: the placeholder shell.
            _ => (sc(VIDEO_W), sc(VIDEO_H) + cap + sc(SCRUB_H)),
        },
        ContentKind::Html => (sc(VIDEO_W), sc(VIDEO_H) + cap), // browser-ish default

        ContentKind::Loading => (sc(LOADING_W), sc(LOADING_H)),
    }
}

/// The size the user chose, when there is one: the live client rect while a resize drag is
/// in progress (or after one), else the remembered `preview_window_size`, clamped to the
/// monitor. None when neither applies and the per-content default should decide.
unsafe fn user_chosen_size(hwnd: HWND, st: &ViewerState) -> Option<(i32, i32)> {
    let sc = |v: i32| crate::win::dpi_scale(hwnd, v);
    if st.user_sized.get() {
        let mut r = RECT::default();
        if GetClientRect(hwnd, &mut r).is_ok() {
            return Some((r.right - r.left, r.bottom - r.top));
        }
    }
    let (w, h) = st2k_base::settings::preview_window_size()?;
    let (_dpi, work) = crate::win::cursor_monitor_metrics();
    Some(clamp_remembered_size(
        (sc(w), sc(h)),
        (sc(MIN_W), sc(MIN_H)),
        (work.right - work.left, work.bottom - work.top),
    ))
}

/// The client size that shows a `dw`×`dh` picture (or clip) at up to 80% of the work area
/// with `chrome` pixels of caption/transport below it, never upscaled past 100% and never
/// under the minimum window size. Images and video clips are fitted the same way.
unsafe fn fit_to_work_area(hwnd: HWND, dw: i32, dh: i32, chrome: i32) -> (i32, i32) {
    let sc = |v: i32| crate::win::dpi_scale(hwnd, v);
    let (_dpi, work) = crate::win::cursor_monitor_metrics();
    let cap_w = (work.right - work.left) * 80 / 100;
    let cap_h = (work.bottom - work.top) * 80 / 100 - chrome;
    let scale = f64::min(cap_w as f64 / dw as f64, cap_h as f64 / dh as f64).min(1.0);
    let w = ((dw as f64 * scale).round() as i32).max(1);
    let h = ((dh as f64 * scale).round() as i32).max(1);
    (w.max(sc(MIN_W)), (h + chrome).max(sc(MIN_H)))
}

/// Resize (and optionally move) the window so its CLIENT area is `cw`×`ch`. `pos` = top-left
/// window position, or `None` to keep the current position.
pub(in super::super) unsafe fn place(hwnd: HWND, cw: i32, ch: i32, pos: Option<(i32, i32)>) {
    let mut rc = RECT {
        left: 0,
        top: 0,
        right: cw,
        bottom: ch,
    };
    let style = WINDOW_STYLE(GetWindowLongPtrW(hwnd, GWL_STYLE) as u32);
    let ex = WINDOW_EX_STYLE(GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32);
    let _ = AdjustWindowRectEx(&mut rc, style, false, ex);
    let (ww, wh) = (rc.right - rc.left, rc.bottom - rc.top);
    match pos {
        Some((x, y)) => {
            let _ = SetWindowPos(hwnd, None, x, y, ww, wh, SWP_NOZORDER | SWP_NOACTIVATE);
        }
        None => {
            let _ = SetWindowPos(
                hwnd,
                None,
                0,
                0,
                ww,
                wh,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }
}

/// Top-left position that centers a `cw`×`ch` client window on the cursor's monitor work area.
pub(in super::super) unsafe fn center_on_cursor_monitor(cw: i32, ch: i32) -> Option<(i32, i32)> {
    let (_dpi, work) = crate::win::cursor_monitor_metrics();
    let x = work.left + (work.right - work.left - cw) / 2;
    let y = work.top + (work.bottom - work.top - ch) / 2;
    Some((x.max(work.left), y.max(work.top)))
}

/// Persist the size the user just dragged the frame to, so the next file — and the next preview —
/// opens at it. Driven by `WM_EXITSIZEMOVE`, and only when `WM_SIZING` actually fired: that pair
/// is what separates a RESIZE from a plain window MOVE, which must not pin whatever size the
/// content happened to pick. Stored in logical px (see `settings::preview_window_size`).
pub(in super::super) unsafe fn remember_size(hwnd: HWND) {
    let st = &*state(hwnd);
    // `replace` consumes the flag either way — a move that follows a resize starts clean.
    if !st.user_sized.replace(false) || st.shot || st.fullscreen.get().is_some() {
        return;
    }
    let mut r = RECT::default();
    if GetClientRect(hwnd, &mut r).is_err() {
        return;
    }
    let size = (
        crate::win::dpi_unscale(hwnd, r.right - r.left),
        crate::win::dpi_unscale(hwnd, r.bottom - r.top),
    );
    let _ = st2k_base::settings::set_preview_window_size(Some(size));
}

/// Forget the remembered size and re-fit the window to the file it is showing — the caption
/// double-click. The escape hatch for "I dragged it out once and now everything opens that big".
pub(in super::super) unsafe fn forget_size(hwnd: HWND) {
    let st = &*state(hwnd);
    st.user_sized.set(false);
    let _ = st2k_base::settings::set_preview_window_size(None);
    if st.shot || st.fullscreen.get().is_some() {
        return;
    }
    let (cw, ch) = client_size(hwnd); // with nothing remembered, the content's own size again
    place(hwnd, cw, ch, None);
    let _ = InvalidateRect(Some(hwnd), None, false);
}
