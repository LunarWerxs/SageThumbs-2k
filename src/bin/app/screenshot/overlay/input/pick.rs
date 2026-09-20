//! Which top-level window sits under the cursor: the candidate filter, its visual bounds and the clamp to the overlay.

use super::*;

/// Is `h` even a candidate: not our own overlay, visible, not minimized (a minimized
/// window's rect is a parked -32000 fiction), and not DWM-cloaked (a UWP app suspended on
/// another virtual desktop LOOKS visible to `IsWindowVisible` but draws nothing — a hint
/// that selected it would capture whatever is behind it)?
pub(super) unsafe fn window_is_candidate(h: HWND, overlay: HWND) -> bool {
    use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
    use windows::Win32::UI::WindowsAndMessaging::{IsIconic, IsWindowVisible};
    if h == overlay || !IsWindowVisible(h).as_bool() || IsIconic(h).as_bool() {
        return false;
    }
    let mut cloaked: u32 = 0;
    let _ = DwmGetWindowAttribute(
        h,
        DWMWA_CLOAKED,
        &mut cloaked as *mut _ as *mut core::ffi::c_void,
        core::mem::size_of::<u32>() as u32,
    );
    cloaked == 0
}

/// `h`'s visual bounds: `DWMWA_EXTENDED_FRAME_BOUNDS` when DWM answers it, else
/// `GetWindowRect`. The DWM value is preferred because `GetWindowRect` includes the
/// invisible resize borders Windows 10+ draws the drop shadow in, which reads as "the
/// capture grabbed a margin of the window behind it".
pub(super) unsafe fn window_visual_bounds(h: HWND) -> Option<RECT> {
    use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
    let mut r = RECT::default();
    if DwmGetWindowAttribute(
        h,
        DWMWA_EXTENDED_FRAME_BOUNDS,
        &mut r as *mut _ as *mut core::ffi::c_void,
        core::mem::size_of::<RECT>() as u32,
    )
    .is_ok()
    {
        return Some(r);
    }
    GetWindowRect(h, &mut r).ok().map(|_| r)
}

/// Back to client space, clamped to the overlay (a window can hang off-screen). `None` when
/// the clamp collapses the rect to nothing.
pub(super) fn clamp_to_overlay(r: RECT, vx: i32, vy: i32, vw: i32, vh: i32) -> Option<RECT> {
    let c = RECT {
        left: (r.left - vx).clamp(0, vw),
        top: (r.top - vy).clamp(0, vh),
        right: (r.right - vx).clamp(0, vw),
        bottom: (r.bottom - vy).clamp(0, vh),
    };
    (c.right > c.left && c.bottom > c.top).then_some(c)
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
/// Skips, in the order they bite: our own overlay/invisible/minimized/cloaked windows (see
/// [`window_is_candidate`]), and the desktop shell pair (Progman/WorkerW — "the desktop" is
/// not a window pick, drag instead).
pub(super) unsafe fn window_under(
    overlay: HWND,
    vx: i32,
    vy: i32,
    vw: i32,
    vh: i32,
    p: POINT,
) -> Option<RECT> {
    use windows::Win32::UI::WindowsAndMessaging::{GetTopWindow, GetWindow, GW_HWNDNEXT};
    let screen = POINT {
        x: p.x + vx,
        y: p.y + vy,
    };
    let mut h = GetTopWindow(None).ok()?;
    loop {
        let next = || GetWindow(h, GW_HWNDNEXT).ok();
        if let Some(r) = candidate_rect_at(h, overlay, screen) {
            // First HIT in z-order decides — either it's a real window (answer) or the desktop
            // shell (no hint at all; everything below it is covered by it anyway).
            if is_desktop_shell(h) {
                return None;
            }
            return clamp_to_overlay(r, vx, vy, vw, vh);
        }
        h = next()?;
    }
}

/// `h`'s visual bounds when `h` is a pick candidate whose rect contains `screen`; `None` means "skip `h` and keep looking down the z-order".
unsafe fn candidate_rect_at(h: HWND, overlay: HWND, screen: POINT) -> Option<RECT> {
    if !window_is_candidate(h, overlay) {
        return None;
    }
    let r = window_visual_bounds(h)?;
    point_in_rect(screen, r).then_some(r)
}

/// Whether `screen` (virtual-screen coordinates) falls inside `r`.
pub(super) fn point_in_rect(screen: POINT, r: RECT) -> bool {
    screen.x >= r.left && screen.x < r.right && screen.y >= r.top && screen.y < r.bottom
}

/// Whether `h` is the desktop shell (Progman/WorkerW): "the desktop" is not a window pick.
pub(super) unsafe fn is_desktop_shell(h: HWND) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::GetClassNameW;
    let mut cls = [0u16; 16];
    let n = GetClassNameW(h, &mut cls) as usize;
    let name = String::from_utf16_lossy(&cls[..n.min(cls.len())]);
    name == "Progman" || name == "WorkerW"
}
