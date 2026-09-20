//! Tray toasts, with and without an action.

use super::*;

/// One-shot tray balloon from a WINDOWLESS helper process: a throwaway hidden window
/// hosts a temporary notify icon, pops a `NIF_INFO` balloon, pumps briefly so it paints
/// and lingers, then removes the icon and returns. This is the feedback channel for
/// processes with no UI of their own (the instant capture's failure note, the
/// post-update "you're now on <ver>" toast) — a modal MessageBox would be wrong there.
/// Best-effort: any failed step just means no toast, never a hang. The `linger` is how
/// long we keep pumping (the shell auto-dismisses the balloon on its own schedule).
pub(crate) unsafe fn notify_toast(title: &str, body: &str, linger: std::time::Duration) {
    // The action toast with nothing to run: a click on the balloon just ends the linger
    // early, which the shell was about to do on its own anyway.
    notify_toast_action(title, body, linger, || {});
}

/// Like [`notify_toast`], but with ONE clickable action: clicking the balloon runs
/// `on_click` before the icon is torn down; letting it dismiss unclicked for `linger` runs
/// nothing. Used by the post-self-update "refresh thumbnails now" offer, which needs
/// a one-click follow-up action a plain informational balloon can't carry.
///
/// Routes the balloon click through `NIF_MESSAGE` + a custom callback message the way
/// `screenshot::daemon`'s resident tray icon does for its own balloons — this is a one-shot
/// version of that same mechanism for a throwaway helper window, since a raw
/// `extern "system"` wndproc can't capture `on_click` directly, the click is flagged via
/// `GWLP_USERDATA` and the pump loop below polls it.
pub(crate) unsafe fn notify_toast_action(
    title: &str,
    body: &str,
    linger: std::time::Duration,
    on_click: impl FnOnce(),
) {
    use windows::Win32::UI::Shell::{
        Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIIF_INFO, NIM_ADD, NIM_DELETE,
        NIM_MODIFY, NOTIFYICONDATAW,
    };

    /// Balloon-click notification code, delivered via the icon's own callback message
    /// (`NOTIFYICONDATAW::uCallbackMessage`) — not a distinct window message of its own.
    const NIN_BALLOONUSERCLICK: u32 = 0x0405;

    unsafe extern "system" fn action_toast_wndproc(
        h: HWND,
        m: u32,
        w: WPARAM,
        l: LPARAM,
    ) -> LRESULT {
        if m == WM_USER + 1 && (l.0 & 0xffff) as u32 == NIN_BALLOONUSERCLICK {
            // No closures in an `extern "system"` fn — flag the click on the window itself;
            // the pump loop below polls it and owns running `on_click`.
            SetWindowLongPtrW(h, GWLP_USERDATA, 1);
            return LRESULT(0);
        }
        DefWindowProcW(h, m, w, l)
    }

    let hmod = windows::Win32::System::LibraryLoader::GetModuleHandleW(None).unwrap_or_default();
    let hinst = windows::Win32::Foundation::HINSTANCE(hmod.0);
    let class = windows::core::w!("SageThumbs2KActionToast");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(action_toast_wndproc),
        hInstance: hinst,
        lpszClassName: class,
        ..Default::default()
    };
    RegisterClassW(&wc); // ok if already registered (one-shot process)
    let Ok(hwnd) = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        class,
        windows::core::w!("st2k-action-toast"),
        WS_OVERLAPPED, // never shown — it only owns the tray icon
        0,
        0,
        0,
        0,
        None,
        None,
        Some(hinst),
        None,
    ) else {
        return;
    };

    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 0xA2,
        uFlags: NIF_ICON | NIF_MESSAGE,
        uCallbackMessage: WM_USER + 1,
        hIcon: app_icon().unwrap_or_default(),
        ..Default::default()
    };
    let _ = Shell_NotifyIconW(NIM_ADD, &nid);

    nid.uFlags = NIF_INFO;
    nid.dwInfoFlags = NIIF_INFO;
    copy_wide_capped(&mut nid.szInfoTitle, title);
    copy_wide_capped(&mut nid.szInfo, body);
    let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);

    let start = std::time::Instant::now();
    let mut msg = MSG::default();
    let mut clicked = false;
    while start.elapsed() < linger {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        if GetWindowLongPtrW(hwnd, GWLP_USERDATA) != 0 {
            clicked = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    let _ = DestroyWindow(hwnd);
    if clicked {
        on_click();
    }
}

#[cfg(test)]
pub(super) mod toast_text_tests {
    use super::*;

    #[test]
    fn copy_wide_capped_nul_terminates_text_that_fits() {
        let mut dst = [0u16; 8];
        copy_wide_capped(&mut dst, "hi");
        assert_eq!(String::from_utf16_lossy(&dst[..2]), "hi");
        assert_eq!(dst[2], 0, "must be NUL-terminated right after the text");
    }

    /// The bug this replaces: a `zip`-based copy stops at whichever of the source/dest is
    /// shorter, so text at least as long as the field leaves NO NUL anywhere in the buffer -
    /// `Shell_NotifyIconW` then reads past the intended text into whatever struct bytes follow.
    #[test]
    fn copy_wide_capped_truncates_and_still_nul_terminates_oversized_text() {
        let mut dst = [0u16; 4];
        copy_wide_capped(&mut dst, "toolong"); // 7 chars into a 4-wide field
                                               // Exactly 3 characters copied (cap = len - 1, reserving the terminator slot).
        assert_eq!(String::from_utf16_lossy(&dst[..3]), "too");
        assert_eq!(
            dst[3], 0,
            "the last slot must hold the terminator even when the source overflows"
        );
    }

    #[test]
    fn copy_wide_capped_handles_a_zero_length_field_without_panicking() {
        let mut dst: [u16; 0] = [];
        copy_wide_capped(&mut dst, "anything"); // must not index out of bounds
    }
}
