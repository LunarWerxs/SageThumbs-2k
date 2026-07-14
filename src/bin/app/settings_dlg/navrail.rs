//! v3 nav-rail + content-pane layout (extracted from settings_dlg; parent-hub pattern).

use super::*;
use crate::gdip;

// ===================== v3 layout: nav rail + content pane =====================
// Geometry (96-dpi design px). The window is nav rail (left) + a content pane that
// shows ONE category at a time; the rest of the controls are hidden.
pub(super) const NAV_X: i32 = 8;
pub(super) const NAV_TOP: i32 = 14;
pub(super) const NAV_W: i32 = 188;
pub(super) const NAV_ITEM_H: i32 = 38;
pub(super) const PANE_X: i32 = 212;
pub(super) const PANE_W: i32 = 528;
pub(super) const PANE_TOP: i32 = 16;
pub(super) const PANE_HEAD_H: i32 = 50; // the icon-chip + title + blurb page header
pub(super) const ID_NAV_BASE: i32 = 1700; // nav items occupy ID_NAV_BASE .. ID_NAV_BASE+NCAT (1700..1708)
pub(super) const ID_PANE_HEADER: i32 = 1710;
pub(super) const NCAT: usize = 9;
/// Localized nav-rail / page-header label for category `ci`. Pulls from `t()` so a
/// live language switch re-texts it (the nav statics + pane header re-read this).
pub(super) fn nav_label(ci: usize) -> &'static str {
    match ci {
        0 => t("nav_general"),
        1 => t("nav_filetypes"),
        2 => t("nav_ebook"),
        3 => t("nav_menu"),
        4 => t("nav_screenshots"),
        5 => t("nav_quickaction"),
        6 => t("nav_advanced"),
        7 => t("nav_quickpreview"),
        _ => t("nav_databackup"),
    }
}

/// One row in a category's content pane. Ids reference controls already created by
/// `build_controls`; the layout just repositions them.
#[derive(Clone, Copy)]
pub(super) enum Row {
    Head(i32),               // group sub-header (owner-draw static)
    Switch(i32),             // a checkbox, drawn as a toggle switch
    Pair(i32, i32, i32, i32),// label, field, field_w, field_h (combo field_h>40)
    Btn(i32, i32),           // button id, width
    BtnStatus(i32, i32, i32),// button (left) + right-aligned status, one row: btn_id, btn_w, status_id
    StatusBtn(i32, i32, i32),// status (left, fills) + right-aligned button, one row: status_id, btn_id, btn_w
    Status(i32),             // dynamic status line
    Btn3(i32, i32, i32),     // three equal buttons on one row
    Wide(i32),               // a full-width control (search edit)
    ListFill(i32),           // a list that fills down to the footer
    ListAuto(i32),           // a list that keeps its measured height
}

// Category order: General (Thumbnails+General merged) · File types · Ebook/comic ·
// Right-click menu · Screenshots · Advanced.
pub(super) fn cat_rows(ci: usize) -> &'static [Row] {
    use Row::*;
    match ci {
        0 => &[
            // General = the merged Thumbnails + General (Custom action is its own tab now).
            Switch(ID_ENABLE_THUMBS), Switch(ID_USE_EMBEDDED), Switch(ID_MENU_CHECKER),
            Head(ID_LBL_LIMITS),
            Pair(ID_LBL_MAXFILE, ID_MAXSIZE, 84, 18), Pair(ID_LBL_MAXTHUMB, ID_SIZE, 84, 18),
            Pair(ID_LBL_JPEG, ID_JPEG, 84, 18), Pair(ID_LBL_PNG, ID_PNG, 84, 18),
            Head(ID_LBL_GENERAL), // "Language & files"
            Pair(ID_LBL_LANG, ID_LANG, 156, 200), Switch(ID_PRESERVE_DATE),
        ],
        1 => &[
            Btn3(ID_SELECT_ALL, ID_CLEAR_ALL, ID_DEFAULTS),
            Wide(ID_SEARCH),
            ListFill(ID_LIST),
        ],
        2 => &[
            // Ebook/comic — its own tab now.
            Switch(ID_C_SORT), Switch(ID_C_PREFER_COVER), Switch(ID_C_SKIP_SCAN),
        ],
        3 => &[
            Switch(ID_ENABLE_MENU), Switch(ID_MENU_ALL_TYPES), Switch(ID_MENU_QUICK),
            Pair(ID_LBL_PREVIEW, ID_MENU_PREVIEW, 156, 200),
            Head(ID_LBL_MENU_ITEMS),
            ListAuto(ID_MENU_ITEMS_LIST), Btn(ID_MENU_RESET, 110),
        ],
        4 => &[
            // Screenshots — custom action moved to General; hotkey service + "Hide tray icon" to Advanced.
            Switch(ID_SHOT_ENABLE), Switch(ID_SHOT_QUICK_ENABLE), Switch(ID_SHOT_USE_DIR),
            Pair(ID_LBL_SHOT_HK, ID_SHOT_HOTKEY, 156, 200),
            Pair(ID_LBL_SHOT_QUICK_HK, ID_SHOT_QUICK_HOTKEY, 156, 200),
            Status(ID_SHOT_DIR), Btn(ID_SHOT_SET_DIR, 150),
            Btn(ID_EDIT_UPLOAD_HOSTS, 184),
        ],
        5 => &[
            // Quick action — bind a global hotkey to run a tool.
            Switch(ID_CUSTOM_ACTION_ENABLE),
            Pair(ID_LBL_SHOT_ACTION, ID_SHOT_ACTION, 156, 200),
            Pair(ID_LBL_SHOT_ACTION_HK, ID_SHOT_ACTION_HK, 156, 200),
        ],
        6 => &[
            // Advanced — system behaviors only: Diagnostics / Updates / Hotkey service.
            // (Settings sync + Backup moved to their own "Data & Backup" tab.)
            Head(ID_LBL_DIAG),
            Switch(ID_VERBOSE_LOG),
            Btn(ID_OPEN_LOG, 320),
            Btn(ID_REBUILD_CACHE, 320),
            Btn(ID_REPAIR_ASSOC, 320),
            Head(ID_LBL_UPDATES),
            Switch(ID_UPDATE_AUTO), Btn(ID_CHECK_UPDATES, 184),
            Head(ID_LBL_HOTKEY_SVC),
            BtnStatus(ID_SHOT_RESTART, 184, ID_SHOT_STATUS), Switch(ID_SHOT_HIDE_TRAY),
        ],
        // Quick preview — QuickLook-style "press Space, see the file". The master toggle drives
        // daemon residency (like Screenshots); the rest are viewer prefs. The HTML/.url rows only
        // exist when the `html-preview` feature is compiled in.
        #[cfg(feature = "html-preview")]
        7 => &[
            Switch(ID_PREVIEW_ENABLED), Switch(ID_PREVIEW_HOLD_PEEK),
            Switch(ID_PREVIEW_CLOSE_FOCUS), Switch(ID_PREVIEW_TOPMOST),
            Switch(ID_PREVIEW_TEXT), Switch(ID_PREVIEW_MARKDOWN),
            Switch(ID_PREVIEW_MD_REMOTE),
            Switch(ID_PREVIEW_HTML), Switch(ID_PREVIEW_URL_LIVE),
        ],
        #[cfg(not(feature = "html-preview"))]
        7 => &[
            Switch(ID_PREVIEW_ENABLED), Switch(ID_PREVIEW_HOLD_PEEK),
            Switch(ID_PREVIEW_CLOSE_FOCUS), Switch(ID_PREVIEW_TOPMOST),
            Switch(ID_PREVIEW_TEXT), Switch(ID_PREVIEW_MARKDOWN),
            Switch(ID_PREVIEW_MD_REMOTE),
        ],
        _ => &[
            // Data & Backup — settings portability: optional cloud sync + local backup/restore.
            // Controls are created in build_controls; listing them here places them into this
            // pane + registers them for nav show/hide.
            Head(ID_LBL_SYNC),
            StatusBtn(ID_SYNC_STATUS, ID_SYNC_BTN, 160),
            Head(ID_LBL_BACKUP),
            Btn(ID_RESET_ALL, 320),
            Btn(ID_IMPORT, 320),
            Btn(ID_EXPORT, 320),
        ],
    }
}

#[derive(Default)]
pub(super) struct NavState {
    pub(super) active: usize,
    pub(super) cats: Vec<Vec<HWND>>,
}
thread_local! {
    pub(super) static NAV: std::cell::RefCell<NavState> = std::cell::RefCell::new(NavState::default());
}

/// Mix `pct`% of `fg` over `bg` (both 0x00BBGGRR COLORREFs) — for accent tints.
pub(super) fn blend(fg: COLORREF, bg: COLORREF, pct: i32) -> COLORREF {
    let ch = |sh: u32| {
        let a = ((fg.0 >> sh) & 0xFF) as i32;
        let b = ((bg.0 >> sh) & 0xFF) as i32;
        (((a * pct + b * (100 - pct)) / 100) as u32) & 0xFF
    };
    COLORREF(ch(0) | (ch(8) << 8) | (ch(16) << 16))
}

/// Draw a category's line icon (matching the v3 web SVGs) in an `sz`×`sz` box at
/// `(x, y)`, stroked in `color`. Hollow shapes, anti-aliased with round caps/joins via
/// GDI+ (so the diagonals and rounded corners read as clean Fluent line icons instead of
/// the stair-stepped raw-GDI strokes they used to be).
pub(super) unsafe fn draw_cat_icon(hdc: HDC, ci: usize, x: i32, y: i32, sz: i32, color: COLORREF) {
    let pw = (sz / 8).max(1);
    // Map the 24-unit SVG space into the box: x-coords via mx, y-coords via my.
    let mx = |v: i32| x + v * sz / 24;
    let my = |v: i32| y + v * sz / 24;
    gdip::with_aa(hdc, |g| {
        let p = gdip::pen_round(color, pw);
        // rounded-rect outline / ellipse outline / polyline, all in the 24-unit SVG space.
        let rr = |a: i32, b: i32, c: i32, d: i32, r: i32| {
            gdip::stroke_round(g, p, mx(a), my(b), mx(c) - mx(a), my(d) - my(b), r);
        };
        let el = |a: i32, b: i32, c: i32, d: i32| {
            gdip::ellipse(g, p, mx(a), my(b), mx(c) - mx(a), my(d) - my(b));
        };
        let ln = |pts: &[(i32, i32)]| {
            let mapped: Vec<(i32, i32)> = pts.iter().map(|&(a, b)| (mx(a), my(b))).collect();
            gdip::polyline(g, p, &mapped);
        };
        match ci {
            0 => {
                // image: framed rect + sun + mountain
                rr(3, 3, 21, 21, sz / 4);
                el(6, 6, 11, 11);
                ln(&[(21, 15), (16, 10), (5, 21)]);
            }
            1 => {
                // grid: four rounded squares
                for (gx, gy) in [(3, 3), (13, 3), (3, 13), (13, 13)] {
                    rr(gx, gy, gx + 8, gy + 8, sz / 8);
                }
            }
            2 => {
                // book: cover + spine + page lines (Ebook/comic)
                rr(5, 4, 19, 20, sz / 8);
                ln(&[(8, 4), (8, 20)]);
                ln(&[(11, 9), (16, 9)]);
                ln(&[(11, 13), (16, 13)]);
            }
            3 => {
                // menu: three lines (last shorter)
                for (yy, x2) in [(6, 20), (12, 20), (18, 14)] {
                    ln(&[(4, yy), (x2, yy)]);
                }
            }
            4 => {
                // camera: body + bump + lens
                rr(3, 8, 21, 19, sz / 8);
                ln(&[(8, 8), (9, 6), (15, 6), (16, 8)]);
                el(9, 10, 15, 16);
            }
            5 => {
                // bolt: a lightning shape (Quick action)
                ln(&[(13, 2), (7, 13), (11, 13), (10, 22), (18, 10), (12, 10), (13, 2)]);
            }
            6 => {
                // sliders: two lines, each with a knob (Advanced)
                ln(&[(4, 8), (20, 8)]);
                ln(&[(4, 16), (20, 16)]);
                el(13, 5, 19, 11);
                el(5, 13, 11, 19);
            }
            7 => {
                // eye: a wide almond outline + a round iris (Quick preview)
                el(3, 8, 21, 16);
                el(10, 9, 14, 15);
            }
            _ => {
                // save/backup: a down-arrow into an open tray (Data & Backup)
                ln(&[(12, 3), (12, 14)]);
                ln(&[(8, 10), (12, 14), (16, 10)]);
                ln(&[(4, 16), (4, 21), (20, 21), (20, 16)]);
            }
        }
        gdip::drop_pen(p);
    });
}

pub(super) fn cat_blurb(ci: usize) -> &'static str {
    match ci {
        0 => t("blurb_general"),
        1 => t("blurb_filetypes"),
        2 => t("blurb_ebook"),
        3 => t("blurb_menu"),
        4 => t("blurb_screenshots"),
        5 => t("blurb_quickaction"),
        6 => t("blurb_advanced"),
        7 => t("blurb_quickpreview"),
        _ => t("blurb_databackup"),
    }
}

/// Owner-draw a nav-rail item: an accent-tinted pill + accent icon + bar when
/// active; a muted icon + plain text otherwise.
pub(super) unsafe fn draw_nav_item(hwnd: HWND, d: &DRAWITEMSTRUCT, active: bool) {
    let hdc = d.hDC;
    let rc = d.rcItem;
    let ci = (d.CtlID as i32 - ID_NAV_BASE) as usize;
    fill(hdc, &rc, DARK_BG());
    if active {
        let tint = blend(ACCENT(), DARK_BG(), 16);
        let (px, py) = (rc.left + dpi_scale(hwnd, 4), rc.top + dpi_scale(hwnd, 3));
        let (pw, ph) = ((rc.right - dpi_scale(hwnd, 4)) - px, (rc.bottom - dpi_scale(hwnd, 3)) - py);
        gdip::with_aa(hdc, |g| {
            let b = gdip::brush(tint);
            gdip::fill_round(g, b, px, py, pw, ph, dpi_scale(hwnd, 8));
            gdip::drop_brush(b);
        });
        let bar = RECT {
            left: rc.left,
            top: rc.top + dpi_scale(hwnd, 10),
            right: rc.left + dpi_scale(hwnd, 3),
            bottom: rc.bottom - dpi_scale(hwnd, 10),
        };
        fill(hdc, &bar, ACCENT());
    }
    let isz = dpi_scale(hwnd, 17);
    let iy = rc.top + (rc.bottom - rc.top - isz) / 2;
    draw_cat_icon(hdc, ci, rc.left + dpi_scale(hwnd, 16), iy, isz, if active { ACCENT() } else { HEADER_TEXT() });
    SelectObject(hdc, HGDIOBJ(gui_font_for(hwnd).0));
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, DARK_TEXT());
    let mut label = control_text(d.hwndItem);
    let n = label.len().saturating_sub(1);
    let mut tr = RECT { left: rc.left + dpi_scale(hwnd, 44), ..rc };
    DrawTextW(hdc, &mut label[..n], &mut tr, DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX);
}

/// Owner-draw the per-pane header: an accent-tinted icon chip + the active
/// category's bold title + a muted blurb (the v3 page-header look).
pub(super) unsafe fn draw_pane_header(hwnd: HWND, d: &DRAWITEMSTRUCT) {
    let hdc = d.hDC;
    let rc = d.rcItem;
    fill(hdc, &rc, DARK_BG());
    let ci = NAV.with(|n| n.borrow().active);
    let chip = dpi_scale(hwnd, 34);
    let tint = blend(ACCENT(), DARK_BG(), 16);
    gdip::with_aa(hdc, |g| {
        let b = gdip::brush(tint);
        gdip::fill_round(g, b, rc.left, rc.top, chip, chip, dpi_scale(hwnd, 9));
        gdip::drop_brush(b);
    });
    let isz = dpi_scale(hwnd, 18);
    draw_cat_icon(hdc, ci, rc.left + (chip - isz) / 2, rc.top + (chip - isz) / 2, isz, ACCENT());
    let tx = rc.left + dpi_scale(hwnd, 46);
    SelectObject(hdc, HGDIOBJ(crate::win::gui_font_title(hwnd).0));
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, DARK_TEXT());
    let mut title = wide(nav_label(ci));
    let tn = title.len().saturating_sub(1);
    let mut tr = RECT { left: tx, top: rc.top - dpi_scale(hwnd, 2), right: rc.right, bottom: rc.top + dpi_scale(hwnd, 24) };
    DrawTextW(hdc, &mut title[..tn], &mut tr, DT_LEFT | DT_SINGLELINE | DT_NOPREFIX);
    SelectObject(hdc, HGDIOBJ(gui_font_for(hwnd).0));
    SetTextColor(hdc, HEADER_TEXT());
    let mut blurb = wide(cat_blurb(ci));
    let bn = blurb.len().saturating_sub(1);
    let mut br = RECT { left: tx, top: rc.top + dpi_scale(hwnd, 26), right: rc.right, bottom: rc.bottom };
    DrawTextW(hdc, &mut blurb[..bn], &mut br, DT_LEFT | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS);
}

/// Show category `ci`'s controls, hide the others, repaint the nav + pane.
pub(super) unsafe fn switch_category(hwnd: HWND, ci: usize) {
    NAV.with(|n| {
        let mut n = n.borrow_mut();
        n.active = ci;
        for (i, ctrls) in n.cats.iter().enumerate() {
            let cmd = if i == ci { SW_SHOW } else { SW_HIDE };
            for &c in ctrls {
                let _ = ShowWindow(c, cmd);
            }
        }
    });
    for i in 0..NCAT as i32 {
        if let Ok(nav) = GetDlgItem(Some(hwnd), ID_NAV_BASE + i) {
            let _ = InvalidateRect(Some(nav), None, true);
        }
    }
    if let Ok(ph) = GetDlgItem(Some(hwnd), ID_PANE_HEADER) {
        let _ = InvalidateRect(Some(ph), None, true);
    }
    let _ = InvalidateRect(Some(hwnd), None, true);
}

pub(super) unsafe fn apply_v3_layout(hwnd: HWND, hinst: HINSTANCE) {
    // Hide the old scrolling chrome + the headers the nav/page-header now title.
    // (ID_LBL_GENERAL is now a sub-header on the merged General page; ID_LBL_EBOOK is
    // orphaned — Ebook/comic is its own tab with no sub-header.)
    for id in [ID_LBL_THUMBS, ID_LBL_EBOOK, ID_LBL_SHOT, ID_LBL_FORMATS, ID_SCROLLBAR, ID_LEFT_MASK, ID_BANNER] {
        if let Ok(c) = GetDlgItem(Some(hwnd), id) {
            let _ = ShowWindow(c, SW_HIDE);
        }
    }

    let mut cr = RECT::default();
    let _ = GetClientRect(hwnd, &mut cr);
    let dpi = windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd).max(96) as i32;
    let client_w = (cr.right - cr.left) * 96 / dpi;
    let client_h = (cr.bottom - cr.top) * 96 / dpi;
    let footer_y = client_h - 40;

    let sc = |v: i32| dpi_scale(hwnd, v);
    let place = |id: i32, x: i32, y: i32, w: i32, h: i32| -> Option<HWND> {
        if let Ok(c) = GetDlgItem(Some(hwnd), id) {
            let _ = SetWindowPos(c, None, sc(x), sc(y), sc(w), sc(h), SWP_NOZORDER | SWP_NOACTIVATE);
            Some(c)
        } else {
            None
        }
    };

    // Nav rail.
    #[allow(clippy::needless_range_loop)] // i drives both the label index and position/id math
    for i in 0..NCAT {
        ctl(
            hwnd,
            STATIC,
            nav_label(i),
            WINDOW_STYLE(SS_OWNERDRAW | SS_NOTIFY),
            NAV_X,
            NAV_TOP + i as i32 * NAV_ITEM_H,
            NAV_W,
            NAV_ITEM_H,
            ID_NAV_BASE + i as i32,
            hinst,
        );
    }

    // Per-pane header (icon chip + bold category title + blurb), redrawn per active
    // category. Always visible; content sits below it.
    ctl(hwnd, STATIC, "", WINDOW_STYLE(SS_OWNERDRAW), PANE_X, PANE_TOP, PANE_W, PANE_HEAD_H, ID_PANE_HEADER, hinst);

    let mut cats: Vec<Vec<HWND>> = vec![Vec::new(); NCAT];
    #[allow(clippy::needless_range_loop)] // ci indexes cats AND is passed to cat_rows(ci)
    for ci in 0..NCAT {
        let mut y = PANE_TOP + PANE_HEAD_H + 8;
        let mut first = true;
        for &row in cat_rows(ci) {
            match row {
                Row::Head(id) => {
                    if !first {
                        y += 14;
                    }
                    if let Some(c) = place(id, PANE_X, y, PANE_W, 18) {
                        cats[ci].push(c);
                    }
                    y += 24;
                }
                Row::Switch(id) => {
                    if let Some(c) = place(id, PANE_X, y, PANE_W, 28) {
                        cats[ci].push(c);
                    }
                    y += 32;
                }
                Row::Pair(lbl, field, fw, fh) => {
                    let lbl_dy = if fh > 40 { 4 } else { 2 };
                    if let Some(c) = place(lbl, PANE_X, y + lbl_dy, 220, 18) {
                        cats[ci].push(c);
                    }
                    if let Some(c) = place(field, PANE_X + PANE_W - fw, y, fw, fh) {
                        cats[ci].push(c);
                    }
                    y += 34;
                }
                Row::Btn(id, w) => {
                    if let Some(c) = place(id, PANE_X, y, w, 26) {
                        cats[ci].push(c);
                    }
                    y += 32;
                }
                Row::BtnStatus(bid, bw, sid) => {
                    if let Some(c) = place(bid, PANE_X, y, bw, 26) {
                        cats[ci].push(c);
                    }
                    // status right-aligned on the SAME row, in the space to the RIGHT of the
                    // button (non-overlapping, so the static's bg fill can't cover the button).
                    let sx = PANE_X + bw + 12;
                    if let Some(c) = place(sid, sx, y + 4, PANE_W - bw - 12, 18) {
                        cats[ci].push(c);
                    }
                    y += 32;
                }
                Row::StatusBtn(sid, bid, bw) => {
                    // Mirror of BtnStatus: the status badge fills the LEFT, the button is
                    // right-aligned (the sync row — "● Synced" left, "Stop syncing" right).
                    if let Some(c) = place(sid, PANE_X, y + 4, PANE_W - bw - 12, 18) {
                        cats[ci].push(c);
                    }
                    if let Some(c) = place(bid, PANE_X + PANE_W - bw, y, bw, 26) {
                        cats[ci].push(c);
                    }
                    y += 32;
                }
                Row::Status(id) => {
                    if let Some(c) = place(id, PANE_X, y, PANE_W, 18) {
                        cats[ci].push(c);
                    }
                    y += 22;
                }
                Row::Btn3(a, b, c3) => {
                    let gap = 8;
                    let w = (PANE_W - 2 * gap) / 3;
                    for (i, id) in [a, b, c3].into_iter().enumerate() {
                        if let Some(c) = place(id, PANE_X + i as i32 * (w + gap), y, w, 26) {
                            cats[ci].push(c);
                        }
                    }
                    y += 34;
                }
                Row::Wide(id) => {
                    y += 8; // extra breathing room above (the search box read squished)
                    if let Some(c) = place(id, PANE_X, y, PANE_W, 24) {
                        cats[ci].push(c);
                    }
                    y += 24 + 12; // and below
                }
                Row::ListFill(id) => {
                    let h = (footer_y - 8 - y).max(60);
                    if let Some(c) = place(id, PANE_X, y, PANE_W, h) {
                        cats[ci].push(c);
                    }
                    y += h + 4;
                }
                Row::ListAuto(id) => {
                    let mut wr = RECT::default();
                    let measured = if let Ok(c) = GetDlgItem(Some(hwnd), id) {
                        let _ = GetWindowRect(c, &mut wr);
                        ((wr.bottom - wr.top) * 96 / dpi).max(40)
                    } else {
                        40
                    };
                    // Cap so the row(s) below (e.g. Reset order) + the footer stay
                    // on-screen; the list scrolls internally if its rows don't fit.
                    let avail = (footer_y - y - 8 - 36).max(80);
                    let cur_h = measured.min(avail);
                    if let Some(c) = place(id, PANE_X, y, PANE_W, cur_h) {
                        cats[ci].push(c);
                    }
                    y += cur_h + 6;
                }
            }
            first = false;
        }
    }

    // Footer (always visible).
    place(ID_ABOUT, 16, footer_y, 90, 28);
    place(ID_PROMO_LINK, 116, footer_y + 6, 240, 20);
    place(IDCANCEL, client_w - 200, footer_y, 88, 28);
    place(IDOK, client_w - 104, footer_y, 96, 28);

    // The file-types list is now PANE_W wide — refit its Description column to fill.
    if let Ok(list) = GetDlgItem(Some(hwnd), ID_LIST) {
        fit_columns(list);
    }

    NAV.with(|n| {
        let mut n = n.borrow_mut();
        n.active = 0;
        n.cats = cats;
    });
    switch_category(hwnd, 0);
}
// =================== end v3 layout ===================
