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

/// The zoom multiplier (relative to `fit`) that reaches true 100% — one image pixel per
/// screen pixel (display scale 1.0). Floored at 1.0 (never shrink below the current
/// zoom baseline) but deliberately NOT capped: a hard 8x ceiling here understated true
/// 1:1 scale for any image more than 8x the pane's fit scale (a large photo opened in a
/// small/default window), which contradicted the "true 100%" the caller advertises.
pub(in crate::preview) fn true_100_zoom(fit: f64) -> f64 {
    (1.0 / fit).max(1.0)
}

/// The zoom ceiling a wheel notch / keyboard step may reach: `full.max(8.0)`, NOT a bare 8.0.
///
/// A hard 8x ceiling here understated true 1:1 scale for any image more than 8x the pane's fit
/// scale (a large photo opened in a small/default window) — double-click's [`toggle_fit_100`]
/// reached true 100% fine, but the wheel silently stopped 2.5x short of it. Split out so
/// the fix is testable without a window.
fn zoom_ceiling(full: f64) -> f64 {
    full.max(8.0)
}

/// Zoom the image in/out by a wheel notch, keeping image point `pt` (CLIENT coords) fixed.
/// Shared by [`zoom_at_cursor`] (mouse wheel, screen coords converted to client) and
/// [`zoom_step_at_center`] (the Ctrl+=/Ctrl+- keyboard step, which has no cursor position to
/// anchor on and uses the content pane's centre instead).
unsafe fn zoom_step_at(hwnd: HWND, delta: i32, pt: POINT) {
    let st = &*state(hwnd);
    let Some((iw, ih)) = image_dims(st) else {
        return;
    };
    let c = content_rect(hwnd);
    let (cw, ch) = (c.right - c.left, c.bottom - c.top);
    let fit = content::fit_scale(iw, ih, cw, ch);
    let old_zoom = st.zoom.get();
    let raw_zoom = old_zoom * if delta > 0 { 1.2 } else { 1.0 / 1.2 };
    let full = true_100_zoom(fit); // true 100% (display scale 1.0), same as toggle_fit_100
    let fit_snap = snap_zoom_step(old_zoom, raw_zoom, 1.0, ZOOM_SNAP_TOLERANCE);
    let snapped = if (fit_snap - raw_zoom).abs() > f64::EPSILON {
        fit_snap
    } else {
        snap_zoom_step(old_zoom, raw_zoom, full, ZOOM_SNAP_TOLERANCE)
    };
    let new_zoom = snapped.clamp(1.0, zoom_ceiling(full));
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

/// Zoom the image in/out by a wheel notch, keeping the image point under the cursor fixed.
pub(in crate::preview) unsafe fn zoom_at_cursor(hwnd: HWND, delta: i32, lparam: LPARAM) {
    // WM_MOUSEWHEEL's lparam is in SCREEN coords.
    let (sx, sy) = lparam_xy(lparam);
    let mut pt = POINT { x: sx, y: sy };
    let _ = ScreenToClient(hwnd, &mut pt);
    zoom_step_at(hwnd, delta, pt);
}

/// Ctrl+=/Ctrl+- keyboard zoom (G22 — the viewer had wheel/double-click zoom only, no keyboard
/// path at all): the same wheel-notch step [`zoom_at_cursor`] takes, anchored on the content
/// pane's CENTRE rather than a cursor position, since a keypress has none.
pub(in crate::preview) unsafe fn zoom_step_at_center(hwnd: HWND, delta: i32) {
    let c = content_rect(hwnd);
    let pt = POINT {
        x: (c.left + c.right) / 2,
        y: (c.top + c.bottom) / 2,
    };
    zoom_step_at(hwnd, delta, pt);
}

/// Toggle between aspect-fit and 100% (native pixels), recentering.
pub(in crate::preview) unsafe fn toggle_fit_100(hwnd: HWND) {
    let st = &*state(hwnd);
    let Some((iw, ih)) = image_dims(st) else {
        return;
    };
    let c = content_rect(hwnd);
    let fit = content::fit_scale(iw, ih, c.right - c.left, c.bottom - c.top);
    let full = true_100_zoom(fit); // 100% == display scale 1.0
    st.zoom.set(if st.zoom.get() <= 1.01 { full } else { 1.0 });
    st.pan.set((0, 0));
    clamp_pan(hwnd);
    let _ = InvalidateRect(Some(hwnd), Some(&c), false);
}

/// The zoom multiplier (relative to `fit`) that makes the image exactly as wide as the
/// content pane, however tall it then runs — the mode a portrait document (a scanned page,
/// a tall screenshot) needs in a landscape-shaped preview window, where aspect-fit is
/// height-limited and leaves empty margins on both sides instead of using the width that's
/// actually there. Returns 1.0 (no-op) when aspect-fit is already width-limited — fit-width
/// then has nothing left to do — and when `iw` is degenerate (mirrors `fit_scale`'s own
/// fallback rather than dividing by zero).
pub(in crate::preview) fn fit_width_zoom(iw: i32, ih: i32, cw: i32, ch: i32) -> f64 {
    if iw <= 0 {
        return 1.0;
    }
    let fit = content::fit_scale(iw, ih, cw, ch);
    (cw as f64 / iw as f64) / fit
}

/// Toggle between aspect-fit and fit-width, recentering (same shape as [`toggle_fit_100`],
/// the third zoom mode alongside aspect-fit/100%).
pub(in crate::preview) unsafe fn toggle_fit_width(hwnd: HWND) {
    let st = &*state(hwnd);
    let Some((iw, ih)) = image_dims(st) else {
        return;
    };
    let c = content_rect(hwnd);
    let (cw, ch) = (c.right - c.left, c.bottom - c.top);
    let width_zoom = fit_width_zoom(iw, ih, cw, ch);
    st.zoom.set(if st.zoom.get() <= 1.01 {
        width_zoom
    } else {
        1.0
    });
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
    fn true_100_zoom_is_not_capped_at_8x_for_a_very_oversized_image() {
        // A photo 20x the pane's fit scale (a large image opened in a small/default
        // window) used to report "100%" at a mere 8x zoom — well under true 1:1.
        assert_eq!(true_100_zoom(0.05), 20.0);
    }

    #[test]
    fn true_100_zoom_never_drops_below_one() {
        // fit >= 1 means the image already fits at or under native size; 100% must
        // never ask for LESS than the current zoom baseline.
        assert_eq!(true_100_zoom(1.0), 1.0);
        assert_eq!(true_100_zoom(2.0), 1.0);
    }

    #[test]
    fn snap_zoom_step_far_away_is_unaffected() {
        // Neither endpoint is near the anchor: the step passes through untouched.
        assert_eq!(snap_zoom_step(1.05, 1.26, 2.0, ZOOM_SNAP_TOLERANCE), 1.26);
    }

    #[test]
    fn fit_width_zoom_fills_the_pane_width_for_a_tall_portrait_image() {
        // A tall portrait image (1000x2000) in a landscape pane (800x400): aspect-fit is
        // height-limited (fit = 400/2000 = 0.2, using only 200 of the 800 px available
        // width). Fit-width must instead reach cw/iw = 800/1000 = 0.8 exactly, i.e. 4x the
        // aspect-fit baseline.
        assert_eq!(fit_width_zoom(1000, 2000, 800, 400), 4.0);
    }

    #[test]
    fn fit_width_zoom_is_a_no_op_when_aspect_fit_is_already_width_limited() {
        // A wide image (2000x1000) in a squarish pane (800x800): aspect-fit is ALREADY
        // width-limited (fit = 800/2000 = 0.4, using the full width) - fit-width has
        // nothing left to do, so it must equal the aspect-fit baseline (1.0x).
        assert_eq!(fit_width_zoom(2000, 1000, 800, 800), 1.0);
    }

    #[test]
    fn fit_width_zoom_falls_back_to_one_for_degenerate_width() {
        assert_eq!(fit_width_zoom(0, 100, 800, 400), 1.0);
    }

    #[test]
    fn zoom_ceiling_uses_true_100_when_it_exceeds_8x() {
        // A large photo in a small window: true 100% is well past 8x, and the wheel must be
        // able to reach it rather than stopping at the old hard cap.
        assert_eq!(zoom_ceiling(20.0), 20.0);
    }

    #[test]
    fn zoom_ceiling_never_drops_below_8x() {
        // The ordinary case: true 100% is UNDER 8x, so 8x stays the ceiling exactly as before.
        assert_eq!(zoom_ceiling(3.0), 8.0);
    }
}
