//! Sizing and placing the window for what it is about to show, and remembering the size the user chose.

use super::*;
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromRect, MONITORINFO, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTONULL,
};

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
        place_initial(hwnd, cw, ch);
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE); // never steals focus (plan §3)
                                                     // Where it lands in the z-order:
                                                     //   * pinned (toolbar pin): genuinely always-on-top.
                                                     //   * open-front (default): in front, see `raise_without_focus`.
                                                     //   * both off: leave it wherever it naturally landed.
        if st.pinned.get() || st.open_front.get() {
            raise_without_focus(hwnd, st.pinned.get());
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

/// Bring the window to the front of the z-order WITHOUT activating it: Explorer stays the
/// foreground window, so its arrow-key selection keeps driving the follow-poll. A plain
/// `HWND_TOP` from this *background* process does NOT reliably beat Explorer's foreground
/// window (it opened BEHIND it), so "bounce" through TOPMOST, which forces us above everything
/// even from the background, then drop straight back to non-topmost unless `pinned`, so the
/// window can still be covered when you click elsewhere.
pub(in super::super) unsafe fn raise_without_focus(hwnd: HWND, pinned: bool) {
    let _ = SetWindowPos(
        hwnd,
        Some(HWND_TOPMOST),
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
    );
    if !pinned {
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
    if let st2k_appkit::explorer_selection::PreviewTarget::Path(path) =
        st2k_appkit::explorer_selection::preview_target()
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
    let sc = |v: i32| st2k_appkit::win::dpi_scale(hwnd, v);
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
    let sc = |v: i32| st2k_appkit::win::dpi_scale(hwnd, v);
    if st.user_sized.get() {
        let mut r = RECT::default();
        if GetClientRect(hwnd, &mut r).is_ok() {
            return Some((r.right - r.left, r.bottom - r.top));
        }
    }
    let (w, h) = st2k_base::settings::preview_window_size()?;
    let work = sizing_work_area(hwnd, st);
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
    let sc = |v: i32| st2k_appkit::win::dpi_scale(hwnd, v);
    let work = sizing_work_area(hwnd, &*state(hwnd));
    let cap_w = (work.right - work.left) * 80 / 100;
    let cap_h = (work.bottom - work.top) * 80 / 100 - chrome;
    let scale = f64::min(cap_w as f64 / dw as f64, cap_h as f64 / dh as f64).min(1.0);
    let w = ((dw as f64 * scale).round() as i32).max(1);
    let h = ((dh as f64 * scale).round() as i32).max(1);
    (w.max(sc(MIN_W)), (h + chrome).max(sc(MIN_H)))
}

/// The outer WINDOW size that gives a `cw`×`ch` client area under this window's styles.
unsafe fn window_size_for_client(hwnd: HWND, cw: i32, ch: i32) -> (i32, i32) {
    let mut rc = RECT {
        left: 0,
        top: 0,
        right: cw,
        bottom: ch,
    };
    let style = WINDOW_STYLE(GetWindowLongPtrW(hwnd, GWL_STYLE) as u32);
    let ex = WINDOW_EX_STYLE(GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32);
    let _ = AdjustWindowRectEx(&mut rc, style, false, ex);
    (rc.right - rc.left, rc.bottom - rc.top)
}

/// Move a window rect so it lies inside `work` (a monitor's work area, which excludes the
/// taskbar), keeping its size; only a rect bigger than the work area is shrunk to it. Pure, so the
/// rule is testable without a window. This is what keeps a large picture's window from opening or
/// growing underneath the taskbar: the rect slides up (or left) instead.
pub(in super::super) fn fit_into_work_area(rect: RECT, work: RECT) -> RECT {
    let w = (rect.right - rect.left).min(work.right - work.left);
    let h = (rect.bottom - rect.top).min(work.bottom - work.top);
    let x = rect.left.clamp(work.left, (work.right - w).max(work.left));
    let y = rect.top.clamp(work.top, (work.bottom - h).max(work.top));
    RECT {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    }
}

/// The work area of the monitor `rc` mostly sits on (the nearest one when it is on none).
unsafe fn work_area_for_rect(rc: &RECT) -> RECT {
    let mon = MonitorFromRect(rc, MONITOR_DEFAULTTONEAREST);
    let mut mi = MONITORINFO {
        cbSize: core::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if GetMonitorInfoW(mon, &mut mi).as_bool() {
        mi.rcWork
    } else {
        st2k_appkit::win::cursor_monitor_metrics().1
    }
}

/// The work area a size is fitted against: the monitor the window is on once it is showing, the
/// monitor of the remembered position before that (the window is about to open there), else the
/// cursor's. `--shot` always uses the cursor's, so a capture never depends on where the developer
/// last left their own viewer.
unsafe fn sizing_work_area(hwnd: HWND, st: &ViewerState) -> RECT {
    if !st.shot {
        if st.shown.get() {
            let mut rc = RECT::default();
            if GetWindowRect(hwnd, &mut rc).is_ok() {
                return work_area_for_rect(&rc);
            }
        } else if let Some(rc) = remembered_rect_on_screen(1, 1) {
            return work_area_for_rect(&rc);
        }
    }
    st2k_appkit::win::cursor_monitor_metrics().1
}

/// A `ww`×`wh` window rect at the remembered position, when there is one and it still touches a
/// connected monitor. A position on a monitor that has since been unplugged is ignored, so the
/// viewer never opens somewhere nobody can see it.
unsafe fn remembered_rect_on_screen(ww: i32, wh: i32) -> Option<RECT> {
    let (x, y) = st2k_base::settings::preview_window_pos()?;
    let rc = RECT {
        left: x,
        top: y,
        right: x.saturating_add(ww.max(1)),
        bottom: y.saturating_add(wh.max(1)),
    };
    (!MonitorFromRect(&rc, MONITOR_DEFAULTTONULL).is_invalid()).then_some(rc)
}

/// First show: open where the user last dragged the viewer to, else centred on the cursor's
/// monitor — either way fitted inside that monitor's work area, so the bottom of a tall picture
/// never lands under the taskbar.
unsafe fn place_initial(hwnd: HWND, cw: i32, ch: i32) {
    let st = &*state(hwnd);
    let (ww, wh) = window_size_for_client(hwnd, cw, ch);
    let remembered = if st.shot {
        None
    } else {
        remembered_rect_on_screen(ww, wh)
    };
    let want = remembered.unwrap_or_else(|| {
        let (_dpi, work) = st2k_appkit::win::cursor_monitor_metrics();
        let x = work.left + (work.right - work.left - ww) / 2;
        let y = work.top + (work.bottom - work.top - wh) / 2;
        RECT {
            left: x,
            top: y,
            right: x + ww,
            bottom: y + wh,
        }
    });
    let fit = fit_into_work_area(want, work_area_for_rect(&want));
    let _ = SetWindowPos(
        hwnd,
        None,
        fit.left,
        fit.top,
        fit.right - fit.left,
        fit.bottom - fit.top,
        SWP_NOZORDER | SWP_NOACTIVATE,
    );
}

/// Resize (and optionally move) the window so its CLIENT area is `cw`×`ch`. `pos` = an exact
/// top-left window position (the headless capture parks the window off-screen this way), or
/// `None` to keep the current position — in which case the grown window is also slid back inside
/// its monitor's work area, since stepping from a small file to a tall one used to extend the
/// window straight down under the taskbar.
pub(in super::super) unsafe fn place(hwnd: HWND, cw: i32, ch: i32, pos: Option<(i32, i32)>) {
    let (ww, wh) = window_size_for_client(hwnd, cw, ch);
    match pos {
        Some((x, y)) => {
            let _ = SetWindowPos(hwnd, None, x, y, ww, wh, SWP_NOZORDER | SWP_NOACTIVATE);
        }
        // Minimized (from its taskbar button) while the follow-selection poll or a video's
        // metadata changed the content: moving the parked window would pull its stub onto the
        // screen, so the fit waits for the restore.
        None if IsIconic(hwnd).as_bool() => (*state(hwnd)).fit_on_restore.set(Some((cw, ch))),
        None => {
            let mut cur = RECT::default();
            let _ = GetWindowRect(hwnd, &mut cur);
            let want = RECT {
                left: cur.left,
                top: cur.top,
                right: cur.left + ww,
                bottom: cur.top + wh,
            };
            let fit = fit_into_work_area(want, work_area_for_rect(&want));
            let _ = SetWindowPos(
                hwnd,
                None,
                fit.left,
                fit.top,
                fit.right - fit.left,
                fit.bottom - fit.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }
}

/// Persist what the user just did to the frame, driven by `WM_EXITSIZEMOVE`: the size when
/// `WM_SIZING` fired (a RESIZE, so the next file — and the next preview — opens at it; a plain
/// MOVE must not pin whatever size the content happened to pick), and the position when the
/// frame was moved or resized at all, so the next preview opens where they put it. The size is
/// stored in logical px (see `settings::preview_window_size`), the position in device px.
pub(in super::super) unsafe fn remember_placement(hwnd: HWND) {
    let st = &*state(hwnd);
    // `replace` consumes both flags either way — the next drag starts clean.
    let sized = st.user_sized.replace(false);
    let moved = st.user_moved.replace(false);
    if !(sized || moved) || st.shot || st.fullscreen.get().is_some() {
        return;
    }
    if sized {
        let mut r = RECT::default();
        if GetClientRect(hwnd, &mut r).is_ok() {
            let size = (
                st2k_appkit::win::dpi_unscale(hwnd, r.right - r.left),
                st2k_appkit::win::dpi_unscale(hwnd, r.bottom - r.top),
            );
            let _ = st2k_base::settings::set_preview_window_size(Some(size));
        }
    }
    let mut w = RECT::default();
    if GetWindowRect(hwnd, &mut w).is_ok() {
        let _ = st2k_base::settings::set_preview_window_pos(Some((w.left, w.top)));
    }
}

/// Forget the remembered size and position and re-fit the window to the file it is showing — the
/// caption double-click. The escape hatch for "I dragged it out once and now everything opens that
/// big, over there". The window stays put for now; the next preview opens centred again.
pub(in super::super) unsafe fn forget_size(hwnd: HWND) {
    let st = &*state(hwnd);
    st.user_sized.set(false);
    st.user_moved.set(false);
    let _ = st2k_base::settings::set_preview_window_size(None);
    let _ = st2k_base::settings::set_preview_window_pos(None);
    if st.shot || st.fullscreen.get().is_some() {
        return;
    }
    let (cw, ch) = client_size(hwnd); // with nothing remembered, the content's own size again
    place(hwnd, cw, ch, None);
    let _ = InvalidateRect(Some(hwnd), None, false);
}
