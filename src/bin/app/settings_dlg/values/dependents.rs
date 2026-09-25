//! Controls that only make sense while another is on: the dependency tables and the greying they drive.

use super::*;

/// Enable the quick-save hotkey picker (its label + combo) only while the "instant
/// screenshot" checkbox is on — mirrors how the feature is gated at save time, so
/// the greyed-out combo can't imply an active second hotkey.
pub(in super::super) unsafe fn update_quick_enabled(hwnd: HWND) {
    let on = checked(hwnd, ID_SHOT_QUICK_ENABLE);
    // Disable only the COMBO — it custom-draws a clean grey. The LABEL stays ENABLED
    // (a disabled static renders an etched/blurry look in dark mode) and is greyed via
    // its WM_CTLCOLORSTATIC handler instead; invalidate it so the colour repaints now.
    if let Ok(c) = GetDlgItem(Some(hwnd), ID_SHOT_QUICK_HOTKEY) {
        let _ = windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow(c, on);
    }
    if let Ok(lbl) = GetDlgItem(Some(hwnd), ID_LBL_SHOT_QUICK_HK) {
        let _ = InvalidateRect(Some(lbl), None, true);
    }
}

/// Gate the custom-action combos by the "Enable custom action" toggle. When off,
/// force its hotkey combo to "(none)" (so Save writes it unbound) and grey both.
pub(in super::super) unsafe fn update_custom_action_enabled(hwnd: HWND) {
    let on = checked(hwnd, ID_CUSTOM_ACTION_ENABLE);
    if let Ok(c) = GetDlgItem(Some(hwnd), ID_SHOT_ACTION) {
        let _ = windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow(c, on);
    }
    if let Ok(c) = GetDlgItem(Some(hwnd), ID_SHOT_ACTION_HK) {
        if !on {
            SendMessageW(c, CB_SETCURSEL, Some(WPARAM(0)), None); // "(none)" — unbound
        }
        let _ = windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow(c, on);
    }
}

/// Grey the "Set save folder…" button + the folder display while the "Save to a set
/// folder on Ctrl+S" toggle is OFF (the folder only matters when auto-save is on). The
/// button custom-draws a clean grey when disabled; the display (a static) is dimmed via
/// its WM_CTLCOLORSTATIC handler, so just invalidate it to repaint.
pub(in super::super) unsafe fn update_save_dir_enabled(hwnd: HWND) {
    let on = checked(hwnd, ID_SHOT_USE_DIR);
    if let Ok(b) = GetDlgItem(Some(hwnd), ID_SHOT_SET_DIR) {
        let _ = windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow(b, on);
    }
    if let Ok(lbl) = GetDlgItem(Some(hwnd), ID_SHOT_DIR) {
        let _ = InvalidateRect(Some(lbl), None, true);
    }
}

/// Every switch whose meaning depends on another switch being ON, as (parent, children)
/// pairs. Children are drawn indented (see `apply_v3_layout`) and greyed while the parent
/// is off — the pattern first-run already uses for the PrtScn row. This is what turns a
/// wall of equal-looking checkboxes back into the 3-4 real decisions each page contains:
/// a page shows its hierarchy instead of asking the user to infer it from the tooltips.
pub(in super::super) const DEPENDENT_SWITCHES: &[(i32, &[i32])] = &[
    (
        ID_ENABLE_MENU,
        &[ID_MENU_ALL_TYPES, ID_MENU_QUICK, ID_MENU_CHECKER],
    ),
    (ID_SHOT_ENABLE, &[ID_SHOT_QUICK_ENABLE, ID_SHOT_USE_DIR]),
    #[cfg(feature = "html-preview")]
    (
        ID_PREVIEW_ENABLED,
        &[
            ID_PREVIEW_HOLD_PEEK,
            ID_PREVIEW_CLOSE_FOCUS,
            ID_PREVIEW_TOPMOST,
            ID_PREVIEW_BLOCKED_EXTS,
            ID_PREVIEW_TEXT,
            ID_PREVIEW_MARKDOWN,
            ID_PREVIEW_HTML,
            ID_PREVIEW_URL_LIVE,
        ],
    ),
    #[cfg(not(feature = "html-preview"))]
    (
        ID_PREVIEW_ENABLED,
        &[
            ID_PREVIEW_HOLD_PEEK,
            ID_PREVIEW_CLOSE_FOCUS,
            ID_PREVIEW_TOPMOST,
            ID_PREVIEW_BLOCKED_EXTS,
            ID_PREVIEW_TEXT,
            ID_PREVIEW_MARKDOWN,
        ],
    ),
];

/// The same idea for a parent that is a COMBO rather than a checkbox, as
/// (combo, the selection index that enables the children, children).
///
/// The badge STYLE row is the case that needed it: its parent stopped being "is the badge on"
/// and became "which of three things is in the corner", and only one of those three has a style
/// to pick. Encoding it as an index rather than a bool keeps this table honest about that —
/// there is no "on" to test, only a specific answer.
pub(in super::super) const DEPENDENT_ON_COMBO: &[(i32, u32, &[i32])] = &[(
    ID_CORNER_MARK,
    st2k_base::settings::CornerMark::Badge.as_dword(),
    // The size COMBO joins it for the same reason. Its LABEL is deliberately not here: a
    // disabled static draws etched (strikethrough-looking) in dark mode, which is how the
    // Appearance page read as "failed to render" in 3.1.0. The label stays enabled and
    // `special_ctlcolor` paints it dim off [`badge_size_active`] instead - the rule the
    // Quick-save hotkey label already follows - and `sync_dependent_switches` repaints it, so
    // the row still greys as one.
    &[ID_BADGE_ICON, ID_BADGE_SIZE],
)];

/// Is the corner-mark combo on the one answer that has a size and a style to pick? The two
/// dependent CONTROLS read this through [`DEPENDENT_ON_COMBO`]; the row's label reads it from
/// `special_ctlcolor`, where it is dimmed by paint rather than disabled (see the table's note).
pub(in super::super) unsafe fn badge_size_active(hwnd: HWND) -> bool {
    combo_sel(hwnd, ID_CORNER_MARK, 2) as u32 == st2k_base::settings::CornerMark::Badge.as_dword()
}

/// Is `id` a dependent (child) switch? The layout indents these.
pub(in super::super) fn is_dependent_switch(id: i32) -> bool {
    DEPENDENT_SWITCHES
        .iter()
        .any(|(_, kids)| kids.contains(&id))
        || DEPENDENT_ON_COMBO
            .iter()
            .any(|(_, _, kids)| kids.contains(&id))
}

/// Grey every dependent switch whose parent is off (and un-grey when it comes back on).
/// Runs on load and whenever a parent switch is clicked. Greying is DISPLAY only — the
/// stored setting keeps its value, so toggling a parent off and on loses nothing.
pub(in super::super) unsafe fn sync_dependent_switches(hwnd: HWND) {
    for &(parent, kids) in DEPENDENT_SWITCHES {
        grey_kids(hwnd, kids, checked(hwnd, parent));
    }
    for &(combo, wants, kids) in DEPENDENT_ON_COMBO {
        grey_kids(hwnd, kids, combo_sel(hwnd, combo, 2) as u32 == wants);
    }
    // The captions dimmed by paint, not disabled (see `DEPENDENT_ON_COMBO` and
    // `dimmed_caption`), have to be asked to repaint here or they keep the colour of the
    // previous state.
    for lbl in [ID_LBL_BADGE_SIZE, ID_LBL_PREVIEW_BLOCKED_EXTS] {
        if let Ok(lbl) = GetDlgItem(Some(hwnd), lbl) {
            let _ = InvalidateRect(Some(lbl), None, true);
        }
    }
}

/// Enable-or-grey one parent's children, and repaint them so the change is visible now.
pub(super) unsafe fn grey_kids(hwnd: HWND, kids: &[i32], on: bool) {
    for &kid in kids {
        if let Ok(c) = GetDlgItem(Some(hwnd), kid) {
            let _ = windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow(c, on);
            let _ = InvalidateRect(Some(c), None, true);
            // A framed field's rounded frame is painted by the DIALOG around the control, and
            // its fill follows the enabled state too (`restyle::paint_chrome`), so the ring
            // outside the control has to repaint with it.
            let mut rc = RECT::default();
            if GetWindowRect(c, &mut rc).is_ok() {
                let mut pts = [
                    POINT {
                        x: rc.left,
                        y: rc.top,
                    },
                    POINT {
                        x: rc.right,
                        y: rc.bottom,
                    },
                ];
                windows::Win32::Graphics::Gdi::MapWindowPoints(None, Some(hwnd), &mut pts);
                let pad = s(hwnd, 12);
                let ring = RECT {
                    left: pts[0].x - pad,
                    top: pts[0].y - pad,
                    right: pts[1].x + pad,
                    bottom: pts[1].y + pad,
                };
                let _ = InvalidateRect(Some(hwnd), Some(&ring), false);
            }
        }
    }
}

/// Grey out the badge STYLE row while the badge itself is off — the house rule for a
/// dependent control, and here it also stops the row reading as a second, separate feature.
/// Kept as the badge-specific entry point; it now rides the general dependents table.
pub(in super::super) unsafe fn update_badge_style_enabled(hwnd: HWND) {
    sync_dependent_switches(hwnd);
}
