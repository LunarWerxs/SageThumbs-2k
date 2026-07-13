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
    ICoreWebView2WebResourceRequestedEventArgs, COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL,
};
use webview2_com::{
    CreateCoreWebView2ControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler,
    WebResourceRequestedEventHandler,
};
use windows::core::{w, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::System::Com::CoTaskMemFree;

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
pub(super) unsafe fn create(parent: HWND, rect: &RECT, url: &str, mode: Mode) -> Option<WebViewHost> {
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
    }

    let url_h = HSTRING::from(url);
    webview.Navigate(PCWSTR(url_h.as_ptr())).ok()?;
    Some(WebViewHost { controller, profile_dir })
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
