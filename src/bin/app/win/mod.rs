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
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    DeleteObject, DrawTextW, FillRect, GetDC, GetTextExtentPoint32W, ReleaseDC, SelectObject,
    SetBkMode, SetTextColor, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DT_CALCRECT, DT_LEFT,
    DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, DT_WORDBREAK, HBITMAP, HFONT, HGDIOBJ, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{DRAWITEMSTRUCT, MEASUREITEMSTRUCT, NMLINK, ODS_SELECTED};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetActiveWindow, SetFocus};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::*;

use st2k_base::i18n;
mod dacl;
mod dialogs;
mod iconfont;
mod pickers;
mod resultwin;
mod scaling;
mod shotwin;
mod textmetrics;
mod toast;
pub(crate) use dacl::{create_mutex_user_only, with_user_only_dacl};
pub(crate) use dialogs::{confirm_verbs, confirm_warning, dialog_tail, message_box, run_dialog};
pub(crate) use iconfont::icon_font;
pub(crate) use pickers::{
    desktop_dir, pick_folder, pick_open_file, pick_open_settings, pick_save_png,
    pick_save_settings, set_clipboard_text,
};
pub(crate) use resultwin::{
    result_buttons, result_edit, result_layout, result_window_proc, result_wndproc, ResultWindow,
};
pub(crate) use scaling::{
    dpi_override, dpi_scale, dpi_scale_dpi, dpi_unscale, gui_font, gui_font_for, gui_font_header,
    gui_font_sized, gui_font_title, set_dpi_override, wm_dpichanged,
};
pub(crate) use shotwin::{
    capture_and_destroy, capture_shot_window, create_shot_window, force_foreground, force_repaint,
    pump_msgs, settle_pump, ShotWindowSpec,
};
#[cfg(test)]
pub(crate) use textmetrics::{design_text_w, design_wrapped_text_h};
pub(crate) use textmetrics::{text_width, wrapped_text_h};
pub(crate) use toast::{notify_toast, notify_toast_action};

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

/// A top-down 32-bpp `BITMAPINFO` for a `w`×`h` canvas — the header every DIB path in this app
/// wants (`CreateDIBSection`, `GetDIBits`, `StretchDIBits`), with the negative `biHeight` that
/// means top-down rows and `BI_RGB` that means packed BGRA, no colour table.
pub(crate) fn top_down_bgra_bmi(w: i32, h: i32) -> BITMAPINFO {
    BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: core::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    }
}

/// The two halves of a `WM_COMMAND` `wParam`: the control/menu id (low word) and the
/// notification code (high word). Every dialog used to unpack them by hand.
pub(crate) fn command_parts(wparam: WPARAM) -> (i32, u32) {
    (
        (wparam.0 & 0xFFFF) as i32,
        ((wparam.0 >> 16) & 0xFFFF) as u32,
    )
}

/// The control/menu id of a `WM_COMMAND` (`wParam`'s low word).
pub(crate) fn command_id(wparam: WPARAM) -> i32 {
    command_parts(wparam).0
}

pub(crate) fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Read a WinInet request handle to EOF, capped at `max_bytes`. Returns the FULL
/// body, or `None` on a read error, an over-cap response, or an expired `deadline` — never a
/// truncated body. Both remote clients (the sponsor
/// GET in `sponsors.rs` and the screenshot POST in `screenshot/upload.rs`) parse/decode the
/// result, so partial bytes must not be handed back looking like success. Shared so the
/// read loop and the over-cap policy live in exactly one place (the POST path used to
/// return the truncated body on over-cap — a corrupt URL; this fixes it for both).
///
/// `deadline` bounds the WHOLE read, checked before every single `InternetReadFile` call —
/// not just once up front. That is what actually stops a slow trickle:
/// `INTERNET_OPTION_RECEIVE_TIMEOUT` only bounds the gap BETWEEN reads and resets on every
/// partial one, so it never fires for a server that keeps sending a few bytes just often
/// enough to stay under it (this is `http.rs::drain`'s documented reason for forking this
/// function in the first place — folded back in here so both callers share one deadline
/// check). `on_progress`, when given, is called after every read with the bytes read SO FAR
/// (a progress readout only — this helper has no cancel-callback contract; a caller that
/// wants to abort mid-read does so through `deadline`).
pub(crate) unsafe fn wininet_drain(
    req: *mut c_void,
    max_bytes: usize,
    deadline: Option<std::time::Instant>,
    mut on_progress: Option<&mut dyn FnMut(usize)>,
) -> Option<Vec<u8>> {
    use windows::Win32::Networking::WinInet::InternetReadFile;
    let mut data = Vec::new();
    let mut buf = [0u8; 16384];
    loop {
        // Checked before EVERY read, not just once up front — see the doc above for why
        // that's what actually bounds a slow trickle.
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            return None; // wall-clock deadline expired → reject (no truncated bodies)
        }
        let mut read = 0u32;
        if InternetReadFile(
            req,
            buf.as_mut_ptr() as *mut c_void,
            buf.len() as u32,
            &mut read,
        )
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
        if let Some(cb) = on_progress.as_deref_mut() {
            cb(data.len());
        }
    }
    Some(data)
}

/// Declares the `copy_source` a verbatim-report dialog hands to [`result_wndproc`]: the whole of
/// its per-thread `report` text, which is what the Copy button puts on the clipboard. The
/// dialogs that copy a stored report verbatim are identical here but for which thread-local
/// holds that text, so the macro takes the caller's own name for it.
macro_rules! report_copy_source {
    ($report:ident) => {
        unsafe fn copy_source(_hwnd: windows::Win32::Foundation::HWND) -> String {
            $report.with(|r| r.borrow().clone())
        }
    };
}
pub(crate) use report_copy_source;

// ===== Headless capture plumbing (the `--shot*` verification/asset modes) =====

/// Effective DPI + work-area rect of the monitor under the cursor (where the user is).
/// A top-level window sizes AND positions itself for the monitor it actually opens on,
/// so the window frame's DPI matches the per-control `dpi_scale()` (`GetDpiForWindow`) —
/// even on a mixed-DPI multi-monitor setup, or after the user changed scale without
/// signing out. This replaced a `dpi_for_system()` helper that read the LOGIN-time primary
/// DPI: wrong in both those cases, and it left the fixed-size v3 Settings window clipping
/// its controls. 96/primary fallback on any failure.
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

/// A plain caption in a dialog: a static with no style bits and no id, at design-pixel
/// `x, y, w, h`. The dialogs' most common control, so its ten-argument `ctl` call is spelled
/// once here.
pub(crate) unsafe fn label(
    hwnd: HWND,
    hinst: HINSTANCE,
    text: &str,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) -> HWND {
    ctl(hwnd, STATIC, text, WINDOW_STYLE(0), x, y, w, h, -1, hinst)
}

/// A single-line text field in a dialog: bordered, tab-stop, scrolling horizontally as the
/// user types past its width, holding `text` initially.
#[allow(clippy::too_many_arguments)] // eight of `ctl`'s ten: the rect and the id ARE the call
pub(crate) unsafe fn edit_field(
    hwnd: HWND,
    hinst: HINSTANCE,
    text: &str,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    id: i32,
) -> HWND {
    let style = WINDOW_STYLE(ES_AUTOHSCROLL as u32) | WS_BORDER | WS_TABSTOP;
    ctl(hwnd, EDIT, text, style, x, y, w, h, id, hinst)
}

/// Register a top-level app window class: the app icon, the arrow cursor, and the palette's
/// window tone as the background in BOTH themes (light mode used to take the system
/// button-face brush here while every control filled with the palette's 243, so each row
/// showed as a lighter block on the pane; one source, one colour). Idempotent: a second
/// registration of the same name returns 0, which is fine.
pub(crate) unsafe fn register_app_class(class: PCWSTR, wndproc: WNDPROC, hinst: HINSTANCE) {
    let wc = WNDCLASSW {
        lpfnWndProc: wndproc,
        hInstance: hinst,
        lpszClassName: class,
        hIcon: app_icon().unwrap_or_default(),
        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
        hbrBackground: crate::dark::dark_bg_brush(),
        ..Default::default()
    };
    RegisterClassW(&wc);
}

/// Create a `tooltips_class32` control owned by `parent`: `TTS_ALWAYSTIP` so it shows without the
/// parent being active and `TTS_NOPREFIX` so an `&` in a hint stays literal. `None` when the class
/// could not be created, so each caller keeps its own failure policy.
pub(crate) unsafe fn create_tooltip_window(parent: HWND, hinst: HINSTANCE) -> Option<HWND> {
    CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("tooltips_class32"),
        PCWSTR::null(),
        WS_POPUP | WINDOW_STYLE(TTS_ALWAYSTIP | TTS_NOPREFIX),
        0,
        0,
        0,
        0,
        Some(parent),
        None,
        Some(hinst),
        None,
    )
    .ok()
}

/// The measuring half of an owner-drawn dark menu (WM_MEASUREITEM for an `ODT_MENU` item):
/// the label's width in the GUI font plus the 14px indent and the padding [`draw_menu_item`]
/// uses, 26px tall.
pub(crate) unsafe fn measure_menu_item(hwnd: HWND, m: &mut MEASUREITEMSTRUCT, label: &str) {
    let label = wide(label);
    let n = label.len().saturating_sub(1);
    let hdc = GetDC(Some(hwnd));
    let old = SelectObject(hdc, HGDIOBJ(gui_font().0));
    let mut sz = SIZE::default();
    let _ = GetTextExtentPoint32W(hdc, &label[..n], &mut sz);
    SelectObject(hdc, old);
    ReleaseDC(Some(hwnd), hdc);
    m.itemWidth = (sz.cx + 30) as u32;
    m.itemHeight = 26;
}

/// The drawing half (WM_DRAWITEM for an `ODT_MENU` item): the dark or selected fill, then
/// the label 14px in, vertically centred, in the GUI font.
pub(crate) unsafe fn draw_menu_item(d: &DRAWITEMSTRUCT, label: &str) {
    let selected = (d.itemState.0 & ODS_SELECTED.0) != 0;
    let bg = if selected {
        crate::dark::dark_menu_sel_brush()
    } else {
        crate::dark::dark_menu_brush()
    };
    FillRect(d.hDC, &d.rcItem, bg);
    SetBkMode(d.hDC, TRANSPARENT);
    SetTextColor(d.hDC, crate::dark::DARK_TEXT());
    SelectObject(d.hDC, HGDIOBJ(gui_font().0));
    let mut label = wide(label);
    let n = label.len().saturating_sub(1);
    let mut rc = d.rcItem;
    rc.left += 14;
    DrawTextW(
        d.hDC,
        &mut label[..n],
        &mut rc,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );
}

/// The plain top-level pump: translate + dispatch until `WM_DESTROY` posts `WM_QUIT` (a
/// `GetMessageW` of 0) or the queue dies (-1). No `IsDialogMessageW` here, unlike
/// [`pump_until_quit`]: the preview, capture overlay, eyedropper and hotkey daemon windows
/// handle every key in their own procedure, and dialog translation would eat Tab, Esc and the
/// arrows before they got there.
pub(crate) unsafe fn pump_plain() {
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

/// Pull one message for `hwnd`'s dialog pump off the queue and dispatch it, translating first
/// unless `IsDialogMessageW` consumed it. `false` means `GetMessageW` reported WM_QUIT (0) or a
/// destroyed queue (-1), which is the caller's cue to stop pumping.
unsafe fn pump_one(hwnd: HWND, msg: &mut MSG) -> bool {
    let r = GetMessageW(msg, None, 0, 0).0;
    if r == 0 || r == -1 {
        return false;
    }
    if !IsDialogMessageW(hwnd, msg).as_bool() {
        let _ = TranslateMessage(msg);
        DispatchMessageW(msg);
    }
    true
}

/// Standard top-level pump: dialog-key translation + dispatch until WM_QUIT. Branches on
/// `GetMessageW`'s raw value: `as_bool()` (`!= 0`) would treat the -1 of a destroyed queue
/// as "keep going" and then spin on a MSG it never populated.
pub(crate) unsafe fn pump_until_quit(hwnd: HWND) {
    let mut msg = MSG::default();
    while pump_one(hwnd, &mut msg) {}
}

/// Modal pump: runs until `hwnd` destroys itself (the popup uses no
/// PostQuitMessage, which would otherwise kill the parent dialog's loop).
unsafe fn pump_until_closed(hwnd: HWND) {
    let mut msg = MSG::default();
    while IsWindow(Some(hwnd)).as_bool() {
        if !pump_one(hwnd, &mut msg) {
            break;
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
    SendMessageW(
        h,
        WM_SETFONT,
        Some(WPARAM(gui_font_for(parent).0 as usize)),
        Some(LPARAM(1)),
    );
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
    let _ = ShellExecuteW(
        None,
        w!("open"),
        PCWSTR(u.as_ptr()),
        PCWSTR::null(),
        PCWSTR::null(),
        SW_SHOWNORMAL,
    );
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

/// Open the `NMLINK` a `WM_NOTIFY`'s `NM_CLICK` / `NM_RETURN` carries — the SysLink
/// click every dialog with a rendered link shares. `link` must be the `lparam` of that
/// notification, cast to `*const NMLINK`.
pub(crate) unsafe fn open_notify_link(link: *const NMLINK) {
    let url = wstr_to_string(&(*link).item.szUrl);
    if !url.is_empty() {
        open_url(&url);
    }
}

/// Decode logo/banner artwork to an HBITMAP sized to `w`x`h`. Prefers a file of
/// `override_name` next to the EXE (user-swappable) and falls back to the
/// embedded `default_png`.
pub(crate) unsafe fn load_art(
    default_png: &[u8],
    override_name: &str,
    w: u32,
    h: u32,
) -> Option<HBITMAP> {
    let from_file = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(override_name)))
        .and_then(|f| std::fs::read(f).ok());
    let data = from_file.as_deref().unwrap_or(default_png);
    sagethumbs2k_core::app_image::image_to_hbitmap_sized(data, w, h)
        .map(|h| HBITMAP(h as *mut c_void))
}

/// How many pid-suffixed names to try before giving up on the temp-icon fallback (mirrors
/// `decode::magick::NamedTemp`'s `MAX_STAGE_ATTEMPTS`). The counter alone already makes a
/// collision improbable; the loop exists so `create_new` can't turn one squatted name into
/// a permanent "no icon" for the whole process.
const MAX_ICON_TEMP_ATTEMPTS: u32 = 8;

/// Claim a pid-suffixed `%TEMP%` path EXCLUSIVELY and fill it with the embedded icon, or
/// give up. `create_new`, never `std::fs::write`: write/truncate follows hard links and
/// reparse points, so a name pre-planted in `%TEMP%` (a fixed `sagethumbs2k.ico` used to be
/// exactly that — predictable and shared by every process) would have our icon bytes
/// written straight THROUGH it into whatever it really points at. The pid+counter suffix
/// already makes the name unpredictable across processes; `create_new` refusing an existing
/// name (reparse point or not) is the actual guard.
fn claim_icon_temp_file() -> Option<std::path::PathBuf> {
    use std::io::Write;
    let dir = std::env::temp_dir();
    let pid = std::process::id();
    for n in 0..MAX_ICON_TEMP_ATTEMPTS {
        let path = dir.join(format!("sagethumbs2k-{pid}-{n}.ico"));
        let Ok(mut f) = std::fs::File::options()
            .write(true)
            .create_new(true)
            .open(&path)
        else {
            continue;
        };
        if f.write_all(APP_ICO).is_ok() {
            drop(f); // close before LoadImageW opens the same path
            return Some(path);
        }
        drop(f);
        let _ = std::fs::remove_file(&path); // partial write — don't leave it claimed
    }
    None
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
        let from_temp = beside.is_none();
        let Some(path) = beside.or_else(claim_icon_temp_file) else {
            return 0;
        };
        let w = wide(&path.to_string_lossy());
        let hicon = match LoadImageW(
            None,
            PCWSTR(w.as_ptr()),
            IMAGE_ICON,
            0,
            0,
            LR_LOADFROMFILE | LR_DEFAULTSIZE,
        ) {
            Ok(h) => h.0 as usize,
            Err(_) => 0,
        };
        if from_temp {
            // LoadImageW read the file synchronously above, so the temp copy has done its
            // job and can go now instead of lingering in %TEMP% for the session.
            let _ = std::fs::remove_file(&path);
        }
        hicon
    });
    (p != 0).then_some(HICON(p as *mut c_void))
}

#[cfg(test)]
mod icon_temp_tests;

/// Set a static control's bitmap, freeing whatever bitmap it held before.
pub(crate) unsafe fn set_static_bitmap(ctl: HWND, hbmp: HBITMAP) {
    let old = SendMessageW(
        ctl,
        STM_SETIMAGE,
        Some(WPARAM(IMAGE_BITMAP.0 as usize)),
        Some(LPARAM(hbmp.0 as isize)),
    );
    if old.0 != 0 {
        let _ = DeleteObject(HGDIOBJ(old.0 as *mut c_void));
    }
}

// ---- Pinned 96-DPI measurement, for layout tests only -------------------------------
//
// [`text_width`] and [`wrapped_text_h`] deliberately follow the headless-shot DPI override,
// which is a PROCESS-WIDE global. `cargo test` runs the whole binary's tests in one process
// on parallel threads, and `scaling.rs`'s own override test flips that global while a
// locale sweep is measuring, so a sweep that passed alone failed in the full suite with a
// geometry number nothing in its own code had changed. These two pin the design-scale
// [`gui_font`] instead, which no test mutates, and return its pixels unscaled.

/// Copy `s` into `dst` as UTF-16, truncated to fit, always leaving a terminating NUL. A
/// `zip`-based copy into a fixed-size `WCHAR` field just stops at whichever of `dst`/`s` is
/// shorter — if `s` is longer than `dst`, no NUL ever lands inside the buffer, and a reader
/// like `Shell_NotifyIconW` walks past the intended text into whatever struct bytes follow
/// (garbled toast text; the fields are contiguous and in-bounds, so not a memory-safety bug,
/// just a display one).
fn copy_wide_capped(dst: &mut [u16], s: &str) {
    let Some(cap) = dst.len().checked_sub(1) else {
        return; // a zero-length field has nowhere to put even the terminator
    };
    let mut n = 0;
    for (d, c) in dst.iter_mut().zip(s.encode_utf16().take(cap)) {
        *d = c;
        n += 1;
    }
    dst[n] = 0;
}

// ---- Small control helpers ---------------------------------------------

pub(crate) unsafe fn check(hwnd: HWND, id: i32, on: bool) {
    if let Ok(h) = GetDlgItem(Some(hwnd), id) {
        SendMessageW(
            h,
            BM_SETCHECK_MSG,
            Some(WPARAM(on as usize)),
            Some(LPARAM(0)),
        );
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
