//! Laying a page out: one placer per row shape, then the pass that builds the rail and positions every category.

use super::*;

/// Controls this dialog creates but the v3 nav-rail layout hides on EVERY page, with
/// no page that ever un-hides them (`ID_LBL_GENERAL` is now a sub-header on the
/// merged General page; `ID_LBL_EBOOK` is orphaned — Ebook/comic is its own tab with
/// no sub-header; the menu-items checklist + its Reset button live in the popup
/// editor (`menuitems.rs`), re-parented in only while that popup is open). Named so
/// other v2-era machinery that still keys off these ids (e.g. mod.rs's resize
/// reflow table) can check itself against the SAME list instead of hand-copying it
/// out of sync — see A048/A261.
pub(in super::super) const V3_ALWAYS_HIDDEN: &[i32] = &[
    ID_LBL_FORMATS,
    ID_SCROLLBAR,
    ID_LEFT_MASK,
    ID_BANNER,
    ID_MENU_ITEMS_LIST,
    ID_MENU_RESET,
];

/// Applies `place` to one control and collects it into the row's `Vec`: the tail every
/// single-control row placer (`Head`, `Switch`, `Btn`, `Status`, `Wide`) repeats verbatim.
pub(super) fn place_one(
    place: &impl Fn(i32, i32, i32, i32, i32) -> Option<HWND>,
    id: i32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) -> Vec<HWND> {
    match place(id, x, y, w, h) {
        Some(c) => vec![c],
        None => Vec::new(),
    }
}

/// `Row::Head`: a section header, extra top margin unless it's the pane's first row.
pub(super) fn place_head_row(
    place: &impl Fn(i32, i32, i32, i32, i32) -> Option<HWND>,
    id: i32,
    y: i32,
    first: bool,
) -> Vec<HWND> {
    let row_y = y + if first { 0 } else { 20 };
    place_one(place, id, PANE_X, row_y, PANE_W, 18)
}

/// `Row::Switch`: dependent switches sit indented under their parent, the same visual
/// nesting first-run gives the PrtScn row: the indent plus the greying
/// (`sync_dependent_switches`) is what makes the hierarchy legible.
pub(super) fn place_switch_row(
    place: &impl Fn(i32, i32, i32, i32, i32) -> Option<HWND>,
    id: i32,
    y: i32,
) -> Vec<HWND> {
    let indent = if super::values::is_dependent_switch(id) {
        18
    } else {
        0
    };
    place_one(place, id, PANE_X + indent, y, PANE_W - indent, 28)
}

/// `Row::Pair`: a label + a right-aligned field of its own width/height.
pub(super) fn place_pair_row(
    place: &impl Fn(i32, i32, i32, i32, i32) -> Option<HWND>,
    lbl: i32,
    field: i32,
    fw: i32,
    fh: i32,
    y: i32,
) -> Vec<HWND> {
    let mut placed = Vec::new();
    let lbl_dy = if fh > 40 { 4 } else { 2 };
    if let Some(c) = place(lbl, PANE_X, y + lbl_dy, 220, 18) {
        placed.push(c);
    }
    if let Some(c) = place(field, PANE_X + PANE_W - fw, y, fw, fh) {
        placed.push(c);
    }
    placed
}

/// `Row::Btn`: a single full-width-or-narrower button.
pub(super) fn place_btn_row(
    place: &impl Fn(i32, i32, i32, i32, i32) -> Option<HWND>,
    id: i32,
    w: i32,
    y: i32,
) -> Vec<HWND> {
    place_one(place, id, PANE_X, y, w, 26)
}

/// `Row::BtnStatus`: a button, then a status badge right-aligned on the SAME row, in the
/// space to the RIGHT of the button (non-overlapping, so the static's bg fill can't cover
/// the button).
pub(super) fn place_btn_status_row(
    place: &impl Fn(i32, i32, i32, i32, i32) -> Option<HWND>,
    bid: i32,
    bw: i32,
    sid: i32,
    y: i32,
) -> Vec<HWND> {
    let mut placed = Vec::new();
    if let Some(c) = place(bid, PANE_X, y, bw, 26) {
        placed.push(c);
    }
    let sx = PANE_X + bw + 12;
    if let Some(c) = place(sid, sx, y + 4, PANE_W - bw - 12, 18) {
        placed.push(c);
    }
    placed
}

/// `Row::StatusBtn`: mirror of `BtnStatus`: the status badge fills the LEFT, the button is
/// right-aligned (the sync row, "● Synced" left, "Stop syncing" right).
pub(super) fn place_status_btn_row(
    place: &impl Fn(i32, i32, i32, i32, i32) -> Option<HWND>,
    sid: i32,
    bid: i32,
    bw: i32,
    y: i32,
) -> Vec<HWND> {
    let mut placed = Vec::new();
    if let Some(c) = place(sid, PANE_X, y + 4, PANE_W - bw - 12, 18) {
        placed.push(c);
    }
    if let Some(c) = place(bid, PANE_X + PANE_W - bw, y, bw, 26) {
        placed.push(c);
    }
    placed
}

/// `Row::Status`: one full-width status line.
pub(super) fn place_status_row(
    place: &impl Fn(i32, i32, i32, i32, i32) -> Option<HWND>,
    id: i32,
    y: i32,
) -> Vec<HWND> {
    place_one(place, id, PANE_X, y, PANE_W, 18)
}

/// `Row::Btn3`: three equal-width buttons across the pane with a fixed gap between them.
pub(super) fn place_btn3_row(
    place: &impl Fn(i32, i32, i32, i32, i32) -> Option<HWND>,
    a: i32,
    b: i32,
    c3: i32,
    y: i32,
) -> Vec<HWND> {
    let mut placed = Vec::new();
    let gap = 8;
    let w = (PANE_W - 2 * gap) / 3;
    for (i, id) in [a, b, c3].into_iter().enumerate() {
        if let Some(c) = place(id, PANE_X + i as i32 * (w + gap), y, w, 26) {
            placed.push(c);
        }
    }
    placed
}

/// `Row::Wide`: a single full-width control. 18, not 24: a single-line EDIT top-aligns its
/// text, so the taller box never centred the cue text; it just left 11px of dead space
/// under it. With the 5/3 frame this is a 26px box with the ink centred.
pub(super) fn place_wide_row(
    place: &impl Fn(i32, i32, i32, i32, i32) -> Option<HWND>,
    id: i32,
    y: i32,
) -> Vec<HWND> {
    place_one(place, id, PANE_X, y + 8, PANE_W, 18)
}

/// `Row::WideBtn`: a wide edit that fills the row up to a right-aligned button (the licence
/// key and its Redeem button). The edit keeps `Row::Wide`'s 18px box at `y + 8`, so its 5/3
/// frame makes a 26px box whose centre is `y + 17`; the 26px button is placed at `y + 4` so
/// its centre lands on the same line, and the 12px gap between them is the one every other
/// side-by-side row on this dialog uses.
pub(super) fn place_wide_btn_row(
    place: &impl Fn(i32, i32, i32, i32, i32) -> Option<HWND>,
    eid: i32,
    bid: i32,
    bw: i32,
    y: i32,
) -> Vec<HWND> {
    let mut placed = Vec::new();
    if let Some(c) = place(eid, PANE_X, y + 8, PANE_W - bw - 12, 18) {
        placed.push(c);
    }
    if let Some(c) = place(bid, PANE_X + PANE_W - bw, y + 4, bw, 26) {
        placed.push(c);
    }
    placed
}

/// `Row::ListFill`: grows to fill down to `content_bottom`, returning the `y` it advances
/// past itself to (the fixed-row table in `fixed_row_next_y` cannot know that height ahead
/// of placing it, so it deliberately leaves this row's case out).
pub(super) unsafe fn place_list_fill_row(
    hwnd: HWND,
    place: &impl Fn(i32, i32, i32, i32, i32) -> Option<HWND>,
    id: i32,
    y: i32,
    content_bottom: i32,
) -> (Vec<HWND>, i32) {
    let mut placed = Vec::new();
    let h = (content_bottom - 8 - y).max(60);
    if let Some(c) = place(id, PANE_X, y, PANE_W, h) {
        // Square list corners poked out of the pane's rounded look
        // (report: "a little edge of the table poking into the
        // roundedness") — clip them.
        super::restyle::round_corners(hwnd, c, 8);
        placed.push(c);
    }
    (placed, y + h + 4)
}

/// Places one page row's control(s) and returns the handles that were placed (for the
/// caller to push into `cats[ci]`) plus the `y` this row leaves behind — unchanged for
/// every row except `Row::ListFill`, which grows to fill down to `content_bottom` and
/// advances `y` past itself (the fixed-row table in `fixed_row_next_y` cannot know that
/// height ahead of placing it, so it deliberately leaves this row's case out).
pub(super) unsafe fn place_row(
    hwnd: HWND,
    place: &impl Fn(i32, i32, i32, i32, i32) -> Option<HWND>,
    row: Row,
    y: i32,
    first: bool,
    content_bottom: i32,
) -> (Vec<HWND>, i32) {
    match row {
        Row::Head(id) => (place_head_row(place, id, y, first), y),
        Row::Switch(id) => (place_switch_row(place, id, y), y),
        Row::Pair(lbl, field, fw, fh) => (place_pair_row(place, lbl, field, fw, fh, y), y),
        Row::Btn(id, w) => (place_btn_row(place, id, w, y), y),
        Row::BtnStatus(bid, bw, sid) => (place_btn_status_row(place, bid, bw, sid, y), y),
        Row::StatusBtn(sid, bid, bw) => (place_status_btn_row(place, sid, bid, bw, y), y),
        Row::Status(id) => (place_status_row(place, id, y), y),
        Row::Btn3(a, b, c3) => (place_btn3_row(place, a, b, c3, y), y),
        Row::Wide(id) => (place_wide_row(place, id, y), y),
        Row::WideBtn(eid, bid, bw) => (place_wide_btn_row(place, eid, bid, bw, y), y),
        Row::ListFill(id) => place_list_fill_row(hwnd, place, id, y, content_bottom),
    }
}

/// Create the nav-rail rows. WS_TABSTOP + the subclass below give it keyboard access (Tab
/// into the rail, arrows to move between categories, Enter/Space to switch).
pub(super) unsafe fn build_nav_rail(hwnd: HWND, hinst: HINSTANCE) {
    #[allow(clippy::needless_range_loop)] // i drives both the label index and position/id math
    for i in 0..NCAT {
        let nav = ctl(
            hwnd,
            STATIC,
            nav_label(i),
            WINDOW_STYLE(SS_OWNERDRAW | SS_NOTIFY) | WS_TABSTOP,
            NAV_X,
            NAV_TOP + i as i32 * NAV_ITEM_H,
            NAV_W,
            NAV_ITEM_H,
            ID_NAV_BASE + i as i32,
            hinst,
        );
        let _ = SetWindowSubclass(nav, Some(nav_item_subclass), 0, 0);
    }
}

/// Lay out every row of category `ci`, returning the controls it placed.
pub(super) unsafe fn build_category_rows(
    hwnd: HWND,
    place: &impl Fn(i32, i32, i32, i32, i32) -> Option<HWND>,
    ci: usize,
    content_bottom: i32,
) -> Vec<HWND> {
    let mut placed = Vec::new();
    let mut y = PANE_TOP + PANE_HEAD_H + 8;
    let mut first = true;
    for &row in cat_rows(ci) {
        let fixed_next_y = fixed_row_next_y(row, y, first);
        let (p, new_y) = place_row(hwnd, place, row, y, first, content_bottom);
        placed.extend(p);
        y = new_y;
        if let Some(next_y) = fixed_next_y {
            y = next_y;
        }
        first = false;
    }
    // File Types fills its list to the footer. Every other page is fixed and
    // must retain visible breathing room above it.
    if ci != 2 {
        debug_assert!(
            y <= content_bottom - 12,
            "settings category {ci} reaches the footer ({y} > {})",
            content_bottom - 12
        );
    }
    placed
}

/// Hide the old scrolling chrome + the headers the nav/page-header now title.
pub(super) unsafe fn hide_always_hidden(hwnd: HWND) {
    for &id in V3_ALWAYS_HIDDEN {
        if let Ok(c) = GetDlgItem(Some(hwnd), id) {
            let _ = ShowWindow(c, SW_HIDE);
        }
    }
}

pub(in super::super) unsafe fn apply_v3_layout(hwnd: HWND, hinst: HINSTANCE) {
    hide_always_hidden(hwnd);

    let mut cr = RECT::default();
    let _ = GetClientRect(hwnd, &mut cr);
    let dpi = windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd).max(96) as i32;
    let client_w = (cr.right - cr.left) * 96 / dpi;
    let client_h = (cr.bottom - cr.top) * 96 / dpi;
    let footer_y = client_h - 40;
    // The sign-in banner takes its strip out of the bottom of the pane, and the window was
    // created exactly that much taller to pay for it - so `content_bottom` lands back where
    // `footer_y` would have been without a banner, and every page keeps the rhythm it had.
    //
    // Everything that lays out PAGE CONTENT (the list that fills to the bottom, and the assert
    // that each fixed page still clears the footer) uses `content_bottom`; only the footer row
    // itself uses `footer_y`. The Business-licence banner (`biznag`) reserves its own strip the
    // same way, stacked below the sign-in one when both happen to apply at once (rare — one
    // needs a signed-out account, the other a Business install — but neither rules the other
    // out, so both get their own room rather than one silently overwriting the other).
    let content_bottom = footer_y - nudge::extra_height() - biznag::extra_height();

    let sc = |v: i32| dpi_scale(hwnd, v);
    let place = |id: i32, x: i32, y: i32, w: i32, h: i32| -> Option<HWND> {
        if let Ok(c) = GetDlgItem(Some(hwnd), id) {
            let _ = SetWindowPos(
                c,
                None,
                sc(x),
                sc(y),
                sc(w),
                sc(h),
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            Some(c)
        } else {
            None
        }
    };

    build_nav_rail(hwnd, hinst);

    // Per-pane header (icon chip + bold category title + blurb), redrawn per active
    // category. Always visible; content sits below it.
    //
    // PANE_W + 4, not PANE_W: everything framed below (the list card, the format filter)
    // is a PANE_W-wide control plus a 4px painted frame inflation, so the pane's real
    // right edge is PANE_X + PANE_W + 4. The search box floats over this header and its
    // frame is drawn by (and therefore CLIPPED to) it — at PANE_W the frame's right round
    // was sliced off, and pulling the box left to fit left it 6px short of the right edge
    // every other row lines up on. Widening the header fixes both at once.
    ctl(
        hwnd,
        STATIC,
        "",
        WINDOW_STYLE(SS_OWNERDRAW),
        PANE_X,
        PANE_TOP,
        PANE_W + 4,
        PANE_HEAD_H,
        ID_PANE_HEADER,
        hinst,
    );

    let mut cats: Vec<Vec<HWND>> = vec![Vec::new(); NCAT];
    #[allow(clippy::needless_range_loop)] // ci indexes cats AND is passed to cat_rows(ci)
    for ci in 0..NCAT {
        cats[ci] = build_category_rows(hwnd, &place, ci, content_bottom);
    }

    // The sign-in banner, in the strip between the pane and the footer. Page-independent chrome
    // like the footer, so it is placed here rather than inside any category's row list - and it
    // is therefore visible whichever page the user navigates to.
    nudge::place(
        hwnd,
        content_bottom + 8,
        PANE_X,
        PANE_W,
        |id, x, y, w, h| {
            place(id, x, y, w, h);
        },
    );
    // The Business-licence banner, stacked directly under the sign-in one (which reserved
    // `nudge::extra_height()` of the strip above — 0 when it isn't showing, so this sits right
    // after content_bottom on the far more common run where neither or only this one is live).
    biznag::place(
        hwnd,
        content_bottom + 8 + nudge::extra_height(),
        PANE_X,
        PANE_W,
        |id, x, y, w, h| {
            place(id, x, y, w, h);
        },
    );

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
    // The settings-wide search lives OUTSIDE the per-category lists on purpose: it must
    // stay visible whatever page is active, or it could not take you to another page.
    super::search::build_search(hwnd, hinst);
    switch_category(hwnd, 0);
}
