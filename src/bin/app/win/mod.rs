//! Shared Win32 primitives for the SageThumbs 2K app binary.
//!
//! Low-level, reused-across-dialogs helpers: control creation + font, the
//! translated-string shorthand, wide-string conversion, the app icon / artwork
//! loaders, button & combo & edit & folder-picker & clipboard helpers, the
//! `http(s)`-only `open_url` guard, and the small Win32 const/style bits that the
//! `windows` metadata doesn't surface.

use core::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::sync::OnceLock;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetTextExtentPoint32W,
    ReleaseDC, SelectObject, HBITMAP, HBRUSH, HGDIOBJ,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::*;


use sagethumbs2k_core::i18n;
mod scaling;
mod pickers;
pub(crate) use scaling::{gui_font, gui_font_for, gui_font_header, gui_font_title, gui_font_sized, dpi_scale, dpi_scale_dpi, set_dpi_override, wm_dpichanged};
pub(crate) use pickers::{desktop_dir, pick_folder, pick_save_png, pick_save_settings, pick_open_settings, set_clipboard_text};

/// Shorthand for a translated UI string in the active language.
pub(crate) fn t(key: &str) -> &'static str {
    i18n::t(key)
}

// ---- Control IDs (shared across every dialog) --------------------------
pub(crate) const IDOK: i32 = 1;
pub(crate) const IDCANCEL: i32 = 2;

// --- Branding (edit these / swap the assets to rebrand) -----------------
pub(crate) const URL_PARENT: &str = "https://lunarwerx.com";
// The product's own home. No dedicated domain yet, so this is the GitHub repo
// (where users actually get + engage with it). Repoint if a product site appears.
pub(crate) const URL_PRODUCT: &str = "https://github.com/LunarWerxs/SageThumbs-2k";
pub(crate) const URL_GITHUB: &str = "https://github.com/LunarWerxs/SageThumbs-2k";

/// Window/taskbar icon (16/32/48). Embedded; the EXE-file icon in Explorer comes
/// from the installer's shortcut. A `app.ico` next to the EXE overrides at runtime.
const APP_ICO: &[u8] = include_bytes!("../../../../assets/app-win.ico");

pub(crate) fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

/// Read a WinInet request handle to EOF, capped at `max_bytes`. Returns the FULL
/// body, or `None` on a read error or an over-cap response — never a truncated
/// body. Both remote clients (the sponsor GET in `sponsors.rs` and the screenshot POST in
/// `screenshot/upload.rs`) parse/decode the result, so partial bytes must not be
/// handed back looking like success. Shared so the read loop and the over-cap
/// policy live in exactly one place (the POST path used to return the truncated
/// body on over-cap — a corrupt URL; this fixes it for both).
pub(crate) unsafe fn wininet_drain(req: *mut c_void, max_bytes: usize) -> Option<Vec<u8>> {
    use windows::Win32::Networking::WinInet::InternetReadFile;
    let mut data = Vec::new();
    let mut buf = [0u8; 16384];
    loop {
        let mut read = 0u32;
        if InternetReadFile(req, buf.as_mut_ptr() as *mut c_void, buf.len() as u32, &mut read)
            .is_err()
        {
            return None; // read error → response is incomplete, don't trust it
        }
        if read == 0 {
            break; // end of stream
        }
        data.extend_from_slice(&buf[..read as usize]);
        if data.len() > max_bytes {
            return None; // oversized / never-ending → reject (no truncated bodies)
        }
    }
    Some(data)
}

/// The system message font (Segoe UI / Segoe UI Variable on Win11), cached.
/// Falls back to the stock GUI font if the metrics query fails.
/// Register a class, create + show a dialog, run its message pump. `w`/`h` are 96-DPI design px.
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn run_dialog(
    class: PCWSTR,
    wndproc: WNDPROC,
    title: &str,
    w: i32,
    h: i32,
    modal: Option<HWND>,
) -> Option<HWND> {
    let hinst: HINSTANCE = GetModuleHandleW(None).ok()?.into();
    let dark = crate::dark::is_dark();
    let wc = WNDCLASSW {
        lpfnWndProc: wndproc,
        hInstance: hinst,
        lpszClassName: class,
        // A top-level dialog carries the app icon + arrow cursor; the modal popup
        // inherits its owner's icon (the original popup set neither).
        hIcon: if modal.is_none() { app_icon().unwrap_or_default() } else { Default::default() },
        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
        hbrBackground: if dark { crate::dark::dark_bg_brush() } else { HBRUSH(16isize as *mut c_void) },
        ..Default::default()
    };
    RegisterClassW(&wc); // idempotent: re-register returns 0 (already registered) — fine

    // Geometry: design pixels scaled to the relevant DPI. The owner's DPI for a
    // modal popup; the primary monitor's DPI otherwise (a top-level dialog opens
    // at CW_USEDEFAULT, so we use the system DPI as the creation DPI).
    let dpi_ref = modal.unwrap_or_default();
    let creation_dpi = if dpi_ref.0.is_null() { dpi_for_system() } else { GetDpiForWindow(dpi_ref) as i32 };
    let (sw, sh) = (dpi_scale_dpi(w, creation_dpi), dpi_scale_dpi(h, creation_dpi));

    let (ex_style, style, x, y, parent) = match modal {
        None => (
            WS_EX_CONTROLPARENT | WS_EX_DLGMODALFRAME,
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            None,
        ),
        Some(owner) => {
            // Center over the owner.
            let mut orc = RECT::default();
            let _ = GetWindowRect(owner, &mut orc);
            let px = orc.left + ((orc.right - orc.left) - sw) / 2;
            let py = orc.top + ((orc.bottom - orc.top) - sh) / 2;
            (WS_EX_DLGMODALFRAME, WS_POPUP | WS_CAPTION | WS_SYSMENU, px, py, Some(owner))
        }
    };

    let title_w = wide(title);
    let hwnd = CreateWindowExW(
        ex_style,
        class,
        PCWSTR(title_w.as_ptr()),
        style,
        x,
        y,
        sw,
        sh,
        parent,
        None,
        Some(hinst),
        None,
    )
    .ok()?;

    if dark {
        crate::dark::dark_control(hwnd, w!("DarkMode_Explorer"));
        crate::dark::dark_titlebar(hwnd);
    }

    match modal {
        None => {
            let _ = ShowWindow(hwnd, SW_SHOW);
            pump_until_quit(hwnd);
        }
        Some(owner) => {
            let _ = EnableWindow(owner, false);
            let _ = ShowWindow(hwnd, SW_SHOW);
            pump_until_closed(hwnd);
            let _ = EnableWindow(owner, true);
        }
    }
    Some(hwnd)
}

// ===== Headless capture plumbing (the `--shot*` verification/asset modes) =====

/// Drain the message queue `frames` times (tiny sleep between) so async WM_PAINT / timer /
/// layout work settles before a headless PrintWindow capture. Shared by every `--shot` path.
pub(crate) unsafe fn pump_msgs(frames: usize) {
    let mut msg = MSG::default();
    for _ in 0..frames {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        std::thread::sleep(std::time::Duration::from_millis(16));
    }
}

/// Force a SYNCHRONOUS paint of `hwnd` AND every child (RDW_UPDATENOW). Owner-drawn statics
/// (nav rail, pane header, toggle switches) only paint on a real WM_PAINT, so without this a
/// headless capture races them and leaves blank gaps.
pub(crate) unsafe fn force_repaint(hwnd: HWND) {
    use windows::Win32::Graphics::Gdi::{RedrawWindow, RDW_ALLCHILDREN, RDW_INVALIDATE, RDW_UPDATENOW};
    let _ = RedrawWindow(Some(hwnd), None, None, RDW_INVALIDATE | RDW_ALLCHILDREN | RDW_UPDATENOW);
}

/// Create a top-level dialog window OFF-SCREEN + non-activated — a real window that never
/// appears on screen and steals no focus — for headless `PrintWindow` capture. Same class
/// registration + dark styling as [`run_dialog`], but returns the HWND WITHOUT a message
/// loop: the caller pumps ([`pump_msgs`]), captures, and `DestroyWindow`s it. `design_w/h`
/// are 96-dpi design pixels (scaled to the primary DPI here).
pub(crate) unsafe fn create_shot_window(
    hinst: HINSTANCE,
    dark: bool,
    class: PCWSTR,
    wndproc: WNDPROC,
    title: &str,
    design_w: i32,
    design_h: i32,
) -> Option<HWND> {
    let wc = WNDCLASSW {
        lpfnWndProc: wndproc,
        hInstance: hinst,
        lpszClassName: class,
        hIcon: app_icon().unwrap_or_default(),
        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
        hbrBackground: if dark { crate::dark::dark_bg_brush() } else { HBRUSH(16isize as *mut c_void) },
        ..Default::default()
    };
    RegisterClassW(&wc); // idempotent

    // Position it ON-SCREEN (centered on the cursor monitor), NOT off the virtual desktop: an
    // off-screen window's DWM redirection surface can be stale/blank when PrintWindow grabs it
    // (that raced the capture — some frames came out blank or showed the previous tab). DWM keeps
    // an on-screen window's surface current. `WS_EX_LAYERED` + alpha 0 makes it fully transparent
    // → invisible to the user, while PrintWindow still captures the real (opaque) content;
    // SW_SHOWNOACTIVATE + tool-window means it steals no focus and shows no taskbar entry. Sizing
    // to the cursor monitor's DPI also matches the per-control layout DPI (GetDpiForWindow).
    let (dpi, work) = cursor_monitor_metrics();
    let (sw, sh) = (dpi_scale_dpi(design_w, dpi), dpi_scale_dpi(design_h, dpi));
    let x = work.left + ((work.right - work.left) - sw).max(0) / 2;
    let y = work.top + ((work.bottom - work.top) - sh).max(0) / 2;
    let title_w = wide(title);
    let hwnd = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_CONTROLPARENT | WS_EX_DLGMODALFRAME,
        class,
        PCWSTR(title_w.as_ptr()),
        WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_CLIPCHILDREN,
        x,
        y,
        sw,
        sh,
        None,
        None,
        Some(hinst),
        None,
    )
    .ok()?;
    // Fully transparent (alpha 0) → composited by DWM but invisible on screen.
    let _ = SetLayeredWindowAttributes(hwnd, windows::Win32::Foundation::COLORREF(0), 0, LWA_ALPHA);
    if dark {
        crate::dark::dark_control(hwnd, w!("DarkMode_Explorer"));
        crate::dark::dark_titlebar(hwnd);
    }
    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    Some(hwnd)
}

/// The primary monitor's effective DPI (for a CW_USEDEFAULT top-level dialog,
/// which has no HWND yet to query). Falls back to 96.
pub(crate) fn dpi_for_system() -> i32 {
    unsafe {
        let dc = GetDC(None);
        let dpi = windows::Win32::Graphics::Gdi::GetDeviceCaps(
            Some(dc),
            windows::Win32::Graphics::Gdi::LOGPIXELSX,
        );
        ReleaseDC(None, dc);
        if dpi == 0 { 96 } else { dpi }
    }
}

/// Effective DPI + work-area rect of the monitor under the cursor (where the user is).
/// A top-level window sizes AND positions itself for the monitor it actually opens on,
/// so the window frame's DPI matches the per-control `dpi_scale()` (`GetDpiForWindow`) —
/// even on a mixed-DPI multi-monitor setup, or after the user changed scale without
/// signing out. `dpi_for_system()` reports the LOGIN-time primary DPI, which is wrong in
/// those cases and left the fixed-size v3 Settings window clipping its controls. 96/primary
/// fallback on any failure.
pub(crate) fn cursor_monitor_metrics() -> (i32, windows::Win32::Foundation::RECT) {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTOPRIMARY,
    };
    use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
    unsafe {
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTOPRIMARY);
        let (mut dx, mut dy) = (96u32, 96u32);
        let _ = GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        let mut mi = MONITORINFO {
            cbSize: core::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let _ = GetMonitorInfoW(mon, &mut mi);
        ((if dx == 0 { 96 } else { dx as i32 }), mi.rcWork)
    }
}

/// Standard top-level pump: dialog-key translation + dispatch until WM_QUIT.
unsafe fn pump_until_quit(hwnd: HWND) {
    let mut msg = MSG::default();
    loop {
        let r = GetMessageW(&mut msg, None, 0, 0).0;
        if r == 0 || r == -1 {
            break;
        }
        if !IsDialogMessageW(hwnd, &msg).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Modal pump: runs until `hwnd` destroys itself (the popup uses no
/// PostQuitMessage, which would otherwise kill the parent dialog's loop).
unsafe fn pump_until_closed(hwnd: HWND) {
    let mut msg = MSG::default();
    while IsWindow(Some(hwnd)).as_bool() {
        let r = GetMessageW(&mut msg, None, 0, 0).0;
        if r == 0 || r == -1 {
            break;
        }
        if !IsDialogMessageW(hwnd, &msg).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Create a child control, set the GUI font, return its HWND. `x/y/cw/ch` are
/// 96-DPI design pixels — routed through [`dpi_scale`] for the parent's DPI, so
/// at 96 DPI the geometry is unchanged (identity).
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn ctl(
    parent: HWND,
    class: PCWSTR,
    text: &str,
    style: WINDOW_STYLE,
    x: i32,
    y: i32,
    cw: i32,
    ch: i32,
    id: i32,
    hinst: HINSTANCE,
) -> HWND {
    let (x, y, cw, ch) = (
        dpi_scale(parent, x),
        dpi_scale(parent, y),
        dpi_scale(parent, cw),
        dpi_scale(parent, ch),
    );
    let t = wide(text);
    let h = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        class,
        PCWSTR(t.as_ptr()),
        // WS_CLIPSIBLINGS so a control can't repaint over a higher-z-order sibling
        // (the Settings dialog's scroll mask relies on this; harmless elsewhere).
        WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS | style,
        x,
        y,
        cw,
        ch,
        Some(parent),
        Some(HMENU(id as usize as *mut c_void)),
        Some(hinst),
        None,
    )
    .expect("create control");
    SendMessageW(h, WM_SETFONT, Some(WPARAM(gui_font_for(parent).0 as usize)), Some(LPARAM(1)));
    if crate::dark::is_dark() {
        // Edit boxes use the dark common-file-dialog style; everything else the
        // dark Explorer style (themed checkbox glyphs, scrollbars, list rows).
        let theme = if class.0 == EDIT.0 {
            w!("DarkMode_CFD")
        } else {
            w!("DarkMode_Explorer")
        };
        crate::dark::dark_control(h, theme);
    }
    h
}

pub(crate) const STATIC: PCWSTR = w!("STATIC");
pub(crate) const BUTTON: PCWSTR = w!("BUTTON");
pub(crate) const EDIT: PCWSTR = w!("EDIT");
pub(crate) const COMBOBOX: PCWSTR = w!("COMBOBOX");
pub(crate) const SYSLINK: PCWSTR = w!("SysLink");

// ---- Layout cursor ------------------------------------------------------
// A tiny row-cursor for the form-style dialogs: a left margin, an indent for
// nested rows, a label column, an edit column, and a row pitch. Values are
// 96-DPI DESIGN pixels — `ctl()` scales them to the live DPI, so the cursor and
// item #1's DPI seam are one and the same (no separate scaling here). The cursor
// reproduces a section's exact original geometry (so a 96-DPI layout is
// byte-identical), it just removes the hand-copied per-row arithmetic.

pub(crate) const MARGIN: i32 = 16; // left edge of group labels
pub(crate) const INDENT: i32 = 26; // left edge of indented (in-group) controls
pub(crate) const LABEL_W: i32 = 190; // label column width (settings limits rows)
pub(crate) const EDIT_X: i32 = 224; // left edge of the edit/value column (settings)
pub(crate) const BTN_H: i32 = 28; // standard pushbutton height

/// A tidy home for the hand-rolled Win32 message/style constants the `windows`
/// metadata doesn't surface. Re-exported below, so callers still reference them
/// as `crate::win::SS_BITMAP` etc. — gathering them here is purely organizational
/// (no behavior change).
pub(crate) mod winshim {
    // STATIC control styles.
    pub(crate) const SS_CENTER: u32 = 0x0000_0001;
    /// Vertically center single-line text (the upload "busy pill" uses it).
    pub(crate) const SS_CENTERIMAGE: u32 = 0x0000_0200;
    pub(crate) const SS_OWNERDRAW: u32 = 0x0000_000D;
    pub(crate) const SS_BITMAP: u32 = 0x0000_000E;
    pub(crate) const SS_NOTIFY: u32 = 0x0000_0100;
    /// Pin the static to its created size and fit the image to it, instead of the
    /// default (the static grows to the image — which let oversized remote sponsor
    /// banners cover the footer buttons).
    pub(crate) const SS_REALSIZECONTROL: u32 = 0x0000_0040;

    // Tooltip-window style bits.
    pub(crate) const TTS_ALWAYSTIP: u32 = 0x01;
    pub(crate) const TTS_NOPREFIX: u32 = 0x02;

    // Button control messages (CheckDlgButton/IsDlgButtonChecked aren't in this
    // windows-rs metadata, so drive the BUTTON control directly) + result.
    pub(crate) const BM_GETCHECK_MSG: u32 = 0x00F0;
    pub(crate) const BM_SETCHECK_MSG: u32 = 0x00F1;
    pub(crate) const BST_CHECKED: isize = 1;

    /// Edit-control "select text" message.
    pub(crate) const EM_SETSEL: u32 = 0x00B1;

    // ListView checkbox state-image bits — INDEXTOSTATEIMAGEMASK(2 / 1).
    pub(crate) const CHECKED: u32 = 0x2000;
    pub(crate) const UNCHECKED: u32 = 0x1000;
}
pub(crate) use winshim::*;

pub(crate) const fn make_lparam(low: i32, high: i32) -> isize {
    ((low & 0xFFFF) | (high << 16)) as isize
}

/// Open a URL in the default browser (sponsor links + the remote sponsor banner).
/// Refuses anything that isn't `http(s)://` so a compromised sponsor manifest can't
/// route us to `file:`, a UNC path, or a custom protocol handler.
pub(crate) unsafe fn open_url(url: &str) {
    if !crate::sponsors::is_web_url(url) {
        return;
    }
    let u = wide(url);
    let _ = ShellExecuteW(None, w!("open"), PCWSTR(u.as_ptr()), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
}

/// Read a DLL-handed list file (one path per line) into a Vec, trimming each
/// line and dropping blanks, then deleting the temp list file. Shared by the
/// three `--xxx <listfile>` dialog modes (Convert, Files-to-folder,
/// Tags-to-folders), which all consumed it identically.
pub(crate) fn read_listfile(path: &str) -> Vec<String> {
    let files: Vec<String> = std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    let _ = std::fs::remove_file(path);
    files
}

/// A NUL-terminated wide buffer (e.g. a SysLink's szUrl) as a String.
pub(crate) fn wstr_to_string(w: &[u16]) -> String {
    let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..end])
}

/// Decode logo/banner artwork to an HBITMAP sized to `w`x`h`. Prefers a file of
/// `override_name` next to the EXE (user-swappable) and falls back to the
/// embedded `default_png`.
pub(crate) unsafe fn load_art(default_png: &[u8], override_name: &str, w: u32, h: u32) -> Option<HBITMAP> {
    let from_file = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(override_name)))
        .and_then(|f| std::fs::read(f).ok());
    let data = from_file.as_deref().unwrap_or(default_png);
    sagethumbs2k_core::app_image::image_to_hbitmap_sized(data, w, h).map(|h| HBITMAP(h as *mut c_void))
}

/// Load the app icon for the title bar + taskbar. Prefers an `app.ico` next to
/// the EXE (swappable), else the embedded icon written to a temp file (LoadImageW
/// needs a path). None if unavailable.
///
/// Cached in a `OnceLock` like [`gui_font`]: every dialog asks for the icon at
/// creation, so loading it once avoids leaking a fresh HICON (and rewriting the
/// temp file) on every call. 0 in the slot means "tried and failed".
pub(crate) unsafe fn app_icon() -> Option<HICON> {
    static ICON: OnceLock<usize> = OnceLock::new();
    let p = *ICON.get_or_init(|| {
        let beside = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("app.ico")))
            .filter(|p| p.exists());
        let path = beside.unwrap_or_else(|| {
            let mut p = std::env::temp_dir();
            p.push("sagethumbs2k.ico");
            let _ = std::fs::write(&p, APP_ICO);
            p
        });
        let w = wide(&path.to_string_lossy());
        match LoadImageW(None, PCWSTR(w.as_ptr()), IMAGE_ICON, 0, 0, LR_LOADFROMFILE | LR_DEFAULTSIZE) {
            Ok(h) => h.0 as usize,
            Err(_) => 0,
        }
    });
    (p != 0).then_some(HICON(p as *mut c_void))
}

/// Set a static control's bitmap, freeing whatever bitmap it held before.
pub(crate) unsafe fn set_static_bitmap(ctl: HWND, hbmp: HBITMAP) {
    let old = SendMessageW(ctl, STM_SETIMAGE, Some(WPARAM(IMAGE_BITMAP.0 as usize)), Some(LPARAM(hbmp.0 as isize)));
    if old.0 != 0 {
        let _ = DeleteObject(HGDIOBJ(old.0 as *mut c_void));
    }
}

/// Pixel width of `s` rendered in the GUI font (for centering controls).
pub(crate) unsafe fn text_width(s: &str) -> i32 {
    let hdc = GetDC(None);
    let old = SelectObject(hdc, HGDIOBJ(gui_font().0));
    let w = wide(s);
    let n = w.len().saturating_sub(1);
    let mut sz = windows::Win32::Foundation::SIZE::default();
    let _ = GetTextExtentPoint32W(hdc, &w[..n], &mut sz);
    SelectObject(hdc, old);
    ReleaseDC(None, hdc);
    sz.cx
}

/// Show a simple warning message box owned by the dialog.
pub(crate) unsafe fn message_box(hwnd: HWND, text: &str, caption: &str) {
    let t = wide(text);
    let c = wide(caption);
    MessageBoxW(Some(hwnd), PCWSTR(t.as_ptr()), PCWSTR(c.as_ptr()), MB_OK | MB_ICONWARNING);
}

/// One-shot tray balloon from a WINDOWLESS helper process: a throwaway hidden window
/// hosts a temporary notify icon, pops a `NIF_INFO` balloon, pumps briefly so it paints
/// and lingers, then removes the icon and returns. This is the feedback channel for
/// processes with no UI of their own (the instant capture's failure note, the
/// post-update "you're now on <ver>" toast) — a modal MessageBox would be wrong there.
/// Best-effort: any failed step just means no toast, never a hang. The `linger` is how
/// long we keep pumping (the shell auto-dismisses the balloon on its own schedule).
pub(crate) unsafe fn notify_toast(title: &str, body: &str, linger: std::time::Duration) {
    use windows::Win32::UI::Shell::{
        Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIIF_INFO, NIM_ADD, NIM_DELETE, NIM_MODIFY,
        NOTIFYICONDATAW,
    };

    unsafe extern "system" fn toast_wndproc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        DefWindowProcW(h, m, w, l)
    }

    let hmod = windows::Win32::System::LibraryLoader::GetModuleHandleW(None).unwrap_or_default();
    let hinst = windows::Win32::Foundation::HINSTANCE(hmod.0);
    let class = windows::core::w!("SageThumbs2KToast");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(toast_wndproc),
        hInstance: hinst,
        lpszClassName: class,
        ..Default::default()
    };
    RegisterClassW(&wc); // ok if already registered (one-shot process)
    let Ok(hwnd) = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        class,
        windows::core::w!("st2k-toast"),
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
        uID: 0xA1,
        uFlags: NIF_ICON,
        hIcon: app_icon().unwrap_or_default(),
        ..Default::default()
    };
    let _ = Shell_NotifyIconW(NIM_ADD, &nid);

    nid.uFlags = NIF_INFO;
    nid.dwInfoFlags = NIIF_INFO;
    let t = wide(title);
    let i = wide(body);
    for (d, s) in nid.szInfoTitle.iter_mut().zip(t.iter()) {
        *d = *s;
    }
    for (d, s) in nid.szInfo.iter_mut().zip(i.iter()) {
        *d = *s;
    }
    let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);

    // Pump so the balloon paints + lingers, then clean up and return to the caller.
    let start = std::time::Instant::now();
    let mut msg = MSG::default();
    while start.elapsed() < linger {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    let _ = DestroyWindow(hwnd);
}

// ---- Small control helpers ---------------------------------------------

pub(crate) unsafe fn check(hwnd: HWND, id: i32, on: bool) {
    if let Ok(h) = GetDlgItem(Some(hwnd), id) {
        SendMessageW(h, BM_SETCHECK_MSG, Some(WPARAM(on as usize)), Some(LPARAM(0)));
    }
}
pub(crate) unsafe fn checked(hwnd: HWND, id: i32) -> bool {
    match GetDlgItem(Some(hwnd), id) {
        Ok(h) => SendMessageW(h, BM_GETCHECK_MSG, None, None).0 == BST_CHECKED,
        Err(_) => false,
    }
}

pub(crate) unsafe fn combo_sel(hwnd: HWND, id: i32) -> usize {
    GetDlgItem(Some(hwnd), id)
        .map(|c| SendMessageW(c, CB_GETCURSEL, None, None).0.max(0) as usize)
        .unwrap_or(0)
}

pub(crate) unsafe fn set_edit_text(hwnd: HWND, id: i32, text: &str) {
    if let Ok(h) = GetDlgItem(Some(hwnd), id) {
        let w = wide(text);
        let _ = SetWindowTextW(h, PCWSTR(w.as_ptr()));
    }
}

pub(crate) unsafe fn get_edit_text(hwnd: HWND, id: i32) -> String {
    let Ok(h) = GetDlgItem(Some(hwnd), id) else {
        return String::new();
    };
    let n = GetWindowTextLengthW(h);
    if n <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; n as usize + 1];
    let got = GetWindowTextW(h, &mut buf) as usize;
    String::from_utf16_lossy(&buf[..got])
}
