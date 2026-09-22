//! The big `build_controls` — creates every dialog control (extracted from settings_dlg).

use super::*;
mod left;
use left::*;
mod right;
use right::*;
mod strips;
use strips::*;
mod licence;
use licence::*;

/// The three control styles every section of `build_controls` shares.
struct Styles {
    cb: WINDOW_STYLE,
    edit_style: WINDOW_STYLE,
    hdr: WINDOW_STYLE,
}

impl Styles {
    fn new() -> Self {
        let cb = WINDOW_STYLE(BS_AUTOCHECKBOX as u32) | WS_TABSTOP;
        // Borderless, right-aligned number fields in both themes (a rounded field
        // frame is drawn behind them in WM_PAINT regardless of theme).
        let edit_style = WINDOW_STYLE((ES_NUMBER | ES_AUTOHSCROLL | ES_RIGHT) as u32) | WS_TABSTOP;
        // Section headers always owner-draw (uppercase label + hairline divider) in
        // both themes; a localized '&' (e.g. "Limits & quality") isn't eaten as a
        // mnemonic because draw_section_header passes DT_NOPREFIX. The width is
        // widened so the divider runs to the column edge.
        let hdr = WINDOW_STYLE(SS_OWNERDRAW);
        Styles {
            cb,
            edit_style,
            hdr,
        }
    }
}

pub(super) unsafe fn build_controls(hwnd: HWND, hinst: HINSTANCE) {
    let sty = Styles::new();
    let mut lc = LeftCol::new(hwnd, hinst);
    build_thumbnails(hwnd, &mut lc, &sty);
    build_general(&mut lc, &sty);
    build_menu_items(hwnd, &mut lc, &sty);
    build_screenshots(hwnd, &mut lc, &sty);
    build_diagnostics(&mut lc, &sty);
    build_sync(&mut lc, &sty);
    build_quick_preview(&mut lc, &sty);
    build_file_types(hwnd, hinst, &sty);
    build_scrollbar(hwnd, hinst);
    let layout = sponsor_layout(is_dark());
    build_sponsor(hwnd, hinst);
    build_signin_banner(hwnd, hinst);
    build_business_nag(hwnd, hinst);
    build_bottom_row(hwnd, hinst, &sty, &layout);
    build_licence_page(hwnd, hinst, &sty);
}

/// Format a hotkey chord that isn't one of the curated [`SHOT_PRESETS`] — e.g. a
/// value an older preset list offered and has since dropped, or one written by
/// hand into the registry — so Save can round-trip it instead of silently
/// replacing it with preset 0. Deliberately plain (modifier names + a raw VK
/// hex byte), not a friendly key name: this is a recovery display for values
/// outside the curated list, not worth a `GetKeyNameTextW` round trip for.
fn describe_unknown_chord(packed: u32) -> String {
    let hkf = (packed >> 8) & 0xFF;
    let vk = packed & 0xFF;
    let mut mods = Vec::new();
    if hkf & 0x02 != 0 {
        mods.push("Ctrl");
    }
    if hkf & 0x01 != 0 {
        mods.push("Shift");
    }
    if hkf & 0x04 != 0 {
        mods.push("Alt");
    }
    if mods.is_empty() {
        format!("Custom (VK 0x{vk:02X})")
    } else {
        format!("Custom ({} + VK 0x{vk:02X})", mods.join(" + "))
    }
}

/// Append one combo item for a chord outside the curated list, with its packed
/// value stashed as the item's data (read back at Save via `CB_GETITEMDATA`
/// instead of re-deriving it from position). Returns the new item's index.
pub(super) unsafe fn append_unknown_chord_item(combo: HWND, packed: u32) -> usize {
    let label = wide(&describe_unknown_chord(packed));
    let idx = SendMessageW(
        combo,
        CB_ADDSTRING,
        None,
        Some(LPARAM(label.as_ptr() as isize)),
    )
    .0;
    SendMessageW(
        combo,
        CB_SETITEMDATA,
        Some(WPARAM(idx as usize)),
        Some(LPARAM(packed as isize)),
    );
    idx as usize
}

/// Decide which combo index a stored chord should select: a curated preset's
/// index, `default_when_unset` when nothing is genuinely saved yet (`current ==
/// 0`), or `None` when `current` is a real value that just isn't in the curated
/// list — the caller must then append a dedicated item for it rather than
/// falling back to a default. This is the exact decision the original bug got
/// wrong (`SHOT_PRESETS.position(...).unwrap_or(0)` treated "unknown" and
/// "unset" as the same thing, both collapsing to preset 0).
fn preset_index_for(current: u32, default_when_unset: usize) -> Option<usize> {
    if current == 0 {
        return Some(default_when_unset);
    }
    SHOT_PRESETS.iter().position(|&(_, p)| p == current)
}

/// Populate a hotkey combo with the curated [`SHOT_PRESETS`] (each item's data =
/// its packed chord), append a trailing item for `current` when it's a real
/// (non-zero) chord absent from that list, and return the index to select —
/// `default_when_unset` when `current` is 0 (a combo-specific "nothing saved
/// yet" default; see callers). Save-time code reads the selection back with
/// `CB_GETITEMDATA`, so the appended item round-trips exactly like a curated
/// one instead of collapsing to preset 0 on the next Save (the bug this fixes).
unsafe fn populate_hotkey_presets(combo: HWND, current: u32, default_when_unset: usize) -> usize {
    for &(label, packed) in SHOT_PRESETS {
        let w = wide(label);
        let idx = SendMessageW(combo, CB_ADDSTRING, None, Some(LPARAM(w.as_ptr() as isize))).0;
        SendMessageW(
            combo,
            CB_SETITEMDATA,
            Some(WPARAM(idx as usize)),
            Some(LPARAM(packed as isize)),
        );
    }
    match preset_index_for(current, default_when_unset) {
        Some(idx) => idx,
        None => append_unknown_chord_item(combo, current),
    }
}

#[cfg(test)]
mod tests;
