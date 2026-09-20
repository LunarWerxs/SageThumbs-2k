//! The token-based "Menu items" row model: turning saved/default menu-order tokens into
//! `(lParam, checked)` list rows, and reading a row's toggle index / raw lParam back out
//! after a drag-reorder. Split out of `mod.rs`; the ListView plumbing itself (subclass,
//! drag, bulk-toggle menu) stays in `list.rs`.

use super::*;

/// Parse `tokens` (item keys + `verbs::MENU_SEP_TOKEN` divider markers) into menu-list
/// rows `(lParam, checked)`: an item row carries its `MENU_ITEM_TOGGLES` index + its
/// `check(index)` state; a divider token becomes a `list::SEP_PARAM` row. Items are
/// de-duped and any missing from a stale order are appended in default order, so every
/// toggle appears exactly once.
pub(super) fn menu_rows_from_tokens(
    tokens: &[String],
    check: impl Fn(usize) -> bool,
) -> Vec<(isize, bool)> {
    let mut rows = Vec::with_capacity(tokens.len() + MENU_ITEM_TOGGLES.len());
    let mut seen = vec![false; MENU_ITEM_TOGGLES.len()];
    for tok in tokens {
        if tok == MENU_SEP_TOKEN {
            rows.push((list::SEP_PARAM, false));
        } else if let Some(i) = MENU_ITEM_TOGGLES
            .iter()
            .position(|(_, k)| *k == tok.as_str())
        {
            if !seen[i] {
                seen[i] = true;
                rows.push((i as isize, check(i)));
            }
        }
    }
    for (i, &shown) in seen.iter().enumerate() {
        if !shown {
            rows.push((i as isize, check(i)));
        }
    }
    // Show the list in the SAME normalized form the menu renders (no double/edge
    // dividers), so a saved order with a stray double loads cleanly + mirrors the menu.
    list::normalize_rows(&rows)
}

/// Menu-list rows for the CURRENT saved order (or the factory order if none saved), each
/// item's checkbox seeded from its saved visibility.
pub(super) fn saved_menu_rows() -> Vec<(isize, bool)> {
    let saved = settings::menu_order();
    let tokens: Vec<String> = if saved.is_empty() {
        default_menu_tokens()
            .iter()
            .map(|s| s.to_string())
            .collect()
    } else {
        saved
    };
    menu_rows_from_tokens(&tokens, |i| {
        settings::menu_item_shown(MENU_ITEM_TOGGLES[i].1)
    })
}

/// The factory (default) menu-list rows — items + dividers in tree order, each item
/// checked per `check`. Backs "Reset order" (current checks) and "Defaults" (all on).
pub(super) fn default_menu_rows(check: impl Fn(usize) -> bool) -> Vec<(isize, bool)> {
    let tokens: Vec<String> = default_menu_tokens()
        .iter()
        .map(|s| s.to_string())
        .collect();
    menu_rows_from_tokens(&tokens, check)
}

/// The raw `lParam` of a menu-list row, or None if the row can't be read.
unsafe fn menu_row_lparam(list: HWND, row: i32) -> Option<isize> {
    let mut item = LVITEMW {
        mask: windows::Win32::UI::Controls::LVIF_PARAM,
        iItem: row,
        ..Default::default()
    };
    let ok = SendMessageW(
        list,
        windows::Win32::UI::Controls::LVM_GETITEMW,
        Some(WPARAM(0)),
        Some(LPARAM(&mut item as *mut _ as isize)),
    );
    (ok.0 != 0).then_some(item.lParam.0)
}

/// The toggle index stored in a menu-list row's `lParam` (its `MENU_ITEM_TOGGLES`
/// index), or None if the row/param is out of range. Lets load/save map row→key
/// after the rows have been drag-reordered.
pub(super) unsafe fn menu_row_toggle(list: HWND, row: i32) -> Option<usize> {
    let ti = menu_row_lparam(list, row)? as usize;
    (ti < MENU_ITEM_TOGGLES.len()).then_some(ti)
}

/// The raw `lParam` of a menu-list row (a toggle index, or `list::SEP_PARAM` for a
/// divider row); `isize::MIN` if the row can't be read. Lets save distinguish divider
/// rows from item rows after a drag-reorder.
pub(super) unsafe fn menu_row_param(list: HWND, row: i32) -> isize {
    menu_row_lparam(list, row).unwrap_or(isize::MIN)
}
