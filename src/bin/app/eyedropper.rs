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

use std::sync::Mutex;
use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject,
    DrawTextW, EndPaint, FillRect, FrameRect, GetDC, GetPixel, GetStockObject, InvalidateRect,
    ReleaseDC, SelectObject, SetBkMode, SetDCBrushColor, SetStretchBltMode, SetTextColor,
    StretchBlt, COLORONCOLOR, DC_BRUSH, DT_LEFT, DT_SINGLELINE, DT_VCENTER, HBRUSH, HDC, HGDIOBJ,
    PAINTSTRUCT, SRCCOPY, TRANSPARENT,
};

use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL, VK_ESCAPE, VK_SPACE};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::dark::rgb;
use crate::win::{app_icon, gui_font, set_clipboard_text, t, wide};

const EYE_K: i32 = 7; // half-window: a (2K+1)² block of screen pixels in the loupe
const EYE_SPAN: i32 = 2 * EYE_K + 1; // 15 px sampled across
const EYE_MAG: i32 = 150; // magnified loupe size (px) → 10× zoom
const EYE_LBL: i32 = 64; // loupe label strip (px): hex row + stash row + hint row
/// Stash swatch size and how many fit on the row before the count takes over.
const EYE_SW: i32 = 12;
const EYE_SW_MAX: i32 = 11;

/// The frozen screen snapshot: a memory DC (with its bitmap selected) we BitBlt
/// to display, StretchBlt for the loupe, and GetPixel for sampling.
static EYE_SHOT: OnceLock<usize> = OnceLock::new(); // HDC
static EYE_SHOT_BMP: OnceLock<usize> = OnceLock::new(); // HBITMAP (freed on close)
static EYE_VW: AtomicI32 = AtomicI32::new(0); // snapshot / window size
static EYE_VH: AtomicI32 = AtomicI32::new(0);
/// Last cursor client position (drives the loupe; starts off-screen).
static EYE_LAST_X: AtomicI32 = AtomicI32::new(-10000);
static EYE_LAST_Y: AtomicI32 = AtomicI32::new(-10000);
/// Colours collected with Ctrl+click / Ctrl+Space without closing the overlay.
///
/// Picking one colour at a time means re-opening the picker for every swatch in a
/// palette; this keeps the overlay up so a run of colours can be grabbed in one
/// go, then copied as a list. Cleared on open, so one session never inherits the
/// last one's leftovers.
static EYE_STASH: Mutex<Vec<(u8, u8, u8)>> = Mutex::new(Vec::new());

fn hex_of((r, g, b): (u8, u8, u8)) -> String {
    format!("#{r:02X}{g:02X}{b:02X}")
}

/// Is a modifier held? Ctrl means "add to the list and keep picking".
unsafe fn ctrl_held() -> bool {
    GetKeyState(VK_CONTROL.0 as i32) < 0
}

/// Put the final pick on the clipboard, prepended by anything stashed.
///
/// One colour copies as bare hex, exactly as before. Several copy newline-
/// separated, which is what every editor, stylesheet and spreadsheet expects to
/// receive as a list.
unsafe fn eye_finish(pick: Option<(u8, u8, u8)>) {
    let mut all = EYE_STASH.lock().map(|s| s.clone()).unwrap_or_default();
    if let Some(c) = pick {
        all.push(c);
    }
    if all.is_empty() {
        return;
    }
    let text: Vec<String> = all.into_iter().map(hex_of).collect();
    set_clipboard_text(&text.join("\r\n"));
}

pub(crate) unsafe fn run_eyedropper(hinst: HINSTANCE) {
    if let Ok(mut st) = EYE_STASH.lock() {
        st.clear();
    }
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
        // Same trap as the capture overlay: without this the picker takes mouse
        // clicks but no keys, so Space and Esc silently do nothing.
        crate::win::force_foreground(hwnd);
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
    let (vw, vh) = (
        EYE_VW.load(Ordering::Relaxed),
        EYE_VH.load(Ordering::Relaxed),
    );
    let x = x.clamp(0, (vw - 1).max(0));
    let y = y.clamp(0, (vh - 1).max(0));
    let c = unsafe { GetPixel(HDC(dc as *mut c_void), x, y) }.0; // 0x00BBGGRR, or CLR_INVALID
    if c == 0xFFFF_FFFF {
        return (0, 0, 0);
    }
    (
        (c & 0xFF) as u8,
        ((c >> 8) & 0xFF) as u8,
        ((c >> 16) & 0xFF) as u8,
    )
}

/// The loupe's box rect for a cursor at (cx, cy), nudged to stay on-screen.
fn eye_loupe_box(cx: i32, cy: i32) -> RECT {
    let (vw, vh) = (
        EYE_VW.load(Ordering::Relaxed),
        EYE_VH.load(Ordering::Relaxed),
    );
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
    RECT {
        left: bx,
        top: by,
        right: bx + bw,
        bottom: by + bh,
    }
}

extern "system" fn eyedropper_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
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
                let c = eye_sample(mx, my);
                if ctrl_held() {
                    if let Ok(mut st) = EYE_STASH.lock() {
                        st.push(c);
                    }
                    let _ = InvalidateRect(Some(hwnd), Some(&eye_loupe_box(mx, my)), false);
                    return LRESULT(0);
                }
                eye_finish(Some(c));
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            // Space picks the pixel under the cursor (a steadier alternative to a
            // click — your hand doesn't move).
            WM_KEYDOWN if wparam.0 == VK_SPACE.0 as usize => {
                let cx = EYE_LAST_X.load(Ordering::Relaxed);
                let cy = EYE_LAST_Y.load(Ordering::Relaxed);
                let pick = (cx > -10000).then(|| eye_sample(cx, cy));
                if ctrl_held() {
                    if let Some(c) = pick {
                        if let Ok(mut st) = EYE_STASH.lock() {
                            st.push(c);
                        }
                        let _ = InvalidateRect(Some(hwnd), Some(&eye_loupe_box(cx, cy)), false);
                    }
                    return LRESULT(0);
                }
                eye_finish(pick);
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            // Esc is cancel, so it deliberately does NOT copy — not even a stash
            // that was mid-build. Space is the one key that commits.
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
        let _ = BitBlt(
            hdc,
            pr.left,
            pr.top,
            pr.right - pr.left,
            pr.bottom - pr.top,
            Some(shotdc),
            pr.left,
            pr.top,
            SRCCOPY,
        );
        // Draw the loupe at the current cursor.
        let cx = EYE_LAST_X.load(Ordering::Relaxed);
        let cy = EYE_LAST_Y.load(Ordering::Relaxed);
        if cx > -10000 {
            eye_draw_loupe(hdc, shotdc, cx, cy);
        }
    }
    let _ = EndPaint(hwnd, &ps);
}

/// Recolor the stock DC brush and hand it back ready to `Fill`/`FrameRect` with.
///
/// `eye_draw_loupe` used to `CreateSolidBrush` + `DeleteObject` a fresh brush for every
/// shape on every repaint, and the loupe repaints on every `WM_MOUSEMOVE` — real GDI
/// object churn on a high-frequency path. `DC_BRUSH` is a single stock object shared by
/// the whole process; `SetDCBrushColor` just recolors it, so there is nothing to free.
unsafe fn dc_brush(hdc: HDC, color: COLORREF) -> HBRUSH {
    SetDCBrushColor(hdc, color);
    HBRUSH(GetStockObject(DC_BRUSH).0)
}

/// The `EYE_SPAN`² source window for the loupe's `StretchBlt`, shifted to stay fully
/// inside a `vw`×`vh` snapshot, plus the cursor pixel's cell `(kx, ky)` within that
/// (possibly shifted) window. Returns `(sx, sy, kx, ky)`.
///
/// A bare `cx - EYE_K, cy - EYE_K` source (the previous behaviour) reads outside the
/// snapshot DC near a screen edge — `GetPixel`/`StretchBlt` on out-of-bounds source
/// coordinates return blank/garbage, not an error, so the loupe silently showed a
/// corrupted magnifier there instead of failing loudly. Mirrors
/// `screenshot::overlay::loupe::draw_loupe`'s clamped `sx`/`sy`/`kx`/`ky`; that version
/// can't be called directly from here (`loupe`'s module is private to `overlay`), so
/// the fix is ported rather than shared.
fn eye_sample_window(cx: i32, cy: i32, vw: i32, vh: i32) -> (i32, i32, i32, i32) {
    let sx = (cx - EYE_K).clamp(0, (vw - EYE_SPAN).max(0));
    let sy = (cy - EYE_K).clamp(0, (vh - EYE_SPAN).max(0));
    let kx = (cx - sx).clamp(0, EYE_SPAN - 1);
    let ky = (cy - sy).clamp(0, EYE_SPAN - 1);
    (sx, sy, kx, ky)
}

/// Draw the magnifier loupe (zoomed pixels + crosshair + hex label) near the
/// cursor, sampling from the frozen `shotdc`.
unsafe fn eye_draw_loupe(hdc: HDC, shotdc: HDC, cx: i32, cy: i32) {
    let lb = eye_loupe_box(cx, cy);
    let (bx, by) = (lb.left, lb.top);

    let (vw, vh) = (
        EYE_VW.load(Ordering::Relaxed),
        EYE_VH.load(Ordering::Relaxed),
    );
    let (sx, sy, kx, ky) = eye_sample_window(cx, cy, vw, vh);

    // Magnified pixels — nearest-neighbor so each screen pixel is a crisp block.
    SetStretchBltMode(hdc, COLORONCOLOR);
    let _ = StretchBlt(
        hdc,
        bx,
        by,
        EYE_MAG,
        EYE_MAG,
        Some(shotdc),
        sx,
        sy,
        EYE_SPAN,
        EYE_SPAN,
        SRCCOPY,
    );

    // Crosshair on the cursor's cell (the pixel that gets picked).
    let cell = EYE_MAG / EYE_SPAN;
    let cc = RECT {
        left: bx + kx * cell,
        top: by + ky * cell,
        right: bx + kx * cell + cell,
        bottom: by + ky * cell + cell,
    };
    let red = dc_brush(hdc, rgb(255, 40, 40));
    FrameRect(hdc, &cc, red);

    // Label strip: swatch + hex (top row), then a "Press Space to copy" hint.
    let (r, g, b) = eye_sample(cx, cy);
    let lbl = RECT {
        left: bx,
        top: by + EYE_MAG,
        right: bx + EYE_MAG,
        bottom: by + EYE_MAG + EYE_LBL,
    };
    let lbg = dc_brush(hdc, rgb(24, 24, 24));
    FillRect(hdc, &lbl, lbg);
    let sw = RECT {
        left: bx + 5,
        top: by + EYE_MAG + 5,
        right: bx + 21,
        bottom: by + EYE_MAG + 21,
    };
    let swb = dc_brush(hdc, rgb(r, g, b));
    FillRect(hdc, &sw, swb);

    SelectObject(hdc, HGDIOBJ(gui_font().0));
    SetBkMode(hdc, TRANSPARENT);
    // Hex (row 1).
    SetTextColor(hdc, rgb(240, 240, 240));
    let mut hex = wide(&format!("#{r:02X}{g:02X}{b:02X}"));
    let hn = hex.len().saturating_sub(1);
    let mut hr = RECT {
        left: bx + 28,
        top: by + EYE_MAG + 2,
        right: bx + EYE_MAG,
        bottom: by + EYE_MAG + 24,
    };
    DrawTextW(
        hdc,
        &mut hex[..hn],
        &mut hr,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );
    // Stash row: the colours collected so far, oldest first.
    let stash = EYE_STASH.lock().map(|s| s.clone()).unwrap_or_default();
    if !stash.is_empty() {
        let y = by + EYE_MAG + 24;
        for (i, &c) in stash
            .iter()
            .rev()
            .take(EYE_SW_MAX as usize)
            .rev()
            .enumerate()
        {
            let x = bx + 6 + i as i32 * (EYE_SW + 1);
            let cell = RECT {
                left: x,
                top: y,
                right: x + EYE_SW,
                bottom: y + EYE_SW,
            };
            let br = dc_brush(hdc, rgb(c.0, c.1, c.2));
            FillRect(hdc, &cell, br);
        }
        // More than fits: say how many, rather than silently showing a subset.
        if stash.len() as i32 > EYE_SW_MAX {
            SelectObject(hdc, HGDIOBJ(gui_font().0));
            SetBkMode(hdc, TRANSPARENT);
            SetTextColor(hdc, rgb(200, 200, 200));
            let mut n = wide(&format!("{}", stash.len()));
            let nn = n.len().saturating_sub(1);
            let mut nr = RECT {
                left: bx + 6 + EYE_SW_MAX * (EYE_SW + 1),
                top: y - 2,
                right: bx + EYE_MAG - 4,
                bottom: y + EYE_SW + 2,
            };
            DrawTextW(
                hdc,
                &mut n[..nn],
                &mut nr,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE,
            );
        }
    }

    // Hint (row 3).
    SetTextColor(hdc, rgb(150, 150, 150));
    let mut hint = wide(t(if stash.is_empty() {
        "eye_hint"
    } else {
        "eye_hint_stash"
    }));
    let hin = hint.len().saturating_sub(1);
    let mut hir = RECT {
        left: bx + 6,
        top: by + EYE_MAG + 42,
        right: bx + EYE_MAG,
        bottom: by + EYE_MAG + EYE_LBL,
    };
    DrawTextW(
        hdc,
        &mut hint[..hin],
        &mut hir,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );

    // Outer + magnifier borders.
    let border = dc_brush(hdc, rgb(0, 0, 0));
    let outer = RECT {
        left: bx,
        top: by,
        right: bx + EYE_MAG,
        bottom: by + EYE_MAG + EYE_LBL,
    };
    FrameRect(hdc, &outer, border);
    let mag = RECT {
        left: bx,
        top: by,
        right: bx + EYE_MAG,
        bottom: by + EYE_MAG,
    };
    FrameRect(hdc, &mag, border);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression for the unclamped `StretchBlt` source: near any edge of the
    /// snapshot, `sx`/`sy` must stay in `[0, v - EYE_SPAN]` so the source rect never
    /// extends past the snapshot bounds, and `kx`/`ky` must still land on the cursor's
    /// own cell inside that (possibly shifted) window.
    #[test]
    fn eye_sample_window_stays_inside_the_snapshot_near_every_edge() {
        let (vw, vh) = (1920, 1080);
        for (cx, cy) in [
            (0, 0),           // top-left corner
            (vw - 1, vh - 1), // bottom-right corner
            (0, vh / 2),      // left edge
            (vw - 1, vh / 2), // right edge
            (vw / 2, 0),      // top edge
            (vw / 2, vh - 1), // bottom edge
            (vw / 2, vh / 2), // interior — no clamp should be needed
        ] {
            let (sx, sy, kx, ky) = eye_sample_window(cx, cy, vw, vh);
            assert!(
                sx >= 0 && sx + EYE_SPAN <= vw,
                "sx {sx} puts the source rect outside [0, {vw}) at cursor ({cx}, {cy})"
            );
            assert!(
                sy >= 0 && sy + EYE_SPAN <= vh,
                "sy {sy} puts the source rect outside [0, {vh}) at cursor ({cx}, {cy})"
            );
            // The cursor's own pixel, mapped into the (possibly shifted) window, must
            // still be the cell that gets the crosshair.
            assert_eq!(sx + kx, cx, "kx must map back to the cursor's own column");
            assert_eq!(sy + ky, cy, "ky must map back to the cursor's own row");
        }
    }

    /// A degenerate/tiny snapshot (smaller than the sample span) must not panic or
    /// produce a negative-width source rect — `(vw - EYE_SPAN).max(0)` is what
    /// prevents that.
    #[test]
    fn eye_sample_window_handles_a_snapshot_smaller_than_the_span() {
        let (sx, sy, kx, ky) = eye_sample_window(2, 2, 4, 4);
        assert_eq!((sx, sy), (0, 0));
        assert!(kx < EYE_SPAN && ky < EYE_SPAN);
    }
}
