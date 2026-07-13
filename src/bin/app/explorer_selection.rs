//! Resolve the file(s) a global-hotkey action should operate on.
//!
//! A hotkey has no shell selection of its own, so we read the CURRENT selection of the
//! foreground Explorer window via the shell automation interfaces
//! (`IShellWindows` → `IWebBrowser2` → `IShellFolderViewDual` → `FolderItems`). If that
//! yields nothing (no Explorer focused, or an empty selection), we fall back to a
//! multi-select file picker so the action still works (the owner's chosen behaviour).

use core::ffi::c_void;

use windows::core::{w, Interface, PCWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::Storage::FileSystem::WIN32_FIND_DATAW;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, IDispatch, IPersistFile,
    IServiceProvider, CLSCTX_ALL, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, STGM_READ,
};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::{
    FileOpenDialog, IFileOpenDialog, IShellBrowser, IShellFolderViewDual, IShellItem,
    IShellItemArray, IShellLinkW, IShellWindows, IWebBrowser2, ShellLink, ShellWindows,
    FOS_ALLOWMULTISELECT, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, SIGDN_FILESYSPATH,
    SID_STopLevelBrowser, SVGIO_BACKGROUND, SWC_DESKTOP, SWFO_NEEDDISPATCH,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, GetClassNameW, GetForegroundWindow, IsWindowVisible,
};

use crate::win::wide;

/// Initialise COM (STA) for the lifetime of a scope, undoing it on drop. Mirrors the
/// pattern in `win.rs`'s file-dialog helpers.
struct ComGuard(bool);
impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

/// Target files for a hotkey verb: the foreground Explorer selection, or — when that's empty
/// — a multi-select file picker. `images_only` filters the picker to image extensions (for the
/// verbs that only make sense on images). Returns an empty Vec if the user cancels.
pub(crate) unsafe fn selection_or_pick(images_only: bool) -> Vec<String> {
    let _com = ComGuard(CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok());
    let sel = foreground_explorer_selection();
    if !sel.is_empty() {
        return sel;
    }
    pick_files(images_only).unwrap_or_default()
}

/// The single file the Quick preview hotkey should show: the FIRST item selected in the
/// foreground Explorer window — or, when the foreground is the DESKTOP, the first item selected
/// there — or `None` when nothing is selected. A selected `.lnk` shortcut resolves to its target
/// so Space previews the pointed-at file, not the shortcut stub. Inits COM STA itself (called
/// from the viewer process's own thread).
pub(crate) unsafe fn preview_target() -> Option<String> {
    let _com = ComGuard(CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok());
    let raw = foreground_explorer_selection()
        .into_iter()
        .next()
        .or_else(|| foreground_desktop_selection().into_iter().next())?;
    Some(resolve_lnk(&raw))
}

/// Resolve an explicit `--preview <path>` argument: follows a `.lnk` to its target (so a manual
/// preview of a shortcut shows the pointed-at file), leaving anything else unchanged. Inits its
/// own COM STA (the explicit path doesn't otherwise touch the shell).
pub(crate) unsafe fn resolve_explicit(path: &str) -> String {
    let _com = ComGuard(CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok());
    resolve_lnk(path)
}

/// The file paths currently selected in the FOREGROUND Explorer window, or an empty Vec if the
/// foreground window isn't an Explorer view (or has no selection). Best-effort: any COM failure
/// degrades to empty, which the caller turns into a picker prompt.
///
/// Win11 tabbed Explorer: every TAB of a window is its own `IShellWindows` item, but they all
/// report the same top-level frame HWND — so the frame match alone can land on a background
/// tab. Disambiguate by ALSO matching each item's browser window against the frame's ACTIVE
/// (visible) `ShellTabWindowClass` child; when that can't be resolved (older builds, single
/// tab, QueryService quirks), fall back to the first frame-matched item (the old behaviour).
unsafe fn foreground_explorer_selection() -> Vec<String> {
    let fg = GetForegroundWindow();
    if fg.0.is_null() {
        return Vec::new();
    }
    let active_tab = active_shell_tab(fg);
    let shell_windows: IShellWindows = match CoCreateInstance(&ShellWindows, None, CLSCTX_ALL) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let count = shell_windows.Count().unwrap_or(0);
    let mut fallback: Option<Vec<String>> = None;
    for i in 0..count {
        let Ok(disp) = shell_windows.Item(&VARIANT::from(i)) else { continue };
        let Ok(wb) = disp.cast::<IWebBrowser2>() else { continue };
        // Only the window the user is actually looking at.
        let Ok(handle) = wb.HWND() else { continue };
        if HWND(handle.0 as *mut c_void) != fg {
            continue;
        }
        let Ok(doc) = wb.Document() else { continue };
        let Ok(view) = doc.cast::<IShellFolderViewDual>() else { continue };
        let tab_match = match (active_tab, browser_window(&wb)) {
            (Some(tab), Some(bw)) => bw == tab,
            _ => true, // can't disambiguate — accept the frame match as before
        };
        if tab_match {
            return paths_from_view(&view);
        }
        if fallback.is_none() {
            fallback = Some(paths_from_view(&view));
        }
    }
    // No item matched the active tab (e.g. GetWindow semantics differ on this build) — use the
    // first frame-matched item rather than returning nothing.
    fallback.unwrap_or_default()
}

/// The ACTIVE tab of a (possibly tabbed) Explorer frame: its visible `ShellTabWindowClass`
/// child. Background tabs' windows exist but are hidden. `None` on pre-tab builds / not found.
unsafe fn active_shell_tab(frame: HWND) -> Option<HWND> {
    let mut child: Option<HWND> = None;
    loop {
        let next =
            FindWindowExW(Some(frame), child, w!("ShellTabWindowClass"), PCWSTR::null()).ok()?;
        if next.0.is_null() {
            return None;
        }
        if IsWindowVisible(next).as_bool() {
            return Some(next);
        }
        child = Some(next);
    }
}

/// The browser window of one shell-windows item — for a Win11 Explorer TAB this is its
/// `ShellTabWindowClass` window (each tab has its own top-level browser object).
unsafe fn browser_window(wb: &IWebBrowser2) -> Option<HWND> {
    let sp = wb.cast::<IServiceProvider>().ok()?;
    let browser = sp.QueryService::<IShellBrowser>(&SID_STopLevelBrowser).ok()?;
    browser.GetWindow().ok()
}

/// The file paths currently selected on the DESKTOP, or empty if the foreground isn't the
/// desktop (or nothing is selected). The desktop's shell view isn't in `IShellWindows`, so it's
/// reached via `FindWindowSW(SWC_DESKTOP)` → top-level `IShellBrowser` → the active `IShellView`
/// → its `IShellFolderViewDual` (the same selection interface the Explorer path uses).
unsafe fn foreground_desktop_selection() -> Vec<String> {
    if !is_desktop_foreground() {
        return Vec::new();
    }
    let shell_windows: IShellWindows = match CoCreateInstance(&ShellWindows, None, CLSCTX_ALL) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let loc = VARIANT::default(); // VT_EMPTY — ignored for SWC_DESKTOP
    let mut phwnd: i32 = 0;
    let Ok(disp) =
        shell_windows.FindWindowSW(&loc, &loc, SWC_DESKTOP, &mut phwnd, SWFO_NEEDDISPATCH)
    else {
        return Vec::new();
    };
    let Ok(sp) = disp.cast::<IServiceProvider>() else { return Vec::new() };
    let Ok(browser) = sp.QueryService::<IShellBrowser>(&SID_STopLevelBrowser) else {
        return Vec::new();
    };
    let Ok(view) = browser.QueryActiveShellView() else { return Vec::new() };
    // GetItemObject(SVGIO_BACKGROUND, IID_IDispatch) yields an IDispatch we QI to the folder's
    // IShellFolderViewDual (requesting the dual's IID directly from GetItemObject returns
    // E_NOINTERFACE — the background item is only handed out as an IDispatch).
    let Ok(bg) = view.GetItemObject::<IDispatch>(SVGIO_BACKGROUND) else { return Vec::new() };
    let Ok(sfvd) = bg.cast::<IShellFolderViewDual>() else { return Vec::new() };
    paths_from_view(&sfvd)
}

/// Extract the filesystem paths of the SELECTED items from a shell folder view. Virtual items
/// (Recycle Bin, This PC, …) have no `Path()` and are skipped.
unsafe fn paths_from_view(view: &IShellFolderViewDual) -> Vec<String> {
    let Ok(items) = view.SelectedItems() else { return Vec::new() };
    let n = items.Count().unwrap_or(0);
    let mut out = Vec::with_capacity(n.max(0) as usize);
    for j in 0..n {
        if let Ok(item) = items.Item(&VARIANT::from(j)) {
            if let Ok(bstr) = item.Path() {
                let s = bstr.to_string();
                if !s.is_empty() {
                    out.push(s);
                }
            }
        }
    }
    out
}

/// Whether the foreground window is the desktop (its class is `Progman` or a `WorkerW`). Gates
/// the desktop-selection probe so an empty Explorer selection never silently grabs the desktop's.
unsafe fn is_desktop_foreground() -> bool {
    let fg = GetForegroundWindow();
    if fg.0.is_null() {
        return false;
    }
    let mut buf = [0u16; 64];
    let n = GetClassNameW(fg, &mut buf);
    if n <= 0 {
        return false;
    }
    let cls = String::from_utf16_lossy(&buf[..n as usize]);
    cls == "Progman" || cls == "WorkerW"
}

/// Resolve a `.lnk` shortcut to its filesystem target (so Space previews the pointed-at file, not
/// the stub). Non-shortcuts and any resolution failure return the input unchanged. COM STA is
/// already initialised by the caller.
unsafe fn resolve_lnk(path: &str) -> String {
    if !path.to_ascii_lowercase().ends_with(".lnk") {
        return path.to_string();
    }
    let target = (|| -> windows::core::Result<String> {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        let pf: IPersistFile = link.cast()?;
        let w = wide(path);
        pf.Load(PCWSTR(w.as_ptr()), STGM_READ)?;
        let mut buf = [0u16; 260];
        let mut fd = WIN32_FIND_DATAW::default();
        link.GetPath(&mut buf, &mut fd, 0)?;
        let t = String::from_utf16_lossy(&buf);
        Ok(t.trim_end_matches('\0').to_string())
    })();
    match target {
        Ok(t) if !t.is_empty() => t,
        _ => path.to_string(), // unresolvable → preview the .lnk itself (info card)
    }
}

/// A multi-select "open files" dialog. `images_only` restricts the filter to image types.
/// Returns the chosen paths, or `None` if the user cancelled. COM is already initialised by
/// the caller ([`selection_or_pick`]).
unsafe fn pick_files(images_only: bool) -> Option<Vec<String>> {
    let dlg: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
    if let Ok(opts) = dlg.GetOptions() {
        let _ = dlg.SetOptions(opts | FOS_ALLOWMULTISELECT | FOS_FILEMUSTEXIST | FOS_FORCEFILESYSTEM);
    }
    let name = wide("Images");
    let spec = wide("*.png;*.jpg;*.jpeg;*.gif;*.bmp;*.tif;*.tiff;*.webp;*.avif;*.heic;*.heif;*.ico;*.tga");
    if images_only {
        let specs = [COMDLG_FILTERSPEC {
            pszName: PCWSTR(name.as_ptr()),
            pszSpec: PCWSTR(spec.as_ptr()),
        }];
        let _ = dlg.SetFileTypes(&specs);
    }
    dlg.Show(None).ok()?;
    let results: IShellItemArray = dlg.GetResults().ok()?;
    let n = results.GetCount().ok()?;
    let mut out = Vec::with_capacity(n as usize);
    for i in 0..n {
        let Ok(item): windows::core::Result<IShellItem> = results.GetItemAt(i) else { continue };
        if let Ok(pw) = item.GetDisplayName(SIGDN_FILESYSPATH) {
            let s = pw.to_string().unwrap_or_default();
            CoTaskMemFree(Some(pw.0 as *const c_void));
            if !s.is_empty() {
                out.push(s);
            }
        }
    }
    Some(out)
}
