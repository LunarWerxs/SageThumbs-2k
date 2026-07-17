//! Fallback "info card" for files the viewer can't render as an image (and for folders):
//! the shell icon + the file name + a modified-date / size line. Never an error box — a
//! calm card is the graceful degradation (plan §2).

use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    CreateSolidBrush, DeleteObject, FillRect, SelectObject, SetBkMode, SetTextColor,
    DT_END_ELLIPSIS, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, HDC, TRANSPARENT,
};

use super::paint::draw_text;
use windows::Win32::UI::Shell::{SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON};
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, DrawIconEx, DI_NORMAL, HICON};

/// The data shown on the card. Owns the shell `HICON` (destroyed on drop).
pub(super) struct InfoCard {
    name: String,
    detail: String,
    icon: Option<HICON>,
}

impl InfoCard {
    /// The card's visible text (name + detail line) — what the viewer's Ctrl+C copies.
    pub(super) fn copy_text(&self) -> String {
        format!("{}\r\n{}", self.name, self.detail)
    }
}

impl Drop for InfoCard {
    fn drop(&mut self) {
        if let Some(icon) = self.icon {
            unsafe {
                let _ = DestroyIcon(icon);
            }
        }
    }
}

/// Gather the card for `path`: the shell's large icon, the leaf name, and a one-line
/// detail (modified date + size for a file, item count for a folder).
pub(super) unsafe fn gather(path: &str) -> InfoCard {
    let p = std::path::Path::new(path);
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or(path).to_string();
    let icon = shell_icon(path);
    let detail = if p.is_dir() {
        let count = std::fs::read_dir(p).map(|it| it.count()).unwrap_or(0);
        match modified_string(path) {
            Some(w) => format!("{count} items  ·  {w}"),
            None => format!("{count} items"),
        }
    } else {
        let sz = human_size(std::fs::metadata(p).map(|m| m.len()).unwrap_or(0));
        match modified_string(path) {
            Some(w) => format!("{sz}  ·  {w}"),
            None => sz,
        }
    };
    InfoCard { name, detail, icon }
}

/// Paint the card centered in `rc`: bg fill, then a left-aligned icon + name/detail text
/// block, centered vertically. Colours come from the caller's resolved dark/light palette.
pub(super) unsafe fn paint(
    hwnd: HWND,
    hdc: HDC,
    rc: &RECT,
    card: &InfoCard,
    bg: u32,
    text: u32,
    subtle: u32,
) {
    let brush = CreateSolidBrush(COLORREF(bg));
    FillRect(hdc, rc, brush);
    let _ = DeleteObject(brush.into());

    let sc = |v: i32| crate::win::dpi_scale(hwnd, v);
    let cw = rc.right - rc.left;
    let ch = rc.bottom - rc.top;
    let icon_sz = sc(48);
    let gap = sc(16);
    let text_w = sc(300);
    let block_w = icon_sz + gap + text_w;
    let x0 = rc.left + (cw - block_w).max(0) / 2;
    let icon_y = rc.top + (ch - icon_sz).max(0) / 2;

    if let Some(icon) = card.icon {
        let _ = DrawIconEx(hdc, x0, icon_y, icon, icon_sz, icon_sz, 0, None, DI_NORMAL);
    }

    SetBkMode(hdc, TRANSPARENT);
    let tx = x0 + icon_sz + gap;
    let line_h = sc(22);
    let name_top = rc.top + (ch - line_h * 2).max(0) / 2;
    let fmt = DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS;

    let name_font = crate::win::gui_font_sized(hwnd, 15, 600);
    let oldf = SelectObject(hdc, name_font.into());
    SetTextColor(hdc, COLORREF(text));
    let mut name_rc = RECT { left: tx, top: name_top, right: tx + text_w, bottom: name_top + line_h };
    let mut name_w: Vec<u16> = card.name.encode_utf16().collect();
    draw_text(hdc, &mut name_w, &mut name_rc, fmt);

    let det_font = crate::win::gui_font_sized(hwnd, 12, 400);
    SelectObject(hdc, det_font.into());
    SetTextColor(hdc, COLORREF(subtle));
    let mut det_rc =
        RECT { left: tx, top: name_top + line_h, right: tx + text_w, bottom: name_top + line_h * 2 };
    let mut det_w: Vec<u16> = card.detail.encode_utf16().collect();
    draw_text(hdc, &mut det_w, &mut det_rc, fmt);

    SelectObject(hdc, oldf);
}

/// The shell's large icon for `path` (via `SHGetFileInfoW`). Caller-owned `HICON`.
unsafe fn shell_icon(path: &str) -> Option<HICON> {
    let wide = crate::win::wide(path);
    let mut sfi = SHFILEINFOW::default();
    let r = SHGetFileInfoW(
        PCWSTR(wide.as_ptr()),
        windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES(0),
        Some(&mut sfi),
        core::mem::size_of::<SHFILEINFOW>() as u32,
        SHGFI_ICON | SHGFI_LARGEICON,
    );
    if r == 0 || sfi.hIcon.is_invalid() {
        None
    } else {
        Some(sfi.hIcon)
    }
}

/// "1.2 MB"-style size string.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["bytes", "KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} bytes");
    }
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    format!("{v:.1} {}", UNITS[u])
}

/// The file's last-modified time as "YYYY-MM-DD HH:MM" in local time. `None` if the file
/// can't be stat'd.
unsafe fn modified_string(path: &str) -> Option<String> {
    use windows::Win32::Foundation::SYSTEMTIME;
    use windows::Win32::Storage::FileSystem::{
        GetFileAttributesExW, GetFileExInfoStandard, WIN32_FILE_ATTRIBUTE_DATA,
    };
    use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};

    let wide = crate::win::wide(path);
    let mut data = WIN32_FILE_ATTRIBUTE_DATA::default();
    GetFileAttributesExW(
        PCWSTR(wide.as_ptr()),
        GetFileExInfoStandard,
        &mut data as *mut _ as *mut core::ffi::c_void,
    )
    .ok()?;
    let mut utc = SYSTEMTIME::default();
    FileTimeToSystemTime(&data.ftLastWriteTime, &mut utc).ok()?;
    // Convert UTC → the machine's current local time zone (None = active TZ).
    let mut st = SYSTEMTIME::default();
    if SystemTimeToTzSpecificLocalTime(None, &utc, &mut st).is_err() {
        st = utc; // fall back to UTC if the conversion fails
    }
    Some(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute
    ))
}
