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
use windows::Win32::UI::Input::KeyboardAndMouse::{SetFocus, VK_DOWN, VK_ESCAPE, VK_RETURN, VK_UP};

/// One searchable control: the page that owns it, the control whose TEXT is the label,
/// and the control that should receive FOCUS on a jump (differs for label+field pairs).
/// The three `_lc` fields are computed ONCE when the index is built (`ensure_index`), not
/// re-derived on every keystroke — see that function's doc comment.
struct Entry {
    page: usize,
    label_id: i32,
    focus_id: i32,
    /// Lower-cased label text, cached at index-build time.
    label_lc: String,
    /// Lower-cased tooltip text for `label_id` / `focus_id`, cached at index-build time.
    tip_label_lc: String,
    tip_focus_lc: String,
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
    // Borderless, like every other input on this dialog: its rounded frame is PAINTED
    // (anti-aliased) behind it — by `navrail::draw_pane_header`, since the box floats over
    // that header. WS_BORDER draws a hard SQUARE 1px frame, and the rounded region clip
    // that used to sit on top of it then bit the corners off, which is exactly what made
    // the box look like its edges were chewed.
    // Geometry, all of it load-bearing:
    //   x: right edge at PANE_X + PANE_W, so the 4px-inflated frame lands on PANE_W + 4 —
    //      the same right edge the list card and the format filter frame use. The pane
    //      header is created 4px wider than PANE_W precisely so it can host that frame
    //      without clipping the right-hand round off it.
    //   h: 18, not 24. A single-line EDIT top-aligns its text, so a taller box does not
    //      centre it, it just leaves dead space underneath — the text rode 4.5px high in
    //      a 32px box. 18 + a 5/3 frame is a 26px box with the ink dead centre.
    //   y: PANE_TOP + 17 puts that 26px frame at PANE_TOP + 12, centred in the 50px header.
    let edit = ctl(
        hwnd,
        EDIT,
        "",
        WINDOW_STYLE(ES_AUTOHSCROLL as u32),
        navrail::PANE_X + navrail::PANE_W - 176,
        navrail::PANE_TOP + 17,
        176,
        18,
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
    // Keyboard path into the results dropdown: VK_DOWN moves the highlighted row,
    // VK_RETURN commits it (or the first row when none is highlighted yet).
    let _ = SetWindowSubclass(edit, Some(search_edit_subclass), 1, 0);
    // Owner-drawn (hover highlight, dark rows) with LBS_HASSTRINGS so the row text
    // still lives in the listbox itself. Borderless for the same reason as the box above:
    // the outline is stroked anti-aliased in `paint_dropdown_border`, along the rounded
    // region clip, instead of being a square frame the clip then chews.
    let list = ctl(
        hwnd,
        w!("LISTBOX"),
        "",
        WINDOW_STYLE(LBS_NOTIFY as u32 | LBS_OWNERDRAWFIXED as u32 | LBS_HASSTRINGS as u32),
        DROP_X,
        DROP_Y,
        DROP_W,
        180,
        ID_SEARCH_RESULTS,
        hinst,
    );
    hover_subclass(list);
    let _ = ShowWindow(list, SW_HIDE);
}

/// Keyboard path for the search box itself: the dialog's own accelerator table never
/// sees these keys because the box is a plain EDIT with no `DLGC_WANTALLKEYS` claim, so
/// Down/Enter used to fall through to `IsDialogMessageW` and do nothing (Down) or click
/// the default button (Enter) instead of reaching the results dropdown.
unsafe extern "system" fn search_edit_subclass(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    match msg {
        WM_NCDESTROY => {
            let _ = RemoveWindowSubclass(hwnd, Some(search_edit_subclass), 1);
        }
        WM_GETDLGCODE => return search_edit_dlgcode(hwnd, msg, wparam, lparam),
        WM_KEYDOWN => {
            if let Some(result) = search_edit_keydown(hwnd, wparam) {
                return result;
            }
        }
        _ => {}
    }
    DefSubclassProc(hwnd, msg, wparam, lparam)
}

/// WM_GETDLGCODE handling for `search_edit_subclass`: claim only the keys the dropdown
/// consumes (Down/Up/Return/Escape), and only while it is showing results. Claiming every
/// key would swallow Tab out of this box while the list is open, and an unconditional claim
/// would also swallow Enter (Save) on ordinary keystrokes. `lparam` is the MSG the dialog
/// manager is asking about, or null for a generic query.
unsafe fn search_edit_dlgcode(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let base = DefSubclassProc(hwnd, msg, wparam, lparam).0 as u32;
    let list_open = GetParent(hwnd)
        .and_then(|p| GetDlgItem(Some(p), ID_SEARCH_RESULTS))
        .is_ok_and(|l| IsWindowVisible(l).as_bool());
    if !list_open {
        return LRESULT(base as isize);
    }
    let asked = (lparam.0 as *const MSG).as_ref();
    let wanted = asked.is_some_and(|m| {
        m.message == WM_KEYDOWN
            && [VK_DOWN.0, VK_UP.0, VK_RETURN.0, VK_ESCAPE.0].contains(&(m.wParam.0 as u16))
    });
    if wanted {
        return LRESULT((base | DLGC_WANTMESSAGE) as isize);
    }
    LRESULT((base | DLGC_WANTARROWS) as isize)
}

/// WM_KEYDOWN handling for `search_edit_subclass`. Returns `Some` to return that value from
/// the subclass proc immediately, or `None` to fall through to the default `DefSubclassProc`
/// call (same `hwnd`/`msg`/`wparam`/`lparam` the caller already holds).
unsafe fn search_edit_keydown(hwnd: HWND, wparam: WPARAM) -> Option<LRESULT> {
    let vk = wparam.0 as u16;
    let Ok(parent) = GetParent(hwnd) else {
        return None;
    };
    let Ok(list) = GetDlgItem(Some(parent), ID_SEARCH_RESULTS) else {
        return None;
    };
    if !IsWindowVisible(list).as_bool() {
        return None;
    }
    let count = SendMessageW(list, LB_GETCOUNT, None, None).0 as i32;
    if count <= 0 {
        return None;
    }
    if vk == VK_ESCAPE.0 {
        // Close the dropdown, not the dialog: Escape reaches here only while the
        // list is open (see WM_GETDLGCODE), so the dialog's own cancel is untouched.
        let _ = ShowWindow(list, SW_HIDE);
        return Some(LRESULT(0));
    }
    if vk == VK_DOWN.0 || vk == VK_UP.0 {
        let cur = SendMessageW(list, LB_GETCURSEL, None, None).0 as i32;
        let next = search_edit_next_index(vk, cur, count);
        SendMessageW(list, LB_SETCURSEL, Some(WPARAM(next as usize)), None);
        // The dropdown's owner-draw highlight is driven by `HOT` (mouse hover), not
        // by LB_GETCURSEL, so a keyboard move has to update it too or the highlighted
        // row would only ever track the mouse.
        HOT.with(|h| h.set(next));
        let _ = InvalidateRect(Some(list), None, false);
        return Some(LRESULT(0));
    }
    if vk == VK_RETURN.0 {
        let cur = SendMessageW(list, LB_GETCURSEL, None, None).0 as i32;
        let sel = if cur < 0 { 0 } else { cur };
        SendMessageW(list, LB_SETCURSEL, Some(WPARAM(sel as usize)), None);
        on_pick(parent);
        return Some(LRESULT(0));
    }
    None
}

/// Next highlighted row for Up/Down in the results dropdown, wrapping at both ends.
fn search_edit_next_index(vk: u16, cur: i32, count: i32) -> i32 {
    if vk == VK_DOWN.0 {
        if cur < 0 {
            0
        } else {
            (cur + 1) % count
        }
    } else if cur < 0 {
        count - 1
    } else {
        (cur + count - 1) % count
    }
}

/// Row height for the owner-drawn dropdown, 96-DPI design px.
const ROW_H: i32 = 26;

/// Dropdown geometry (96-DPI design px), shared by `build_search` and `on_change` so the
/// created rect and the re-sized rect cannot drift apart. The right edge lines up with the
/// search box's PAINTED frame (edit right + its 4px inflation), not with the edit itself,
/// and Y clears the frame's bottom by 4px so the two rounded shapes don't touch.
const DROP_W: i32 = 330;
const DROP_X: i32 = navrail::PANE_X + navrail::PANE_W + 4 - DROP_W;
const DROP_Y: i32 = navrail::PANE_TOP + 42;

thread_local! {
    /// The row under the mouse, for the hover highlight. -1 = none.
    static HOT: std::cell::Cell<i32> = const { std::cell::Cell::new(-1) };
}

/// WM_MEASUREITEM for the dropdown (dispatched from the settings wndproc).
pub(super) unsafe fn measure_row(hwnd: HWND, m: &mut MEASUREITEMSTRUCT) {
    m.itemHeight = crate::win::dpi_scale(hwnd, ROW_H) as u32;
}

/// Read one listbox row's text via the safe two-step LB_GETTEXTLEN/LB_GETTEXT pattern
/// (mirrors restyle.rs's CB_GETLBTEXTLEN/CB_GETLBTEXT for combo boxes) instead of trusting a
/// fixed-size stack buffer: rows are built only from our own localized strings today, but
/// LB_GETTEXT writes into whatever buffer we hand it with no length check of its own, so a
/// longer string would silently overflow a fixed `[u16; N]`. Returns `None` for an
/// empty/invalid row.
unsafe fn read_row_text(list: HWND, idx: i32) -> Option<Vec<u16>> {
    let text_len = SendMessageW(list, LB_GETTEXTLEN, Some(WPARAM(idx as usize)), None).0;
    if text_len <= 0 {
        return None;
    }
    let mut buf = vec![0u16; text_len as usize + 1];
    let len = SendMessageW(
        list,
        LB_GETTEXT,
        Some(WPARAM(idx as usize)),
        Some(LPARAM(buf.as_mut_ptr() as isize)),
    )
    .0;
    if len <= 0 {
        return None;
    }
    buf.truncate(len as usize);
    Some(buf)
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
    let Some(mut buf) = read_row_text(d.hwndItem, d.itemID as i32) else {
        return;
    };
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
        &mut buf,
        &mut rc,
        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
    );
}

/// Stroke the dropdown's rounded outline, anti-aliased, along the rounded region clip
/// `on_change` applies. Two things this replaces: WS_BORDER (a SQUARE 1px frame that the
/// same clip bit the corners off — the chewed-edge look), and nothing at all covering the
/// clip's own hard staircase. Drawn after the default paint, so it sits over the rows.
unsafe fn paint_dropdown_border(list: HWND) {
    use windows::Win32::Graphics::Gdi::{GetDC, ReleaseDC};
    let hdc = GetDC(Some(list));
    if hdc.is_invalid() {
        return;
    }
    let mut rc = RECT::default();
    let _ = GetClientRect(list, &mut rc);
    let bw = crate::win::dpi_scale(list, 1).max(1);
    let rad = crate::win::dpi_scale(list, 8);
    crate::gdip::with_aa(hdc, |g| {
        let p = crate::gdip::pen(BORDER(), bw);
        crate::gdip::stroke_round(
            g,
            p,
            rc.left,
            rc.top,
            (rc.right - rc.left) - bw,
            (rc.bottom - rc.top) - bw,
            rad,
        );
        crate::gdip::drop_pen(p);
    });
    ReleaseDC(Some(list), hdc);
}

/// Subclass the dropdown for hover tracking: WM_MOUSEMOVE sets the hot row (and asks for
/// WM_MOUSELEAVE), leave clears it. Repaints only the rows that changed. Also owns the
/// dropdown's background + rounded outline (see `paint_dropdown_border`).
unsafe fn hover_subclass(list: HWND) {
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
        WM_ERASEBKGND => {
            // The rows paint their own SURFACE background, but the listbox's native erase
            // uses DARK_CTL_BG — which shows through the few pixels of slack under the last
            // row as a lighter band along the bottom. Erase in the row colour so the
            // dropdown reads as one solid card right into its rounded corners.
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            fill(HDC(wparam.0 as *mut std::ffi::c_void), &rc, SURFACE());
            return LRESULT(1);
        }
        WM_PAINT => {
            let r = DefSubclassProc(hwnd, msg, wparam, lparam);
            paint_dropdown_border(hwnd);
            return r;
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

/// Walk every page's rows and collect the controls a user would search FOR, caching each
/// entry's lower-cased label + tooltip text ONCE here instead of re-deriving it from the
/// live control (two `GetDlgItem`/`WM_GETTEXT` round trips) and re-scanning `TOOLTIPS`
/// (two linear scans) on every keystroke `on_change` runs — EN_CHANGE fires per character,
/// and this index runs ~90 entries deep. Rebuilt on the next keystroke after `invalidate()`
/// (a live language change), so the cache can't go stale.
unsafe fn ensure_index(hwnd: HWND) {
    SEARCH.with(|s| {
        let mut s = s.borrow_mut();
        if !s.entries.is_empty() {
            return;
        }
        let push = |page: usize, label_id: i32, focus_id: i32, entries: &mut Vec<Entry>| {
            entries.push(Entry {
                page,
                label_id,
                focus_id,
                label_lc: label_lc(hwnd, label_id),
                tip_label_lc: tip_lc(label_id),
                tip_focus_lc: tip_lc(focus_id),
            });
        };
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
                            push(page, id, id, &mut s.entries);
                        }
                        continue;
                    }
                    // The format filter box and the two lists aren't setting rows.
                    Wide(_) | ListFill(_) | Status(_) => continue,
                };
                push(page, label_id, focus_id, &mut s.entries);
            }
        }
    });
}

/// Lower-cased text of a control, for matching.
unsafe fn label_lc(hwnd: HWND, id: i32) -> String {
    label_text(hwnd, id).to_lowercase()
}

/// Case-preserved text of a control, for the row's DISPLAY string. `label_lc` lowercases
/// this same text for matching only: the dropdown row itself must keep the control's real
/// casing, or every result reads permanently lowercase.
unsafe fn label_text(hwnd: HWND, id: i32) -> String {
    match GetDlgItem(Some(hwnd), id) {
        Ok(c) => String::from_utf16_lossy(&control_text(c))
            .trim_end_matches('\0')
            .to_string(),
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
    ensure_index(hwnd);
    SendMessageW(list, LB_RESETCONTENT, None, None);
    let mut shown = 0usize;
    SEARCH.with(|s| {
        let mut s = s.borrow_mut();
        let mut hits: Vec<usize> = Vec::new();
        for (i, e) in s.entries.iter().enumerate() {
            if e.label_lc.is_empty() {
                continue;
            }
            let page_name = navrail::nav_label(e.page).to_lowercase();
            if e.label_lc.contains(&needle)
                || page_name.contains(&needle)
                || e.tip_label_lc.contains(&needle)
                || e.tip_focus_lc.contains(&needle)
            {
                // Row text: "Page > Label", both already localized. Re-fetch the label in its
                // real casing: the cached `label_lc` is lowercased for matching only, and must
                // not leak into what the user sees.
                let row = format!(
                    "{}  >  {}",
                    navrail::nav_label(e.page),
                    label_text(hwnd, e.label_id)
                );
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
        let w = crate::win::dpi_scale(hwnd, DROP_W);
        let x = crate::win::dpi_scale(hwnd, DROP_X);
        let y = crate::win::dpi_scale(hwnd, DROP_Y);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Plain `DefWindowProcW` forwarder with the required `extern "system"` ABI —
    /// `DefWindowProcW` itself is a normal Rust fn item, not a fn pointer of that ABI, so
    /// `WNDCLASSW::lpfnWndProc` (mirrors `win::notify_toast`'s `toast_wndproc`) can't take it
    /// directly.
    unsafe extern "system" fn test_wndproc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        DefWindowProcW(h, m, w, l)
    }

    /// A throwaway, invisible top-level window to host real child controls. `GetDlgItem`/`ctl`
    /// need a real parent/child relationship, which can't be faked without one.
    unsafe fn test_window() -> HWND {
        let hmod =
            windows::Win32::System::LibraryLoader::GetModuleHandleW(None).unwrap_or_default();
        let hinst = windows::Win32::Foundation::HINSTANCE(hmod.0);
        let class = w!("SageThumbs2KSearchTest");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(test_wndproc),
            hInstance: hinst,
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc); // ok if already registered by a prior test in this process
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!("search-test"),
            WS_POPUP,
            0,
            0,
            100,
            100,
            None,
            None,
            Some(hinst),
            None,
        )
        .expect("create throwaway test window")
    }

    /// `label_text` must preserve the control's real casing while `label_lc` lowercases the
    /// same text for matching only. Regression for the bug where the dropdown row reused
    /// `label_lc`'s output for DISPLAY too, so every search result rendered lowercase.
    #[test]
    fn label_text_preserves_case_while_label_lc_lowercases() {
        unsafe {
            let hwnd = test_window();
            let hmod =
                windows::Win32::System::LibraryLoader::GetModuleHandleW(None).unwrap_or_default();
            let hinst = windows::Win32::Foundation::HINSTANCE(hmod.0);
            let edit = ctl(
                hwnd,
                EDIT,
                "Enable Thumbnails",
                WINDOW_STYLE(0),
                0,
                0,
                50,
                12,
                9001,
                hinst,
            );
            assert!(!edit.is_invalid());

            assert_eq!(label_text(hwnd, 9001), "Enable Thumbnails");
            assert_eq!(label_lc(hwnd, 9001), "enable thumbnails");

            let _ = DestroyWindow(hwnd);
        }
    }

    /// `read_row_text` must round-trip a row longer than the old fixed 256-`u16` stack
    /// buffer without truncating it. Before the fix, LB_GETTEXT wrote into that fixed buffer
    /// with no length check of its own, so a row this long would overflow it.
    #[test]
    fn read_row_text_handles_a_row_longer_than_the_old_fixed_buffer() {
        unsafe {
            let hwnd = test_window();
            let hmod =
                windows::Win32::System::LibraryLoader::GetModuleHandleW(None).unwrap_or_default();
            let hinst = windows::Win32::Foundation::HINSTANCE(hmod.0);
            let list = ctl(
                hwnd,
                w!("LISTBOX"),
                "",
                WINDOW_STYLE(0),
                0,
                0,
                50,
                50,
                9002,
                hinst,
            );
            assert!(!list.is_invalid());

            let long = "x".repeat(300);
            let w = wide(&long);
            SendMessageW(list, LB_ADDSTRING, None, Some(LPARAM(w.as_ptr() as isize)));

            let text = read_row_text(list, 0).expect("row 0 should have text");
            assert_eq!(text.len(), long.len(), "row text must not be truncated");
            assert_eq!(String::from_utf16_lossy(&text), long);

            let _ = DestroyWindow(hwnd);
        }
    }
}
