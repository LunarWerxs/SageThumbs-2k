//! Headless settings-window capture (run_shot_driven / run_shot_menu_items / run_shot_gif) (extracted from settings_dlg; parent-hub pattern).

use super::*;
use st2k_appkit::win::IDOK;
use windows::Win32::UI::Controls::{LVIS_FOCUSED, LVIS_SELECTED};
use windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, SetWindowsHookExW, UnhookWindowsHookEx, CWPSTRUCT, WH_CALLWNDPROC,
};

// ===== Headless self-capture (verification + README/site assets) =====

/// Build the Settings window OFF-SCREEN (invisible, steals no focus), returning the HWND once
/// its controls have realized + painted. Shared by `run_shot_driven` (one pane → PNG) and
/// `run_shot_gif` (all panes → animated GIF).
pub(super) unsafe fn build_settings_shot_window(hinst: HINSTANCE, dark: bool) -> Option<HWND> {
    // Ask the sign-in engine, exactly as a real launch does. It does NOT force a banner into
    // existence: with no stored history the gate is shut, this returns false, and the capture
    // honestly shows the window without one. Seed the `SignInNudge` registry value with a
    // long-time user's history to shoot the banner. Sizing has to happen AFTER asking, because
    // the banner adds a strip between the pane and the footer.
    nudge::decide();
    // The Business-licence strip, decided the same way and for the same reason (it adds its
    // own strip). Absent on a Personal machine; to shoot it, force Business with
    // `ST2K_LICENCE_MODE=business` and point `ST2K_LICENCE_HOME` at a scratch breadcrumb
    // (`scratchpad/shot-trial.ps1` in the 2026-09-13 session is the shape).
    biznag::decide();
    let hwnd = st2k_appkit::win::create_shot_window(
        hinst,
        dark,
        w!("SageThumbs2KOptions"),
        Some(wndproc),
        "SageThumbs 2K — Settings",
        772,
        588 + nudge::extra_height() + biznag::extra_height(),
    )?;
    // Let the controls — the ListView especially — realize + paint before we drive panes.
    st2k_appkit::win::pump_msgs(20);
    Some(hwnd)
}

/// Switch to category `tab` and settle it for capture. The shared owner-drawn chrome (nav
/// rail, pane header, footer buttons) only fully repaints on a *real* category transition —
/// re-selecting the current tab is a no-op that leaves them blank in a headless grab. So we
/// PRIME with a switch to a different tab first, making the switch to `tab` a real transition,
/// then double `RDW_UPDATENOW` around pumps so every control has actually painted.
pub(super) unsafe fn settle_pane(hwnd: HWND, tab: usize) {
    let tab = tab.min(NCAT - 1);
    switch_category(hwnd, prime_category(tab, NCAT));
    st2k_appkit::win::pump_msgs(5);
    switch_category(hwnd, tab);
    st2k_appkit::win::force_repaint(hwnd);
    st2k_appkit::win::pump_msgs(12);
    st2k_appkit::win::force_repaint(hwnd);
    st2k_appkit::win::pump_msgs(4);
}

/// Which category [`settle_pane`] should switch to FIRST so the switch to `tab` lands as a
/// REAL transition (a same-tab re-select is a no-op and leaves the owner-drawn chrome
/// blank, see `settle_pane`'s doc). Split out from it purely so this bit of arithmetic is
/// unit-testable without a window.
fn prime_category(tab: usize, ncat: usize) -> usize {
    if tab == 0 {
        ncat - 1
    } else {
        0
    }
}

/// The app's `--shot` mode: build the Settings window off-screen, switch to category `tab`,
/// render it to a PNG at `out` via `PrintWindow`, then tear it down. Lets a UI change be
/// screenshotted programmatically — no window ever appears and the desktop is never driven.
/// Returns whether the PNG was written.
///
/// Optionally driven first: `--click <name>` clicks one of [`SHOT_CONTROLS`] the way the mouse
/// does (a check box flips, then its parent hears `BN_CLICKED`; `BM_CLICK` would need the
/// window active, which a headless shot must not take), and `--report <file>` writes each
/// one's state, plus what Save would store for the custom action's hotkey.
pub(crate) unsafe fn run_shot_driven(
    hinst: HINSTANCE,
    dark: bool,
    out: &str,
    tab: usize,
    args: &[String],
) -> bool {
    let Some(hwnd) = build_settings_shot_window(hinst, dark) else {
        return false;
    };
    settle_pane(hwnd, tab);
    if let Some(&(_, id)) =
        arg_after(args, "--click").and_then(|name| SHOT_CONTROLS.iter().find(|(n, _)| *n == name))
    {
        click_control(hwnd, id);
        st2k_appkit::win::force_repaint(hwnd);
        st2k_appkit::win::pump_msgs(4);
    }
    // Space over every row of the File types list at once, the bulk check a user does with
    // Ctrl+A then Space; the report then counts the ticked rows against the state Space
    // sets them to (the focused row's opposite).
    let mut space_target = String::new();
    if args.iter().any(|a| a == "--space-all-formats") {
        if let Ok(list) = GetDlgItem(Some(hwnd), ID_LIST) {
            set_row_state(list, -1, LVIS_SELECTED, true);
            set_row_state(list, 0, LVIS_FOCUSED, true);
            space_target = format!("formats_space_target={}\n", !is_checked(list, 0) as u8);
            SendMessageW(
                list,
                WM_KEYDOWN,
                Some(WPARAM(VK_SPACE.0 as usize)),
                Some(LPARAM(0)),
            );
            SendMessageW(
                list,
                WM_KEYUP,
                Some(WPARAM(VK_SPACE.0 as usize)),
                Some(LPARAM(0)),
            );
            st2k_appkit::win::pump_msgs(4);
        }
    }
    if let Some(file) = arg_after(args, "--report") {
        write_controls_report(hwnd, file, &space_target);
    }
    st2k_appkit::win::capture_and_destroy(hwnd, out)
}

/// `--report`: each [`SHOT_CONTROLS`] entry's state, then what Save would store for the
/// custom action's hotkey, then `extra`.
unsafe fn write_controls_report(hwnd: HWND, file: &str, extra: &str) {
    let mut report = String::new();
    for &(name, id) in SHOT_CONTROLS {
        let Ok(c) = GetDlgItem(Some(hwnd), id) else {
            continue;
        };
        report.push_str(&format!(
            "{name} checked={} enabled={} data={}\n",
            checked(hwnd, id) as u8,
            IsWindowEnabled(c).as_bool() as u8,
            super::values::combo_item_data(hwnd, id),
        ));
    }
    report.push_str(&format!(
        "custom_action_saves={}\n",
        super::values::custom_action_chord(hwnd)
    ));
    if let Ok(list) = GetDlgItem(Some(hwnd), ID_LIST) {
        let count = SendMessageW(list, LVM_GETITEMCOUNT, None, None).0 as i32;
        let ticked = (0..count).filter(|&r| is_checked(list, r)).count();
        report.push_str(&format!("formats_checked={ticked} of {count}\n"));
    }
    report.push_str(extra);
    let _ = std::fs::write(file, report);
}

/// Controls a Settings `--shot` can click (`--click <name>`) and report (`--report <file>`),
/// by a name that survives an id renumbering.
const SHOT_CONTROLS: &[(&str, i32)] = &[
    ("custom_action_enable", ID_CUSTOM_ACTION_ENABLE),
    ("custom_action", ID_SHOT_ACTION),
    ("custom_action_hotkey", ID_SHOT_ACTION_HK),
];

/// The value after `flag` in `args`.
fn arg_after<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|p| args.get(p + 1))
        .map(String::as_str)
}

/// What a mouse click on control `id` does: an auto check box flips, then the parent hears
/// `BN_CLICKED`.
unsafe fn click_control(hwnd: HWND, id: i32) {
    let Ok(c) = GetDlgItem(Some(hwnd), id) else {
        return;
    };
    let kind = GetWindowLongW(c, GWL_STYLE) as u32 & 0x0F; // BS_TYPEMASK
    if kind == BS_AUTOCHECKBOX as u32 {
        let on = SendMessageW(c, BM_GETCHECK, None, None).0 == 1;
        SendMessageW(c, BM_SETCHECK, Some(WPARAM(usize::from(!on))), None);
    }
    SendMessageW(
        hwnd,
        WM_COMMAND,
        Some(WPARAM(id as u16 as usize)), // BN_CLICKED is 0
        Some(LPARAM(c.0 as isize)),
    );
}

/// `--shot <png> --window menuitems [--scroll ROW] [--drag FROM,TO] [--divider ROW,EDIT]
/// [--space ROW] [--reset] [--done|--cancel|--escape] [--report FILE]`: the "Edit menu items"
/// editor over a headless Settings, driven the way a user drives it, in that order.
/// `--scroll` scrolls the list so row ROW is at the top. `--drag` starts a drag of row FROM with
/// the notification comctl32 sends, delivered to the window it really notifies
/// ([`notify_target`]), then drops it just above row TO (TO = the row count drops it last) with
/// the list's own mouse messages. `--divider` runs the right-click menu's EDIT (`above`, `below`
/// or `remove`) on row ROW. `--space` presses Space on row ROW. `--reset` clicks Reset. Then
/// `--done` clicks Done, `--cancel` closes the window the way its title bar's X does, and
/// `--escape` presses Escape through the dialog key handling the modal pump runs.
///
/// The report holds `top <row>` (the list's first visible row) and `row <i> <lParam> <checked>`
/// per row before the close, then after it `final <i> <lParam> <checked>` per row of the list
/// handed back to Settings, `saved <MenuOrder>`, and `owner_enabled <0|1>`: Settings starts
/// disabled, as under the modal editor, and the close must enable it again before the editor
/// goes, or Windows activates some other program. The PNG is the editor, or Settings after a
/// close.
pub(crate) unsafe fn run_shot_menu_items(
    hinst: HINSTANCE,
    dark: bool,
    out: &str,
    args: &[String],
) -> bool {
    let Some(settings) = build_settings_shot_window(hinst, dark) else {
        return false;
    };
    let Some(popup) = menuitems::open_for_shot(settings, hinst, dark) else {
        let _ = DestroyWindow(settings);
        return false;
    };
    st2k_appkit::win::settle_pump(popup, 10, 6, false);
    let Ok(list) = GetDlgItem(Some(popup), ID_MENU_ITEMS_LIST) else {
        let _ = DestroyWindow(popup);
        let _ = DestroyWindow(settings);
        return false;
    };
    // Disabled, as the modal editor leaves it, so a close can show it re-enables Settings itself.
    let _ = windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow(settings, false);
    let mut report = drive_menu_items(list, settings, popup, args);
    st2k_appkit::win::settle_pump(popup, 4, 4, false);
    let top = SendMessageW(
        list,
        windows::Win32::UI::Controls::LVM_GETTOPINDEX,
        None,
        None,
    )
    .0;
    report.push_str(&format!("top {top}\n"));
    let colw = SendMessageW(
        list,
        windows::Win32::UI::Controls::LVM_GETCOLUMNWIDTH,
        Some(WPARAM(0)),
        None,
    )
    .0;
    let mut lrc = RECT::default();
    let _ = GetClientRect(list, &mut lrc);
    report.push_str(&format!("column {colw} {}\n", lrc.right - lrc.left));
    report.push_str(&rows_report(list, "row"));
    let ok = finish_menu_items(settings, popup, list, out, args, &mut report);
    if let Some(file) = arg_after(args, "--report") {
        let _ = std::fs::write(file, report);
    }
    ok
}

/// `--scroll`, `--drag`, `--divider`, `--space` and `--reset`, in that order; returns the
/// `scrolled <top>` line (the first visible row once scrolled, as far as the list goes) and the
/// drag's `notify` line, if any.
unsafe fn drive_menu_items(list: HWND, settings: HWND, popup: HWND, args: &[String]) -> String {
    let mut report = String::new();
    if let Some(row) = arg_after(args, "--scroll").and_then(|s| s.parse::<i32>().ok()) {
        scroll_to_row(list, row);
        let top = SendMessageW(
            list,
            windows::Win32::UI::Controls::LVM_GETTOPINDEX,
            None,
            None,
        )
        .0;
        report.push_str(&format!("scrolled {top}\n"));
    }
    report.push_str(&drag_from_args(list, settings, popup, args));
    if let Some((row, edit)) = arg_after(args, "--divider").and_then(|s| s.split_once(',')) {
        let edit = match edit {
            "above" => Some(list::DividerEdit::Above),
            "below" => Some(list::DividerEdit::Below),
            "remove" => Some(list::DividerEdit::Remove),
            _ => None,
        };
        if let (Ok(row), Some(edit)) = (row.parse(), edit) {
            list::apply_divider_edit(list, row, edit);
        }
    }
    if let Some(row) = arg_after(args, "--space").and_then(|s| s.parse().ok()) {
        press_space(list, row);
    }
    if args.iter().any(|a| a == "--reset") {
        SendMessageW(popup, WM_COMMAND, Some(WPARAM(ID_POP_RESET as usize)), None);
    }
    report
}

/// Scroll `list` so `row` is its first visible row (as far as the list can scroll).
unsafe fn scroll_to_row(list: HWND, row: i32) {
    let mut r = RECT::default(); // .left = LVIR_BOUNDS (0)
    SendMessageW(
        list,
        windows::Win32::UI::Controls::LVM_GETITEMRECT,
        Some(WPARAM(0)),
        Some(LPARAM(&mut r as *mut _ as isize)),
    );
    let top = SendMessageW(
        list,
        windows::Win32::UI::Controls::LVM_GETTOPINDEX,
        None,
        None,
    )
    .0 as i32;
    SendMessageW(
        list,
        windows::Win32::UI::Controls::LVM_SCROLL,
        Some(WPARAM(0)),
        Some(LPARAM(((row - top) * (r.bottom - r.top)) as isize)),
    );
}

/// `--drag FROM,TO`: the drag, and a `notify <who>` line naming the window comctl32 notified
/// (Settings, though the list is the editor's child by now). Empty without `--drag`.
unsafe fn drag_from_args(list: HWND, settings: HWND, popup: HWND, args: &[String]) -> String {
    let pair = arg_after(args, "--drag").and_then(|s| s.split_once(','));
    let Some((from, to)) = pair.and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?))) else {
        return String::new();
    };
    let who = match drag_row(list, from, to) {
        Some(t) if t == settings => "settings",
        Some(t) if t == popup => "popup",
        Some(_) => "other",
        None => "none",
    };
    format!("notify {who}\n")
}

/// `<tag> <i> <lParam> <checked>` for every row of the editor's list.
unsafe fn rows_report(list: HWND, tag: &str) -> String {
    let count = SendMessageW(list, LVM_GETITEMCOUNT, None, None).0 as i32;
    (0..count)
        .map(|row| {
            format!(
                "{tag} {row} {} {}\n",
                menu_row_param(list, row),
                is_checked(list, row) as u8
            )
        })
        .collect()
}

/// `--done`, `--cancel` or `--escape` closes the editor that way, records the rows Settings got
/// back and the saved `MenuOrder`, and captures Settings; without one the editor is captured.
/// Both windows are gone afterwards. Returns whether the PNG was written.
unsafe fn finish_menu_items(
    settings: HWND,
    popup: HWND,
    list: HWND,
    out: &str,
    args: &[String],
    report: &mut String,
) -> bool {
    let has = |flag: &str| args.iter().any(|a| a == flag);
    if has("--done") {
        SendMessageW(popup, WM_COMMAND, Some(WPARAM(IDOK as usize)), None);
    } else if has("--cancel") {
        SendMessageW(popup, WM_CLOSE, None, None);
    } else if has("--escape") {
        press_escape(popup, list);
    } else {
        let ok = st2k_appkit::win::capture_and_destroy(popup, out);
        let _ = DestroyWindow(settings);
        return ok;
    }
    report.push_str(&rows_report(list, "final"));
    report.push_str(&format!("saved {}\n", settings::menu_order().join(",")));
    let enabled = windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled(settings);
    report.push_str(&format!("owner_enabled {}\n", u8::from(enabled.as_bool())));
    st2k_appkit::win::settle_pump(settings, 4, 4, false);
    st2k_appkit::win::capture_and_destroy(settings, out)
}

/// Escape on the list, through `IsDialogMessageW` exactly as the editor's modal pump hands it
/// every message: that is what turns the key into the popup's IDCANCEL.
unsafe fn press_escape(popup: HWND, list: HWND) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{SetFocus, VK_ESCAPE};
    let _ = SetFocus(Some(list));
    let msg = MSG {
        hwnd: list,
        message: WM_KEYDOWN,
        wParam: WPARAM(VK_ESCAPE.0 as usize),
        lParam: LPARAM(1),
        ..Default::default()
    };
    if !IsDialogMessageW(popup, &msg).as_bool() {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}

thread_local! {
    /// `(list, first window seen receiving a WM_NOTIFY from it)` for [`notify_target`].
    static SPY: std::cell::Cell<(isize, isize)> = const { std::cell::Cell::new((0, 0)) };
}

unsafe extern "system" fn spy_notify(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    if code >= 0 {
        let cwp = &*(l.0 as *const CWPSTRUCT);
        if cwp.message == WM_NOTIFY && cwp.lParam.0 != 0 {
            let from = (*(cwp.lParam.0 as *const NMHDR)).hwndFrom.0 as isize;
            SPY.with(|s| {
                let (list, seen) = s.get();
                if from == list && seen == 0 {
                    s.set((list, cwp.hwnd.0 as isize));
                }
            });
        }
    }
    CallNextHookEx(None, code, w, l)
}

/// The window comctl32 sends `list`'s notifications to: not necessarily its parent, since a
/// list remembers the window it was created under (see `notify::on_notify_begindrag`). Found by
/// watching where one lands while row 0's focus is moved off and back.
unsafe fn notify_target(list: HWND) -> Option<HWND> {
    use windows::Win32::System::Threading::GetCurrentThreadId;
    SPY.with(|s| s.set((list.0 as isize, 0)));
    let hook =
        SetWindowsHookExW(WH_CALLWNDPROC, Some(spy_notify), None, GetCurrentThreadId()).ok()?;
    set_row_state(list, 0, LVIS_FOCUSED, false);
    set_row_state(list, 0, LVIS_FOCUSED, true);
    let _ = UnhookWindowsHookEx(hook);
    let seen = SPY.with(|s| s.get().1);
    (seen != 0).then_some(HWND(seen as *mut c_void))
}

/// Set or clear `flag` on `row` (-1 = every row).
unsafe fn set_row_state(list: HWND, row: i32, flag: LIST_VIEW_ITEM_STATE_FLAGS, on: bool) {
    let it = LVITEMW {
        stateMask: flag,
        state: if on {
            flag
        } else {
            LIST_VIEW_ITEM_STATE_FLAGS(0)
        },
        ..Default::default()
    };
    SendMessageW(
        list,
        LVM_SETITEMSTATE,
        Some(WPARAM(row as usize)),
        Some(LPARAM(&it as *const _ as isize)),
    );
}

/// Drag row `from` and drop it just above row `to` (below the last row when `to` is the row
/// count), the way a mouse does once the drag has begun. Returns the window comctl32 notified.
unsafe fn drag_row(list: HWND, from: i32, to: i32) -> Option<HWND> {
    let target = notify_target(list)?;
    let mut nm = NMLISTVIEW {
        hdr: NMHDR {
            hwndFrom: list,
            idFrom: GetDlgCtrlID(list) as usize,
            code: windows::Win32::UI::Controls::LVN_BEGINDRAG,
        },
        iItem: from,
        ..Default::default()
    };
    SendMessageW(
        target,
        WM_NOTIFY,
        Some(WPARAM(nm.hdr.idFrom)),
        Some(LPARAM(&mut nm as *mut _ as isize)),
    );
    let count = SendMessageW(list, LVM_GETITEMCOUNT, None, None).0 as i32;
    let mut r = RECT::default(); // .left = LVIR_BOUNDS (0)
    SendMessageW(
        list,
        windows::Win32::UI::Controls::LVM_GETITEMRECT,
        Some(WPARAM(to.min(count - 1).max(0) as usize)),
        Some(LPARAM(&mut r as *mut _ as isize)),
    );
    let y = if to >= count { r.bottom + 2 } else { r.top + 1 };
    let at = LPARAM((((y as u16 as u32) << 16) | (r.left + 12) as u16 as u32) as isize);
    SendMessageW(
        list,
        WM_MOUSEMOVE,
        Some(WPARAM(0x0001)), /* MK_LBUTTON */
        Some(at),
    );
    SendMessageW(list, WM_LBUTTONUP, Some(WPARAM(0)), Some(at));
    Some(target)
}

/// Space on row `row` alone: select and focus it, then the key.
unsafe fn press_space(list: HWND, row: i32) {
    set_row_state(list, -1, LVIS_SELECTED, false);
    set_row_state(
        list,
        row,
        LIST_VIEW_ITEM_STATE_FLAGS(LVIS_SELECTED.0 | LVIS_FOCUSED.0),
        true,
    );
    SendMessageW(
        list,
        WM_KEYDOWN,
        Some(WPARAM(VK_SPACE.0 as usize)),
        Some(LPARAM(0)),
    );
    SendMessageW(
        list,
        WM_KEYUP,
        Some(WPARAM(VK_SPACE.0 as usize)),
        Some(LPARAM(0)),
    );
}

/// `--audit-layout`'s Settings half: build the window ONCE and hand `each` every page in
/// turn, laid out and settled, instead of building it again per page the way a `--shot --tab N`
/// run has to (creating the window's ~150 controls is most of a capture's cost). Pixels do not
/// matter here, so none of `settle_pane`'s repaints are needed; the first switch goes to the
/// last page so that the switch to page 0 is a real transition too. Returns false when the
/// window could not be built.
pub(crate) unsafe fn audit_every_page(
    hinst: HINSTANCE,
    dark: bool,
    mut each: impl FnMut(usize, HWND),
) -> bool {
    let Some(hwnd) = build_settings_shot_window(hinst, dark) else {
        return false;
    };
    switch_category(hwnd, NCAT - 1);
    for tab in 0..NCAT {
        switch_category(hwnd, tab);
        st2k_appkit::win::pump_msgs(2);
        each(tab, hwnd);
    }
    let _ = DestroyWindow(hwnd);
    true
}

/// `--shot … --search <needle>[!]` : drive the settings-wide search headlessly and capture
/// the result — proof the feature works, not just that the box renders. Types `needle`
/// into the search box (EN_CHANGE fires synchronously, populating the dropdown) and
/// captures with the dropdown open; with a trailing `!` it then PICKS the first hit and
/// captures the landing page instead, focus ring and all.
pub(crate) unsafe fn run_shot_search(hinst: HINSTANCE, dark: bool, out: &str, arg: &str) -> bool {
    let Some(hwnd) = build_settings_shot_window(hinst, dark) else {
        return false;
    };
    settle_pane(hwnd, 0);
    let (needle, pick) = match arg.strip_suffix('!') {
        Some(n) => (n, true),
        None => (arg, false),
    };
    if let Ok(edit) = GetDlgItem(Some(hwnd), ID_SEARCH_GLOBAL) {
        let w = st2k_appkit::win::wide(needle);
        SendMessageW(edit, WM_SETTEXT, None, Some(LPARAM(w.as_ptr() as isize)));
    }
    st2k_appkit::win::pump_msgs(8);
    if pick {
        if let Ok(list) = GetDlgItem(Some(hwnd), ID_SEARCH_RESULTS) {
            SendMessageW(list, LB_SETCURSEL, Some(WPARAM(0)), None);
            // LB_SETCURSEL doesn't notify; deliver the pick the way a click would.
            SendMessageW(
                hwnd,
                WM_COMMAND,
                Some(WPARAM(
                    (ID_SEARCH_RESULTS as usize) | ((LBN_SELCHANGE as usize) << 16),
                )),
                Some(LPARAM(list.0 as isize)),
            );
        }
        st2k_appkit::win::force_repaint(hwnd);
        st2k_appkit::win::pump_msgs(8);
    }
    st2k_appkit::win::force_repaint(hwnd);
    st2k_appkit::win::pump_msgs(4);
    st2k_appkit::win::capture_and_destroy(hwnd, out)
}

/// The app's `--shot-gif` mode: capture every category tab, each in its own fresh `--shot --tab N`
/// process, then encode them into an animated (infinite-loop) GIF at
/// `out` — the regenerable README/site asset that cycles the Settings tabs. Frames are
/// downscaled to the 96-dpi design width so the GIF stays crisp + small. Returns whether the
/// GIF was written.
pub(crate) unsafe fn run_shot_gif(_hinst: HINSTANCE, _dark: bool, out: &str) -> bool {
    // Capture each frame in a FRESH PROCESS (`--shot --tab N`), then assemble the GIF from the
    // PNGs. This is deliberate: the single-shot path is the ONLY one that reliably renders every
    // tab correctly. Reusing one window across tabs in-process raced the owner-drawn nav-rail
    // highlight (the capture grabbed the PREVIOUS tab's selection off a trailing DWM surface),
    // and churning fresh windows in a tight in-process loop left some frames blank (each window
    // hadn't finished painting). A separate, fully-initialized process per frame sidesteps both.
    // ~1 s each; this is a rarely-run asset-regen path, so the extra spawns don't matter.
    use std::os::windows::process::CommandExt;
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let tmp = std::env::temp_dir();
    let pid = std::process::id();
    let mut frames = Vec::with_capacity(NCAT);
    for tab in 0..NCAT {
        let png = tmp.join(format!("st2k_gifframe_{pid}_{tab}.png"));
        let Some(png_s) = png.to_str() else { continue };
        let ok = std::process::Command::new(&exe)
            .args(["--shot", png_s, "--tab", &tab.to_string()])
            .creation_flags(st2k_base::host::CREATE_NO_WINDOW)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            if let Ok(img) = image::open(&png) {
                frames.push(st2k_appkit::win::window_shot::downscale_to_width(
                    img.to_rgba8(),
                    772,
                ));
            }
        }
        let _ = std::fs::remove_file(&png);
    }
    if frames.is_empty() {
        return false;
    }
    // ~1.6 s per tab so a reader can take each pane in before it advances.
    st2k_appkit::win::window_shot::encode_gif(&frames, std::path::Path::new(out), 1600)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prime_category_targets_the_last_tab_when_the_wanted_tab_is_the_first() {
        assert_eq!(prime_category(0, 10), 9);
    }

    #[test]
    fn prime_category_targets_the_first_tab_for_any_other_wanted_tab() {
        assert_eq!(prime_category(1, 10), 0);
        assert_eq!(prime_category(9, 10), 0);
    }
}
