//! The preview pane's child window: its class lease, window procedure and painting.

use super::*;

/// Our window class's lifetime: registered while at least one UI thread holds it (from before
/// its `CreateWindowExW` until after its message loop has ended), UNREGISTERED when the last
/// one lets go.
///
/// It used to be registered once per process and never unregistered. Windows does not
/// unregister a DLL's classes when the DLL unloads, so after `DllCanUnloadNow` let this DLL
/// go the host still held a class whose WndProc pointed into unmapped memory (2026-09-19
/// audit F15: `GetClassInfoW` still found it, `VirtualQuery` said `MEM_FREE`). Every holder
/// also holds a `ModuleRef` for the same span, so the class is always gone before the DLL
/// can be; and the count, not a `Once`, is what lets a later preview register it again.
pub(super) struct ClassLease {
    pub(super) registered: bool,
    pub(super) holders: usize,
}

pub(super) static CLASS: std::sync::Mutex<ClassLease> = std::sync::Mutex::new(ClassLease {
    registered: false,
    holders: 0,
});

pub(super) fn class_acquire() {
    let mut c = CLASS.lock().unwrap_or_else(|p| p.into_inner());
    if !c.registered {
        unsafe {
            let wc = WNDCLASSW {
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(wndproc),
                hInstance: HINSTANCE(crate::dll_hmodule().0),
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                lpszClassName: CLASS_NAME,
                ..Default::default()
            };
            // ATOM 0 = failure, which for a class that ALREADY exists (an earlier unregister
            // was refused) is the outcome wanted anyway; a genuine failure leaves
            // CreateWindowExW to fail and the pane empty, exactly as before.
            RegisterClassW(&wc);
        }
        c.registered = true;
    }
    c.holders += 1;
}

pub(super) fn class_release() {
    let mut c = CLASS.lock().unwrap_or_else(|p| p.into_inner());
    c.holders = c.holders.saturating_sub(1);
    if c.holders == 0 && c.registered {
        // Refused (ERROR_CLASS_HAS_WINDOWS) while any window of the class is still alive; then
        // it stays registered and the next acquire simply does not re-register.
        let gone = unsafe {
            UnregisterClassW(CLASS_NAME, Some(HINSTANCE(crate::dll_hmodule().0))).is_ok()
        };
        if gone {
            c.registered = false;
        }
    }
}

pub(super) unsafe extern "system" fn wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_PAINT => {
            // Painting touches only GDI on a validated DIB; still guard so a freak
            // panic can't unwind across the system-driven callback.
            let _ = safety::guard_hr(|| {
                paint(hwnd);
                windows::Win32::Foundation::S_OK
            });
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1), // WM_PAINT fills the whole client itself
        WM_PRINTCLIENT => {
            // Render into the caller-supplied DC (PrintWindow / thumbnail capture).
            let hdc = windows::Win32::Graphics::Gdi::HDC(wparam.0 as *mut c_void);
            let mut rc = RECT::default();
            _ = GetClientRect(hwnd, &mut rc);
            // Guarded exactly like the WM_PAINT arm above. This is the SAME `draw` reached
            // by a different system-driven callback (PrintWindow / thumbnail capture), so
            // leaving it bare meant a panic that WM_PAINT would have contained instead
            // unwound across the callback and aborted the host.
            let _ = safety::guard_hr(|| {
                draw(hwnd, hdc, &rc);
                windows::Win32::Foundation::S_OK
            });
            LRESULT(0)
        }
        WM_NCDESTROY => {
            let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut RenderData;
            if !p.is_null() {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                let rd = Box::from_raw(p);
                _ = DeleteObject(rd.hbmp.into());
            }
            // The window is gone — end its dedicated UI thread's message loop. (The thread's
            // ModuleRef then drops, letting the DLL unload.)
            PostQuitMessage(0);
            LRESULT(0)
        }
        // Our own "close" request: the COM thread asks us (the window-owning UI thread) to destroy
        // the window on THIS thread — a same-thread DestroyWindow the loop services instantly.
        WM_PREVIEW_CLOSE => {
            _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        // Fresh decoded image handed over from the COM thread (lparam = Box<(DecodedRgba, bg)>).
        // Build the composited DIB + swap the RenderData HERE (this thread owns the window), then
        // invalidate — the loop pumps WM_PAINT next, so it actually paints (no cross-thread race).
        WM_PREVIEW_RENDER => {
            // Drop what we are showing FIRST, unconditionally. A NULL lparam means the new
            // selection produced no image, and the pane must then go EMPTY: keeping the previous
            // file's pixels up is exactly what "the preview stopped refreshing" looks like when
            // the host reuses one handler across selections (issue #11).
            let old = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut RenderData;
            if !old.is_null() {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                let rd = Box::from_raw(old);
                _ = DeleteObject(rd.hbmp.into());
            }
            let p = lparam.0 as *mut (DecodedRgba, u32);
            if !p.is_null() {
                let (dec, bg) = *Box::from_raw(p);
                // `opaque: None`: nothing upstream has scanned the alpha channel, so the
                // shared compositor works it out itself (the same scan the private copy did).
                let hbmp =
                    safety::composite_rgba_over_bg(dec.w as i32, dec.h as i32, &dec.rgba, bg, None);
                if let Some(hbmp) = hbmp {
                    let rd = Box::new(RenderData {
                        hbmp,
                        iw: dec.w as i32,
                        ih: dec.h as i32,
                        bg,
                    });
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(rd) as isize);
                }
            }
            _ = InvalidateRect(Some(hwnd), None, true);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

pub(super) unsafe fn paint(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    if hdc.is_invalid() {
        return;
    }
    let mut rc = RECT::default();
    _ = GetClientRect(hwnd, &mut rc);
    draw(hwnd, hdc, &rc);
    _ = EndPaint(hwnd, &ps);
}

/// Paint the (background-filled, aspect-fit) image into `hdc` for the client `rc`.
/// Shared by `WM_PAINT` and `WM_PRINTCLIENT`.
pub(super) unsafe fn draw(hwnd: HWND, hdc: windows::Win32::Graphics::Gdi::HDC, rc: &RECT) {
    let rd = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RenderData;
    // No image yet / decode failed: fill with the themed default rather than hardcoded white.
    let bg = if rd.is_null() {
        theme_default_bg()
    } else {
        (*rd).bg
    };

    // Fill the whole client with the host background colour first.
    let brush = CreateSolidBrush(COLORREF(bg));
    FillRect(hdc, rc, brush);
    _ = DeleteObject(brush.into());

    if !rd.is_null() {
        let rd = &*rd;
        let cw = rc.right - rc.left;
        let ch = rc.bottom - rc.top;
        if cw > 0 && ch > 0 && rd.iw > 0 && rd.ih > 0 {
            // Aspect-preserving fit (scales up or down — preview panes show small
            // images large, unlike the never-upscale thumbnail path).
            let scale = f64::min(cw as f64 / rd.iw as f64, ch as f64 / rd.ih as f64);
            let dw = ((rd.iw as f64 * scale).round() as i32).max(1);
            let dh = ((rd.ih as f64 * scale).round() as i32).max(1);
            let dx = (cw - dw) / 2;
            let dy = (ch - dh) / 2;
            let memdc = CreateCompatibleDC(Some(hdc));
            let old = SelectObject(memdc, rd.hbmp.into());
            SetStretchBltMode(hdc, HALFTONE);
            _ = StretchBlt(
                hdc,
                dx,
                dy,
                dw,
                dh,
                Some(memdc),
                0,
                0,
                rd.iw,
                rd.ih,
                SRCCOPY,
            );
            SelectObject(memdc, old);
            _ = DeleteDC(memdc);
        }
    }
}
