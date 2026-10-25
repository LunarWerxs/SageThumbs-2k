//! A single subclass on the ListView does the three things SetWindowTheme can't:
//!   * dark HEADER text — the header is a child of the ListView, so its
//!     NM_CUSTOMDRAW arrives here (the theme darkens the header fill but leaves
//!     the text drawn black; only custom-draw overrides the per-item color);
//!   * SPACE bulk-toggles the checkboxes of every selected row (the control would
//!     otherwise toggle only the focused one);
//!   * right-click / Shift+F10 opens a Check / Uncheck / Toggle-selected menu on the file-types
//!     list, and an add / remove divider menu on the menu-items list.

use super::*;

unsafe fn list_header(list: HWND) -> HWND {
    HWND(SendMessageW(list, LVM_GETHEADER, None, None).0 as *mut c_void)
}

unsafe fn lv_next(list: HWND, start: i32, flags: u32) -> i32 {
    SendMessageW(
        list,
        LVM_GETNEXTITEM,
        Some(WPARAM(start as usize)),
        Some(LPARAM(flags as isize)),
    )
    .0 as i32
}

/// Every row of both lists has a box: the menu list's dividers too, since they can be
/// switched off. This used to skip any row whose lParam was not a menu toggle index, which on
/// the file-types list (lParam = FORMATS index) left every format past the first few dozen
/// untouched by a bulk check.
unsafe fn bulk_set_selected(list: HWND, target: bool) {
    let mut i = lv_next(list, -1, LVNI_SELECTED);
    while i >= 0 {
        set_check(list, i, target);
        i = lv_next(list, i, LVNI_SELECTED);
    }
}

/// Toggle the checkboxes of all selected rows to a single uniform state — the
/// inverse of the focused row — so a mixed selection collapses predictably.
unsafe fn bulk_toggle_selected(list: HWND) {
    let focus = lv_next(list, -1, LVNI_FOCUSED);
    let target = if focus >= 0 {
        !is_checked(list, focus)
    } else {
        true
    };
    bulk_set_selected(list, target);
}

// Context-menu command ids. One numbering for both lists' menus, because the owner-drawn
// (dark mode) items are labelled by id alone in WM_MEASUREITEM / WM_DRAWITEM.
const CTX_CHECK: usize = 1;
const CTX_UNCHECK: usize = 2;
const CTX_TOGGLE: usize = 3;
const CTX_DIVIDER_ABOVE: usize = 4;
const CTX_DIVIDER_BELOW: usize = 5;
const CTX_DIVIDER_REMOVE: usize = 6;

/// Label for an owner-drawn context-menu item of either list.
pub(super) fn ctx_menu_label(id: usize) -> &'static str {
    match id {
        CTX_CHECK => t("ctx_check_selected"),
        CTX_UNCHECK => t("ctx_uncheck_selected"),
        CTX_TOGGLE => t("ctx_toggle_selected"),
        CTX_DIVIDER_ABOVE => t("ctx_divider_above"),
        CTX_DIVIDER_BELOW => t("ctx_divider_below"),
        CTX_DIVIDER_REMOVE => t("ctx_divider_remove"),
        _ => "",
    }
}

/// A `WM_CONTEXTMENU` lParam as screen coordinates, or `None` for the keyboard (Shift+F10 /
/// the Apps key), which sets BOTH halves to -1. Not the whole lParam: real multi-monitor
/// coordinates can be negative, so the sign-extended halves are tested separately.
fn ctx_point(l: LPARAM) -> Option<(i32, i32)> {
    let x = (l.0 & 0xFFFF) as u16 as i16 as i32;
    let y = ((l.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
    (x != -1 || y != -1).then_some((x, y))
}

/// Show a context menu of `ids` at screen point `(px, py)` and return the id picked (0 for
/// none). Dark mode owner-draws the items: a normal menu renders black text on the immersive
/// dark background (unreadable until the row is highlighted), so `owner`'s WM_DRAWITEM draws
/// the text light. Light mode uses a normal menu.
unsafe fn track_ctx_menu(owner: HWND, px: i32, py: i32, ids: &[usize]) -> usize {
    let Ok(menu) = CreatePopupMenu() else {
        return 0;
    };
    let labels: Vec<Vec<u16>> = ids.iter().map(|&id| wide(ctx_menu_label(id))).collect();
    let dark = is_dark();
    for (&id, label) in ids.iter().zip(&labels) {
        let _ = if dark {
            AppendMenuW(menu, MF_OWNERDRAW, id, PCWSTR(std::ptr::dangling::<u16>()))
        } else {
            AppendMenuW(menu, MF_STRING, id, PCWSTR(label.as_ptr()))
        };
    }
    // Foreground + WM_NULL bracket: the documented fix for the "menu shows then
    // immediately vanishes" quirk. Owner is the top-level dialog, not the list.
    st2k_appkit::win::force_foreground(owner);
    let cmd = TrackPopupMenu(
        menu,
        TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
        px,
        py,
        Some(0),
        owner,
        None,
    );
    let _ = PostMessageW(Some(owner), WM_NULL, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(menu);
    cmd.0.max(0) as usize
}

/// The file-types list's right-click menu: check, uncheck or toggle every selected row.
pub(super) unsafe fn list_context_menu(list: HWND, owner: HWND, l: LPARAM) {
    let (px, py) = ctx_point(l).unwrap_or_else(|| {
        let mut r = RECT::default();
        let _ = GetWindowRect(list, &mut r);
        (r.left + 8, r.top + 8)
    });
    match track_ctx_menu(owner, px, py, &[CTX_CHECK, CTX_UNCHECK, CTX_TOGGLE]) {
        CTX_CHECK => bulk_set_selected(list, true),
        CTX_UNCHECK => bulk_set_selected(list, false),
        CTX_TOGGLE => bulk_toggle_selected(list),
        _ => {}
    }
}

/// The row a context menu is for and the screen point to open it at: the row under the cursor,
/// or from the keyboard the focused row, opened just under it. `None` over empty space.
unsafe fn ctx_row(list: HWND, l: LPARAM) -> Option<(i32, i32, i32)> {
    use windows::Win32::Graphics::Gdi::{ClientToScreen, ScreenToClient};
    if let Some((x, y)) = ctx_point(l) {
        let mut p = POINT { x, y };
        let _ = ScreenToClient(list, &mut p);
        let row = hit_row(list, p.x, p.y);
        return (row >= 0).then_some((row, x, y));
    }
    let row = lv_next(list, -1, LVNI_FOCUSED);
    if row < 0 {
        return None;
    }
    let r = row_rect(list, row);
    let mut p = POINT {
        x: r.left + 8,
        y: r.bottom,
    };
    let _ = ClientToScreen(list, &mut p);
    Some((row, p.x, p.y))
}

/// What the menu-items list's right-click menu does to the dividers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DividerEdit {
    Above,
    Below,
    Remove,
}

/// The menu-items list's right-click menu: add a divider above or below the row, or remove the
/// row when it is a divider. The list selects one row at a time, so the file-types list's
/// Check / Uncheck / Toggle selected did nothing a click on the row's box does not (feedback,
/// 2026-10-22: "there's no option to add new divider items").
pub(super) unsafe fn menu_list_context_menu(list: HWND, owner: HWND, l: LPARAM) {
    let Some((row, px, py)) = ctx_row(list, l) else {
        return;
    };
    let on_divider = menu_row_param(list, row) == SEP_PARAM;
    let ids: &[usize] = if on_divider {
        &[CTX_DIVIDER_ABOVE, CTX_DIVIDER_BELOW, CTX_DIVIDER_REMOVE]
    } else {
        &[CTX_DIVIDER_ABOVE, CTX_DIVIDER_BELOW]
    };
    let edit = match track_ctx_menu(owner, px, py, ids) {
        CTX_DIVIDER_ABOVE => DividerEdit::Above,
        CTX_DIVIDER_BELOW => DividerEdit::Below,
        CTX_DIVIDER_REMOVE => DividerEdit::Remove,
        _ => return,
    };
    apply_divider_edit(list, row, edit);
}

/// Carry out `edit` on `row` of the menu-items list, keeping its scroll position and selecting
/// the new divider (or the row that took a removed one's place).
pub(super) unsafe fn apply_divider_edit(list: HWND, row: i32, edit: DividerEdit) {
    if row < 0 {
        return;
    }
    let (rows, select) = edit_dividers(&snapshot_rows(list), row as usize, edit);
    rebuild_rows_keep_scroll(list, &rows, select);
}

/// `rows` with `edit` applied at `row`, and the row to select afterwards. A new divider is
/// ticked (the menu draws it). Remove only ever takes a divider out: on an item row, or past the
/// end, the rows come back unchanged.
fn edit_dividers(
    rows: &[(isize, bool)],
    row: usize,
    edit: DividerEdit,
) -> (Vec<(isize, bool)>, Option<usize>) {
    let mut out = rows.to_vec();
    if row >= out.len() {
        return (out, None);
    }
    match edit {
        DividerEdit::Above => {
            out.insert(row, (SEP_PARAM, true));
            (out, Some(row))
        }
        DividerEdit::Below => {
            out.insert(row + 1, (SEP_PARAM, true));
            (out, Some(row + 1))
        }
        DividerEdit::Remove if out[row].0 == SEP_PARAM => {
            out.remove(row);
            let select = (!out.is_empty()).then(|| row.min(out.len() - 1));
            (out, select)
        }
        DividerEdit::Remove => (out, Some(row)),
    }
}

// ---- Drag-to-reorder (the "Menu items" checklist only) ------------------

use std::cell::Cell;

use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};

thread_local! {
    /// The menu-items row being drag-reordered, or -1 when idle. The subclass is shared
    /// with the format list, but a drag only ever STARTS on the menu list (see the
    /// dialog's LVN_BEGINDRAG handler), and mouse-capture routes the moves/drop back
    /// here — so gating the move/drop handlers on `>= 0` is sufficient.
    static DRAG_SRC: Cell<i32> = const { Cell::new(-1) };
    /// Current drop-indicator position during a drag: `(anchor_row, after)` — the line
    /// draws just above `anchor_row` (after=false) or just below it (after=true).
    /// `anchor_row < 0` hides it. We paint it ourselves (WM_PAINT) rather than via the
    /// native insertion mark, which crashes comctl32 in REPORT view.
    static INSERT_MARK: Cell<(i32, bool)> = const { Cell::new((-1, false)) };
    /// Which way the list is auto-scrolling while the cursor is held at its top or bottom
    /// edge mid-drag: -1 up, 1 down, 0 not.
    static DRAG_SCROLL: Cell<i32> = const { Cell::new(0) };
}

/// The timer that steps [`DRAG_SCROLL`]. Far from comctl32's own small listview timer ids.
const DRAG_SCROLL_TIMER: usize = 0x5354_4453;
/// How often the held-at-the-edge auto-scroll moves one row, in ms.
const DRAG_SCROLL_MS: u32 = 70;

/// Begin a drag-reorder of the menu-items list (from the dialog's LVN_BEGINDRAG).
/// Captures the mouse so WM_MOUSEMOVE/WM_LBUTTONUP come back to this subclass; the
/// accent drop-indicator line is painted in WM_PAINT while the drag is active.
pub(super) unsafe fn begin_menu_drag(list: HWND, source: i32) {
    if source < 0 {
        return;
    }
    DRAG_SRC.with(|s| s.set(source));
    INSERT_MARK.with(|m| m.set((-1, false)));
    SetCapture(list);
}

/// Cancel a drag in progress (Escape while dragging): nothing moves. `false` when no drag was
/// active, so the caller's own Escape (closing the editor) goes ahead.
pub(super) unsafe fn cancel_menu_drag(list: HWND) -> bool {
    if DRAG_SRC.with(|s| s.get()) < 0 {
        return false;
    }
    end_drag(list);
    true
}

/// End the drag: release the mouse and drop every piece of drag state.
unsafe fn end_drag(list: HWND) {
    clear_drag_state(list);
    let _ = ReleaseCapture();
}

/// The source row, the auto-scroll timer and the drop-indicator line, but not the capture: on
/// WM_CAPTURECHANGED it is already someone else's.
unsafe fn clear_drag_state(list: HWND) {
    use windows::Win32::UI::WindowsAndMessaging::KillTimer;
    DRAG_SRC.with(|s| s.set(-1));
    DRAG_SCROLL.with(|d| d.set(0));
    let _ = KillTimer(Some(list), DRAG_SCROLL_TIMER);
    set_insert_mark(list, -1, false);
}

/// `row`'s bounds in the list's client coordinates.
unsafe fn row_rect(list: HWND, row: i32) -> RECT {
    let mut r = RECT::default(); // .left = LVIR_BOUNDS (0)
    SendMessageW(
        list,
        windows::Win32::UI::Controls::LVM_GETITEMRECT,
        Some(WPARAM(row.max(0) as usize)),
        Some(LPARAM(&mut r as *mut _ as isize)),
    );
    r
}

/// The height of one row in device px (every row of a report-view list has the same).
unsafe fn row_height(list: HWND) -> i32 {
    let r = row_rect(list, 0);
    (r.bottom - r.top).max(1)
}

/// Which way to auto-scroll for a cursor at client `y` mid-drag: within one row of the top
/// edge (or above it) is up, within one row of the bottom edge (or below it) is down.
fn drag_scroll_dir(y: i32, client_h: i32, row_h: i32) -> i32 {
    if y < row_h {
        -1
    } else if y >= client_h - row_h {
        1
    } else {
        0
    }
}

/// Start, steer or stop the edge auto-scroll for a cursor at client `y`: a row dragged to the
/// edge of the visible rows keeps the list moving, so it can reach a row scrolled out of view.
unsafe fn update_drag_scroll(list: HWND, y: i32) {
    use windows::Win32::UI::WindowsAndMessaging::{KillTimer, SetTimer};
    let mut cr = RECT::default();
    let _ = windows::Win32::UI::WindowsAndMessaging::GetClientRect(list, &mut cr);
    let dir = drag_scroll_dir(y, cr.bottom - cr.top, row_height(list));
    let was = DRAG_SCROLL.with(|d| d.replace(dir));
    if dir != 0 && was == 0 {
        SetTimer(Some(list), DRAG_SCROLL_TIMER, DRAG_SCROLL_MS, None);
    } else if dir == 0 && was != 0 {
        let _ = KillTimer(Some(list), DRAG_SCROLL_TIMER);
    }
}

/// One auto-scroll step: move the list one row, then put the drop line back under the cursor.
unsafe fn drag_scroll_step(list: HWND) {
    use windows::Win32::Graphics::Gdi::ScreenToClient;
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
    let dir = DRAG_SCROLL.with(|d| d.get());
    if dir == 0 {
        return;
    }
    // The old line scrolls with the rows, so repaint everything rather than just its band.
    INSERT_MARK.with(|m| m.set((-1, false)));
    SendMessageW(
        list,
        windows::Win32::UI::Controls::LVM_SCROLL,
        Some(WPARAM(0)),
        Some(LPARAM((dir * row_height(list)) as isize)),
    );
    let _ = InvalidateRect(Some(list), None, true);
    let mut p = POINT::default();
    if GetCursorPos(&mut p).is_ok() && ScreenToClient(list, &mut p).as_bool() {
        let (row, after) = insert_point(list, p.x, p.y);
        set_insert_mark(list, row, after);
    }
}

/// Client-point → row index (LVM_HITTEST); -1 if past the last row.
unsafe fn hit_row(list: HWND, x: i32, y: i32) -> i32 {
    let mut ht = windows::Win32::UI::Controls::LVHITTESTINFO {
        pt: POINT { x, y },
        ..Default::default()
    };
    SendMessageW(
        list,
        windows::Win32::UI::Controls::LVM_HITTEST,
        Some(WPARAM(usize::MAX)),
        Some(LPARAM(&mut ht as *mut _ as isize)),
    )
    .0 as i32
}

/// The insertion point under the cursor, as `(anchor_row, after)`: the dragged item
/// lands just before `anchor_row` (after=false) or just after it (after=true). Past
/// the last row → after the last item; an empty list → `(-1, false)`.
unsafe fn insert_point(list: HWND, x: i32, y: i32) -> (i32, bool) {
    let count = SendMessageW(list, LVM_GETITEMCOUNT, None, None).0 as i32;
    if count == 0 {
        return (-1, false);
    }
    let row = hit_row(list, x, y);
    if row < 0 {
        // Above the first visible row (dragging into the top edge) is just above it; anywhere
        // else off the rows is after the last one.
        let top = SendMessageW(
            list,
            windows::Win32::UI::Controls::LVM_GETTOPINDEX,
            None,
            None,
        )
        .0 as i32;
        return if y < row_rect(list, top).top {
            (top, false)
        } else {
            (count - 1, true)
        };
    }
    let r = row_rect(list, row);
    let mid = (r.top + r.bottom) / 2;
    (row, y >= mid)
}

/// Record the drop-indicator position `(anchor_row, after)` and repaint so the line is
/// redrawn there; `row < 0` clears it. We draw the line ourselves (`draw_insert_line`
/// in WM_PAINT) because comctl32's native insertion mark (LVM_SETINSERTMARK) is
/// unsupported in REPORT view and access-violates the control there. Only the OLD and
/// NEW line bands are invalidated (with background erase) so the previous line is wiped
/// without flickering the whole list on every mouse-move.
unsafe fn set_insert_mark(list: HWND, row: i32, after: bool) {
    let old = INSERT_MARK.with(|m| m.replace((row, after)));
    invalidate_mark_band(list, old.0, old.1);
    invalidate_mark_band(list, row, after);
}

/// The mark line's y on the anchor row (its top, or its bottom when `after`) plus the
/// list's client rect whose `left`/`right` the line spans.
unsafe fn mark_y_and_client(list: HWND, anchor: i32, after: bool) -> (i32, RECT) {
    let ir = row_rect(list, anchor);
    let y = if after { ir.bottom } else { ir.top };
    let mut cr = RECT::default();
    let _ = windows::Win32::UI::WindowsAndMessaging::GetClientRect(list, &mut cr);
    (y, cr)
}

/// Invalidate (erase) the ~4px band around a mark line so a stale line there is wiped
/// and the rows under it repaint cleanly.
unsafe fn invalidate_mark_band(list: HWND, anchor: i32, after: bool) {
    if anchor < 0 {
        return;
    }
    let (y, cr) = mark_y_and_client(list, anchor, after);
    let band = RECT {
        left: cr.left,
        top: y - 2,
        right: cr.right,
        bottom: y + 2,
    };
    let _ = InvalidateRect(Some(list), Some(&band), true);
}

/// Paint the accent drop-indicator line for the current `INSERT_MARK`: a 2px rule just
/// above the anchor row (after=false) or just below it (after=true), inset to the list
/// width. Called from WM_PAINT after the default row paint, so it sits on top.
unsafe fn draw_insert_line(list: HWND) {
    use windows::Win32::Graphics::Gdi::{
        CreateSolidBrush, DeleteObject, FillRect, GetDC, ReleaseDC, HGDIOBJ,
    };
    let (anchor, after) = INSERT_MARK.with(|m| m.get());
    if anchor < 0 {
        return;
    }
    let (y, cr) = mark_y_and_client(list, anchor, after);
    let line = RECT {
        left: cr.left + 2,
        top: y - 1,
        right: cr.right - 2,
        bottom: y + 1,
    };
    let hdc = GetDC(Some(list));
    let br = CreateSolidBrush(st2k_appkit::dark::ACCENT());
    FillRect(hdc, &line, br);
    let _ = DeleteObject(HGDIOBJ(br.0));
    ReleaseDC(Some(list), hdc);
}

/// Collapse meaningless dividers so the list shows EXACTLY the menu it produces: drop a
/// leading divider, collapse consecutive dividers to one (ticked if either was, the same rule
/// the menu applies to the saved tokens), drop a trailing divider (the menu builder normalizes
/// identically + adds its own divider before Settings). Applied when the list loads and when
/// Done saves it, never mid-edit: a divider that vanished the moment it was dragged next to
/// another, or to either end, made reordering jarring (feedback, 2026-10-22).
pub(super) fn normalize_rows(rows: &[(isize, bool)]) -> Vec<(isize, bool)> {
    let mut out: Vec<(isize, bool)> = Vec::with_capacity(rows.len());
    for &(p, c) in rows {
        if keep_row(&mut out, p, c) {
            out.push((p, c));
        }
    }
    while out.last().map(|r| r.0) == Some(SEP_PARAM) {
        out.pop();
    }
    out
}

/// Normalize the list in place ([`normalize_rows`]); Done calls it before saving.
pub(super) unsafe fn normalize_list(list: HWND) {
    let rows = snapshot_rows(list);
    let tidy = normalize_rows(&rows);
    if tidy != rows {
        rebuild_rows_keep_scroll(list, &tidy, None);
    }
}

/// Snapshot every row as `(lParam-key, checked)` in current display order.
pub(super) unsafe fn snapshot_rows(list: HWND) -> Vec<(isize, bool)> {
    use windows::Win32::UI::Controls::{LVIF_PARAM, LVM_GETITEMW};
    let count = SendMessageW(list, LVM_GETITEMCOUNT, None, None).0 as i32;
    let mut rows = Vec::with_capacity(count.max(0) as usize);
    for r in 0..count {
        let mut it = LVITEMW {
            mask: LVIF_PARAM,
            iItem: r,
            ..Default::default()
        };
        SendMessageW(
            list,
            LVM_GETITEMW,
            Some(WPARAM(0)),
            Some(LPARAM(&mut it as *mut _ as isize)),
        );
        rows.push((it.lParam.0, is_checked(list, r)));
    }
    rows
}

/// A persisted divider row's `lParam` sentinel (item rows carry their toggle index 0..N).
pub(super) const SEP_PARAM: isize = -1;
/// The divider row's label — a run of box-drawing rules that reads as one horizontal line.
pub(super) const SEP_LABEL: &str = "──────────────────────────────────────";

/// Rebuild the list from `(lParam, checked)` rows: item rows (lParam = toggle index) get
/// their translated label; divider rows (lParam == [`SEP_PARAM`]) get the rule label. Every
/// row gets its checkbox: a divider's says whether the menu draws it. Optionally selects the
/// row at display INDEX `select` — not by lParam key, because every divider shares
/// [`SEP_PARAM`] and a key match would always land on the FIRST divider rather than whichever
/// one the caller actually means (A266).
pub(super) unsafe fn rebuild_rows(list: HWND, rows: &[(isize, bool)], select: Option<usize>) {
    use windows::Win32::UI::Controls::{
        LVIF_PARAM, LVIF_TEXT, LVIS_FOCUSED, LVIS_SELECTED, LVM_DELETEALLITEMS,
    };
    SendMessageW(list, LVM_DELETEALLITEMS, None, None);
    for (row, &(param, checked)) in rows.iter().enumerate() {
        let is_sep = param == SEP_PARAM;
        let ti = param as usize;
        if !is_sep && ti >= MENU_ITEM_TOGGLES.len() {
            continue;
        }
        let label = wide(if is_sep {
            SEP_LABEL
        } else {
            t(MENU_ITEM_TOGGLES[ti].1)
        });
        let mut it = LVITEMW {
            mask: LVIF_TEXT | LVIF_PARAM,
            iItem: row as i32,
            pszText: PWSTR(label.as_ptr() as *mut u16),
            lParam: LPARAM(param),
            ..Default::default()
        };
        SendMessageW(
            list,
            LVM_INSERTITEMW,
            Some(WPARAM(0)),
            Some(LPARAM(&mut it as *mut _ as isize)),
        );
        set_check(list, row as i32, checked);
    }
    if let Some(idx) = select {
        if idx < rows.len() {
            let f = LIST_VIEW_ITEM_STATE_FLAGS(LVIS_SELECTED.0 | LVIS_FOCUSED.0);
            let sel = LVITEMW {
                stateMask: f,
                state: f,
                ..Default::default()
            };
            SendMessageW(
                list,
                LVM_SETITEMSTATE,
                Some(WPARAM(idx)),
                Some(LPARAM(&sel as *const _ as isize)),
            );
        }
    }
}

/// [`rebuild_rows`] without the jump back to the top: the rows rebuild with drawing off, then
/// the list scrolls back so the row that was at the top stays there, and `select` is brought
/// into view. Every drag used to send the scrollbar to the top, which made sorting a long list
/// a chore (feedback, 2026-10-22).
pub(super) unsafe fn rebuild_rows_keep_scroll(
    list: HWND,
    rows: &[(isize, bool)],
    select: Option<usize>,
) {
    use windows::Win32::UI::Controls::{LVM_ENSUREVISIBLE, LVM_SCROLL};
    use windows::Win32::UI::WindowsAndMessaging::WM_SETREDRAW;
    let top = SendMessageW(
        list,
        windows::Win32::UI::Controls::LVM_GETTOPINDEX,
        None,
        None,
    )
    .0 as i32;
    SendMessageW(list, WM_SETREDRAW, Some(WPARAM(0)), None);
    rebuild_rows(list, rows, select);
    // Drawing back on BEFORE the scroll: with it off the list does not update its scroll range
    // for the new rows, and the scroll stopped a row short at the bottom. Nothing paints until
    // the next message pump, by which time the list is back where it was.
    SendMessageW(list, WM_SETREDRAW, Some(WPARAM(1)), None);
    if top > 0 {
        // Report view scrolls in whole rows, by pixels.
        SendMessageW(
            list,
            LVM_SCROLL,
            Some(WPARAM(0)),
            Some(LPARAM((top * row_height(list)) as isize)),
        );
    }
    if let Some(i) = select {
        SendMessageW(list, LVM_ENSUREVISIBLE, Some(WPARAM(i)), Some(LPARAM(0)));
    }
    let _ = InvalidateRect(Some(list), None, true);
}

/// Finish the drag: move the source row to the insertion point under the cursor (key and
/// check kept), then select it where it landed. Dividers are left exactly where they are put,
/// next to each other or at either end included; Done tidies them ([`normalize_list`]).
unsafe fn finish_menu_drag(list: HWND, x: i32, y: i32) {
    let src = DRAG_SRC.with(|s| s.get());
    let (anchor, after) = insert_point(list, x, y);
    end_drag(list);
    let rows = snapshot_rows(list);
    if let Some((rows, dest)) = moved_rows(&rows, src, anchor, after) {
        rebuild_rows_keep_scroll(list, &rows, Some(dest));
    }
}

/// `rows` with row `src` moved to the insertion point `(anchor, after)` (just before `anchor`,
/// or just after it when `after`), and the index it lands at. `None` when `src` or `anchor`
/// is not a row.
fn moved_rows(
    rows: &[(isize, bool)],
    src: i32,
    anchor: i32,
    after: bool,
) -> Option<(Vec<(isize, bool)>, usize)> {
    let len = rows.len() as i32;
    if !(0..len).contains(&src) || !(0..len).contains(&anchor) {
        return None;
    }
    // Destination in the ORIGINAL list, then adjusted for removing `src` first.
    let mut dest = anchor + i32::from(after);
    if src < dest {
        dest -= 1;
    }
    let mut out = rows.to_vec();
    let elem = out.remove(src as usize);
    let dest = (dest.max(0) as usize).min(out.len());
    out.insert(dest, elem);
    Some((out, dest))
}

/// Whether row `(p, c)` goes after `out`: a leading divider is dropped, and a divider right
/// after another is merged into it (ticked if either was).
fn keep_row(out: &mut [(isize, bool)], p: isize, c: bool) -> bool {
    if p != SEP_PARAM {
        return true;
    }
    match out.last_mut() {
        None => false,
        Some(last) if last.0 == SEP_PARAM => {
            last.1 |= c;
            false
        }
        Some(_) => true,
    }
}

/// Put the menu-items list back to the factory menu: the default order, every entry and
/// divider ticked, added dividers gone. Wired to the editor's Reset, which used to restore only
/// the order and leave no way back for the ticks (feedback, 2026-10-22); the editor's Cancel
/// still brings back what was there before.
pub(super) unsafe fn reset_menu_rows(list: HWND) {
    rebuild_rows(list, &super::default_menu_rows(|_| true), None);
}

pub(super) unsafe extern "system" fn list_subclass(
    h: HWND,
    msg: u32,
    w: WPARAM,
    l: LPARAM,
    uid: usize,
    _data: usize,
) -> LRESULT {
    match msg {
        WM_NCDESTROY => {
            let _ = RemoveWindowSubclass(h, Some(list_subclass), uid);
        }
        WM_NOTIFY => {
            if let Some(res) = on_notify(h, l).or_else(|| on_divider_dblclick(h, msg, w, l)) {
                return res;
            }
        }
        WM_KEYDOWN
            if w.0 as u16 == VK_SPACE.0
                && SendMessageW(h, LVM_GETSELECTEDCOUNT, None, None).0 > 1 =>
        {
            bulk_toggle_selected(h);
            return LRESULT(0); // eat the key so the control doesn't single-toggle too
        }
        // Menu-items drag-reorder: while a drag is active, track the drop target and
        // commit on release (capture routes these here). Idle → fall through to default.
        // While a drag is active, repaint the rows normally, then draw our accent
        // drop-indicator line on top (the native insertion mark crashes in report view).
        WM_PAINT | WM_MOUSEMOVE | WM_LBUTTONUP | WM_CAPTURECHANGED | WM_TIMER
            if DRAG_SRC.with(|s| s.get()) >= 0 =>
        {
            if let Some(res) = on_drag_msg(h, msg, w, l) {
                return res;
            }
        }
        // WM_CONTEXTMENU is handled in the dialog proc (it bubbles to the parent).
        _ => {}
    }
    DefSubclassProc(h, msg, w, l)
}

/// `list_subclass`'s drag-active messages: repaint the rows normally then draw the accent
/// drop-indicator line, move/release to commit the reorder, step the edge auto-scroll, or
/// cancel on capture-loss. `Some(res)` means the subclass should return `res`; `None` falls
/// through to `DefSubclassProc`.
unsafe fn on_drag_msg(h: HWND, msg: u32, w: WPARAM, l: LPARAM) -> Option<LRESULT> {
    let x = (l.0 & 0xFFFF) as u16 as i16 as i32;
    let y = ((l.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
    match msg {
        WM_PAINT => {
            let res = DefSubclassProc(h, msg, w, l);
            draw_insert_line(h);
            Some(res)
        }
        WM_MOUSEMOVE => {
            let (row, after) = insert_point(h, x, y);
            set_insert_mark(h, row, after);
            update_drag_scroll(h, y);
            Some(LRESULT(0))
        }
        WM_LBUTTONUP => {
            finish_menu_drag(h, x, y);
            Some(LRESULT(0))
        }
        WM_TIMER if w.0 == DRAG_SCROLL_TIMER => {
            drag_scroll_step(h);
            Some(LRESULT(0))
        }
        WM_CAPTURECHANGED => {
            // Capture pulled away (another window) — cancel cleanly.
            clear_drag_state(h);
            None
        }
        _ => None,
    }
}

/// `list_subclass`'s `WM_NOTIFY` handling: a finished column drag (floor + refit), the
/// Description-column drag refusal, and the header's custom-draw. `Some(res)` means the
/// subclass should return `res` immediately instead of falling through to `DefSubclassProc`.
unsafe fn on_notify(h: HWND, l: LPARAM) -> Option<LRESULT> {
    let nmhdr = l.0 as *const NMHDR;
    // A finished column drag (issue #26.3). Dragging Extension or Category has to
    // reflow Description so the three still exactly fill the list; dragging
    // Description itself means the user wants that width, so the auto-fit stands down
    // rather than snapping the drag back.
    //
    // GATED ON ID_LIST, and that is load-bearing: this subclass is installed on TWO
    // listviews (the file-types list in `build.rs`, and the single-column checklist in
    // `mod.rs::checklist`, which reuses it for the SPACE bulk-toggle). The checklist
    // has no visible header to drag, so this cannot fire for it today — but `fit_columns`
    // is written for the four-column file-types list specifically, so pointing it at
    // any other list would be wrong the moment that changes.
    // DESCRIPTION IS NOT DRAGGABLE — refuse the drag before it starts.
    //
    // It is the LAST column and `fit_columns` sizes it to exactly fill the list, so the
    // only thing dragging it can do is make it narrower and leave dead space against the
    // scrollbar. There is no width the user could want there that fitting does not
    // already give them, and the gap reads as a rendering bug rather than as their own
    // layout. Returning TRUE from HDN_BEGINTRACK is the documented way to say no.
    //
    // This replaces a "remember that the user dragged Description and stop auto-fitting
    // it" flag. That flag existed only to stop the auto-fit snapping such a drag back —
    // once the drag itself is refused, it had nothing left to do.
    if (*nmhdr).hwndFrom == list_header(h)
        && refuses_header_drag(
            (*nmhdr).code,
            GetDlgCtrlID(h),
            (*(l.0 as *const windows::Win32::UI::Controls::NMHEADERW)).iItem,
        )
    {
        return Some(LRESULT(1));
    }
    if (*nmhdr).code == windows::Win32::UI::Controls::HDN_ENDTRACKW
        && (*nmhdr).hwndFrom == list_header(h)
        && GetDlgCtrlID(h) == ID_LIST
    {
        on_notify_header_endtrack(h, l.0 as *const windows::Win32::UI::Controls::NMHEADERW);
    }
    if (*nmhdr).code == NM_CUSTOMDRAW && (*nmhdr).hwndFrom == list_header(h) {
        if let Some(res) = on_notify_header_customdraw(l.0 as *const NMCUSTOMDRAW, list_header(h)) {
            return Some(res);
        }
    }
    None
}

/// A double-click on a file-types header divider: the control fits that column to its content
/// itself, then Description has to take up the difference. `None` for any other notification.
unsafe fn on_divider_dblclick(h: HWND, msg: u32, w: WPARAM, l: LPARAM) -> Option<LRESULT> {
    let nmhdr = l.0 as *const NMHDR;
    let ours = (*nmhdr).code == windows::Win32::UI::Controls::HDN_DIVIDERDBLCLICKW
        && (*nmhdr).hwndFrom == list_header(h)
        && GetDlgCtrlID(h) == ID_LIST;
    if !ours {
        return None;
    }
    let res = DefSubclassProc(h, msg, w, l);
    super::fit_columns(h);
    Some(res)
}

/// `on_notify`'s `HDN_ENDTRACKW`: floor the just-dragged column at a usable minimum, then refit
/// the other columns to fill the list.
unsafe fn on_notify_header_endtrack(h: HWND, hdn: *const windows::Win32::UI::Controls::NMHEADERW) {
    // FLOOR THE DRAGGED COLUMN FIRST. A Win32 header divider can be dragged
    // all the way to zero, and a 0 px column is a trap rather than a choice:
    // its two dividers land on the same pixel, so there is no longer anything
    // to grab to drag it back, and nothing on this page resets column widths
    // (Defaults resets the format ticks, not the layout). Snapping back to a
    // usable minimum keeps the column reachable; the user can still make it
    // genuinely narrow, just not vanish it.
    // 40 design px, scaled to this window's DPI — `LVM_GETCOLUMNWIDTH` reports device px,
    // so comparing it against a bare 40 floored the column far too aggressively (or not at
    // all) off the 96-DPI display this constant was tuned on.
    let min_col_w = dpi_scale(h, 40);
    let col = (*hdn).iItem;
    // Every draggable column floors - that's everything except Description (`DESC_COLUMN`,
    // refused outright above and never reaches here).
    if (0..DESC_COLUMN).contains(&col) {
        let w = SendMessageW(
            h,
            windows::Win32::UI::Controls::LVM_GETCOLUMNWIDTH,
            Some(WPARAM(col as usize)),
            None,
        )
        .0 as i32;
        if w < min_col_w {
            SendMessageW(
                h,
                LVM_SETCOLUMNWIDTH,
                Some(WPARAM(col as usize)),
                Some(LPARAM(min_col_w as isize)),
            );
        }
    }
    super::fit_columns(h);
    // Repaint: the rows underneath still carry the pre-drag column geometry.
    let _ = windows::Win32::Graphics::Gdi::InvalidateRect(Some(h), None, true);
}

/// `on_notify`'s `NM_CUSTOMDRAW` on the header: mutes the header text and hides the last
/// column's divider. `Some(res)` means the caller should return `res` immediately.
unsafe fn on_notify_header_customdraw(nmcd: *const NMCUSTOMDRAW, header: HWND) -> Option<LRESULT> {
    let stage = (*nmcd).dwDrawStage;
    if stage == CDDS_PREPAINT {
        return Some(LRESULT(CDRF_NOTIFYITEMDRAW as isize));
    }
    if stage == CDDS_ITEMPREPAINT {
        // Muted column-header text to match the mockup. The color is
        // only honored if we return CDRF_NEWFONT (not CDRF_DODEFAULT).
        SetTextColor((*nmcd).hdc, HEADER_TEXT());
        return Some(LRESULT((CDRF_NEWFONT | CDRF_NOTIFYPOSTPAINT) as isize));
    }
    if stage == CDDS_ITEMPOSTPAINT {
        // Kill the LAST column's right-hand divider. The theme draws it as a
        // bright vertical line against the dark header — it reads as a drag
        // grabber, and Description refuses a drag (`refuses_header_drag`).
        // The inner dividers stay: those separate real columns and do drag.
        let cols = SendMessageW(
            header,
            windows::Win32::UI::Controls::HDM_GETITEMCOUNT,
            None,
            None,
        )
        .0;
        if cols > 0 && (*nmcd).dwItemSpec as isize == cols - 1 {
            let rc = (*nmcd).rc;
            let hdc = (*nmcd).hdc;
            // Sample the header's own painted background rather than naming a
            // colour, so this stays right in light mode and under a future theme.
            let bg = windows::Win32::Graphics::Gdi::GetPixel(hdc, rc.right - 4, rc.top + 2);
            if bg.0 != windows::Win32::Graphics::Gdi::CLR_INVALID {
                let strip = RECT {
                    left: rc.right - 2,
                    top: rc.top,
                    right: rc.right,
                    bottom: rc.bottom,
                };
                fill(hdc, &strip, bg);
            }
        }
        return Some(LRESULT(CDRF_DODEFAULT as isize));
    }
    None
}

/// Index of the Description column in the file-types list: the LAST of the four
/// (Extension | Category | How | Description - audit E03 inserted "How" at index 2).
pub(super) const DESC_COLUMN: i32 = 3;

/// Should this header notification be refused outright?
///
/// The `hwndFrom` check stays at the call site (it compares live HWNDs and cannot be a value),
/// but everything that is a RULE lives here so it can be tested. Before this it was an inline
/// four-term conjunction inside a wndproc, which is only checkable by dragging a column with a
/// mouse, and "someone please drag this" is not a test.
///
/// See the call site for why Description specifically is not draggable: it is the last column
/// and `fit_columns` sizes it to exactly fill the list, so the only thing a drag can achieve is
/// dead space against the scrollbar.
pub(super) fn refuses_header_drag(code: u32, ctrl_id: i32, column: i32) -> bool {
    code == windows::Win32::UI::Controls::HDN_BEGINTRACKW
        && ctrl_id == ID_LIST
        && column == DESC_COLUMN
}

#[cfg(test)]
mod header_drag_tests {
    use super::*;
    use windows::Win32::UI::Controls::{HDN_BEGINTRACKW, HDN_ENDTRACKW};

    /// Issue #26.3's second half: the Description column must refuse a drag, and NOTHING else
    /// may. The three ways to get this wrong are all pinned here, because each one ships as a
    /// silently different UI rather than as a failure.
    #[test]
    fn only_the_description_column_of_the_file_types_list_refuses_a_drag() {
        // The one case that is refused.
        assert!(refuses_header_drag(HDN_BEGINTRACKW, ID_LIST, DESC_COLUMN));

        // Extension and Category stay draggable — refusing those would be a regression of the
        // original issue, which was that the columns could not be resized at all.
        assert!(!refuses_header_drag(HDN_BEGINTRACKW, ID_LIST, 0));
        assert!(!refuses_header_drag(HDN_BEGINTRACKW, ID_LIST, 1));

        // The subclass is installed on a SECOND listview (the single-column checklist), and
        // `fit_columns` is written for the three-column list specifically. A drag there must
        // never be answered by this rule.
        assert!(!refuses_header_drag(
            HDN_BEGINTRACKW,
            ID_MENU_ITEMS_LIST,
            DESC_COLUMN
        ));

        // END of a drag is a different notification and must fall through to the width-flooring
        // handler below it; swallowing it here would strand a column at zero width.
        assert!(!refuses_header_drag(HDN_ENDTRACKW, ID_LIST, DESC_COLUMN));
    }
}

#[cfg(test)]
mod menu_drag_reorder_tests {
    use super::*;

    const D: (isize, bool) = (SEP_PARAM, true);

    /// Feedback 2026-10-22: a divider dragged next to another, or to either end, vanished the
    /// moment it was dropped. A drop now only moves the row, and reports where it landed, so the
    /// dropped divider (not the first one sharing [`SEP_PARAM`], A266) is the one selected.
    #[test]
    fn a_dropped_divider_stays_where_it_was_put() {
        let rows = vec![(0, true), D, (1, true), (SEP_PARAM, false), (2, true)];

        // The second divider dropped just above the first: two dividers in a row, both kept.
        let (out, at) = moved_rows(&rows, 3, 1, false).unwrap();
        assert_eq!(
            out,
            vec![(0, true), (SEP_PARAM, false), D, (1, true), (2, true)]
        );
        assert_eq!(at, 1, "the dragged divider, not the first one");

        // Dropped above the first row, and after the last: kept at either end.
        let (out, at) = moved_rows(&rows, 1, 0, false).unwrap();
        assert_eq!((out[0], at), (D, 0));
        let (out, at) = moved_rows(&rows, 1, 4, true).unwrap();
        assert_eq!((out[4], at), (D, 4));
    }

    /// Row 0 dropped just above row 3 lands at index 2 (removing it first shifts the rest up),
    /// and a drop onto itself changes nothing.
    #[test]
    fn a_drop_accounts_for_the_row_it_takes_out() {
        let rows = vec![(0, true), (1, true), (2, true), (3, true)];
        let (out, at) = moved_rows(&rows, 0, 3, false).unwrap();
        assert_eq!(out, vec![(1, true), (2, true), (0, true), (3, true)]);
        assert_eq!(at, 2);
        assert_eq!(moved_rows(&rows, 2, 2, false).unwrap(), (rows.clone(), 2));
        assert_eq!(moved_rows(&rows, -1, 0, false), None);
        assert_eq!(moved_rows(&rows, 0, 4, false), None);
    }

    /// Done tidies what the drags left: a leading or trailing divider goes, and two that meet
    /// become one, ticked if either was (a shown divider never vanishes behind a switched-off
    /// one).
    #[test]
    fn done_collapses_doubled_and_edge_dividers() {
        let rows = vec![
            (SEP_PARAM, false),
            (0, true),
            (SEP_PARAM, false),
            D,
            (1, true),
            D,
        ];
        assert_eq!(normalize_rows(&rows), vec![(0, true), D, (1, true)]);
        let tidy = vec![(0, true), D, (1, false)];
        assert_eq!(normalize_rows(&tidy), tidy);
    }

    /// The right-click menu's three edits, and the row each leaves selected.
    #[test]
    fn dividers_are_added_and_removed_around_the_row() {
        let rows = vec![(0, true), D, (1, true)];
        assert_eq!(
            edit_dividers(&rows, 0, DividerEdit::Above),
            (vec![D, (0, true), D, (1, true)], Some(0))
        );
        assert_eq!(
            edit_dividers(&rows, 2, DividerEdit::Below),
            (vec![(0, true), D, (1, true), D], Some(3))
        );
        assert_eq!(
            edit_dividers(&rows, 1, DividerEdit::Remove),
            (vec![(0, true), (1, true)], Some(1))
        );
        // Remove never takes out an item, and a row past the end changes nothing.
        assert_eq!(
            edit_dividers(&rows, 0, DividerEdit::Remove),
            (rows.clone(), Some(0))
        );
        assert_eq!(edit_dividers(&rows, 3, DividerEdit::Above), (rows, None));
        // The last row removed leaves nothing to select.
        assert_eq!(edit_dividers(&[D], 0, DividerEdit::Remove), (vec![], None));
    }

    /// Held within a row of the top or bottom edge (or past it) mid-drag, the list scrolls that
    /// way; anywhere between, it stays put.
    #[test]
    fn the_drag_scrolls_only_at_the_edges() {
        assert_eq!(drag_scroll_dir(-5, 400, 20), -1);
        assert_eq!(drag_scroll_dir(19, 400, 20), -1);
        assert_eq!(drag_scroll_dir(20, 400, 20), 0);
        assert_eq!(drag_scroll_dir(379, 400, 20), 0);
        assert_eq!(drag_scroll_dir(380, 400, 20), 1);
        assert_eq!(drag_scroll_dir(900, 400, 20), 1);
    }
}
