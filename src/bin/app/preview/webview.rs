//! WebView2 HTML host for Quick preview (feature `html-preview`, EXE-only). Renders a local
//! `.html` file or, strictly opt-in, live-loads a `.url` target. LOCKED DOWN: local HTML runs with
//! JavaScript OFF and every non-`file://` request blocked (a tracking-pixel page physically cannot
//! phone home); both modes use a unique, EPHEMERAL, per-process user-data folder (no
//! cookie/session reuse, no cross-process lock contention), wiped on close. All WebView2 code
//! lives behind the `html-preview` feature so the shell-extension DLL never links `webview2-com`.

use std::cell::RefCell;
use std::rc::Rc;

use webview2_com::Microsoft::Web::WebView2::Win32::{
    CreateCoreWebView2EnvironmentWithOptions, ICoreWebView2, ICoreWebView2Controller,
    ICoreWebView2DownloadStartingEventArgs, ICoreWebView2Environment,
    ICoreWebView2NavigationStartingEventArgs, ICoreWebView2NewWindowRequestedEventArgs,
    ICoreWebView2WebResourceRequestedEventArgs, COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL,
};
use webview2_com::{
    CreateCoreWebView2ControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler,
    DownloadStartingEventHandler, NavigationStartingEventHandler, NewWindowRequestedEventHandler,
    WebResourceRequestedEventHandler,
};
use windows::core::{w, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// Local file (sandboxed: scripts off + no network) vs live remote (`.url`). Both get an
/// ephemeral, per-process profile.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum Mode {
    Local,
    Live,
}

/// A live WebView2 host over the viewer's content area. Dropping it closes the controller and
/// wipes the ephemeral profile.
pub(super) struct WebViewHost {
    controller: ICoreWebView2Controller,
    profile_dir: Option<std::path::PathBuf>,
}

/// Create a WebView2 over `parent` at `rect`, navigate to `url`, and lock it down per `mode`.
/// `None` on any failure (missing runtime, non-writable profile, async error) — the caller falls
/// back to a text/card preview. Blocks briefly while pumping messages for the two async creates.
pub(super) unsafe fn create(
    parent: HWND,
    rect: &RECT,
    url: &str,
    mode: Mode,
) -> Option<WebViewHost> {
    // WebView2 requires the calling (UI) thread to be a COM Single-Threaded Apartment. The preview
    // thread isn't otherwise COM-initialized, so init it here (idempotent — S_FALSE if already STA;
    // we intentionally never CoUninitialize, leaving the apartment for the thread's life).
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

    // The user-data folder MUST be writable (never Program Files); both modes get a unique
    // per-process ephemeral dir, wiped on drop.
    let (profile_dir, udf) = resolve_profile_dir(mode)?;
    let udf_h = HSTRING::from(udf.as_os_str());

    let environment = create_environment(&udf_h)?;
    let controller = create_controller(&environment, parent)?;
    // Closes the controller on every early exit below (a lockdown failure, a guard-install
    // failure, a failed Navigate); forgotten only once ownership passes to `WebViewHost` at the
    // bottom, whose own `Drop` then closes it instead. Without this every `?` between here and
    // there left the controller visible and live.
    let close_guard = ControllerCloseGuard(controller.clone());

    controller.SetBounds(*rect).ok()?;
    controller.SetIsVisible(true).ok()?;
    let webview = controller.CoreWebView2().ok()?;

    apply_lockdown(&webview, mode)?;
    if mode == Mode::Local {
        install_local_mode_guards(&webview, &environment, parent)?;
    }

    let url_h = HSTRING::from(url);
    webview.Navigate(PCWSTR(url_h.as_ptr())).ok()?;
    std::mem::forget(close_guard);
    Some(WebViewHost {
        controller,
        profile_dir,
    })
}

/// Closes the wrapped controller when dropped. See `create`'s use of it for why.
struct ControllerCloseGuard(ICoreWebView2Controller);

impl Drop for ControllerCloseGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = self.0.Close();
        }
    }
}

/// Resolve the WebView2 user-data folder: `(ephemeral profile dir to remember for cleanup, the
/// folder to actually pass as the user-data folder)`. Both modes get a unique per-process
/// folder now, not just Live — a fixed shared folder let two concurrent preview processes (two
/// Explorer windows previewing local HTML at once, say) race for the same WebView2 environment
/// lock, so one of them failed to open at all. `None` only when neither the portable ini's
/// directory nor `LOCALAPPDATA` is available.
fn resolve_profile_dir(_mode: Mode) -> Option<(Option<std::path::PathBuf>, std::path::PathBuf)> {
    let root = profile_root()?;
    // Clear out any ephemeral profile a previous run failed to remove before making ours. `Drop`
    // calls `remove_dir_all` right after `controller.Close()`, but Close returns when the
    // CONTROLLER is torn down, not when the msedgewebview2 host process has exited and released
    // its handles — so the removal can lose that race, and nothing retried it. These hold a live
    // page's cookies and cache; they shouldn't accumulate under LOCALAPPDATA.
    sweep_stale_profiles(&root);
    let d = root.join(format!("wv2-ephemeral-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&d);
    Some((Some(d.clone()), d))
}

/// Root folder the WebView2 user-data folder is created under.
///
/// Portable mode: beside the portable ini, matching the portable-aware resolution
/// `settings_io.rs` and `sync_client.rs` already use elsewhere — a portable copy is supposed to
/// leave nothing behind under the host's own profile.
///
/// Otherwise: `%LOCALAPPDATA%\SageThumbs2K`, same as every non-portable install. This IS the
/// one path this module cannot make portable-relative in principle even if it wanted to:
/// WebView2 requires its user-data folder be writable, and a machine-wide install lives under
/// Program Files, which ordinary users cannot write to — so a non-portable install always needs
/// a per-user, writable location regardless of where the exe itself sits (the same reasoning
/// `screenshot/enable.rs::autostart_allowed` documents for why ITS portable exception runs the
/// other way, skipping a write non-portable installs make).
fn profile_root() -> Option<std::path::PathBuf> {
    if sagethumbs2k_core::settings::portable() {
        if let Some(dir) = sagethumbs2k_core::settings::ini_path().and_then(|p| p.parent()) {
            return Some(dir.join("SageThumbs2K-wv2"));
        }
    }
    let base = std::env::var("LOCALAPPDATA").ok()?;
    Some(std::path::Path::new(&base).join("SageThumbs2K"))
}

/// Async #1: create the WebView2 environment over `udf` (pumps messages until ready).
unsafe fn create_environment(udf_h: &HSTRING) -> Option<ICoreWebView2Environment> {
    let env_cell: Rc<RefCell<Option<ICoreWebView2Environment>>> = Rc::new(RefCell::new(None));
    let ec = env_cell.clone();
    let udf_h = udf_h.clone();
    CreateCoreWebView2EnvironmentCompletedHandler::wait_for_async_operation(
        Box::new(move |handler| {
            CreateCoreWebView2EnvironmentWithOptions(
                PCWSTR::null(),
                PCWSTR(udf_h.as_ptr()),
                None,
                &handler,
            )
            .map_err(webview2_com::Error::WindowsError)
        }),
        Box::new(move |error_code, environment| {
            error_code?;
            *ec.borrow_mut() = environment;
            Ok(())
        }),
    )
    .ok()?;
    let out = env_cell.borrow_mut().take();
    out
}

/// Async #2: create the controller parented on the viewer.
unsafe fn create_controller(
    environment: &ICoreWebView2Environment,
    parent: HWND,
) -> Option<ICoreWebView2Controller> {
    let ctrl_cell: Rc<RefCell<Option<ICoreWebView2Controller>>> = Rc::new(RefCell::new(None));
    let cc = ctrl_cell.clone();
    let env2 = environment.clone();
    CreateCoreWebView2ControllerCompletedHandler::wait_for_async_operation(
        Box::new(move |handler| {
            env2.CreateCoreWebView2Controller(parent, &handler)
                .map_err(webview2_com::Error::WindowsError)
        }),
        Box::new(move |error_code, controller| {
            error_code?;
            *cc.borrow_mut() = controller;
            Ok(())
        }),
    )
    .ok()?;
    let out = ctrl_cell.borrow_mut().take();
    out
}

/// Turn off dev tools, the default context menu, the status bar, and (local mode only)
/// JavaScript. Fallible and fail-closed: if `Settings()` or any setter errors, the caller must
/// bail to the safe card rather than navigate with the lockdown only partly (or not at all)
/// applied.
unsafe fn apply_lockdown(webview: &ICoreWebView2, mode: Mode) -> Option<()> {
    let settings = webview.Settings().ok()?;
    settings.SetAreDevToolsEnabled(false).ok()?;
    settings.SetAreDefaultContextMenusEnabled(false).ok()?;
    settings.SetIsStatusBarEnabled(false).ok()?;
    if mode == Mode::Local {
        settings.SetIsScriptEnabled(false).ok()?; // no JS for a local file
    }
    Some(())
}

/// Read an event's URI after calling its `Uri(&mut PWSTR)` accessor (`res`, `uri_p` are that
/// call's return value and out-param), freeing the returned string with `CoTaskMemFree`. Empty
/// string on any failure (the call errored, or returned a null pointer); shared by both guard
/// handlers below, which otherwise had this read-and-free dance duplicated verbatim.
unsafe fn read_event_uri(res: windows::core::Result<()>, uri_p: PWSTR) -> String {
    if res.is_ok() && !uri_p.is_null() {
        let s = uri_p.to_string().unwrap_or_default();
        CoTaskMemFree(Some(uri_p.as_ptr() as *const _));
        s
    } else {
        String::new()
    }
}

/// True when `uri` is a `file:` URI whose authority (the host between `file://` and the next
/// `/` or `\`) is empty or `localhost` — the only shapes that stay on the local machine. A
/// non-empty, non-localhost authority is the two/four-slash UNC form (`file://host/share/...`),
/// which the OS network redirector resolves as an outbound SMB connection and can leak the
/// current user's Net-NTLM hash on load, no click required.
fn is_local_file_uri(uri: &str) -> bool {
    let Some(rest) = uri.strip_prefix("file:") else {
        return false;
    };
    // Count the whole slash run: `file:C:...` and `file:/C:/...` have no authority,
    // `file:///C:/...` is an empty authority before an absolute path, while `file://host/`
    // and the four-slash `file:////host/share/` both name a host. Stripping exactly two
    // would leave `//host/...`, whose first token is empty, and wave the UNC form through.
    let after_slashes = rest.trim_start_matches(['/', '\\']);
    let slashes = rest.len() - after_slashes.len();
    if slashes == 0 || slashes == 1 || slashes == 3 {
        return true;
    }
    let authority = after_slashes.split(['/', '\\']).next().unwrap_or("");
    authority.is_empty() || authority.eq_ignore_ascii_case("localhost")
}

/// `WebResourceRequested` handler body: block every request that isn't a local `file://` URI
/// (per `is_local_file_uri`) with a 403, so a local page can't fetch remote images/fonts/beacons
/// or open a UNC path.
unsafe fn on_web_resource_requested(
    env: &ICoreWebView2Environment,
    args: Option<ICoreWebView2WebResourceRequestedEventArgs>,
) -> windows::core::Result<()> {
    let Some(args) = args else {
        return Ok(());
    };
    let Ok(request) = args.Request() else {
        return Ok(());
    };
    let mut uri_p = PWSTR::null();
    let res = request.Uri(&mut uri_p);
    let uri = read_event_uri(res, uri_p);
    if !is_local_file_uri(&uri) {
        if let Ok(resp) = env.CreateWebResourceResponse(None, 403, w!("Blocked"), w!("")) {
            let _ = args.SetResponse(&resp);
        }
    }
    Ok(())
}

/// `NavigationStarting` handler body. The resource filter installed alongside this blocks a
/// clicked link's own navigation too (Document is one of its ALL contexts), which just swaps in
/// a blank 403, i.e. "does nothing" from the user's seat; intercepting navigation directly is
/// what lets a local `file://` (the loaded page, or an in-page anchor) pass unchanged while
/// `http(s)` is canceled and handed to the OS default browser via `ShellExecuteW`, same
/// allow-and-launch shape as the Markdown link path in `window.rs::open_preview_link`; anything
/// else — including a UNC-authority `file://` — is canceled and dropped outright, since an
/// untrusted local HTML file must not launch an arbitrary protocol handler or open an SMB
/// session from a click.
unsafe fn on_navigation_starting(
    parent: HWND,
    args: Option<ICoreWebView2NavigationStartingEventArgs>,
) -> windows::core::Result<()> {
    let Some(args) = args else {
        return Ok(());
    };
    let mut uri_p = PWSTR::null();
    let res = args.Uri(&mut uri_p);
    let uri = read_event_uri(res, uri_p);
    if is_local_file_uri(&uri) {
        return Ok(()); // the page load itself, or a same-file anchor: unchanged
    }
    let _ = args.SetCancel(true);
    let lower = uri.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        let w = HSTRING::from(uri.as_str());
        let _ = ShellExecuteW(
            Some(parent),
            w!("open"),
            PCWSTR(w.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
    Ok(())
}

/// `NewWindowRequested` handler body: a `target="_blank"` link (or `window.open`) would
/// otherwise get a brand-new WebView2 with none of `install_local_mode_guards`' filters. Marking
/// the event `Handled` suppresses that pop-up window entirely; `http(s)` targets are instead
/// routed through the same `ShellExecuteW` path `on_navigation_starting` uses, and everything
/// else is dropped.
unsafe fn on_new_window_requested(
    parent: HWND,
    args: Option<ICoreWebView2NewWindowRequestedEventArgs>,
) -> windows::core::Result<()> {
    let Some(args) = args else {
        return Ok(());
    };
    let mut uri_p = PWSTR::null();
    let res = args.Uri(&mut uri_p);
    let uri = read_event_uri(res, uri_p);
    let _ = args.SetHandled(true);
    let lower = uri.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        let w = HSTRING::from(uri.as_str());
        let _ = ShellExecuteW(
            Some(parent),
            w!("open"),
            PCWSTR(w.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
    Ok(())
}

/// `DownloadStarting` handler body: an untrusted local page must not be able to drop a file to
/// disk, so every download is unconditionally canceled.
unsafe fn on_download_starting(
    args: Option<ICoreWebView2DownloadStartingEventArgs>,
) -> windows::core::Result<()> {
    if let Some(args) = args {
        let _ = args.SetCancel(true);
    }
    Ok(())
}

/// Local mode's guards: block every non-local resource request, intercept navigation so a
/// clicked link opens in the OS default browser instead of silently doing nothing (the resource
/// filter alone would blank-403 it), suppress `target="_blank"` pop-ups (routing `http(s)`
/// through the same OS-launch path), and cancel every download.
unsafe fn install_local_mode_guards(
    webview: &ICoreWebView2,
    environment: &ICoreWebView2Environment,
    parent: HWND,
) -> Option<()> {
    // Block EVERY non-local request so a local page can't fetch remote images/fonts/beacons.
    webview
        .AddWebResourceRequestedFilter(w!("*"), COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL)
        .ok()?;
    let env3 = environment.clone();
    let handler = WebResourceRequestedEventHandler::create(Box::new(
        move |_wv, args: Option<ICoreWebView2WebResourceRequestedEventArgs>| {
            on_web_resource_requested(&env3, args)
        },
    ));
    let mut token: i64 = 0;
    let _ = webview.add_WebResourceRequested(&handler, &mut token);

    let nav_handler = NavigationStartingEventHandler::create(Box::new(
        move |_wv, args: Option<ICoreWebView2NavigationStartingEventArgs>| {
            on_navigation_starting(parent, args)
        },
    ));
    let mut nav_token: i64 = 0;
    let _ = webview.add_NavigationStarting(&nav_handler, &mut nav_token);

    let new_window_handler = NewWindowRequestedEventHandler::create(Box::new(
        move |_wv, args: Option<ICoreWebView2NewWindowRequestedEventArgs>| {
            on_new_window_requested(parent, args)
        },
    ));
    let mut new_window_token: i64 = 0;
    let _ = webview.add_NewWindowRequested(&new_window_handler, &mut new_window_token);

    let download_handler = DownloadStartingEventHandler::create(Box::new(
        move |_wv, args: Option<ICoreWebView2DownloadStartingEventArgs>| on_download_starting(args),
    ));
    let mut download_token: i64 = 0;
    let _ = webview.add_DownloadStarting(&download_handler, &mut download_token);
    Some(())
}

/// Remove `wv2-ephemeral-<pid>` folders left behind by earlier runs.
///
/// The pid in the name is the check: a folder is only removed when NO live process holds that id,
/// so a second viewer running right now (or this process's own folder) is never touched. Pid reuse
/// can make us skip one — the next sweep gets it. Entirely best-effort: a folder whose files are
/// still locked simply stays for next time.
fn sweep_stale_profiles(root: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for e in entries.flatten().take(256) {
        let name = e.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|n| n.strip_prefix("wv2-ephemeral-"))
            .and_then(|p| p.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == std::process::id() || pid_is_alive(pid) {
            continue;
        }
        let _ = std::fs::remove_dir_all(e.path());
    }
}

/// Is a process with this id currently running? Used only to decide whether a leftover profile
/// folder is safe to delete, so "can't tell" is treated as "alive" (leave it alone).
fn pid_is_alive(pid: u32) -> bool {
    use windows::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false; // no such process (or gone) — the folder is fair game
        };
        let mut code = 0u32;
        let alive = GetExitCodeProcess(h, &mut code).is_ok() && code == STILL_ACTIVE.0 as u32;
        let _ = CloseHandle(h);
        alive
    }
}

impl WebViewHost {
    /// Resize the webview to `rect` (client coords of the parent).
    pub(super) unsafe fn place(&self, rect: &RECT) {
        let _ = self.controller.SetBounds(*rect);
    }
}

impl Drop for WebViewHost {
    fn drop(&mut self) {
        unsafe {
            let _ = self.controller.Close();
        }
        if let Some(d) = &self.profile_dir {
            let _ = std::fs::remove_dir_all(d); // ephemeral profile — wipe cookies/cache
        }
    }
}
