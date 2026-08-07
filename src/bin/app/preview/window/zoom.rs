//! Zoom, pan, fit, text scrolling and the fullscreen toggle.
//!
//! Split out of `window.rs` 2026-07-31 (pure move).

use super::*;

/// Wheel-zoom snap tolerance, as a fraction of the anchor value: a step landing within this
/// band of aspect-fit or true 100% snaps exactly onto it instead of stepping past by a
/// fraction (QuickLook-style magnetism at those two levels).
const ZOOM_SNAP_TOLERANCE: f64 = 0.03;

/// If the step from `old` to `new` crosses `anchor` (the anchor, widened by `tolerance`, falls
/// inside the `[old, new]` interval), return `anchor` exactly. Does nothing when `old` already
/// sits on the anchor, so a user parked there keeps moving past it on the next notch instead of
/// sticking.
pub(in crate::preview) fn snap_zoom_step(old: f64, new: f64, anchor: f64, tolerance: f64) -> f64 {
    let band = anchor * tolerance;
    if (old - anchor).abs() <= band {
        return new;
    }
    let (lo, hi) = if old < new { (old, new) } else { (new, old) };
    if lo <= anchor + band && hi >= anchor - band {
        anchor
    } else {
        new
    }
}

/// Video-only: the render child's rect = content area minus the bottom scrub strip.
/// Zoom the image in/out by a wheel notch, keeping the image point under the cursor fixed.
pub(in crate::preview) unsafe fn zoom_at_cursor(hwnd: HWND, delta: i32, lparam: LPARAM) {
    let st = &*state(hwnd);
    let Some((iw, ih)) = image_dims(st) else {
        return;
    };
    let c = content_rect(hwnd);
    let (cw, ch) = (c.right - c.left, c.bottom - c.top);
    // WM_MOUSEWHEEL's lparam is in SCREEN coords.
    let (sx, sy) = lparam_xy(lparam);
    let mut pt = POINT { x: sx, y: sy };
    let _ = ScreenToClient(hwnd, &mut pt);
    let fit = content::fit_scale(iw, ih, cw, ch);
    let old_zoom = st.zoom.get();
    let raw_zoom = old_zoom * if delta > 0 { 1.2 } else { 1.0 / 1.2 };
    let full = (1.0 / fit).clamp(1.0, 8.0); // true 100% (display scale 1.0), same as toggle_fit_100
    let fit_snap = snap_zoom_step(old_zoom, raw_zoom, 1.0, ZOOM_SNAP_TOLERANCE);
    let snapped = if (fit_snap - raw_zoom).abs() > f64::EPSILON {
        fit_snap
    } else {
        snap_zoom_step(old_zoom, raw_zoom, full, ZOOM_SNAP_TOLERANCE)
    };
    let new_zoom = snapped.clamp(1.0, 8.0);
    if (new_zoom - old_zoom).abs() < 1e-6 {
        return;
    }
    let (px, py) = st.pan.get();
    let old_scale = fit * old_zoom;
    let old_dx = c.left as f64 + (cw as f64 - iw as f64 * old_scale) / 2.0 + px as f64;
    let old_dy = c.top as f64 + (ch as f64 - ih as f64 * old_scale) / 2.0 + py as f64;
    let img_x = (pt.x as f64 - old_dx) / old_scale;
    let img_y = (pt.y as f64 - old_dy) / old_scale;
    let new_scale = fit * new_zoom;
    let new_px = (pt.x as f64
        - img_x * new_scale
        - c.left as f64
        - (cw as f64 - iw as f64 * new_scale) / 2.0)
        .round() as i32;
    let new_py = (pt.y as f64
        - img_y * new_scale
        - c.top as f64
        - (ch as f64 - ih as f64 * new_scale) / 2.0)
        .round() as i32;
    st.zoom.set(new_zoom);
    st.pan.set((new_px, new_py));
    clamp_pan(hwnd);
    let _ = InvalidateRect(Some(hwnd), Some(&c), false);
}

/// Toggle between aspect-fit and 100% (native pixels), recentering.
pub(in crate::preview) unsafe fn toggle_fit_100(hwnd: HWND) {
    let st = &*state(hwnd);
    let Some((iw, ih)) = image_dims(st) else {
        return;
    };
    let c = content_rect(hwnd);
    let fit = content::fit_scale(iw, ih, c.right - c.left, c.bottom - c.top);
    let full = (1.0 / fit).clamp(1.0, 8.0); // 100% == display scale 1.0
    st.zoom.set(if st.zoom.get() <= 1.01 { full } else { 1.0 });
    st.pan.set((0, 0));
    clamp_pan(hwnd);
    let _ = InvalidateRect(Some(hwnd), Some(&c), false);
}

/// Keep the (zoomed) image covering the content — clamp pan so no empty margin shows.
pub(in crate::preview) unsafe fn clamp_pan(hwnd: HWND) {
    let st = &*state(hwnd);
    let Some((iw, ih)) = image_dims(st) else {
        return;
    };
    let c = content_rect(hwnd);
    let (cw, ch) = (c.right - c.left, c.bottom - c.top);
    let scale = content::fit_scale(iw, ih, cw, ch) * st.zoom.get();
    let dw = (iw as f64 * scale) as i32;
    let dh = (ih as f64 * scale) as i32;
    let (maxx, maxy) = (((dw - cw) / 2).max(0), ((dh - ch) / 2).max(0));
    let (px, py) = st.pan.get();
    st.pan.set((px.clamp(-maxx, maxx), py.clamp(-maxy, maxy)));
}

pub(in crate::preview) fn wheel_notches(remainder: i32, delta: i32) -> (i32, i32) {
    let total = remainder.saturating_add(delta);
    (total / 120, total % 120)
}

/// Accumulate precision-wheel deltas, scrolling ~3 lines whenever they reach a full notch.
pub(in crate::preview) unsafe fn scroll_text(hwnd: HWND, delta: i32) {
    let st = &*state(hwnd);
    let (notches, remainder) = wheel_notches(st.wheel_remainder.get(), delta);
    st.wheel_remainder.set(remainder);
    if notches == 0 {
        return;
    }
    let step = crate::win::dpi_scale(hwnd, 40);
    let _ = scroll_text_by(hwnd, notches.saturating_mul(-step));
}

/// One outline-sidebar slide frame: move the animated width a third of the remaining distance
/// (min step so it always lands), settle + kill the timer at the target.
pub(in crate::preview) unsafe fn tick_toc_anim(hwnd: HWND) {
    let st = &*state(hwnd);
    let w_full = crate::win::dpi_scale(hwnd, 220);
    let target = if st.toc_open.get() { w_full } else { 0 };
    let cur = st.toc_anim.get().unwrap_or(target);
    let d = target - cur;
    let step = (d.abs() / 3).max(crate::win::dpi_scale(hwnd, 16));
    if d.abs() <= step {
        st.toc_anim.set(None); // settled
        let _ = KillTimer(Some(hwnd), TOC_TIMER_ID);
    } else {
        st.toc_anim.set(Some(cur + step * d.signum()));
    }
    let _ = InvalidateRect(Some(hwnd), None, false);
}

/// Toggle borderless full-screen (F11): fill the current monitor and hide the resize border;
/// Esc or F11 again restores the exact windowed geometry saved on entry.
pub(in crate::preview) unsafe fn toggle_fullscreen(hwnd: HWND) {
    let st = &*state(hwnd);
    if let Some(prev) = st.fullscreen.get() {
        let style = (GetWindowLongPtrW(hwnd, GWL_STYLE) as u32) | WS_THICKFRAME.0;
        SetWindowLongPtrW(hwnd, GWL_STYLE, style as isize);
        let _ = SetWindowPos(
            hwnd,
            None,
            prev.left,
            prev.top,
            prev.right - prev.left,
            prev.bottom - prev.top,
            SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );
        st.fullscreen.set(None);
    } else {
        let mut wr = RECT::default();
        if GetWindowRect(hwnd, &mut wr).is_err() {
            return;
        }
        let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: core::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(mon, &mut mi).as_bool() {
            return;
        }
        st.fullscreen.set(Some(wr));
        let style = (GetWindowLongPtrW(hwnd, GWL_STYLE) as u32) & !WS_THICKFRAME.0;
        SetWindowLongPtrW(hwnd, GWL_STYLE, style as isize);
        let r = mi.rcMonitor;
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOP),
            r.left,
            r.top,
            r.right - r.left,
            r.bottom - r.top,
            SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );
    }
    let _ = InvalidateRect(Some(hwnd), None, false);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snap_zoom_step_crossing_up_lands_on_anchor() {
        // Step from below the anchor to past it: expect an exact landing, not the overshoot.
        assert_eq!(snap_zoom_step(1.8, 2.16, 2.0, ZOOM_SNAP_TOLERANCE), 2.0);
    }

    #[test]
    fn snap_zoom_step_crossing_down_lands_on_anchor() {
        // Step from above the anchor to below it (zooming out): same magnetism in reverse.
        assert_eq!(snap_zoom_step(2.3, 1.9, 2.0, ZOOM_SNAP_TOLERANCE), 2.0);
    }

    #[test]
    fn snap_zoom_step_already_at_anchor_moves_past() {
        // Already sitting on the anchor: this notch must NOT stick, or scrolling would freeze.
        assert_eq!(snap_zoom_step(2.0, 2.4, 2.0, ZOOM_SNAP_TOLERANCE), 2.4);
    }

    #[test]
    fn snap_zoom_step_far_away_is_unaffected() {
        // Neither endpoint is near the anchor: the step passes through untouched.
        assert_eq!(snap_zoom_step(1.05, 1.26, 2.0, ZOOM_SNAP_TOLERANCE), 1.26);
    }
}
