//! Shared owner-draw + ListView check helpers (extracted from settings_dlg; parent-hub pattern).

use super::*;

// ===================== Shared owner-draw helpers =========================
// The owner-draw painting lives in the `restyle` and `scroll` submodules and runs
// in BOTH themes — light mode is a recolored clone of dark (the palette fns like
// `SURFACE`/`BORDER`/`INPUT_BG` are theme-aware), NOT the native light dialog, so
// the entry points are NOT `is_dark()`-gated. (A few genuinely dark-only bits — e.g.
// the native dark item-view theme — still check `is_dark()` at their own call site.)
// These few small helpers stay in the parent because more than one cluster needs
// them: `s` and `fill` are used by both `restyle` and `scroll`, and `control_text` /
// `is_button_class` back the button/header painters.

/// 96-DPI design pixels → device pixels for this window's DPI.
pub(super) unsafe fn s(hwnd: HWND, v: i32) -> i32 {
    dpi_scale(hwnd, v)
}

/// Fill `rc` with a flat `color` using the stock DC brush (no allocation).
pub(super) unsafe fn fill(hdc: HDC, rc: &RECT, color: COLORREF) {
    SetDCBrushColor(hdc, color);
    FillRect(hdc, rc, HBRUSH(GetStockObject(DC_BRUSH).0));
}

/// A control's window text as a NUL-terminated wide buffer.
pub(super) unsafe fn control_text(h: HWND) -> Vec<u16> {
    let n = GetWindowTextLengthW(h).max(0) as usize;
    let mut buf = vec![0u16; n + 1];
    let got = GetWindowTextW(h, &mut buf).max(0) as usize;
    buf.truncate(got + 1);
    buf
}

/// True when `h` is a standard BUTTON-class control — so an NM_CUSTOMDRAW from it
/// is ours to paint (as opposed to, e.g., the SysLink credit, which isn't).
pub(super) unsafe fn is_button_class(h: HWND) -> bool {
    let mut buf = [0u16; 16];
    let n = GetClassNameW(h, &mut buf);
    n > 0 && String::from_utf16_lossy(&buf[..n as usize]).eq_ignore_ascii_case("button")
}

// ---- Small ListView check helpers --------------------------------------

pub(super) unsafe fn set_check(list: HWND, item: i32, on: bool) {
    let st = LVITEMW {
        state: LIST_VIEW_ITEM_STATE_FLAGS(if on { CHECKED } else { UNCHECKED }),
        stateMask: LVIS_STATEIMAGEMASK,
        ..Default::default()
    };
    SendMessageW(
        list,
        LVM_SETITEMSTATE,
        Some(WPARAM(item as usize)),
        Some(LPARAM(&st as *const _ as isize)),
    );
}
/// Remove a row's checkbox glyph (state image 0) — used for the menu list's divider rows.
pub(super) unsafe fn clear_checkbox(list: HWND, item: i32) {
    let st = LVITEMW {
        state: LIST_VIEW_ITEM_STATE_FLAGS(0),
        stateMask: LVIS_STATEIMAGEMASK,
        ..Default::default()
    };
    SendMessageW(
        list,
        LVM_SETITEMSTATE,
        Some(WPARAM(item as usize)),
        Some(LPARAM(&st as *const _ as isize)),
    );
}
pub(super) unsafe fn is_checked(list: HWND, item: i32) -> bool {
    let st = SendMessageW(
        list,
        LVM_GETITEMSTATE,
        Some(WPARAM(item as usize)),
        Some(LPARAM(LVIS_STATEIMAGEMASK.0 as isize)),
    );
    (st.0 as u32 & 0x3000) == CHECKED
}

