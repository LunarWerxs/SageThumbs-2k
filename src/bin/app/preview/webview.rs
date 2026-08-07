//! WebView2 HTML host for Quick preview (feature `html-preview`, EXE-only). Renders a local
//! `.html` file or, strictly opt-in, live-loads a `.url` target. LOCKED DOWN: local HTML runs with
//! JavaScript OFF and every non-`file://` request blocked (a tracking-pixel page physically cannot
//! phone home); the live-`.url` mode uses an EPHEMERAL user-data folder (no cookie/session reuse),
//! wiped on close. All WebView2 code lives behind the `html-preview` feature so the shell-extension
//! DLL never links `webview2-com`.

use std::cell::RefCell;
use std::rc::Rc;

use webview2_com::Microsoft::Web::WebView2::Win32::{
    CreateCoreWebView2EnvironmentWithOptions, ICoreWebView2Controller, ICoreWebView2Environment,
    ICoreWebView2NavigationStartingEventArgs, ICoreWebView2WebResourceRequestedEventArgs,
    COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL,
};
use webview2_com::{
    CreateCoreWebView2ControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler,
    NavigationStartingEventHandler, WebResourceRequestedEventHandler,
};
use windows::core::{w, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// Local file (sandboxed: scripts off + no network) vs live remote (`.url`, ephemeral profile).
#[derive(Clone, Copy, PartialEq)]
pub(super) enum Mode {
    Local,
    Live,
}

/// A live WebView2 host over the viewer's content area. Dropping it closes the controller and, in
/// live mode, wipes the ephemeral profile.
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

    // The user-data folder MUST be writable (never Program Files). Live mode uses a unique
    // ephemeral dir wiped on drop; local mode reuses a fixed per-user cache dir.
    let base = std::env::var("LOCALAPPDATA").ok()?;
    let root = std::path::Path::new(&base).join("SageThumbs2K");
    let profile_dir = if mode == Mode::Live {
        // Clear out any ephemeral profile a previous run failed to remove before making ours.
        // `Drop` calls `remove_dir_all` right after `controller.Close()`, but Close returns when
        // the CONTROLLER is torn down, not when the msedgewebview2 host process has exited and
        // released its handles — so the removal can lose that race, and nothing retried it. These
        // hold a live page's cookies and cache; they shouldn't accumulate under LOCALAPPDATA.
        sweep_stale_profiles(&root);
        let d = root.join(format!("wv2-ephemeral-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        Some(d)
    } else {
        None
    };
    let udf = profile_dir.clone().unwrap_or_else(|| root.join("wv2"));
    let _ = std::fs::create_dir_all(&udf);
    let udf_h = HSTRING::from(udf.as_os_str());

    // --- async #1: create the environment (pumps messages until ready) ---
    let env_cell: Rc<RefCell<Option<ICoreWebView2Environment>>> = Rc::new(RefCell::new(None));
    let ec = env_cell.clone();
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
    let environment = env_cell.borrow_mut().take()?;

    // --- async #2: create the controller parented on the viewer ---
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
    let controller = ctrl_cell.borrow_mut().take()?;

    controller.SetBounds(*rect).ok()?;
    controller.SetIsVisible(true).ok()?;
    let webview = controller.CoreWebView2().ok()?;

    // --- lockdown ---
    if let Ok(settings) = webview.Settings() {
        let _ = settings.SetAreDevToolsEnabled(false);
        let _ = settings.SetAreDefaultContextMenusEnabled(false);
        let _ = settings.SetIsStatusBarEnabled(false);
        if mode == Mode::Local {
            let _ = settings.SetIsScriptEnabled(false); // no JS for a local file
        }
    }
    if mode == Mode::Local {
        // Block EVERY non-file:// request so a local page can't fetch remote images/fonts/beacons.
        webview
            .AddWebResourceRequestedFilter(w!("*"), COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL)
            .ok()?;
        let env3 = environment.clone();
        let handler = WebResourceRequestedEventHandler::create(Box::new(
            move |_wv, args: Option<ICoreWebView2WebResourceRequestedEventArgs>| {
                if let Some(args) = args {
                    if let Ok(request) = args.Request() {
                        let mut uri_p = PWSTR::null();
                        let uri = if request.Uri(&mut uri_p).is_ok() && !uri_p.is_null() {
                            let s = uri_p.to_string().unwrap_or_default();
                            CoTaskMemFree(Some(uri_p.as_ptr() as *const _));
                            s
                        } else {
                            String::new()
                        };
                        if !uri.starts_with("file:") {
                            if let Ok(resp) =
                                env3.CreateWebResourceResponse(None, 403, w!("Blocked"), w!(""))
                            {
                                let _ = args.SetResponse(&resp);
                            }
                        }
                    }
                }
                Ok(())
            },
        ));
        let mut token: i64 = 0;
        let _ = webview.add_WebResourceRequested(&handler, &mut token);

        // The resource filter above blocks a clicked link's own navigation too (Document is one
        // of its ALL contexts), which just swaps in a blank 403, i.e. "does nothing" from the
        // user's seat. Intercept the navigation itself instead: file:// (the loaded page, or an
        // in-page anchor) passes unchanged; http(s) is canceled and handed to the OS default
        // browser via ShellExecuteW, same allow-and-launch shape as the Markdown link path in
        // `window.rs::open_preview_link`; anything else is canceled and dropped outright, since
        // an untrusted local HTML file must not launch an arbitrary protocol handler from a click.
        let nav_handler = NavigationStartingEventHandler::create(Box::new(
            move |_wv, args: Option<ICoreWebView2NavigationStartingEventArgs>| {
                if let Some(args) = args {
                    let mut uri_p = PWSTR::null();
                    let uri = if args.Uri(&mut uri_p).is_ok() && !uri_p.is_null() {
                        let s = uri_p.to_string().unwrap_or_default();
                        CoTaskMemFree(Some(uri_p.as_ptr() as *const _));
                        s
                    } else {
                        String::new()
                    };
                    if uri.starts_with("file:") {
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
                }
                Ok(())
            },
        ));
        let mut nav_token: i64 = 0;
        let _ = webview.add_NavigationStarting(&nav_handler, &mut nav_token);
    }

    let url_h = HSTRING::from(url);
    webview.Navigate(PCWSTR(url_h.as_ptr())).ok()?;
    Some(WebViewHost {
        controller,
        profile_dir,
    })
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
