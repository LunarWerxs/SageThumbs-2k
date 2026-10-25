//! The "Menu items" popup editor — the drag-to-reorder visibility checklist in its own
//! small modal window instead of squeezed into the Right-click menu page.
//!
//! On that page the list was the reason everything felt cramped: it competed for height
//! with the page's own sections and had to be capped so the footer stayed visible. A
//! popup gives it the one thing it actually needs — room — and gives the page back its
//! breathing space.
//!
//! Implementation: the checklist control is NOT rebuilt here. The real ListView (still
//! created by `build_controls`, hidden by `apply_v3_layout`) is RE-PARENTED into the
//! popup for the dialog's lifetime and re-parented back on close. Everything that reads
//! it — `load_values` seeding checks, `apply_settings` persisting visibility + order,
//! `list::` drag-reorder state — keeps working on the same HWND with zero duplication.
//! The popup is modal over Settings, so nothing can read the list while it is away.
//!
//! The list's own notifications (the drag start among them) still go to Settings, the
//! window it was created under: comctl32 never follows a re-parent. `notify::on_notify_begindrag`
//! is written for that.
//!
//! Done saves the list straight away and tells the shell, so the next right-click shows it.
//! Through 3.7.4 it only closed, and the change waited for Settings' Save. Cancel, Escape and
//! the title bar's X put the list back the way it was when the editor opened (feedback,
//! 2026-10-22: in 3.9.0 the X saved too, so there was no way to back out of an edit).

use super::*;
use st2k_appkit::win::{IDCANCEL, IDOK};

thread_local! {
    // The Settings window, so the close path can hand the list back. Set for the popup's
    // lifetime only.
    static OWNER: std::cell::Cell<Option<isize>> = const { std::cell::Cell::new(None) };
    /// The list's rows as the editor opened with them, which Cancel puts back.
    static OPENED_WITH: std::cell::RefCell<Vec<(isize, bool)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

const POP_W: i32 = 420;
const POP_H: i32 = 560;
const M: i32 = 14;
/// Design height of the footer buttons.
const BTN_H: i32 = 28;
/// Gap between the footer buttons, and between the help text and the list.
const GAP: i32 = 8;
// ID_POP_RESET and ID_POP_HELP live in ids.rs (so `control_ids_are_unique` there covers them).
// IDOK ("Done") and IDCANCEL ("Cancel") come from st2k_appkit::win.

/// Open the editor, modal over `settings`. Returns after it closes.
pub(super) unsafe fn open(settings: HWND) {
    OWNER.with(|o| o.set(Some(settings.0 as isize)));
    st2k_appkit::win::run_dialog(
        w!("SageThumbs2KMenuItems"),
        Some(popup_wndproc),
        t("grp_menu_items"),
        POP_W,
        POP_H,
        Some(settings),
    );
    OWNER.with(|o| o.set(None));
}

/// The editor over a headless Settings for `--shot --window menuitems`: the same window and
/// wndproc, built invisible and with no modal loop. The caller drives and captures it; the
/// popup hands the list back to `settings` when it is destroyed.
pub(super) unsafe fn open_for_shot(settings: HWND, hinst: HINSTANCE, dark: bool) -> Option<HWND> {
    OWNER.with(|o| o.set(Some(settings.0 as isize)));
    st2k_appkit::win::create_shot_window(
        hinst,
        dark,
        w!("SageThumbs2KMenuItems"),
        Some(popup_wndproc),
        t("grp_menu_items"),
        POP_W,
        POP_H,
    )
}

unsafe fn settings_hwnd() -> Option<HWND> {
    OWNER.with(|o| o.get()).map(|p| HWND(p as *mut _))
}

/// The popup's client geometry in 96-DPI design px: the help text across the top, the
/// checklist under it down to the footer, Reset at the left margin, Cancel and Done at the
/// right. Pure so the margin and footer arithmetic is testable without a window.
struct PopupLayout {
    /// `(x, y, w, h)` of the help text above the list.
    help: (i32, i32, i32, i32),
    /// `(x, y, w, h)` of the re-parented checklist.
    list: (i32, i32, i32, i32),
    /// `(x, y, w, h)` of the Reset button.
    reset: (i32, i32, i32, i32),
    /// `(x, y, w, h)` of the Cancel button.
    cancel: (i32, i32, i32, i32),
    /// `(x, y, w, h)` of the Done button.
    done: (i32, i32, i32, i32),
}

/// Design widths of the three footer buttons, each as wide as its translated text needs.
struct ButtonWidths {
    reset: i32,
    cancel: i32,
    done: i32,
}

fn popup_layout(cw: i32, ch: i32, help_h: i32, bw: &ButtonWidths) -> PopupLayout {
    let by = ch - M - BTN_H;
    let list_y = M + help_h + GAP;
    let done_x = cw - M - bw.done;
    let cancel_x = done_x - GAP - bw.cancel;
    PopupLayout {
        help: (M, M, cw - M * 2, help_h),
        list: (M, list_y, cw - M * 2, by - 10 - list_y),
        reset: (M, by, bw.reset, BTN_H),
        cancel: (cancel_x, by, bw.cancel, BTN_H),
        done: (done_x, by, bw.done, BTN_H),
    }
}

/// Take the checklist from Settings, fill the popup with it under its help text, and add
/// Reset, Cancel and Done. The help text used to be the list's tooltip, which followed the
/// mouse over every row and the scrollbar (feedback, 2026-10-22).
unsafe fn build(hwnd: HWND, hinst: HINSTANCE) {
    use st2k_appkit::win::{dpi_scale, dpi_unscale, fit_button_w, wrapped_text_h};
    let Some(settings) = settings_hwnd() else {
        return;
    };
    let Ok(list) = GetDlgItem(Some(settings), ID_MENU_ITEMS_LIST) else {
        return;
    };
    let _ = SetParent(list, Some(hwnd));
    OPENED_WITH.with(|o| *o.borrow_mut() = list::snapshot_rows(list));

    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    // 96-DPI design px, so the M/BTN_H constants (already design px) compare like-for-like.
    let cw = dpi_unscale(hwnd, rc.right - rc.left);
    let ch = dpi_unscale(hwnd, rc.bottom - rc.top);
    let help = t("lbl_menu_items_help");
    let bw = ButtonWidths {
        reset: fit_button_w(hwnd, t("btn_reset"), 96, 180),
        cancel: fit_button_w(hwnd, t("btn_cancel"), 96, 160),
        done: fit_button_w(hwnd, t("btn_done"), 96, 160),
    };
    let lo = popup_layout(cw, ch, wrapped_text_h(hwnd, help, cw - M * 2), &bw);
    add_ctl(
        hwnd,
        hinst,
        STATIC,
        help,
        WINDOW_STYLE(0),
        lo.help,
        ID_POP_HELP,
    );
    let _ = SetWindowPos(
        list,
        None,
        dpi_scale(hwnd, lo.list.0),
        dpi_scale(hwnd, lo.list.1),
        dpi_scale(hwnd, lo.list.2),
        dpi_scale(hwnd, lo.list.3),
        SWP_NOZORDER | SWP_NOACTIVATE,
    );
    // The list arrives with whatever exact-fit height the page gave it; rounded corners
    // keep it in the family of the other beta surfaces.
    super::restyle::round_corners(hwnd, list, 8);
    let _ = ShowWindow(list, SW_SHOW);
    // Its one column was made 300 px wide for the page; here it fills the list up to the
    // scrollbar, so a selected row's highlight runs the full width. Measured once shown: a
    // hidden list has not added its scrollbar yet, and its client is 17 px too wide.
    let mut lrc = RECT::default();
    let _ = GetClientRect(list, &mut lrc);
    SendMessageW(
        list,
        LVM_SETCOLUMNWIDTH,
        Some(WPARAM(0)),
        Some(LPARAM((lrc.right - lrc.left) as isize)),
    );

    let tab = WINDOW_STYLE(0) | WS_TABSTOP;
    add_ctl(
        hwnd,
        hinst,
        BUTTON,
        t("btn_reset"),
        tab,
        lo.reset,
        ID_POP_RESET,
    );
    add_ctl(
        hwnd,
        hinst,
        BUTTON,
        t("btn_cancel"),
        tab,
        lo.cancel,
        IDCANCEL,
    );
    let def = WINDOW_STYLE(BS_DEFPUSHBUTTON as u32) | WS_TABSTOP;
    add_ctl(hwnd, hinst, BUTTON, t("btn_done"), def, lo.done, IDOK);
}

/// One child control at design-px `(x, y, w, h)`.
unsafe fn add_ctl(
    hwnd: HWND,
    hinst: HINSTANCE,
    class: PCWSTR,
    text: &str,
    style: WINDOW_STYLE,
    (x, y, w, h): (i32, i32, i32, i32),
    id: i32,
) {
    st2k_appkit::win::ctl(hwnd, class, text, style, x, y, w, h, id, hinst);
}

extern "system" fn popup_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if let Some(r) = st2k_appkit::dark::dark_ctlcolor(msg, wparam) {
            return r;
        }
        match msg {
            WM_CREATE => on_create(hwnd),
            // Reset and Done's dark-mode paint. The checklist's own notifications never come
            // here (see the module note).
            WM_NOTIFY => on_notify(hwnd, lparam),
            // Right-click on the checklist: add or remove a divider. Gated on the sender being
            // the list itself — WM_CONTEXTMENU also fires for the buttons and for the popup's
            // own background, and without this guard any of those right-clicks popped the
            // list's menu too. Mirrors the same guard on the format list's own WM_CONTEXTMENU
            // in mod.rs.
            WM_CONTEXTMENU
                if HWND(wparam.0 as *mut c_void)
                    == GetDlgItem(Some(hwnd), ID_MENU_ITEMS_LIST).unwrap_or_default() =>
            {
                list::menu_list_context_menu(HWND(wparam.0 as *mut c_void), hwnd, lparam);
                LRESULT(0)
            }
            // Owner-drawn dark context-menu items (light text on dark). TrackPopupMenu
            // routes WM_MEASUREITEM/WM_DRAWITEM to its `hOwner` argument, which
            // `list::list_context_menu` passes as THIS popup (not Settings), so the popup
            // wndproc must handle them itself — mod.rs's ODT_MENU arms did this for the
            // format list's identical dark-mode menu, but this window never inherited it.
            WM_MEASUREITEM => on_measureitem(hwnd, msg, wparam, lparam),
            WM_DRAWITEM => on_drawitem(hwnd, msg, wparam, lparam),
            WM_COMMAND => on_command(hwnd, wparam),
            // The title bar's X (and Alt+F4) is Cancel: only Done saves.
            WM_CLOSE => {
                cancel(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => on_destroy(hwnd),
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

unsafe fn on_create(hwnd: HWND) -> LRESULT {
    crate::first_run::create_with(hwnd, build)
}

unsafe fn on_notify(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    let nmhdr = lparam.0 as *const NMHDR;
    let code = (*nmhdr).code;
    if code == NM_CUSTOMDRAW {
        if is_button_class((*nmhdr).hwndFrom) {
            return LRESULT(restyle::draw_button_cd(
                hwnd,
                lparam.0 as *const NMCUSTOMDRAW,
            ));
        }
        return LRESULT(CDRF_DODEFAULT as isize);
    }
    LRESULT(0)
}

unsafe fn on_measureitem(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let m = &mut *(lparam.0 as *mut MEASUREITEMSTRUCT);
    if m.CtlType != ODT_MENU {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    let label = list::ctx_menu_label(m.itemID as usize);
    st2k_appkit::win::measure_menu_item(hwnd, m, label);
    LRESULT(1)
}

unsafe fn on_drawitem(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let d = &*(lparam.0 as *const DRAWITEMSTRUCT);
    if d.CtlType != ODT_MENU {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    st2k_appkit::win::draw_menu_item(d, list::ctx_menu_label(d.itemID as usize));
    LRESULT(1)
}

/// What a `WM_COMMAND` id asks the popup to do. Anything other than Reset / Done / Cancel (a
/// stray id, the checklist's own forwarded notifications) must fall through untouched.
#[derive(Debug, PartialEq, Eq)]
enum PopupCommand {
    Reset,
    Done,
    Cancel,
    Ignore,
}

fn popup_command(id: i32) -> PopupCommand {
    match id {
        ID_POP_RESET => PopupCommand::Reset,
        IDOK => PopupCommand::Done,
        // The Cancel button, and Escape: the modal pump's IsDialogMessage turns Escape into
        // IDCANCEL, which 3.9.0 ignored.
        IDCANCEL => PopupCommand::Cancel,
        _ => PopupCommand::Ignore,
    }
}

unsafe fn on_command(hwnd: HWND, wparam: WPARAM) -> LRESULT {
    match popup_command(st2k_appkit::win::command_id(wparam)) {
        PopupCommand::Reset => {
            if let Ok(list) = GetDlgItem(Some(hwnd), ID_MENU_ITEMS_LIST) {
                list::reset_menu_rows(list);
            }
        }
        PopupCommand::Done => done(hwnd),
        PopupCommand::Cancel => cancel(hwnd),
        PopupCommand::Ignore => {}
    }
    LRESULT(0)
}

/// Done: tidy the dividers the edit left (doubled, or at either end), save the list's
/// visibility, order and dividers now, then close.
unsafe fn done(hwnd: HWND) {
    if let Ok(list) = GetDlgItem(Some(hwnd), ID_MENU_ITEMS_LIST) {
        list::normalize_list(list);
    }
    super::values::apply_menu_list_now(hwnd);
    close(hwnd);
}

/// Cancel: Escape during a drag drops only the drag; otherwise the list goes back to how the
/// editor found it, so neither the menu nor a later Save in Settings sees the edit.
unsafe fn cancel(hwnd: HWND) {
    let Ok(list) = GetDlgItem(Some(hwnd), ID_MENU_ITEMS_LIST) else {
        close(hwnd);
        return;
    };
    if list::cancel_menu_drag(list) {
        return;
    }
    let opened_with = OPENED_WITH.with(|o| std::mem::take(&mut *o.borrow_mut()));
    list::rebuild_rows(list, &opened_with, None);
    close(hwnd);
}

/// Close the editor with Settings already enabled again, so Windows hands the activation back
/// to Settings. Destroying a modal popup while its owner is still disabled activates whatever
/// window is next in line instead, which put Settings behind another app every time (feedback,
/// 2026-10-22).
unsafe fn close(hwnd: HWND) {
    if let Some(settings) = settings_hwnd() {
        let _ = windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow(settings, true);
    }
    let _ = DestroyWindow(hwnd);
}

/// Hand the checklist back BEFORE the window goes away, hidden again, so load/save find
/// it exactly where they always did.
unsafe fn on_destroy(hwnd: HWND) -> LRESULT {
    if let (Some(settings), Ok(list)) =
        (settings_hwnd(), GetDlgItem(Some(hwnd), ID_MENU_ITEMS_LIST))
    {
        let _ = ShowWindow(list, SW_HIDE);
        let _ = SetParent(list, Some(settings));
    }
    OPENED_WITH.with(|o| o.borrow_mut().clear());
    LRESULT(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HELP_H: i32 = 32;
    const BW: ButtonWidths = ButtonWidths {
        reset: 96,
        cancel: 96,
        done: 96,
    };

    fn lo() -> PopupLayout {
        popup_layout(POP_W, POP_H, HELP_H, &BW)
    }

    #[test]
    fn the_help_text_and_the_checklist_keep_both_side_margins() {
        let lo = lo();
        for (x, _, w, _) in [lo.help, lo.list] {
            assert_eq!(x, M);
            assert_eq!(w, POP_W - M * 2);
        }
    }

    #[test]
    fn the_checklist_starts_under_the_help_text() {
        let lo = lo();
        assert_eq!(lo.help.1, M);
        assert_eq!(lo.help.3, HELP_H);
        assert_eq!(lo.list.1, M + HELP_H + GAP);
    }

    #[test]
    fn the_checklist_leaves_a_gap_above_the_footer_buttons() {
        let lo = lo();
        let list_bottom = lo.list.1 + lo.list.3;
        assert_eq!(
            list_bottom + 10,
            lo.reset.1,
            "the footer must clear the list by exactly the 10 px gap"
        );
        assert!(lo.list.3 > 200, "the list keeps most of the popup");
    }

    #[test]
    fn the_footer_buttons_share_a_baseline_and_height() {
        let lo = lo();
        for b in [lo.cancel, lo.done] {
            assert_eq!((b.1, b.3), (lo.reset.1, lo.reset.3));
        }
        assert_eq!(lo.reset.3, BTN_H);
    }

    #[test]
    fn reset_hugs_the_left_margin_and_cancel_sits_left_of_done_on_the_right() {
        let wide = ButtonWidths {
            reset: 150,
            cancel: 120,
            done: 110,
        };
        let lo = popup_layout(POP_W, POP_H, HELP_H, &wide);
        assert_eq!((lo.reset.0, lo.reset.2), (M, 150));
        assert_eq!(lo.done.0 + lo.done.2, POP_W - M);
        assert_eq!(lo.cancel.0 + lo.cancel.2 + GAP, lo.done.0);
        assert!(lo.reset.0 + lo.reset.2 < lo.cancel.0, "no overlap");
    }

    #[test]
    fn each_button_maps_to_its_action() {
        assert_eq!(popup_command(ID_POP_RESET), PopupCommand::Reset);
        assert_eq!(popup_command(IDOK), PopupCommand::Done);
        assert_eq!(popup_command(IDCANCEL), PopupCommand::Cancel);
    }

    #[test]
    fn any_other_command_id_is_ignored() {
        assert_eq!(popup_command(ID_MENU_ITEMS_LIST), PopupCommand::Ignore);
        assert_eq!(popup_command(ID_POP_HELP), PopupCommand::Ignore);
        assert_eq!(popup_command(0), PopupCommand::Ignore);
    }
}
