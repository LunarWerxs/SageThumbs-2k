//! Settings-wide search: an always-visible box at the top-right of the pane header that
//! filters EVERY page's controls by their (already-localized) label, tooltip text, and
//! page name, and jumps to the match — page switch + focus — on click.
//!
//! Why: nine pages and forty-odd switches make "where is that setting" the dominant cost
//! of the dialog, and the per-format filter on File types only searches file formats.
//! This search indexes the strings the dialog ALREADY owns (control captions, the
//! `TOOLTIPS` table), so it costs nothing in translation and can never drift from what
//! the user actually sees on screen.
//!
//! Shape: EN_CHANGE rebuilds the hit list into a dropdown LISTBOX under the box; picking
//! a row switches to the owning page, focuses the control (with focus rings forced
//! visible, so the jump lands somewhere the eye can find), and hides the dropdown. The
//! index is built lazily on first use — after `apply_v3_layout` has created and labeled
//! every control — and rebuilt on a live language change (`relabel` calls `invalidate`).

use super::*;
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;

/// One searchable control: the page that owns it, the control whose TEXT is the label,
/// and the control that should receive FOCUS on a jump (differs for label+field pairs).
struct Entry {
    page: usize,
    label_id: i32,
    focus_id: i32,
}

/// The lazily-built index + the hit list backing the visible dropdown rows.
#[derive(Default)]
struct SearchState {
    entries: Vec<Entry>,
    /// `hits[i]` = index into `entries` for dropdown row `i`.
    hits: Vec<usize>,
}

thread_local! {
    static SEARCH: std::cell::RefCell<SearchState> = std::cell::RefCell::new(SearchState::default());
}

/// Create the search box + its (hidden) results dropdown. Called from `apply_v3_layout`,
/// OUTSIDE the per-category control lists, so both stay visible on every page.
pub(super) unsafe fn build_search(hwnd: HWND, hinst: HINSTANCE) {
    use crate::win::{ctl, EDIT};
    let edit = ctl(
        hwnd,
        EDIT,
        "",
        WINDOW_STYLE(ES_AUTOHSCROLL as u32 | WS_BORDER.0),
        navrail::PANE_X + navrail::PANE_W - 176,
        navrail::PANE_TOP + 12,
        176,
        24,
        ID_SEARCH_GLOBAL,
        hinst,
    );
    // The box floats over the owner-drawn pane header, which fills its whole rect on
    // every repaint — the edit must sit ABOVE it in the sibling z-order or it never shows.
    let _ = SetWindowPos(
        edit,
        Some(HWND_TOP),
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
    );
    // Cue banner: the affordance text lives inside the box, costing no layout row.
    let cue = wide(t("search_settings_cue"));
    SendMessageW(
        edit,
        EM_SETCUEBANNER,
        Some(WPARAM(1)), // keep the cue while focused, until typing starts
        Some(LPARAM(cue.as_ptr() as isize)),
    );
    // Owner-drawn (hover highlight, dark rows) with LBS_HASSTRINGS so the row text
    // still lives in the listbox itself.
    let list = ctl(
        hwnd,
        w!("LISTBOX"),
        "",
        WINDOW_STYLE(
            LBS_NOTIFY as u32 | LBS_OWNERDRAWFIXED as u32 | LBS_HASSTRINGS as u32 | WS_BORDER.0,
        ),
        navrail::PANE_X + navrail::PANE_W - 330,
        navrail::PANE_TOP + 38,
        330,
        180,
        ID_SEARCH_RESULTS,
        hinst,
    );
    hover_subclass(list);
    super::restyle::round_corners(hwnd, edit, 6);
    let _ = ShowWindow(list, SW_HIDE);
}

/// Row height for the owner-drawn dropdown, 96-DPI design px.
const ROW_H: i32 = 26;

thread_local! {
    /// The row under the mouse, for the hover highlight. -1 = none.
    static HOT: std::cell::Cell<i32> = const { std::cell::Cell::new(-1) };
}

/// WM_MEASUREITEM for the dropdown (dispatched from the settings wndproc).
pub(super) unsafe fn measure_row(hwnd: HWND, m: &mut MEASUREITEMSTRUCT) {
    m.itemHeight = crate::win::dpi_scale(hwnd, ROW_H) as u32;
}

/// WM_DRAWITEM for the dropdown: dark row, accent-tinted hover, "Page > Label" text.
pub(super) unsafe fn draw_row(hwnd: HWND, d: &DRAWITEMSTRUCT) {
    use windows::Win32::Graphics::Gdi::{SetBkMode, SetTextColor, TRANSPARENT};
    let hot = HOT.with(|h| h.get());
    let bg = if d.itemID as i32 == hot {
        navrail::blend(ACCENT(), SURFACE(), 22)
    } else {
        SURFACE()
    };
    fill(d.hDC, &d.rcItem, bg);
    if (d.itemID as i32) < 0 {
        return;
    }
    let mut buf = [0u16; 256];
    let len = SendMessageW(
        d.hwndItem,
        LB_GETTEXT,
        Some(WPARAM(d.itemID as usize)),
        Some(LPARAM(buf.as_mut_ptr() as isize)),
    )
    .0;
    if len <= 0 {
        return;
    }
    SetBkMode(d.hDC, TRANSPARENT);
    SetTextColor(d.hDC, DARK_TEXT());
    windows::Win32::Graphics::Gdi::SelectObject(
        d.hDC,
        windows::Win32::Graphics::Gdi::HGDIOBJ(gui_font_for(hwnd).0),
    );
    let mut rc = d.rcItem;
    rc.left += crate::win::dpi_scale(hwnd, 10);
    DrawTextW(
        d.hDC,
        &mut buf[..len as usize],
        &mut rc,
        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
    );
}

/// Subclass the dropdown for hover tracking: WM_MOUSEMOVE sets the hot row (and asks for
/// WM_MOUSELEAVE), leave clears it. Repaints only the rows that changed.
unsafe fn hover_subclass(list: HWND) {
    use windows::Win32::UI::Shell::SetWindowSubclass;
    let _ = SetWindowSubclass(list, Some(hover_proc), 1, 0);
}

unsafe extern "system" fn hover_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    use windows::Win32::UI::Controls::WM_MOUSELEAVE;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
    };
    use windows::Win32::UI::Shell::DefSubclassProc;
    match msg {
        WM_MOUSEMOVE => {
            let hit = SendMessageW(hwnd, LB_ITEMFROMPOINT, None, Some(lparam)).0;
            // High word set = outside the client area; low word = index.
            let idx = if (hit >> 16) & 0xFFFF == 0 {
                (hit & 0xFFFF) as i32
            } else {
                -1
            };
            let prev = HOT.with(|h| h.replace(idx));
            if prev != idx {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            let mut tme = TRACKMOUSEEVENT {
                cbSize: core::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            let _ = TrackMouseEvent(&mut tme);
        }
        WM_MOUSELEAVE if HOT.with(|h| h.replace(-1)) != -1 => {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
        _ => {}
    }
    DefSubclassProc(hwnd, msg, wparam, lparam)
}

/// Drop the built index (live language change re-labels every control, so the cached
/// label/tooltip text is stale). It rebuilds on the next keystroke.
pub(super) fn invalidate() {
    SEARCH.with(|s| s.borrow_mut().entries.clear());
}

/// Re-set the cue banner in the (possibly just-changed) active language.
pub(super) unsafe fn refresh_cue(hwnd: HWND) {
    if let Ok(edit) = GetDlgItem(Some(hwnd), ID_SEARCH_GLOBAL) {
        let cue = wide(t("search_settings_cue"));
        SendMessageW(
            edit,
            EM_SETCUEBANNER,
            Some(WPARAM(1)),
            Some(LPARAM(cue.as_ptr() as isize)),
        );
    }
}

/// Walk every page's rows and collect the controls a user would search FOR. Labels are
/// read live from the controls (already localized); tooltips join the haystack at match
/// time via the shared `TOOLTIPS` table.
unsafe fn ensure_index() {
    SEARCH.with(|s| {
        let mut s = s.borrow_mut();
        if !s.entries.is_empty() {
            return;
        }
        for page in 0..navrail::NCAT {
            for &row in navrail::cat_rows(page) {
                use navrail::Row::*;
                let (label_id, focus_id) = match row {
                    Switch(id) | Btn(id, _) | Head(id) => (id, id),
                    Pair(lbl, field, _, _) => (lbl, field),
                    BtnStatus(bid, _, _) | StatusBtn(_, bid, _) => (bid, bid),
                    // Btn3 rows are Select all/Clear all/Defaults — reachable, useful.
                    Btn3(a, b, c) => {
                        for id in [a, b, c] {
                            s.entries.push(Entry {
                                page,
                                label_id: id,
                                focus_id: id,
                            });
                        }
                        continue;
                    }
                    // The format filter box and the two lists aren't setting rows.
                    Wide(_) | ListFill(_) | Status(_) => continue,
                };
                s.entries.push(Entry {
                    page,
                    label_id,
                    focus_id,
                });
            }
        }
    });
}

/// Lower-cased text of a control, for matching.
unsafe fn label_lc(hwnd: HWND, id: i32) -> String {
    match GetDlgItem(Some(hwnd), id) {
        Ok(c) => String::from_utf16_lossy(&control_text(c))
            .trim_end_matches('\0')
            .to_lowercase(),
        Err(_) => String::new(),
    }
}

/// Tooltip text for a control id, from the shared table (empty when it has none).
fn tip_lc(id: i32) -> String {
    TOOLTIPS
        .iter()
        .find(|(tid, _)| *tid == id)
        .map(|(_, key)| t(key).to_lowercase())
        .unwrap_or_default()
}

/// EN_CHANGE: rebuild the dropdown for the current needle. Under 2 chars hides it — a
/// 1-char needle matches half the dialog and reads as noise, not as search results.
pub(super) unsafe fn on_change(hwnd: HWND) {
    let needle = get_edit_text(hwnd, ID_SEARCH_GLOBAL).trim().to_lowercase();
    let Ok(list) = GetDlgItem(Some(hwnd), ID_SEARCH_RESULTS) else {
        return;
    };
    if needle.len() < 2 {
        let _ = ShowWindow(list, SW_HIDE);
        return;
    }
    ensure_index();
    SendMessageW(list, LB_RESETCONTENT, None, None);
    let mut shown = 0usize;
    SEARCH.with(|s| {
        let mut s = s.borrow_mut();
        let mut hits: Vec<usize> = Vec::new();
        for (i, e) in s.entries.iter().enumerate() {
            let label = label_lc(hwnd, e.label_id);
            if label.is_empty() {
                continue;
            }
            let page_name = navrail::nav_label(e.page).to_lowercase();
            if label.contains(&needle)
                || page_name.contains(&needle)
                || tip_lc(e.label_id).contains(&needle)
                || tip_lc(e.focus_id).contains(&needle)
            {
                // Row text: "Page > Label", both already localized.
                let row = format!("{}  >  {}", navrail::nav_label(e.page), label);
                let w = wide(&row);
                SendMessageW(list, LB_ADDSTRING, None, Some(LPARAM(w.as_ptr() as isize)));
                hits.push(i);
                shown += 1;
                if shown >= 12 {
                    break; // a dozen rows is a usable dropdown; past that, keep typing
                }
            }
        }
        s.hits = hits;
    });
    if shown > 0 {
        // Size the dropdown to EXACTLY its rows (the fixed 180px box left dead space
        // under short result lists), then re-clip the rounded corners to the new size.
        let h = crate::win::dpi_scale(hwnd, ROW_H * shown as i32 + 4);
        let w = crate::win::dpi_scale(hwnd, 330);
        let x = crate::win::dpi_scale(hwnd, navrail::PANE_X + navrail::PANE_W - 330);
        let y = crate::win::dpi_scale(hwnd, navrail::PANE_TOP + 38);
        let _ = SetWindowPos(
            list,
            Some(HWND_TOP),
            x,
            y,
            w,
            h,
            SWP_SHOWWINDOW | SWP_NOACTIVATE,
        );
        super::restyle::round_corners(hwnd, list, 8);
        HOT.with(|hot| hot.set(-1));
    } else {
        let _ = ShowWindow(list, SW_HIDE);
    }
}

/// LBN_SELCHANGE: jump to the picked control — switch page, force focus rings visible,
/// focus it, and put the dropdown away.
pub(super) unsafe fn on_pick(hwnd: HWND) {
    let Ok(list) = GetDlgItem(Some(hwnd), ID_SEARCH_RESULTS) else {
        return;
    };
    let sel = SendMessageW(list, LB_GETCURSEL, None, None).0;
    if sel < 0 {
        return;
    }
    let target = SEARCH.with(|s| {
        let s = s.borrow();
        s.hits
            .get(sel as usize)
            .and_then(|&i| s.entries.get(i).map(|e| (e.page, e.focus_id)))
    });
    let Some((page, focus_id)) = target else {
        return;
    };
    navrail::switch_category(hwnd, page);
    // Show focus rectangles (normally hidden until the keyboard is used) so the landing
    // spot is visible, then focus the control the row described.
    SendMessageW(
        hwnd,
        WM_CHANGEUISTATE,
        Some(WPARAM(
            (UIS_CLEAR as usize) | ((UISF_HIDEFOCUS as usize) << 16),
        )),
        None,
    );
    if let Ok(c) = GetDlgItem(Some(hwnd), focus_id) {
        let _ = SetFocus(Some(c));
    }
    let _ = ShowWindow(list, SW_HIDE);
}
