//! The right column: the format label and buttons, the live search box, and the file-type list.

use super::*;

/// Right column: supported file types
pub(super) unsafe fn build_file_types(hwnd: HWND, hinst: HINSTANCE, sty: &Styles) {
    let rx = 348;
    ctl(
        hwnd,
        STATIC,
        t("lbl_formats"),
        sty.hdr,
        rx,
        12,
        356,
        18,
        ID_LBL_FORMATS,
        hinst,
    );
    ctl(
        hwnd,
        BUTTON,
        t("btn_select_all"),
        WS_TABSTOP,
        rx,
        34,
        84,
        26,
        ID_SELECT_ALL,
        hinst,
    );
    ctl(
        hwnd,
        BUTTON,
        t("btn_clear_all"),
        WS_TABSTOP,
        rx + 90,
        34,
        84,
        26,
        ID_CLEAR_ALL,
        hinst,
    );
    ctl(
        hwnd,
        BUTTON,
        t("btn_defaults"),
        WS_TABSTOP,
        rx + 180,
        34,
        84,
        26,
        ID_DEFAULTS,
        hinst,
    );

    // Live search box (filters the list as you type). Borderless + rounded panel in
    // both themes (like the other inputs); the frame is painted, not native.
    let search_style = WINDOW_STYLE(ES_AUTOHSCROLL as u32) | WS_TABSTOP;
    let search = ctl(
        hwnd,
        EDIT,
        "",
        search_style,
        rx,
        70,
        356,
        18,
        ID_SEARCH,
        hinst,
    );
    let cue = wide(t("search_formats"));
    SendMessageW(
        search,
        EM_SETCUEBANNER,
        Some(WPARAM(1)),
        Some(LPARAM(cue.as_ptr() as isize)),
    );

    // No square WS_BORDER — a rounded card frame is drawn behind
    // the list in WM_PAINT, in both themes.
    let list_style = WINDOW_STYLE(LVS_REPORT | LVS_NOSORTHEADER) | WS_TABSTOP;
    // Shorter list in dark mode (scrollable left column lets the window be shorter);
    // y=98 leaves room (with padding) for the search box above. Dark bottom = 442.
    let list_h = 344;
    let list = ctl(
        hwnd,
        WC_LISTVIEWW,
        "",
        list_style,
        rx,
        98,
        356,
        list_h,
        ID_LIST,
        hinst,
    );
    // Lift the list onto SURFACE() (a card) so the zebra alternates against it —
    // theme-aware: a white card in light, a near-black one in dark.
    theme_checkbox_list(list);
    let header = HWND(SendMessageW(list, LVM_GETHEADER, None, None).0 as *mut c_void);
    // COLUMNS ARE DRAG-RESIZABLE. This used to OR in `HDS_NOSIZING`, on the reasoning that
    // `fit_columns` already sized Description to exactly fill the list so a drag could only
    // truncate it or open a dead gap. That reasoning covered the LAST column and quietly took
    // the other two with it: Extension and Category are fixed at 64 and 92 px, which is not
    // enough for their own labels in a long language, and no amount of window resizing helps
    // because only Description grows. The reporter of issue #26.3 could read neither.
    //
    // Sizing is back on for THE FIRST THREE, and `fit_columns` now measures them instead of
    // assuming their widths, so widening Extension reflows Description and the total still
    // exactly fills the list.
    //
    // DESCRIPTION ITSELF IS REFUSED, in `list::list_subclass` via HDN_BEGINTRACK. It is the last
    // column and it is auto-fitted to fill, so a drag can only shrink it and leave dead space
    // against the scrollbar — which looks like a rendering fault, not a layout the user chose.
    // An earlier attempt allowed the drag and turned the auto-fit off to stop it snapping back;
    // that traded a snap-back for a permanent gap, so the drag is simply not offered now.
    //
    // The last column's right-hand divider is also painted out in `list::list_subclass`: it sits
    // at the far edge of the list where there is nothing to drag INTO. The INNER dividers — the
    // ones that do something — are left alone.
    if is_dark() {
        // Native dark item-view theme is dark-only; light keeps the native light header.
        dark_control(header, w!("DarkMode_ItemsView"));
    }
    // Subclass for dark header text, the column-drag reflow, and the SPACE/right-click bulk
    // checkbox toggle.
    let _ = SetWindowSubclass(list, Some(list::list_subclass), 0, 0);
    // Extension | Category | How | Description. FORMATS is ordered by category, so the
    // list naturally clusters: Images, then Camera RAW, then Ebooks & comics —
    // and the Category column labels each (robust in dark mode, unlike native
    // ListView group headers, which the dark theme refuses to render). "How" (audit E03)
    // names the capability's source kind (`settings_dlg::capability_label`) - Extension
    // and Category keep their existing widths; the room comes out of Description, which
    // `fit_columns` auto-sizes to fill whatever is left.
    insert_column(list, 0, t("col_extension"), 64);
    insert_column(list, 1, t("col_category"), 92);
    insert_column(list, 2, t("col_capability"), 110);
    insert_column(list, 3, t("col_description"), 196);

    // The per-format checked state lives in a model (FMT_STATE), not the list —
    // so the search can rebuild the list view without losing toggles. Seed it from
    // settings, then populate the (unfiltered) view.
    super::super::values::seed_format_state();
    populate_list(list, "");
}
