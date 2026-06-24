//! SageThumbs 2K — Options.
//!
//! A native Win32 settings window (a faithful, modernized port of the original
//! SageThumbs Options dialog) that edits HKCU\Software\SageThumbs2K via the
//! crate's `settings` module, plus a per-format checkbox list. It is also the
//! `Application` entry the sparse package needs.
//!
//! Built programmatically (CreateWindowExW) rather than from a dialog-template
//! resource: the layout is computed and DPI-scaled at runtime (no .rc dialog
//! template to keep in sync), a faithful match to how the original was laid out.
//! (Aside: build.rs *does* run `windres` for the icon/version resource — it just
//! compiles into OUT_DIR, which sidesteps the spaces in this project's path.)
//!
//! Reachable settings take effect immediately (the provider reads them per
//! request). Changing the per-format list rewrites the HKCR `shellex` keys,
//! which needs elevation — handled by re-running `regsvr32` (which honors the
//! per-extension flags we just wrote) elevated, exactly as the original did.
//!
//! This file is the facade / entry point. The UI is split into submodules:
//! `win` (shared Win32 primitives), `dark` (dark mode), `sponsors` (the remote
//! banner), `settings_dlg` (the main window), `about`, `convert`,
//! `files_to_folder`, `tags_to_folders`, and `eyedropper`.
// `not(test)`: under `cargo test` we need the console subsystem so the harness can
// print results; the shipped binary stays a GUI ("windows") subsystem app.
#![cfg_attr(not(test), windows_subsystem = "windows")]
#![allow(non_snake_case)]

mod about;
mod sponsors;
mod convert;
mod dark;
mod eyedropper;
mod files_to_folder;
mod image_info;
mod screenshot;
mod settings_dlg;
mod settings_io;
mod tags_to_folders;
mod update;
mod win;

use core::ffi::c_void;

use windows::core::w;
use windows::Win32::Foundation::HINSTANCE;
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    InitCommonControlsEx, ICC_LISTVIEW_CLASSES, INITCOMMONCONTROLSEX, ICC_LINK_CLASS,
    ICC_STANDARD_CLASSES, ICC_BAR_CLASSES, ICC_PROGRESS_CLASS,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use sagethumbs2k_core::i18n;

use crate::convert::run_convert_dialog;
use crate::dark::{dark_bg_brush, dark_control, dark_titlebar, init_dark_app, is_dark};
use crate::eyedropper::run_eyedropper;
use crate::files_to_folder::run_files_to_folder_dialog;
use crate::tags_to_folders::run_tags_to_folders_dialog;
use crate::win::app_icon;

fn main() {
    // Capture panics to the diagnostics log before the process aborts (panic=abort).
    sagethumbs2k_core::safety::install_panic_hook("app");
    unsafe {
        let hinst: HINSTANCE = GetModuleHandleW(None).unwrap().into();

        // Resolve the UI language (HKCU override or system) before any control
        // is created so the dialog opens already localized.
        i18n::ensure_init();

        let icc = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES
                | ICC_LINK_CLASS
                | ICC_STANDARD_CLASSES
                | ICC_BAR_CLASSES
                | ICC_PROGRESS_CLASS,
        };
        let _ = InitCommonControlsEx(&icc);

        let dark = is_dark();
        if dark {
            init_dark_app();
        }

        // Convert… mode: `--convert <listfile>` (spawned by the DLL verb) shows
        // the batch-convert dialog instead of the Options window.
        let args: Vec<String> = std::env::args().collect();
        if let Some(pos) = args.iter().position(|a| a == "--convert") {
            if let Some(listfile) = args.get(pos + 1) {
                run_convert_dialog(hinst, listfile);
            }
            return;
        }
        // Eyedropper mode: `--eyedropper` (spawned by the DLL verb) opens the
        // system-wide screen color picker.
        if args.iter().any(|a| a == "--eyedropper") {
            run_eyedropper(hinst);
            return;
        }
        // Image info: `--image-info <path>` (spawned by the DLL's Image info verb) shows
        // a verbose, copyable metadata dump for the file.
        if let Some(pos) = args.iter().position(|a| a == "--image-info") {
            if let Some(path) = args.get(pos + 1) {
                image_info::run_image_info(path);
            }
            return;
        }
        // Instant capture: `--screenshot-instant` grabs the whole screen straight to
        // the clipboard + a PNG, no overlay — the optional "quick-save" hotkey's
        // action. Checked before `--screenshot` (exact match, so they don't overlap).
        if args.iter().any(|a| a == "--screenshot-instant") {
            crate::screenshot::capture_instant();
            return;
        }
        // Screenshot mode: `--screenshot` opens the Flameshot-style capture +
        // annotation overlay (region → draw → copy/save). Wired to a hotkey by the
        // opt-in tray daemon; runnable directly for testing.
        if args.iter().any(|a| a == "--screenshot") {
            crate::screenshot::run_capture(hinst);
            return;
        }
        // Screenshot daemon: `--screenshot-daemon` runs the opt-in tray helper that
        // registers the global hotkey and spawns captures. Launched at logon (HKCU
        // autostart) only after the user enables it in Settings.
        if args.iter().any(|a| a == "--screenshot-daemon") {
            crate::screenshot::run_daemon(hinst);
            return;
        }
        // Upload mode: `--upload <png>` POSTs a capture to a keyless host and copies
        // the URL to the clipboard (spawned by the capture overlay's Upload button).
        if let Some(pos) = args.iter().position(|a| a == "--upload") {
            if let Some(path) = args.get(pos + 1) {
                crate::screenshot::run_upload(path);
            }
            return;
        }
        // Toggle the screenshot hotkey on/off (HKCU autostart + the tray daemon).
        // The Settings checkbox will drive this via screenshot::set_enabled; exposed
        // here so it's usable/testable without the UI.
        if args.iter().any(|a| a == "--screenshot-toggle") {
            crate::screenshot::set_enabled(!crate::screenshot::is_enabled());
            return;
        }
        // Files-to-folder mode: `--files-to-folder <listfile>` (spawned by the DLL
        // verb for a multi-file selection) prompts for a folder name, then moves.
        if let Some(pos) = args.iter().position(|a| a == "--files-to-folder") {
            if let Some(listfile) = args.get(pos + 1) {
                run_files_to_folder_dialog(hinst, listfile);
            }
            return;
        }
        // Tags-to-folders mode: `--tags-to-folders <listfile>` (spawned by the DLL
        // verb) sorts audio files into folders by their tags.
        if let Some(pos) = args.iter().position(|a| a == "--tags-to-folders") {
            if let Some(listfile) = args.get(pos + 1) {
                run_tags_to_folders_dialog(hinst, listfile);
            }
            return;
        }
        // Self-update confirmation: `--updated <ver>` is launched by the installer's [Run]
        // step right after a SILENT self-update finishes — pop a NON-BLOCKING tray toast
        // ("you're now on <ver>"), then exit. No modal dialog, so the update stays silent.
        // The installer gates this relaunch on its own /UPDATED marker, so a normal
        // interactive install never triggers it.
        if let Some(pos) = args.iter().position(|a| a == "--updated") {
            let ver = args.get(pos + 1).map_or(env!("CARGO_PKG_VERSION"), String::as_str);
            crate::update::show_updated_toast(ver);
            return;
        }

        let class = w!("SageThumbs2KOptions");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(settings_dlg::wndproc),
            hInstance: hinst,
            lpszClassName: class,
            hIcon: app_icon().unwrap_or_default(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            // Dark window background when the system is dark; otherwise the
            // classic button-face system color ((COLOR_BTNFACE + 1) as HBRUSH).
            hbrBackground: if dark {
                dark_bg_brush()
            } else {
                HBRUSH(16isize as *mut c_void)
            },
            ..Default::default()
        };
        RegisterClassW(&wc);

        // WS_THICKFRAME lets the user drag the window TALLER; the dialog's
        // WM_GETMINMAXINFO locks the width + a minimum height, and WM_SIZE reflows the
        // bottom-anchored controls (right list / scrollbar / fold-mask / footer) so the
        // left options get a bigger scroll viewport.
        //
        // WS_CLIPCHILDREN: the left options are real child controls that the scroll
        // path slides with SetWindowPos + a full-band invalidate each tick. Without it,
        // the parent's background erase paints INTO the child rects before they repaint,
        // which flashes on a fast scroll. Clipping the children out of the parent's paint
        // — paired with the double-buffered WM_PAINT + WM_ERASEBKGND no-op in
        // `settings_dlg` — makes each scroll frame atomic (no erase-then-paint flicker).
        let style =
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_THICKFRAME | WS_CLIPCHILDREN;
        // The control layout is laid out in 96-DPI design pixels and scaled per
        // control by `ctl()`; the window frame itself must scale to match. The
        // window opens at CW_USEDEFAULT (no HWND yet), so size against the primary
        // monitor's DPI. At 96 DPI this is the original 736×680 (identity).
        let sys_dpi = crate::win::dpi_for_system();
        let win_w = win::dpi_scale_dpi(736, sys_dpi);
        // The settings module owns the footer/banner geometry; derive the outer
        // window height from the same values so disabled sponsors leave no gap.
        let sponsors_on = sponsors::sponsors_enabled();
        let win_h_design = settings_dlg::window_height_design(dark, sponsors_on);
        let win_h = win::dpi_scale_dpi(win_h_design, sys_dpi);
        let hwnd = CreateWindowExW(
            WS_EX_CONTROLPARENT | WS_EX_DLGMODALFRAME,
            class,
            w!("SageThumbs 2K — Settings"),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            win_w,
            win_h,
            None,
            None,
            Some(hinst),
            None,
        )
        .expect("create window");

        if dark {
            dark_control(hwnd, w!("DarkMode_Explorer"));
            dark_titlebar(hwnd);
        }

        let _ = ShowWindow(hwnd, SW_SHOW);

        let mut msg = MSG::default();
        loop {
            // GetMessageW returns -1 on error, 0 on WM_QUIT, >0 otherwise.
            // as_bool() (`!= 0`) would treat -1 as "keep going" and then spin on
            // a MSG it never populated — branch on the raw value instead.
            let r = GetMessageW(&mut msg, None, 0, 0).0;
            if r == 0 || r == -1 {
                break;
            }
            if !IsDialogMessageW(hwnd, &msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}
