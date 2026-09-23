//! The delayed capture's countdown chip.

use super::*;

/// Honour the configured capture delay: a small topmost countdown chip near the cursor
/// ticks the remaining seconds, then returns `true` to proceed. Esc aborts (`false`).
///
/// Runs BEFORE the screen freeze — that is the entire point: the delay exists so a
/// hover-only menu or tooltip can be summoned and still be on screen when the snapshot
/// happens. The chip deliberately sits near the cursor (where the user's attention already
/// is) and repaints only on whole-second boundaries.
pub(super) unsafe fn countdown_delay() -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_ESCAPE};
    let secs = st2k_base::settings::screenshot_delay_sec();
    if secs == 0 {
        return true;
    }
    // Swallow a stale Esc state from before the countdown started.
    let _ = GetAsyncKeyState(VK_ESCAPE.0 as i32);
    let deadline = GetTickCount64() + u64::from(secs) * 1000;
    let mut shown: u64 = 0;
    let mut chip: Option<HWND> = None;
    loop {
        let now = GetTickCount64();
        if now >= deadline {
            break;
        }
        // Esc pressed since the last poll → abort the capture entirely.
        if GetAsyncKeyState(VK_ESCAPE.0 as i32) as u16 & 0x8001 != 0 {
            if let Some(h) = chip {
                let _ = DestroyWindow(h);
            }
            return false;
        }
        let remaining = (deadline - now).div_ceil(1000);
        if remaining != shown {
            shown = remaining;
            let mut cur = POINT::default();
            let _ = GetCursorPos(&mut cur);
            chip = Some(countdown_chip(chip, cur, remaining));
        }
        crate::win::pump_msgs(4);
        std::thread::sleep(std::time::Duration::from_millis(40));
    }
    if let Some(h) = chip {
        let _ = DestroyWindow(h);
    }
    // One last pump so the chip's pixels are OFF screen before the freeze — otherwise the
    // countdown photographs itself into the capture.
    crate::win::pump_msgs(4);
    std::thread::sleep(std::time::Duration::from_millis(60));
    true
}

/// Create (or move) the countdown chip showing `n`, offset from the cursor. A plain
/// topmost popup painted in WM_PAINT — no layering, no timer of its own; the countdown
/// loop drives it.
pub(super) unsafe fn countdown_chip(existing: Option<HWND>, cur: POINT, n: u64) -> HWND {
    const W: i32 = 46;
    const H: i32 = 40;
    let class = w!("SageThumbs2KShotCountdown");
    if existing.is_none() {
        let wc = WNDCLASSW {
            lpfnWndProc: Some(countdown_wndproc),
            hInstance: HINSTANCE(unsafe {
                windows::Win32::System::LibraryLoader::GetModuleHandleW(None)
                    .map(|h| h.0)
                    .unwrap_or(core::ptr::null_mut())
            }),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc); // idempotent; a re-register just fails quietly
    }
    let hwnd = match existing {
        Some(h) => h,
        None => CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class,
            w!(""),
            WS_POPUP,
            0,
            0,
            W,
            H,
            None,
            None,
            None,
            None,
        )
        .unwrap_or_default(),
    };
    if hwnd.is_invalid() {
        return hwnd;
    }
    // Stash the digit for WM_PAINT, then place beside the cursor (offset so the chip is
    // visible and never UNDER the point being aimed at).
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, n as isize);
    let _ = SetWindowPos(
        hwnd,
        Some(HWND_TOPMOST),
        cur.x + 24,
        cur.y + 24,
        W,
        H,
        SWP_NOACTIVATE | SWP_SHOWWINDOW,
    );
    let _ = InvalidateRect(Some(hwnd), None, true);
    hwnd
}

pub(super) unsafe extern "system" fn countdown_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            let bg = CreateSolidBrush(rgb(24, 24, 24));
            FillRect(hdc, &rc, bg);
            let _ = DeleteObject(bg.into());
            let n = GetWindowLongPtrW(hwnd, GWLP_USERDATA).max(0);
            SelectObject(hdc, HGDIOBJ(crate::win::gui_font_title(hwnd).0));
            SetBkMode(hdc, windows::Win32::Graphics::Gdi::TRANSPARENT);
            SetTextColor(hdc, rgb(240, 240, 240));
            let mut txt = crate::win::wide(&format!("{n}"));
            let tn = txt.len().saturating_sub(1);
            DrawTextW(
                hdc,
                &mut txt[..tn],
                &mut rc,
                windows::Win32::Graphics::Gdi::DT_CENTER
                    | windows::Win32::Graphics::Gdi::DT_VCENTER
                    | windows::Win32::Graphics::Gdi::DT_SINGLELINE,
            );
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
