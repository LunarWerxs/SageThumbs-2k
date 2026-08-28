//! A single subclass on the ListView does the three things SetWindowTheme can't:
//!   * dark HEADER text — the header is a child of the ListView, so its
//!     NM_CUSTOMDRAW arrives here (the theme darkens the header fill but leaves
//!     the text drawn black; only custom-draw overrides the per-item color);
//!   * SPACE bulk-toggles the checkboxes of every selected row (the control would
//!     otherwise toggle only the focused one);
//!   * right-click / Shift+F10 opens a Check / Uncheck / Toggle-selected menu.

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

/// Label for an owner-drawn format-list context-menu item.
pub(super) fn ctx_menu_label(id: usize) -> &'static str {
    match id {
        1 => t("ctx_check_selected"),
        2 => t("ctx_uncheck_selected"),
        3 => t("ctx_toggle_selected"),
        _ => "",
    }
}

pub(super) unsafe fn list_context_menu(list: HWND, owner: HWND, l: LPARAM) {
    // Keyboard invocation (Shift+F10 / Apps key) sets BOTH coords to -1 — not the
    // whole lParam — and real multi-monitor coords can be negative, so test the
    // sign-extended halves separately.
    let x = (l.0 & 0xFFFF) as u16 as i16 as i32;
    let y = ((l.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
    let (px, py) = if x == -1 && y == -1 {
        let mut r = RECT::default();
        let _ = GetWindowRect(list, &mut r);
        (r.left + 8, r.top + 8)
    } else {
        (x, y)
    };
    let menu = match CreatePopupMenu() {
        Ok(m) => m,
        Err(_) => return,
    };
    // Dark mode: owner-draw the items. A normal menu renders black text on the
    // immersive dark background (unreadable until the row is highlighted), so we
    // draw the text light ourselves in WM_DRAWITEM. Light mode uses a normal menu.
    let (s1, s2, s3);
    if is_dark() {
        let _ = AppendMenuW(menu, MF_OWNERDRAW, 1, PCWSTR(std::ptr::dangling::<u16>()));
        let _ = AppendMenuW(menu, MF_OWNERDRAW, 2, PCWSTR(std::ptr::dangling::<u16>()));
        let _ = AppendMenuW(menu, MF_OWNERDRAW, 3, PCWSTR(3 as *const u16));
    } else {
        s1 = wide(t("ctx_check_selected"));
        s2 = wide(t("ctx_uncheck_selected"));
        s3 = wide(t("ctx_toggle_selected"));
        let _ = AppendMenuW(menu, MF_STRING, 1, PCWSTR(s1.as_ptr()));
        let _ = AppendMenuW(menu, MF_STRING, 2, PCWSTR(s2.as_ptr()));
        let _ = AppendMenuW(menu, MF_STRING, 3, PCWSTR(s3.as_ptr()));
    }
    // Foreground + WM_NULL bracket: the documented fix for the "menu shows then
    // immediately vanishes" quirk. Owner is the top-level dialog, not the list.
    crate::win::force_foreground(owner);
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
    match cmd.0 {
        1 => bulk_set_selected(list, true),
        2 => bulk_set_selected(list, false),
        3 => bulk_toggle_selected(list),
        _ => {}
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
}

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
        return (count - 1, true);
    }
    let mut r = RECT::default(); // .left = LVIR_BOUNDS (0)
    SendMessageW(
        list,
        windows::Win32::UI::Controls::LVM_GETITEMRECT,
        Some(WPARAM(row as usize)),
        Some(LPARAM(&mut r as *mut _ as isize)),
    );
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

/// Invalidate (erase) the ~4px band around a mark line so a stale line there is wiped
/// and the rows under it repaint cleanly.
unsafe fn invalidate_mark_band(list: HWND, anchor: i32, after: bool) {
    if anchor < 0 {
        return;
    }
    let mut ir = RECT::default(); // .left = LVIR_BOUNDS (0)
    SendMessageW(
        list,
        windows::Win32::UI::Controls::LVM_GETITEMRECT,
        Some(WPARAM(anchor as usize)),
        Some(LPARAM(&mut ir as *mut _ as isize)),
    );
    let y = if after { ir.bottom } else { ir.top };
    let mut cr = RECT::default();
    let _ = windows::Win32::UI::WindowsAndMessaging::GetClientRect(list, &mut cr);
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
    use windows::Win32::UI::Controls::LVM_GETITEMRECT;
    let (anchor, after) = INSERT_MARK.with(|m| m.get());
    if anchor < 0 {
        return;
    }
    let mut ir = RECT::default(); // .left = LVIR_BOUNDS (0)
    SendMessageW(
        list,
        LVM_GETITEMRECT,
        Some(WPARAM(anchor as usize)),
        Some(LPARAM(&mut ir as *mut _ as isize)),
    );
    let y = if after { ir.bottom } else { ir.top };
    let mut cr = RECT::default();
    let _ = windows::Win32::UI::WindowsAndMessaging::GetClientRect(list, &mut cr);
    let line = RECT {
        left: cr.left + 2,
        top: y - 1,
        right: cr.right - 2,
        bottom: y + 1,
    };
    let hdc = GetDC(Some(list));
    let br = CreateSolidBrush(crate::dark::ACCENT());
    FillRect(hdc, &line, br);
    let _ = DeleteObject(HGDIOBJ(br.0));
    ReleaseDC(Some(list), hdc);
}

/// Collapse meaningless dividers so the list shows EXACTLY the menu it produces: drop a
/// leading divider, collapse consecutive dividers to one, drop a trailing divider (the
/// menu builder normalizes identically + adds its own divider before Settings). Keeps the
/// list truly WYSIWYG — no confusing double/edge dividers that the menu wouldn't show.
pub(super) fn normalize_rows(rows: &[(isize, bool)]) -> Vec<(isize, bool)> {
    let mut out: Vec<(isize, bool)> = Vec::with_capacity(rows.len());
    for &(p, c) in rows {
        if p == SEP_PARAM && (out.is_empty() || out.last().unwrap().0 == SEP_PARAM) {
            continue;
        }
        out.push((p, c));
    }
    while out.last().map(|r| r.0) == Some(SEP_PARAM) {
        out.pop();
    }
    out
}

/// Snapshot every row as `(lParam-key, checked)` in current display order.
unsafe fn snapshot_rows(list: HWND) -> Vec<(isize, bool)> {
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
/// their translated label + checkbox; divider rows (lParam == [`SEP_PARAM`]) get the rule
/// label and no checkbox. Optionally selects the row at display INDEX `select` — not by
/// lParam key, because every divider shares [`SEP_PARAM`] and a key match would always land
/// on the FIRST divider rather than whichever one the caller actually means (see A266 at
/// [`finish_menu_drag`]).
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
        if is_sep {
            clear_checkbox(list, row as i32);
        } else {
            set_check(list, row as i32, checked);
        }
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

/// Finish the drag: move the source row to the insertion point under the cursor by
/// rebuilding the list in the new order (key + check preserved), then reselect it.
unsafe fn finish_menu_drag(list: HWND, x: i32, y: i32) {
    let src = DRAG_SRC.with(|s| s.replace(-1));
    let _ = ReleaseCapture();
    set_insert_mark(list, -1, false);
    let count = SendMessageW(list, LVM_GETITEMCOUNT, None, None).0 as i32;
    if src < 0 || src >= count {
        return;
    }
    let (anchor, after) = insert_point(list, x, y);
    if anchor < 0 {
        return;
    }
    // Destination in the ORIGINAL list, then adjust for removing `src` first.
    let mut dest = if after { anchor + 1 } else { anchor };
    let mut rows = snapshot_rows(list);
    let elem = rows.remove(src as usize);
    if src < dest {
        dest -= 1;
    }
    let dest = (dest.max(0) as usize).min(rows.len());
    rows.insert(dest, elem);
    // Collapse any double/edge divider the drop created so the list mirrors the menu, while
    // tracking where the JUST-DROPPED row (not merely "a row with the same key") ends up. A
    // plain lParam-key lookup can't tell dividers apart — every one shares SEP_PARAM — so it
    // always resolves to the FIRST divider in the list rather than the one just dragged (A266).
    let (rows, sel_idx) = normalize_rows_tracking(&rows, dest);
    rebuild_rows(list, &rows, sel_idx);
}

/// Like [`normalize_rows`], but also reports where the row at input index `track` ended up in
/// the output, or `None` if normalization dropped that exact row (it collapsed into an earlier
/// divider, or it was a trailing divider that got trimmed). Kept as a private twin rather than
/// changing [`normalize_rows`] itself, which `settings_dlg/mod.rs` also calls and has no
/// per-row tracking need.
fn normalize_rows_tracking(
    rows: &[(isize, bool)],
    track: usize,
) -> (Vec<(isize, bool)>, Option<usize>) {
    let mut out: Vec<(isize, bool)> = Vec::with_capacity(rows.len());
    let mut tracked = None;
    for (i, &(p, c)) in rows.iter().enumerate() {
        if p == SEP_PARAM && (out.is_empty() || out.last().unwrap().0 == SEP_PARAM) {
            continue; // dropped: a leading/duplicate divider, same rule as normalize_rows
        }
        if i == track {
            tracked = Some(out.len());
        }
        out.push((p, c));
    }
    while out.last().map(|r| r.0) == Some(SEP_PARAM) {
        if tracked == Some(out.len() - 1) {
            tracked = None; // the tracked row was itself the trimmed trailing divider
        }
        out.pop();
    }
    (out, tracked)
}

/// Reset the menu-items list to its DEFAULT order (items + dividers in tree order),
/// preserving each item's current check state. Wired to the "Reset order" button.
pub(super) unsafe fn reset_menu_order(list: HWND) {
    let snap = snapshot_rows(list);
    let mut checked = vec![true; MENU_ITEM_TOGGLES.len()];
    for &(param, chk) in &snap {
        if param >= 0 && (param as usize) < MENU_ITEM_TOGGLES.len() {
            checked[param as usize] = chk;
        }
    }
    let rows = super::default_menu_rows(|i| checked[i]);
    rebuild_rows(list, &rows, None);
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
            if let Some(res) = on_notify(h, l) {
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
        WM_PAINT if DRAG_SRC.with(|s| s.get()) >= 0 => {
            let res = DefSubclassProc(h, msg, w, l);
            draw_insert_line(h);
            return res;
        }
        WM_MOUSEMOVE if DRAG_SRC.with(|s| s.get()) >= 0 => {
            let x = (l.0 & 0xFFFF) as u16 as i16 as i32;
            let y = ((l.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
            let (row, after) = insert_point(h, x, y);
            set_insert_mark(h, row, after);
            return LRESULT(0);
        }
        WM_LBUTTONUP if DRAG_SRC.with(|s| s.get()) >= 0 => {
            let x = (l.0 & 0xFFFF) as u16 as i16 as i32;
            let y = ((l.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
            finish_menu_drag(h, x, y);
            return LRESULT(0);
        }
        WM_CAPTURECHANGED if DRAG_SRC.with(|s| s.get()) >= 0 => {
            // Capture pulled away (Esc / another window) — cancel cleanly.
            DRAG_SRC.with(|s| s.set(-1));
            set_insert_mark(h, -1, false);
        }
        // WM_CONTEXTMENU is handled in the dialog proc (it bubbles to the parent).
        _ => {}
    }
    DefSubclassProc(h, msg, w, l)
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
    // is written for the three-column file-types list specifically, so pointing it at
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
    const MIN_COL_W: i32 = 40;
    let col = (*hdn).iItem;
    if (0..2).contains(&col) {
        let w = SendMessageW(
            h,
            windows::Win32::UI::Controls::LVM_GETCOLUMNWIDTH,
            Some(WPARAM(col as usize)),
            None,
        )
        .0 as i32;
        if w < MIN_COL_W {
            SendMessageW(
                h,
                LVM_SETCOLUMNWIDTH,
                Some(WPARAM(col as usize)),
                Some(LPARAM(MIN_COL_W as isize)),
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
        // grabber, and since HDS_NOSIZING the columns can't be dragged at all.
        // The inner dividers stay: those separate real columns.
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

/// Index of the Description column in the file-types list: the LAST of the three.
pub(super) const DESC_COLUMN: i32 = 2;

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

    /// A266: dragging a divider row past ANOTHER divider must reselect the one that was just
    /// dropped, not merely "a row with the same lParam" — every divider shares [`SEP_PARAM`],
    /// so a naive key match always resolves to the FIRST divider in the list. Here the two
    /// dividers aren't adjacent (an item sits between them), so normalize collapses nothing —
    /// this pins that the tracker still finds the SECOND one, not the first.
    #[test]
    fn tracks_the_dropped_divider_past_an_earlier_divider() {
        let rows = vec![
            (0, true),
            (SEP_PARAM, false),
            (1, true),
            (SEP_PARAM, false), // dropped here, index 3
            (2, true),
        ];
        let (out, sel) = normalize_rows_tracking(&rows, 3);
        assert_eq!(
            out, rows,
            "no adjacent/edge dividers here, so nothing collapses"
        );
        assert_eq!(
            sel,
            Some(3),
            "must track the SPECIFIC divider instance dropped at index 3, not index 1"
        );
    }

    /// Dropping a divider directly after an existing one collapses the pair (normalize_rows'
    /// existing rule): the JUST-DROPPED instance is the one that gets skipped, so there is no
    /// surviving row that IS the one the user dropped — `None` (no forced selection) is the
    /// honest answer, and strictly better than the old bug of highlighting an unrelated divider.
    #[test]
    fn tracks_none_when_the_dropped_divider_collapses_into_an_earlier_one() {
        let rows = vec![
            (0, true),
            (SEP_PARAM, false),
            (SEP_PARAM, false), // dropped here, index 2, collapses into index 1
            (1, true),
        ];
        let (out, sel) = normalize_rows_tracking(&rows, 2);
        assert_eq!(out, vec![(0, true), (SEP_PARAM, false), (1, true)]);
        assert_eq!(
            sel, None,
            "the dropped instance was itself the one collapsed away"
        );
    }

    /// A divider dropped at the very end is trimmed as a trailing divider — there is no row
    /// left to select, so the tracker must say so rather than pointing past the end (or, worse,
    /// silently pointing at whatever now happens to occupy that index).
    #[test]
    fn tracks_none_when_the_dropped_row_is_itself_trimmed_as_trailing() {
        let rows = vec![(0, true), (SEP_PARAM, false)];
        let (out, sel) = normalize_rows_tracking(&rows, 1);
        assert_eq!(out, vec![(0, true)]);
        assert_eq!(
            sel, None,
            "the trimmed trailing divider has no surviving row to select"
        );
    }

    /// An ordinary item row (not a divider) is unaffected by any of the divider-collapsing
    /// rules, so its tracked index simply carries through unchanged.
    #[test]
    fn tracks_a_plain_item_row_unchanged() {
        let rows = vec![(SEP_PARAM, false), (0, true), (1, false)];
        let (out, sel) = normalize_rows_tracking(&rows, 1);
        // The leading divider is dropped, shifting index 1 down to output index 0.
        assert_eq!(out, vec![(0, true), (1, false)]);
        assert_eq!(sel, Some(0));
    }
}
