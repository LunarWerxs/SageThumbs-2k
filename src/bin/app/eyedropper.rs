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

use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, VK_CONTROL, VK_ESCAPE, VK_SPACE, VK_TAB,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::dark::rgb;
use crate::win::{app_icon, gui_font, set_clipboard_text, t, wide};

const EYE_K: i32 = 7; // half-window: a (2K+1)² block of screen pixels in the loupe
const EYE_SPAN: i32 = 2 * EYE_K + 1; // 15 px sampled across
const EYE_MAG: i32 = 150; // magnified loupe size (px) → 10× zoom
const EYE_LBL: i32 = 78; // loupe label strip (px): value row + swatch row + two hint rows
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

/// Picks from PREVIOUS sessions (most recent first, capped at 10, persisted via settings).
/// Shown as the swatch row while the stash is empty; the 1–9 keys copy an entry directly,
/// which is the point — re-grabbing a colour you picked yesterday without hunting for a
/// pixel that still shows it.
static EYE_HISTORY: Mutex<Vec<(u8, u8, u8)>> = Mutex::new(Vec::new());

/// The clipboard format Tab cycles through (index into [`fmt_color`]'s match; persisted).
static EYE_FMT: AtomicI32 = AtomicI32::new(0);

fn hex_of((r, g, b): (u8, u8, u8)) -> String {
    format!("#{r:02X}{g:02X}{b:02X}")
}

/// RGB → (hue 0..360, saturation 0..1, lightness 0..1). Textbook; kept exact enough that
/// round numbers come out round (pure red is `hsl(0, 100%, 50%)`, not 99.6%).
fn rgb_to_hsl((r, g, b): (u8, u8, u8)) -> (f64, f64, f64) {
    let (r, g, b) = (r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    if max == min {
        return (0.0, 0.0, l);
    }
    let d = max - min;
    let s = if l > 0.5 {
        d / (2.0 - max - min)
    } else {
        d / (max + min)
    };
    let h = if max == r {
        ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    } * 60.0;
    (h, s, l)
}

/// RGB → (hue 0..360, saturation 0..1, value 0..1).
fn rgb_to_hsv((r, g, b): (u8, u8, u8)) -> (f64, f64, f64) {
    let (h, _, _) = rgb_to_hsl((r, g, b));
    let (rf, gf, bf) = (r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0);
    let max = rf.max(gf).max(bf);
    let min = rf.min(gf).min(bf);
    let s = if max == 0.0 { 0.0 } else { (max - min) / max };
    (h, s, max)
}

/// The colour formatted for the ACTIVE format: 0 hex (the historical behaviour and the
/// default), 1 CSS `rgb()`, 2 `hsl()`, 3 `hsv()`. Everything that reaches the clipboard —
/// single pick, stash list, history recall — and the loupe's value row go through here, so
/// what you read is always what you get.
fn fmt_color(fmt: i32, c: (u8, u8, u8)) -> String {
    match fmt {
        1 => format!("rgb({}, {}, {})", c.0, c.1, c.2),
        2 => {
            let (h, s, l) = rgb_to_hsl(c);
            format!(
                "hsl({}, {}%, {}%)",
                h.round() as i32 % 360,
                (s * 100.0).round() as i32,
                (l * 100.0).round() as i32
            )
        }
        3 => {
            let (h, s, v) = rgb_to_hsv(c);
            format!(
                "hsv({}, {}%, {}%)",
                h.round() as i32 % 360,
                (s * 100.0).round() as i32,
                (v * 100.0).round() as i32
            )
        }
        _ => hex_of(c),
    }
}

/// Record picks into the persistent history: most recent first, deduplicated, capped by the
/// settings writer. Called on every commit path, so the history is what you actually took
/// with you, not what you hovered.
fn eye_remember(picked: &[(u8, u8, u8)]) {
    let Ok(mut h) = EYE_HISTORY.lock() else {
        return;
    };
    for &c in picked.iter().rev() {
        h.retain(|&e| e != c);
        h.insert(0, c);
    }
    h.truncate(10);
    let _ = sagethumbs2k_core::settings::set_eyedropper_history(&h);
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
    eye_remember(&all);
    let fmt = EYE_FMT.load(Ordering::Relaxed);
    let text: Vec<String> = all.into_iter().map(|c| fmt_color(fmt, c)).collect();
    set_clipboard_text(&text.join("\r\n"));
}

pub(crate) unsafe fn run_eyedropper(hinst: HINSTANCE) {
    if let Ok(mut st) = EYE_STASH.lock() {
        st.clear();
    }
    // The remembered format + past picks. Loaded here rather than lazily so the loupe's very
    // first paint already shows both.
    EYE_FMT.store(
        sagethumbs2k_core::settings::eyedropper_format() as i32,
        Ordering::Relaxed,
    );
    if let Ok(mut h) = EYE_HISTORY.lock() {
        *h = sagethumbs2k_core::settings::eyedropper_history();
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
    // The headless asset must not vary with this machine's remembered format or history —
    // force the defaults (hex, no swatch row) so the capture is as deterministic as a live
    // screen grab can be.
    EYE_FMT.store(0, Ordering::Relaxed);
    if let Ok(mut h) = EYE_HISTORY.lock() {
        h.clear();
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
            WM_MOUSEMOVE => on_mousemove(hwnd, lparam),
            WM_LBUTTONDOWN | WM_RBUTTONDOWN => on_button_down(hwnd, lparam),
            // Space picks the pixel under the cursor (a steadier alternative to a
            // click — your hand doesn't move).
            WM_KEYDOWN if wparam.0 == VK_SPACE.0 as usize => on_keydown_space(hwnd),
            // Esc is cancel, so it deliberately does NOT copy — not even a stash
            // that was mid-build. Space is the one key that commits.
            WM_KEYDOWN if wparam.0 == VK_ESCAPE.0 as usize => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            // Tab cycles the clipboard format (hex → rgb → hsl → hsv) and remembers it. The
            // value row re-renders in the new format immediately, so the choice is made while
            // LOOKING at the number it produces rather than in a settings page.
            WM_KEYDOWN if wparam.0 == VK_TAB.0 as usize => on_keydown_tab(hwnd),
            // 1–9 copy that history swatch (1 = most recent) and close — the "give me
            // yesterday's brand colour again" path. Ignored when the digit names no entry,
            // so a stray keypress can't close the tool with nothing copied.
            WM_KEYDOWN if (0x31..=0x39).contains(&wparam.0) => on_keydown_digit(hwnd, wparam),
            WM_PAINT => {
                eye_paint(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => on_destroy(),
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

unsafe fn on_mousemove(hwnd: HWND, lparam: LPARAM) -> LRESULT {
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

unsafe fn on_button_down(hwnd: HWND, lparam: LPARAM) -> LRESULT {
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

unsafe fn on_keydown_space(hwnd: HWND) -> LRESULT {
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

unsafe fn on_keydown_tab(hwnd: HWND) -> LRESULT {
    let fmt = (EYE_FMT.load(Ordering::Relaxed) + 1) % 4;
    EYE_FMT.store(fmt, Ordering::Relaxed);
    let _ = sagethumbs2k_core::settings::set_eyedropper_format(fmt as u32);
    let cx = EYE_LAST_X.load(Ordering::Relaxed);
    let cy = EYE_LAST_Y.load(Ordering::Relaxed);
    let _ = InvalidateRect(Some(hwnd), Some(&eye_loupe_box(cx, cy)), false);
    LRESULT(0)
}

unsafe fn on_keydown_digit(hwnd: HWND, wparam: WPARAM) -> LRESULT {
    let idx = wparam.0 - 0x31;
    let c = EYE_HISTORY.lock().ok().and_then(|h| h.get(idx).copied());
    if let Some(c) = c {
        eye_remember(&[c]); // recalling promotes it back to most-recent
        let fmt = EYE_FMT.load(Ordering::Relaxed);
        set_clipboard_text(&fmt_color(fmt, c));
        let _ = DestroyWindow(hwnd);
    }
    LRESULT(0)
}

unsafe fn on_destroy() -> LRESULT {
    if let Some(&dc) = EYE_SHOT.get() {
        let _ = DeleteDC(HDC(dc as *mut c_void));
    }
    if let Some(&bmp) = EYE_SHOT_BMP.get() {
        let _ = DeleteObject(HGDIOBJ(bmp as *mut c_void));
    }
    PostQuitMessage(0);
    LRESULT(0)
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
    // Value (row 1), rendered in the ACTIVE clipboard format so Tab's effect is visible on
    // the number itself, not announced elsewhere.
    SetTextColor(hdc, rgb(240, 240, 240));
    let mut hex = wide(&fmt_color(EYE_FMT.load(Ordering::Relaxed), (r, g, b)));
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
    // Swatch row: the session's stash while one is building, else the persistent HISTORY
    // (most recent leftmost — matching the 1–9 keys, so the row doubles as their legend).
    let stash = EYE_STASH.lock().map(|s| s.clone()).unwrap_or_default();
    let history = if stash.is_empty() {
        EYE_HISTORY.lock().map(|h| h.clone()).unwrap_or_default()
    } else {
        Vec::new()
    };
    let row: Vec<(u8, u8, u8)> = if stash.is_empty() {
        history
    } else {
        // Stash draws oldest-first (the order they'll copy in), reversed below like before.
        stash.clone()
    };
    if !row.is_empty() {
        let y = by + EYE_MAG + 24;
        // The stash keeps its historical oldest-first presentation; history is already
        // most-recent-first and must stay that way to match the digit keys.
        let ordered: Vec<(u8, u8, u8)> = if stash.is_empty() {
            row.iter().take(EYE_SW_MAX as usize).copied().collect()
        } else {
            row.iter()
                .rev()
                .take(EYE_SW_MAX as usize)
                .rev()
                .copied()
                .collect()
        };
        for (i, &c) in ordered.iter().enumerate() {
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

    // Hints (rows 3 + 4). The first line keeps its historical text (and all 36 existing
    // translations); the second names what this session added — the format cycle and the
    // history recall keys.
    SetTextColor(hdc, rgb(150, 150, 150));
    let mut hint = wide(t(if stash.is_empty() {
        "eye_hint"
    } else {
        "eye_hint_stash"
    }));
    let hin = hint.len().saturating_sub(1);
    let mut hir = RECT {
        left: bx + 6,
        top: by + EYE_MAG + 40,
        right: bx + EYE_MAG,
        bottom: by + EYE_MAG + 58,
    };
    DrawTextW(
        hdc,
        &mut hint[..hin],
        &mut hir,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );
    let mut hint2 = wide(t("eye_hint_extras"));
    let h2n = hint2.len().saturating_sub(1);
    let mut h2r = RECT {
        left: bx + 6,
        top: by + EYE_MAG + 58,
        right: bx + EYE_MAG,
        bottom: by + EYE_MAG + EYE_LBL,
    };
    DrawTextW(
        hdc,
        &mut hint2[..h2n],
        &mut h2r,
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

    /// The four formats a pick can copy as. Exactness on the primaries matters: a designer
    /// checking a pure colour reads "100%", and "99%" reads as a broken converter.
    #[test]
    fn formats_render_the_primaries_exactly() {
        let red = (255u8, 0u8, 0u8);
        assert_eq!(fmt_color(0, red), "#FF0000");
        assert_eq!(fmt_color(1, red), "rgb(255, 0, 0)");
        assert_eq!(fmt_color(2, red), "hsl(0, 100%, 50%)");
        assert_eq!(fmt_color(3, red), "hsv(0, 100%, 100%)");
        // Greys have no hue and zero saturation in both models.
        let grey = (128u8, 128u8, 128u8);
        assert_eq!(fmt_color(2, grey), "hsl(0, 0%, 50%)");
        assert_eq!(fmt_color(3, grey), "hsv(0, 0%, 50%)");
    }

    /// An out-of-range format index must fall back to hex, never panic — the value comes
    /// from the registry, which anything can have scribbled on.
    #[test]
    fn unknown_format_is_hex() {
        assert_eq!(fmt_color(17, (1, 2, 3)), "#010203");
        assert_eq!(fmt_color(-1, (1, 2, 3)), "#010203");
    }

    /// Hue must land in the right sextant for each channel-dominant colour (the `rem_euclid`
    /// in the red branch is what keeps a red with a touch of blue from going negative).
    #[test]
    fn hue_sextants() {
        assert_eq!(rgb_to_hsl((255, 255, 0)).0.round() as i32, 60); // yellow
        assert_eq!(rgb_to_hsl((0, 255, 0)).0.round() as i32, 120); // green
        assert_eq!(rgb_to_hsl((0, 255, 255)).0.round() as i32, 180); // cyan
        assert_eq!(rgb_to_hsl((0, 0, 255)).0.round() as i32, 240); // blue
        assert_eq!(rgb_to_hsl((255, 0, 255)).0.round() as i32, 300); // magenta
        let h = rgb_to_hsl((255, 0, 10)).0;
        assert!(h > 350.0, "red-with-blue hue wrapped wrong: {h}");
    }

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
