//! Running a dialog window and the confirmations built on it.

use super::*;

/// Register a class, create + show a dialog, run its message pump. `w`/`h` are 96-DPI design px
/// of the WINDOW, title bar and borders included; see [`create_dialog`] for what happens when
/// the controls need more room than that leaves.
#[allow(clippy::too_many_arguments)]
pub unsafe fn run_dialog(
    class: PCWSTR,
    wndproc: WNDPROC,
    title: &str,
    w: i32,
    h: i32,
    modal: Option<HWND>,
) -> Option<HWND> {
    let hwnd = create_dialog(class, wndproc, title, w, h, modal)?;
    match modal {
        None => {
            let _ = ShowWindow(hwnd, SW_SHOW);
            // These dialogs are launched by the DLL from inside Explorer's context
            // menu, i.e. by a freshly spawned process that Windows may refuse the
            // foreground to. Without this, "Convert…" can open BEHIND the Explorer
            // window that launched it and read as "the menu item did nothing".
            // Same root cause as the screenshot overlay's dead Esc key.
            force_foreground(hwnd);
            pump_until_quit(hwnd);
        }
        Some(owner) => {
            let _ = EnableWindow(owner, false);
            let _ = ShowWindow(hwnd, SW_SHOW);
            force_foreground(hwnd);
            pump_until_closed(hwnd);
            let _ = EnableWindow(owner, true);
        }
    }
    Some(hwnd)
}

/// [`run_dialog`]'s first half: register the class, create the window (its `WM_CREATE` builds
/// the controls), theme it, and grow it with [`fit_to_controls`] so every control is on
/// screen. The window is NOT shown; `run_dialog` shows it and runs the pump.
///
/// `w`/`h` size the WINDOW, but every dialog lays its controls out in CLIENT coordinates, and
/// the title bar and borders (~16 x 39 px at 96 dpi, more with a larger caption font or text
/// size) come out of it first. Four dialogs shipped with their bottom row cut off by exactly
/// that: About and Pre-build were each fixed by adding the frame back by hand, Tags-to-folders
/// by anchoring its buttons to the real client bottom (2026-07-04), and Rename with pattern
/// shipped its Rename/Cancel row as a 5 px sliver until issue #48 (2026-09-28). The fit here
/// makes the whole class impossible instead of fixing the next dialog by hand.
unsafe fn create_dialog(
    class: PCWSTR,
    wndproc: WNDPROC,
    title: &str,
    w: i32,
    h: i32,
    modal: Option<HWND>,
) -> Option<HWND> {
    let hinst: HINSTANCE = GetModuleHandleW(None).ok()?.into();
    let dark = crate::dark::is_dark();
    let wc = WNDCLASSW {
        lpfnWndProc: wndproc,
        hInstance: hinst,
        lpszClassName: class,
        // A top-level dialog carries the app icon + arrow cursor; the modal popup
        // inherits its owner's icon (the original popup set neither).
        hIcon: if modal.is_none() {
            app_icon().unwrap_or_default()
        } else {
            Default::default()
        },
        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
        // The palette's window tone in both themes: it is what `dark_ctlcolor` hands every
        // control, so the window behind them has to be the same colour or each one reads as
        // a block.
        hbrBackground: crate::dark::dark_bg_brush(),
        ..Default::default()
    };
    RegisterClassW(&wc); // idempotent: re-register returns 0 (already registered) — fine

    // Geometry: design pixels scaled to the DPI of the monitor the window will actually
    // open on, then placed there.
    //   - modal popup: the owner's DPI, centered over the owner.
    //   - top-level:   the CURSOR monitor's DPI, centered on that monitor's work area.
    //
    // Top-level dialogs used to open at CW_USEDEFAULT, which cascades from the TOP-LEFT
    // corner of the primary monitor — so the welcome window (and every other dialog that
    // comes through here: convert, feedback, image info, doctor report, …) opened in the
    // corner of the screen rather than in front of the user. The Settings window already
    // sizes AND positions itself this way (see `main.rs`); this brings the rest in line.
    // Sizing to the cursor monitor also makes the frame DPI agree with the per-control
    // `dpi_scale()` (`GetDpiForWindow`) on mixed-DPI multi-monitor setups.
    let (ex_style, style, x, y, sw, sh, parent) = match modal {
        None => {
            let (mon_dpi, work) = cursor_monitor_metrics();
            let (sw, sh) = (dpi_scale_dpi(w, mon_dpi), dpi_scale_dpi(h, mon_dpi));
            (
                WS_EX_CONTROLPARENT | WS_EX_DLGMODALFRAME,
                WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
                work.left + ((work.right - work.left) - sw).max(0) / 2,
                work.top + ((work.bottom - work.top) - sh).max(0) / 2,
                sw,
                sh,
                None,
            )
        }
        Some(owner) => {
            let creation_dpi = GetDpiForWindow(owner) as i32;
            let (sw, sh) = (
                dpi_scale_dpi(w, creation_dpi),
                dpi_scale_dpi(h, creation_dpi),
            );
            // Center over the owner.
            let mut orc = RECT::default();
            let _ = GetWindowRect(owner, &mut orc);
            (
                WS_EX_DLGMODALFRAME,
                WS_POPUP | WS_CAPTION | WS_SYSMENU,
                orc.left + ((orc.right - orc.left) - sw) / 2,
                orc.top + ((orc.bottom - orc.top) - sh) / 2,
                sw,
                sh,
                Some(owner),
            )
        }
    };

    let title_w = wide(title);
    let hwnd = CreateWindowExW(
        ex_style,
        class,
        PCWSTR(title_w.as_ptr()),
        style,
        x,
        y,
        sw,
        sh,
        parent,
        None,
        Some(hinst),
        None,
    )
    .ok()?;

    if dark {
        crate::dark::dark_control(hwnd, w!("DarkMode_Explorer"));
        crate::dark::dark_titlebar(hwnd);
    }
    fit_to_controls(hwnd);
    Some(hwnd)
}

/// A gap under this many design px between a control and the client area's right or bottom
/// edge (or a control past the edge) means the dialog was sized for less room than it got.
/// Every dialog that fits today leaves at least 7 px, so none of them moves.
const FIT_TIGHT_GAP: i32 = 4;
/// The far-side margin a grown axis gets is the dialog's own near-side margin, capped here so a
/// dialog whose first control sits low (under a painted header) does not grow a huge gutter.
const FIT_MAX_MARGIN: i32 = 24;

/// The box a dialog's child controls occupy, in client px: the smallest left/top and the
/// largest right/bottom edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ControlsExtent {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

/// How many px wider and taller a `client_w` x `client_h` client area must grow to show
/// `ext`. An axis whose far gap is at least `tight` px is left alone; a tighter or clipped one
/// grows until its far gap equals the near margin on that axis (at most `max_margin`), so the
/// right edge mirrors the left and the bottom mirrors the top.
fn fit_growth(
    client_w: i32,
    client_h: i32,
    ext: ControlsExtent,
    tight: i32,
    max_margin: i32,
) -> (i32, i32) {
    let axis = |client: i32, near: i32, far: i32| {
        if client - far >= tight {
            0
        } else {
            (far + near.clamp(0, max_margin) - client).max(0)
        }
    };
    (
        axis(client_w, ext.left, ext.right),
        axis(client_h, ext.top, ext.bottom),
    )
}

/// The extent of `hwnd`'s direct child windows in its client coordinates, hidden ones
/// included when they start inside the client area (a progress bar or an error label shown
/// later still needs its room) and left out when they start outside it: a hidden control parked
/// off to the side is not waiting to be shown there, and counting one grew a Settings capture
/// to 756 x 1933. Zero-area children are skipped. `None` when the window has no children.
unsafe fn controls_extent(hwnd: HWND) -> Option<ControlsExtent> {
    let mut client = RECT::default();
    let _ = GetClientRect(hwnd, &mut client);
    super::layoutaudit::children(hwnd)
        .into_iter()
        .filter_map(|c| super::layoutaudit::rect_in_parent(hwnd, c).map(|r| (c, r)))
        .filter(|&(c, r)| IsWindowVisible(c).as_bool() || starts_inside(r, client))
        .map(|(_, r)| ControlsExtent {
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
        })
        .reduce(|a, b| ControlsExtent {
            left: a.left.min(b.left),
            top: a.top.min(b.top),
            right: a.right.max(b.right),
            bottom: a.bottom.max(b.bottom),
        })
}

/// Whether `r` overlaps `client` at all: a hidden control that does is waiting to be shown
/// there; one wholly outside is parked.
fn starts_inside(r: RECT, client: RECT) -> bool {
    r.left < client.right && r.top < client.bottom && r.right > 0 && r.bottom > 0
}

/// Grow `hwnd` until every child control sits inside its client area (see [`fit_growth`] for
/// the rule), keeping it centred where it was and inside its monitor's work area. Never
/// shrinks. A no-op for a dialog that already fits, which is every one but a mis-sized one.
pub(super) unsafe fn fit_to_controls(hwnd: HWND) {
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    let Some(ext) = controls_extent(hwnd) else {
        return;
    };
    let mut client = RECT::default();
    let mut wr = RECT::default();
    if GetClientRect(hwnd, &mut client).is_err() || GetWindowRect(hwnd, &mut wr).is_err() {
        return;
    }
    let (dw, dh) = fit_growth(
        client.right,
        client.bottom,
        ext,
        dpi_scale(hwnd, FIT_TIGHT_GAP),
        dpi_scale(hwnd, FIT_MAX_MARGIN),
    );
    if dw == 0 && dh == 0 {
        return;
    }
    let (w, h) = (wr.right - wr.left + dw, wr.bottom - wr.top + dh);
    let (mut x, mut y) = (wr.left - dw / 2, wr.top - dh / 2);
    let mut mi = MONITORINFO {
        cbSize: core::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut mi).as_bool() {
        let work = mi.rcWork;
        x = x.min(work.right - w).max(work.left);
        y = y.min(work.bottom - h).max(work.top);
    }
    let _ = SetWindowPos(hwnd, None, x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
    st2k_base::safety::log(&format!(
        "dialog fit: grew by {dw}x{dh} px so every control is on screen"
    ));
}

/// The lifecycle tail every worker-backed dialog (Convert, Rename, Files-to-folder,
/// Tags-to-folders) shares once its own messages are handled: a DPI change re-lays out; the
/// title-bar X / Alt+F4 / taskbar close go through the dialog's own `request_close`, which
/// mirrors IDCANCEL's deferred close - a batch started on the worker thread must not be torn out
/// from under it by an unconditional `DestroyWindow` (WM_CLOSE would cascade to WM_DESTROY ->
/// `PostQuitMessage` and kill the worker mid-write); destroy quits the pump; the rest is the
/// default.
pub unsafe fn dialog_tail(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    request_close: unsafe fn(HWND),
) -> LRESULT {
    match msg {
        WM_DPICHANGED => {
            // The controls keep the layout they were built with; a move to a lower-DPI monitor
            // shrinks the frame under them, so fit again.
            wm_dpichanged(hwnd, lparam);
            fit_to_controls(hwnd);
            LRESULT(0)
        }
        WM_CLOSE => {
            request_close(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// A Yes/No question under the warning icon, for an action that briefly disrupts the desktop
/// (restarting Explorer, re-registering every format); `true` when the user chose Yes.
pub unsafe fn confirm_warning(parent: HWND, title: &str, body: &str) -> bool {
    let w_body = wide(body);
    let w_title = wide(title);
    MessageBoxW(
        Some(parent),
        PCWSTR(w_body.as_ptr()),
        PCWSTR(w_title.as_ptr()),
        MB_YESNO | MB_ICONWARNING,
    ) == IDYES
}

/// Show a simple warning message box owned by the dialog.
pub unsafe fn message_box(hwnd: HWND, text: &str, caption: &str) {
    let t = wide(text);
    let c = wide(caption);
    MessageBoxW(
        Some(hwnd),
        PCWSTR(t.as_ptr()),
        PCWSTR(c.as_ptr()),
        MB_OK | MB_ICONWARNING,
    );
}

/// A modal two-choice prompt whose BUTTONS CARRY THE VERBS ("Renew" / "Not now"), rather
/// than a `MessageBox`'s fixed Yes/No. Returns true when the first (affirmative) button was
/// chosen; anything else - the second button, Escape, or the close box - is false, which
/// every caller must treat as "do nothing".
///
/// `TaskDialogIndirect` rather than `MessageBoxW` because a Yes/No pair makes the reader
/// reconstruct which verb "Yes" meant from the sentence above it, and a dialog offering to
/// spend money is the worst place to make anyone guess. The app already links comctl32 v6
/// (its manifest and every owner-drawn control depend on it), so this costs no new
/// dependency; on the impossible failure path it falls back to a plain `MB_YESNO` so the
/// choice is still offered.
pub unsafe fn confirm_verbs(
    parent: HWND,
    title: &str,
    body: &str,
    yes_label: &str,
    no_label: &str,
) -> bool {
    use windows::Win32::UI::Controls::{
        TaskDialogIndirect, TASKDIALOGCONFIG, TASKDIALOG_BUTTON, TASKDIALOG_COMMON_BUTTON_FLAGS,
        TDF_ALLOW_DIALOG_CANCELLATION, TDF_POSITION_RELATIVE_TO_WINDOW,
    };

    // Arbitrary ids; only their identity matters, and neither collides with IDOK/IDCANCEL.
    const ID_YES: i32 = 1001;
    const ID_NO: i32 = 1002;

    let w_title = wide(title);
    let w_body = wide(body);
    let w_yes = wide(yes_label);
    let w_no = wide(no_label);
    let buttons = [
        TASKDIALOG_BUTTON {
            nButtonID: ID_YES,
            pszButtonText: PCWSTR(w_yes.as_ptr()),
        },
        TASKDIALOG_BUTTON {
            nButtonID: ID_NO,
            pszButtonText: PCWSTR(w_no.as_ptr()),
        },
    ];

    let mut cfg = TASKDIALOGCONFIG {
        cbSize: core::mem::size_of::<TASKDIALOGCONFIG>() as u32,
        hwndParent: parent,
        dwFlags: TDF_ALLOW_DIALOG_CANCELLATION | TDF_POSITION_RELATIVE_TO_WINDOW,
        // No stock buttons at all: the two custom ones below carry the whole choice.
        dwCommonButtons: TASKDIALOG_COMMON_BUTTON_FLAGS(0),
        pszWindowTitle: PCWSTR(w_title.as_ptr()),
        pszContent: PCWSTR(w_body.as_ptr()),
        cButtons: buttons.len() as u32,
        pButtons: buttons.as_ptr(),
        nDefaultButton: ID_NO,
        ..Default::default()
    };
    // `pszMainIcon` is a union; the information icon is the same intent `MB_ICONINFORMATION`
    // carried on the MessageBox this replaced.
    cfg.Anonymous1.pszMainIcon = PCWSTR(-3isize as *const u16); // TD_INFORMATION_ICON

    let mut pressed = 0i32;
    // SAFETY: every pointer in `cfg` borrows a local that outlives this call, and the call
    // is synchronous - the dialog is gone before any of them drop.
    if TaskDialogIndirect(&cfg, Some(&mut pressed), None, None).is_ok() {
        return pressed == ID_YES;
    }

    // comctl32 refused (no v6 activation context in some embedding we do not control):
    // still ask, just with the generic buttons - No stays the default, as above, so Enter
    // never takes the action.
    MessageBoxW(
        Some(parent),
        PCWSTR(w_body.as_ptr()),
        PCWSTR(w_title.as_ptr()),
        MB_YESNO | MB_ICONINFORMATION | MB_DEFBUTTON2,
    ) == IDYES
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    type Layout = &'static [(i32, i32, i32, i32)];

    /// A test control at this x or further is created hidden, parked off to the side the way
    /// Settings parks the controls of the pages it is not showing.
    const PARKED_X: i32 = 5000;

    thread_local! {
        /// The design-px (x, y, w, h) buttons the test dialog's `WM_CREATE` builds.
        static LAYOUT: Cell<Layout> = const { Cell::new(&[]) };
    }

    extern "system" fn layout_wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        unsafe {
            if msg == WM_CREATE {
                let hinst: HINSTANCE = GetModuleHandleW(None).unwrap().into();
                for (i, &(x, y, w, h)) in LAYOUT.with(Cell::get).iter().enumerate() {
                    let c = ctl(
                        hwnd,
                        BUTTON,
                        "x",
                        WS_TABSTOP,
                        x,
                        y,
                        w,
                        h,
                        100 + i as i32,
                        hinst,
                    );
                    if x >= PARKED_X {
                        let _ = ShowWindow(c, SW_HIDE);
                    }
                }
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
    }

    /// Create a `w` x `h` design-px dialog holding `layout` the way `run_dialog` does, and
    /// return its window size, its client rect and every control's rect in client px.
    unsafe fn open(w: i32, h: i32, layout: Layout) -> ((i32, i32), RECT, Vec<RECT>) {
        LAYOUT.with(|l| l.set(layout));
        let hwnd = create_dialog(w!("St2kFitTest"), Some(layout_wndproc), "fit", w, h, None)
            .expect("dialog created");
        let (mut wr, mut client) = (RECT::default(), RECT::default());
        GetWindowRect(hwnd, &mut wr).unwrap();
        GetClientRect(hwnd, &mut client).unwrap();
        let controls = (0..layout.len() as i32)
            .map(|i| {
                let c = GetDlgItem(Some(hwnd), 100 + i).unwrap();
                let mut r = RECT::default();
                GetWindowRect(c, &mut r).unwrap();
                let mut pts = [
                    windows::Win32::Foundation::POINT {
                        x: r.left,
                        y: r.top,
                    },
                    windows::Win32::Foundation::POINT {
                        x: r.right,
                        y: r.bottom,
                    },
                ];
                for p in &mut pts {
                    let _ = windows::Win32::Graphics::Gdi::ScreenToClient(hwnd, p);
                }
                RECT {
                    left: pts[0].x,
                    top: pts[0].y,
                    right: pts[1].x,
                    bottom: pts[1].y,
                }
            })
            .collect();
        let _ = DestroyWindow(hwnd);
        ((wr.right - wr.left, wr.bottom - wr.top), client, controls)
    }

    /// Issue #48: "Rename with pattern" laid its controls out for a 460 x 404 CLIENT area in a
    /// 460 x 404 WINDOW, so the title bar pushed the Rename/Cancel row off the bottom (a 5 px
    /// sliver showed) and the fields touched the right edge. Every control must now be inside
    /// the client area, with the right and bottom gaps matching the left and top margins; and a
    /// dialog that already fits must keep exactly the size it asked for.
    #[test]
    fn a_dialog_grows_until_every_control_is_on_screen_and_one_that_fits_does_not_move() {
        // The rename dialog's own geometry.
        const RENAME: Layout = &[
            (16, 16, 300, 18),   // "Pattern:"
            (16, 36, 428, 24),   // the pattern field
            (16, 158, 428, 176), // the preview list
            (260, 360, 90, 30),  // Rename
            (356, 360, 88, 30),  // Cancel
        ];
        let (_, client, controls) = unsafe { open(460, 404, RENAME) };
        let left = controls.iter().map(|r| r.left).min().unwrap();
        let top = controls.iter().map(|r| r.top).min().unwrap();
        for r in &controls {
            assert!(
                r.left >= 0 && r.top >= 0 && r.right <= client.right && r.bottom <= client.bottom,
                "control {r:?} is outside the client area {client:?}"
            );
        }
        let right_gap = client.right - controls.iter().map(|r| r.right).max().unwrap();
        let bottom_gap = client.bottom - controls.iter().map(|r| r.bottom).max().unwrap();
        assert!(
            right_gap + 1 >= left,
            "right gap {right_gap} is narrower than the left margin {left}"
        );
        assert!(
            bottom_gap + 1 >= top,
            "bottom gap {bottom_gap} is narrower than the top margin {top}"
        );

        // The last control is hidden and parked far outside: it must not grow the dialog (it
        // grew a Settings capture to 756 x 1933 once).
        const ROOMY: Layout = &[
            (16, 16, 200, 24),
            (16, 100, 100, 30),
            (PARKED_X, 3000, 100, 30),
        ];
        let (size, _, _) = unsafe { open(300, 220, ROOMY) };
        let (dpi, _) = cursor_monitor_metrics();
        assert_eq!(
            size,
            (dpi_scale_dpi(300, dpi), dpi_scale_dpi(220, dpi)),
            "a dialog that fits keeps its size"
        );
    }
}
