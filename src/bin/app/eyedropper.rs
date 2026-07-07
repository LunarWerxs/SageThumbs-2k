//! A SYSTEM-WIDE screen color picker (launched by the DLL's "Pick color" verb as
//! `--eyedropper`). It freezes a snapshot of the whole (virtual) screen in a
//! fullscreen topmost window, follows the cursor with a magnifier loupe, and on a
//! click samples the pixel under the cursor and copies its #RRGGBB to the
//! clipboard. Esc cancels. The selected file is irrelevant — this picks a color
//! from anywhere on screen (by design; the old image-window version
//! was replaced).

use core::ffi::c_void;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::OnceLock;

use windows::core::w;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateSolidBrush, DeleteDC,
    DeleteObject, DrawTextW, EndPaint, FillRect, FrameRect, GetDC, GetPixel, InvalidateRect,
    ReleaseDC, SelectObject, SetBkMode, SetStretchBltMode, SetTextColor, StretchBlt, COLORONCOLOR,
    DT_LEFT, DT_SINGLELINE, DT_VCENTER, HDC, HGDIOBJ, PAINTSTRUCT, SRCCOPY, TRANSPARENT,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{VK_ESCAPE, VK_SPACE};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::dark::rgb;
use crate::win::{app_icon, gui_font, set_clipboard_text, t, wide};

const EYE_K: i32 = 7; // half-window: a (2K+1)² block of screen pixels in the loupe
const EYE_SPAN: i32 = 2 * EYE_K + 1; // 15 px sampled across
const EYE_MAG: i32 = 150; // magnified loupe size (px) → 10× zoom
const EYE_LBL: i32 = 46; // loupe label strip (px): hex row + hint row

/// The frozen screen snapshot: a memory DC (with its bitmap selected) we BitBlt
/// to display, StretchBlt for the loupe, and GetPixel for sampling.
static EYE_SHOT: OnceLock<usize> = OnceLock::new(); // HDC
static EYE_SHOT_BMP: OnceLock<usize> = OnceLock::new(); // HBITMAP (freed on close)
static EYE_VW: AtomicI32 = AtomicI32::new(0); // snapshot / window size
static EYE_VH: AtomicI32 = AtomicI32::new(0);
/// Last cursor client position (drives the loupe; starts off-screen).
static EYE_LAST_X: AtomicI32 = AtomicI32::new(-10000);
static EYE_LAST_Y: AtomicI32 = AtomicI32::new(-10000);

pub(crate) unsafe fn run_eyedropper(hinst: HINSTANCE) {
    // Snapshot the whole virtual screen into a memory DC.
    let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
    let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
    let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
    let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);
    if vw <= 0 || vh <= 0 {
        return;
    }
    let screen = GetDC(None);
    let mem = CreateCompatibleDC(Some(screen));
    let bmp = CreateCompatibleBitmap(screen, vw, vh);
    SelectObject(mem, HGDIOBJ(bmp.0)); // keep selected → mem is a readable copy of the screen
    let _ = BitBlt(mem, 0, 0, vw, vh, Some(screen), vx, vy, SRCCOPY);
    ReleaseDC(None, screen);
    let _ = EYE_SHOT.set(mem.0 as usize);
    let _ = EYE_SHOT_BMP.set(bmp.0 as usize);
    EYE_VW.store(vw, Ordering::Relaxed);
    EYE_VH.store(vh, Ordering::Relaxed);

    let class = w!("SageThumbs2KEyedropper");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(eyedropper_wndproc),
        hInstance: hinst,
        lpszClassName: class,
        hIcon: app_icon().unwrap_or_default(),
        hCursor: LoadCursorW(None, IDC_CROSS).unwrap_or_default(),
        ..Default::default()
    };
    RegisterClassW(&wc);

    // Fullscreen, borderless, topmost — covers the whole virtual screen so the
    // cursor is always over us (no global hook needed to catch clicks).
    if let Ok(hwnd) = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
        class,
        w!("Pick color"),
        WS_POPUP,
        vx,
        vy,
        vw,
        vh,
        None,
        None,
        Some(hinst),
        None,
    ) {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let mut msg = MSG::default();
        loop {
            let r = GetMessageW(&mut msg, None, 0, 0).0;
            if r == 0 || r == -1 {
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Headless capture of the eyedropper overlay (the `--shot --window eyedropper` mode) for
/// README/site assets. Snapshots the PRIMARY monitor (bounded — the demo doesn't need the
/// whole multi-monitor virtual screen), parks the loupe at its centre, builds the overlay
/// OFF-SCREEN (invisible), and renders it to a PNG at `out`.
///
/// NOTE: like the live tool, this captures whatever is CURRENTLY on the primary monitor
/// (frozen), so it's only a clean asset when the desktop is staged — it is NOT part of the
/// automated README/site pipeline. Returns whether the PNG was written.
pub(crate) unsafe fn run_shot_eyedropper(out: &str) -> bool {
    let pw = GetSystemMetrics(SM_CXSCREEN);
    let ph = GetSystemMetrics(SM_CYSCREEN);
    if pw <= 0 || ph <= 0 {
        return false;
    }
    // Snapshot the primary monitor into a memory DC (same as run_eyedropper, but bounded).
    let screen = GetDC(None);
    let mem = CreateCompatibleDC(Some(screen));
    let bmp = CreateCompatibleBitmap(screen, pw, ph);
    SelectObject(mem, HGDIOBJ(bmp.0));
    let _ = BitBlt(mem, 0, 0, pw, ph, Some(screen), 0, 0, SRCCOPY);
    ReleaseDC(None, screen);
    let _ = EYE_SHOT.set(mem.0 as usize);
    let _ = EYE_SHOT_BMP.set(bmp.0 as usize);
    EYE_VW.store(pw, Ordering::Relaxed);
    EYE_VH.store(ph, Ordering::Relaxed);
    // Park the loupe near the centre so it actually draws (WM_PAINT only draws it when a
    // cursor position is set).
    EYE_LAST_X.store(pw / 2, Ordering::Relaxed);
    EYE_LAST_Y.store(ph / 2, Ordering::Relaxed);

    let hinst: HINSTANCE = match windows::Win32::System::LibraryLoader::GetModuleHandleW(None) {
        Ok(h) => h.into(),
        Err(_) => return false,
    };
    let class = w!("SageThumbs2KEyedropper");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(eyedropper_wndproc),
        hInstance: hinst,
        lpszClassName: class,
        hIcon: app_icon().unwrap_or_default(),
        hCursor: LoadCursorW(None, IDC_CROSS).unwrap_or_default(),
        ..Default::default()
    };
    RegisterClassW(&wc);
    // Off the left edge of the virtual desktop (NOT topmost) so it never appears on screen.
    let x = GetSystemMetrics(SM_XVIRTUALSCREEN) - pw - 64;
    let y = GetSystemMetrics(SM_YVIRTUALSCREEN);
    let Ok(hwnd) = CreateWindowExW(
        WS_EX_TOOLWINDOW,
        class,
        w!("Pick color"),
        WS_POPUP,
        x,
        y,
        pw,
        ph,
        None,
        None,
        Some(hinst),
        None,
    ) else {
        return false;
    };
    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    crate::win::pump_msgs(10);
    crate::win::force_repaint(hwnd);
    crate::win::pump_msgs(6);
    let ok = crate::screenshot::capture_hwnd_to_png(hwnd, std::path::Path::new(out));
    let _ = DestroyWindow(hwnd); // WM_DESTROY frees EYE_SHOT / EYE_SHOT_BMP
    ok
}

/// Sample the screen-snapshot pixel at (x, y) as (r, g, b) via GetPixel.
fn eye_sample(x: i32, y: i32) -> (u8, u8, u8) {
    let Some(&dc) = EYE_SHOT.get() else {
        return (0, 0, 0);
    };
    let (vw, vh) = (EYE_VW.load(Ordering::Relaxed), EYE_VH.load(Ordering::Relaxed));
    let x = x.clamp(0, (vw - 1).max(0));
    let y = y.clamp(0, (vh - 1).max(0));
    let c = unsafe { GetPixel(HDC(dc as *mut c_void), x, y) }.0; // 0x00BBGGRR, or CLR_INVALID
    if c == 0xFFFF_FFFF {
        return (0, 0, 0);
    }
    ((c & 0xFF) as u8, ((c >> 8) & 0xFF) as u8, ((c >> 16) & 0xFF) as u8)
}

/// The loupe's box rect for a cursor at (cx, cy), nudged to stay on-screen.
fn eye_loupe_box(cx: i32, cy: i32) -> RECT {
    let (vw, vh) = (EYE_VW.load(Ordering::Relaxed), EYE_VH.load(Ordering::Relaxed));
    let (bw, bh) = (EYE_MAG, EYE_MAG + EYE_LBL);
    let gap = 18;
    let mut bx = cx + gap;
    let mut by = cy + gap;
    if bx + bw > vw {
        bx = cx - gap - bw;
    }
    if by + bh > vh {
        by = cy - gap - bh;
    }
    bx = bx.clamp(0, (vw - bw).max(0));
    by = by.clamp(0, (vh - bh).max(0));
    RECT { left: bx, top: by, right: bx + bw, bottom: by + bh }
}

extern "system" fn eyedropper_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_ERASEBKGND => LRESULT(1), // the snapshot covers every pixel
            WM_MOUSEMOVE => {
                let mx = (lparam.0 & 0xffff) as u16 as i16 as i32;
                let my = ((lparam.0 >> 16) & 0xffff) as u16 as i16 as i32;
                let ox = EYE_LAST_X.swap(mx, Ordering::Relaxed);
                let oy = EYE_LAST_Y.swap(my, Ordering::Relaxed);
                // Repaint the old + new loupe boxes (erase old, draw new).
                let old = eye_loupe_box(ox, oy);
                let new = eye_loupe_box(mx, my);
                let _ = InvalidateRect(Some(hwnd), Some(&old), false);
                let _ = InvalidateRect(Some(hwnd), Some(&new), false);
                LRESULT(0)
            }
            WM_LBUTTONDOWN | WM_RBUTTONDOWN => {
                let mx = (lparam.0 & 0xffff) as u16 as i16 as i32;
                let my = ((lparam.0 >> 16) & 0xffff) as u16 as i16 as i32;
                let (r, g, b) = eye_sample(mx, my);
                set_clipboard_text(&format!("#{r:02X}{g:02X}{b:02X}"));
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            // Space picks the pixel under the cursor (a steadier alternative to a
            // click — your hand doesn't move).
            WM_KEYDOWN if wparam.0 == VK_SPACE.0 as usize => {
                let cx = EYE_LAST_X.load(Ordering::Relaxed);
                let cy = EYE_LAST_Y.load(Ordering::Relaxed);
                if cx > -10000 {
                    let (r, g, b) = eye_sample(cx, cy);
                    set_clipboard_text(&format!("#{r:02X}{g:02X}{b:02X}"));
                }
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_KEYDOWN if wparam.0 == VK_ESCAPE.0 as usize => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_PAINT => {
                eye_paint(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                if let Some(&dc) = EYE_SHOT.get() {
                    let _ = DeleteDC(HDC(dc as *mut c_void));
                }
                if let Some(&bmp) = EYE_SHOT_BMP.get() {
                    let _ = DeleteObject(HGDIOBJ(bmp as *mut c_void));
                }
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

unsafe fn eye_paint(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    if let Some(&shot) = EYE_SHOT.get() {
        let shotdc = HDC(shot as *mut c_void);
        let pr = ps.rcPaint;
        // Restore the snapshot under the invalid region (erasing the old loupe).
        let _ = BitBlt(hdc, pr.left, pr.top, pr.right - pr.left, pr.bottom - pr.top, Some(shotdc), pr.left, pr.top, SRCCOPY);
        // Draw the loupe at the current cursor.
        let cx = EYE_LAST_X.load(Ordering::Relaxed);
        let cy = EYE_LAST_Y.load(Ordering::Relaxed);
        if cx > -10000 {
            eye_draw_loupe(hdc, shotdc, cx, cy);
        }
    }
    let _ = EndPaint(hwnd, &ps);
}

/// Draw the magnifier loupe (zoomed pixels + crosshair + hex label) near the
/// cursor, sampling from the frozen `shotdc`.
unsafe fn eye_draw_loupe(hdc: HDC, shotdc: HDC, cx: i32, cy: i32) {
    let lb = eye_loupe_box(cx, cy);
    let (bx, by) = (lb.left, lb.top);

    // Magnified pixels — nearest-neighbor so each screen pixel is a crisp block.
    SetStretchBltMode(hdc, COLORONCOLOR);
    let _ = StretchBlt(hdc, bx, by, EYE_MAG, EYE_MAG, Some(shotdc), cx - EYE_K, cy - EYE_K, EYE_SPAN, EYE_SPAN, SRCCOPY);

    // Crosshair on the center cell (the pixel that gets picked).
    let cell = EYE_MAG / EYE_SPAN;
    let cc = RECT {
        left: bx + EYE_K * cell,
        top: by + EYE_K * cell,
        right: bx + EYE_K * cell + cell,
        bottom: by + EYE_K * cell + cell,
    };
    let red = CreateSolidBrush(rgb(255, 40, 40));
    FrameRect(hdc, &cc, red);
    let _ = DeleteObject(red.into());

    // Label strip: swatch + hex (top row), then a "Press Space to copy" hint.
    let (r, g, b) = eye_sample(cx, cy);
    let lbl = RECT { left: bx, top: by + EYE_MAG, right: bx + EYE_MAG, bottom: by + EYE_MAG + EYE_LBL };
    let lbg = CreateSolidBrush(rgb(24, 24, 24));
    FillRect(hdc, &lbl, lbg);
    let _ = DeleteObject(lbg.into());
    let sw = RECT { left: bx + 5, top: by + EYE_MAG + 5, right: bx + 21, bottom: by + EYE_MAG + 21 };
    let swb = CreateSolidBrush(rgb(r, g, b));
    FillRect(hdc, &sw, swb);
    let _ = DeleteObject(swb.into());

    SelectObject(hdc, HGDIOBJ(gui_font().0));
    SetBkMode(hdc, TRANSPARENT);
    // Hex (row 1).
    SetTextColor(hdc, rgb(240, 240, 240));
    let mut hex = wide(&format!("#{r:02X}{g:02X}{b:02X}"));
    let hn = hex.len().saturating_sub(1);
    let mut hr = RECT { left: bx + 28, top: by + EYE_MAG + 2, right: bx + EYE_MAG, bottom: by + EYE_MAG + 24 };
    DrawTextW(hdc, &mut hex[..hn], &mut hr, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
    // Hint (row 2).
    SetTextColor(hdc, rgb(150, 150, 150));
    let mut hint = wide(t("eye_hint"));
    let hin = hint.len().saturating_sub(1);
    let mut hir = RECT { left: bx + 6, top: by + EYE_MAG + 24, right: bx + EYE_MAG, bottom: by + EYE_MAG + EYE_LBL };
    DrawTextW(hdc, &mut hint[..hin], &mut hir, DT_LEFT | DT_VCENTER | DT_SINGLELINE);

    // Outer + magnifier borders.
    let border = CreateSolidBrush(rgb(0, 0, 0));
    let outer = RECT { left: bx, top: by, right: bx + EYE_MAG, bottom: by + EYE_MAG + EYE_LBL };
    FrameRect(hdc, &outer, border);
    let mag = RECT { left: bx, top: by, right: bx + EYE_MAG, bottom: by + EYE_MAG };
    FrameRect(hdc, &mag, border);
    let _ = DeleteObject(border.into());
}
