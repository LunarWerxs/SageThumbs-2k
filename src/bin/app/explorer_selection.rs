//! Resolve the file(s) a global-hotkey action should operate on.
//!
//! A hotkey has no shell selection of its own, so we read the CURRENT selection of the
//! foreground Explorer window via the shell automation interfaces
//! (`IShellWindows` → `IWebBrowser2` → `IShellFolderViewDual` → `FolderItems`). If that
//! yields nothing (no Explorer focused, or an empty selection), we fall back to a
//! multi-select file picker so the action still works (the owner's chosen behaviour).
//!
//! One foreground window is NOT a shell view and is still answered: **Everything**
//! (voidtools). It is not reachable through `IShellWindows` at all — it publishes the focused
//! result itself, through a hidden child window, which is what [`everything_selection`] reads.
//! Everything **1.4** publishes no such window, so it is answered a second way: by reading the
//! focused row straight out of its result list (see [`everything_listview_path`]).

use core::ffi::c_void;
mod everything;
use everything::*;
mod picker;
#[cfg(test)]
pub(crate) use everything::is_everything_exe_name;
pub(crate) use everything::{everything_focus_window, everything_result_list, is_everything_class};
use picker::*;

use windows::core::{w, Interface, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HINSTANCE, HWND, LPARAM, WPARAM};
use windows::Win32::Storage::FileSystem::WIN32_FIND_DATAW;
use windows::Win32::System::Com::{
    CoCreateInstance, CoTaskMemFree, IDispatch, IPersistFile, IServiceProvider, CLSCTX_ALL,
    CLSCTX_INPROC_SERVER, STGM_READ,
};
use windows::Win32::System::Diagnostics::Debug::{ReadProcessMemory, WriteProcessMemory};
use windows::Win32::System::Memory::{
    VirtualAllocEx, VirtualFreeEx, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_VM_OPERATION, PROCESS_VM_READ, PROCESS_VM_WRITE,
};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Controls::{
    HDM_GETITEMCOUNT, LVIF_TEXT, LVITEMW, LVM_GETHEADER, LVM_GETITEMTEXTW, LVM_GETNEXTITEM,
    LVNI_FOCUSED, LVNI_SELECTED,
};
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::{
    FileOpenDialog, IFileOpenDialog, IShellBrowser, IShellFolderViewDual, IShellItem,
    IShellItemArray, IShellLinkW, IShellWindows, IWebBrowser2, SID_STopLevelBrowser, ShellLink,
    ShellWindows, FOS_ALLOWMULTISELECT, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, SIGDN_FILESYSPATH,
    SVGIO_BACKGROUND, SWC_DESKTOP, SWFO_NEEDDISPATCH,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, FindWindowExW, GetClassNameW, GetForegroundWindow,
    GetGUIThreadInfo, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId,
    IsWindowVisible, SendMessageTimeoutW, GUITHREADINFO, SMTO_ABORTIFHUNG, WINDOW_EX_STYLE,
    WS_POPUP,
};

use crate::win::wide;

/// Outcome of resolving the file(s) a hotkey/verb action should act on. Collapsing "nothing was
/// selected" and "what was selected has no filesystem path behind it" into the same empty `Vec`
/// is the exact defect this exists to close: a Recycle Bin / This PC / other virtual-namespace
/// selection used to be byte-for-byte indistinguishable from no selection at all, so the action
/// silently did nothing and left the user unable to tell the feature from a bug.
pub(crate) enum SelectionOutcome {
    /// No item was selected (and, for [`selection_or_pick`], the file picker was cancelled too).
    Empty,
    /// At least one item was selected, but none of them resolve to a real filesystem path —
    /// every item is virtual.
    VirtualOnly,
    /// One or more real filesystem paths.
    Paths(Vec<String>),
}

/// Target files for a hotkey verb: the foreground Explorer selection, or — when that's empty
/// — a multi-select file picker. `images_only` filters the picker to image extensions (for the
/// verbs that only make sense on images). See [`SelectionOutcome`] for what each outcome means;
/// `Empty` covers both "nothing selected" and "the picker was cancelled".
pub(crate) unsafe fn selection_or_pick(images_only: bool) -> SelectionOutcome {
    // Apartment-threaded COM for the shell automation interfaces below. `sta()` is
    // `None` (rather than a guard that no-ops on drop) when `CoInitializeEx` itself
    // failed, so `Drop` only ever balances a real init (issue #158/C17).
    let _com = st2k_base::parallel::ComGuard::sta();
    // Everything is a real answer, not a "no selection" — without this the picker would open
    // over a window that is already pointing at the exact file the user meant.
    if let Some(p) = everything_selection() {
        return SelectionOutcome::Paths(vec![p]);
    }
    match settled_explorer_selection() {
        SelectionOutcome::Paths(paths) => return SelectionOutcome::Paths(paths),
        // Virtual is a real answer too — falling through to the picker would silently swap
        // "you selected the Recycle Bin" for "you selected nothing", which is its own way of
        // hiding the outcome from the caller. `st2k doctor`-style diagnosis needs a trail.
        SelectionOutcome::VirtualOnly => {
            st2k_base::safety::log_debug(
                "selection_or_pick: foreground selection is virtual-only \
                 (no filesystem path behind any selected item)",
            );
            return SelectionOutcome::VirtualOnly;
        }
        SelectionOutcome::Empty => {}
    }
    match pick_files(images_only) {
        Some(paths) if !paths.is_empty() => SelectionOutcome::Paths(paths),
        _ => SelectionOutcome::Empty,
    }
}

/// How long to wait before the ONE retry in [`settled_explorer_selection`]. Short enough that a
/// genuinely empty selection still feels instant, long enough to cover the foreground handover.
const SETTLE_MS: u64 = 40;

/// The foreground Explorer selection, retried ONCE after a short pause if the first read comes back
/// empty.
///
/// A hotkey fires the instant the key goes down, which can be mid-handover: right after an Alt-Tab
/// (or after the Explorer search box gives focus back) `GetForegroundWindow` and the shell's own
/// `IShellWindows` view briefly disagree, and the read returns nothing. The caller reads that as
/// "no selection" and pops a file picker, which looks like the hotkey did the wrong thing.
///
/// Only the EMPTY result is retried, so the overwhelmingly common case (a real selection, found
/// first try) costs nothing at all.
unsafe fn settled_explorer_selection() -> SelectionOutcome {
    let sel = foreground_explorer_selection();
    // Only an EMPTY read is worth retrying — the handover race can only produce "nothing yet",
    // never turn a real virtual selection into a filesystem one, so retrying `VirtualOnly` would
    // just burn `SETTLE_MS` proving the same answer twice.
    if !matches!(sel, SelectionOutcome::Empty) {
        return sel;
    }
    std::thread::sleep(std::time::Duration::from_millis(SETTLE_MS));
    foreground_explorer_selection()
}

/// What the Quick preview hotkey should do with the resolved selection — the seam
/// [`preview_target`] hands to `preview::mod::run_preview` so a virtual-namespace selection
/// (Recycle Bin, This PC, …) can be told apart from no selection at all. Collapsing those two
/// into "nothing to preview" is the exact defect [`SelectionOutcome::VirtualOnly`] exists to
/// close one layer down; this enum carries that distinction the rest of the way to the window.
pub(crate) enum PreviewTarget {
    /// A real filesystem path (a `.lnk` selection already resolved to its target).
    Path(String),
    /// Something was selected, but it has no file behind it — the viewer should open and say
    /// so, not stay silent.
    Virtual,
    /// Nothing was selected at all. The only case that must still open nothing.
    Empty,
}

/// The single file the Quick preview hotkey should show: the FIRST item selected in the
/// foreground Explorer window — or the result focused in a foreground **Everything** window, or,
/// when the foreground is the DESKTOP, the first item selected there — or [`PreviewTarget::Empty`]
/// when nothing is selected. A selected `.lnk` shortcut resolves to its target so Space previews
/// the pointed-at file, not the shortcut stub. Inits COM STA itself (called from the viewer
/// process's own thread).
///
/// Everything is asked FIRST because it is cheap and unambiguous: the shell automation below can
/// only ever say "I have never heard of that window", and it would spend [`SETTLE_MS`] proving it.
pub(crate) unsafe fn preview_target() -> PreviewTarget {
    let _com = st2k_base::parallel::ComGuard::sta();
    if let Some(raw) = everything_selection().or_else(|| foreground_dialog_selection()) {
        return PreviewTarget::Path(resolve_lnk(&raw));
    }
    first_target(settled_explorer_selection(), "preview_target/explorer")
        .or_else(|| first_target(foreground_desktop_selection(), "preview_target/desktop"))
        .unwrap_or(PreviewTarget::Empty)
}

/// One selection walk's outcome, turned into a [`PreviewTarget`] — logging (this file has no
/// UI of its own to show a message through) when the selection turned out to be virtual-only, so
/// a silently-declined preview still leaves a trail an `st2k doctor`-style diagnosis can see.
/// `None` means "this source has no answer, try the next one" (an empty selection); `context`
/// names the call site in the log line, since [`preview_target`] tries two sources.
fn first_target(outcome: SelectionOutcome, context: &str) -> Option<PreviewTarget> {
    match outcome {
        SelectionOutcome::Paths(mut paths) if !paths.is_empty() => {
            Some(PreviewTarget::Path(paths.remove(0)))
        }
        SelectionOutcome::VirtualOnly => {
            st2k_base::safety::log_debugf!(
                "{context}: selection is virtual-only (no filesystem path behind any selected item)"
            );
            Some(PreviewTarget::Virtual)
        }
        SelectionOutcome::Paths(_) | SelectionOutcome::Empty => None,
    }
}

/// Resolve an explicit `--preview <path>` argument: follows a `.lnk` to its target (so a manual
/// preview of a shortcut shows the pointed-at file), leaving anything else unchanged. Inits its
/// own COM STA (the explicit path doesn't otherwise touch the shell).
pub(crate) unsafe fn resolve_explicit(path: &str) -> String {
    let _com = st2k_base::parallel::ComGuard::sta();
    resolve_lnk(path)
}

/// The file paths currently selected in the FOREGROUND Explorer window, as a [`SelectionOutcome`]
/// — `Empty` if the foreground window isn't an Explorer view (or has no selection), `VirtualOnly`
/// if every selected item is virtual. Best-effort: any COM failure degrades to `Empty`.
///
/// Win11 tabbed Explorer: every TAB of a window is its own `IShellWindows` item, but they all
/// report the same top-level frame HWND — so the frame match alone can land on a background
/// tab. Disambiguate by ALSO matching each item's browser window against the frame's ACTIVE
/// (visible) `ShellTabWindowClass` child; when that can't be resolved (older builds, single
/// tab, QueryService quirks), fall back to the first frame-matched item (the old behaviour).
unsafe fn foreground_explorer_selection() -> SelectionOutcome {
    let fg = GetForegroundWindow();
    if fg.0.is_null() {
        return SelectionOutcome::Empty;
    }
    let active_tab = active_shell_tab(fg);
    let shell_windows: IShellWindows = match CoCreateInstance(&ShellWindows, None, CLSCTX_ALL) {
        Ok(s) => s,
        Err(_) => return SelectionOutcome::Empty,
    };
    let count = shell_windows.Count().unwrap_or(0);
    let mut fallback: Option<SelectionOutcome> = None;
    for i in 0..count {
        if let Some(sel) =
            shell_window_selection_step(&shell_windows, i, fg, active_tab, &mut fallback)
        {
            return sel;
        }
    }
    // No item matched the active tab (e.g. GetWindow semantics differ on this build) — use the
    // first frame-matched item rather than returning nothing.
    fallback.unwrap_or(SelectionOutcome::Empty)
}

/// Walk ONE `IShellWindows` item in [`foreground_explorer_selection`]: `Some` when it matches the
/// active tab and its selection should be returned at once, `None` otherwise (in which case the
/// first frame-matched item's selection is stored in `fallback` if it isn't already).
unsafe fn shell_window_selection_step(
    shell_windows: &IShellWindows,
    i: i32,
    fg: HWND,
    active_tab: Option<HWND>,
    fallback: &mut Option<SelectionOutcome>,
) -> Option<SelectionOutcome> {
    let Ok(disp) = shell_windows.Item(&VARIANT::from(i)) else {
        return None;
    };
    let Ok(wb) = disp.cast::<IWebBrowser2>() else {
        return None;
    };
    // Only the window the user is actually looking at.
    let Ok(handle) = wb.HWND() else { return None };
    if HWND(handle.0 as *mut c_void) != fg {
        return None;
    }
    let Ok(doc) = wb.Document() else { return None };
    let Ok(view) = doc.cast::<IShellFolderViewDual>() else {
        return None;
    };
    let tab_match = match (active_tab, browser_window(&wb)) {
        (Some(tab), Some(bw)) => bw == tab,
        _ => true, // can't disambiguate — accept the frame match as before
    };
    if tab_match {
        return Some(paths_from_view(&view));
    }
    if fallback.is_none() {
        *fallback = Some(paths_from_view(&view));
    }
    None
}

/// The ACTIVE tab of a (possibly tabbed) Explorer frame: its visible `ShellTabWindowClass`
/// child. Background tabs' windows exist but are hidden. `None` on pre-tab builds / not found.
unsafe fn active_shell_tab(frame: HWND) -> Option<HWND> {
    let mut child: Option<HWND> = None;
    loop {
        let next = FindWindowExW(
            Some(frame),
            child,
            w!("ShellTabWindowClass"),
            PCWSTR::null(),
        )
        .ok()?;
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
    let browser = sp
        .QueryService::<IShellBrowser>(&SID_STopLevelBrowser)
        .ok()?;
    browser.GetWindow().ok()
}

/// The file paths currently selected on the DESKTOP, or empty if the foreground isn't the
/// desktop (or nothing is selected). The desktop's shell view isn't in `IShellWindows`, so it's
/// reached via `FindWindowSW(SWC_DESKTOP)` → top-level `IShellBrowser` → the active `IShellView`
/// → its `IShellFolderViewDual` (the same selection interface the Explorer path uses).
unsafe fn foreground_desktop_selection() -> SelectionOutcome {
    if !is_desktop_foreground() {
        return SelectionOutcome::Empty;
    }
    let shell_windows: IShellWindows = match CoCreateInstance(&ShellWindows, None, CLSCTX_ALL) {
        Ok(s) => s,
        Err(_) => return SelectionOutcome::Empty,
    };
    let loc = VARIANT::default(); // VT_EMPTY — ignored for SWC_DESKTOP
    let mut phwnd: i32 = 0;
    let Ok(disp) =
        shell_windows.FindWindowSW(&loc, &loc, SWC_DESKTOP, &mut phwnd, SWFO_NEEDDISPATCH)
    else {
        return SelectionOutcome::Empty;
    };
    let Ok(sp) = disp.cast::<IServiceProvider>() else {
        return SelectionOutcome::Empty;
    };
    let Ok(browser) = sp.QueryService::<IShellBrowser>(&SID_STopLevelBrowser) else {
        return SelectionOutcome::Empty;
    };
    let Ok(view) = browser.QueryActiveShellView() else {
        return SelectionOutcome::Empty;
    };
    // GetItemObject(SVGIO_BACKGROUND, IID_IDispatch) yields an IDispatch we QI to the folder's
    // IShellFolderViewDual (requesting the dual's IID directly from GetItemObject returns
    // E_NOINTERFACE — the background item is only handed out as an IDispatch).
    let Ok(bg) = view.GetItemObject::<IDispatch>(SVGIO_BACKGROUND) else {
        return SelectionOutcome::Empty;
    };
    let Ok(sfvd) = bg.cast::<IShellFolderViewDual>() else {
        return SelectionOutcome::Empty;
    };
    paths_from_view(&sfvd)
}

/// The item selected in a foreground Open/Save dialog, or `None` when the foreground isn't
/// one. The mechanics (and why they need a hook DLL at all) live in [`crate::dialog_hook`].
unsafe fn foreground_dialog_selection() -> Option<String> {
    let fg = GetForegroundWindow();
    if fg.0.is_null() {
        return None;
    }
    crate::dialog_hook::dialog_selection(fg)
}

/// Whether the thread owning `hwnd` has a live text caret — i.e. the user is typing in it.
unsafe fn caret_active(hwnd: HWND) -> bool {
    let tid = GetWindowThreadProcessId(hwnd, None);
    let mut gti = GUITHREADINFO {
        cbSize: core::mem::size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    GetGUIThreadInfo(tid, &mut gti).is_ok() && !gti.hwndCaret.0.is_null()
}

/// Extract the filesystem paths of the SELECTED items from a shell folder view, distinguishing
/// an empty selection from one that is entirely virtual (Recycle Bin, This PC, …) — see
/// [`SelectionOutcome`]. `item.Path()` does NOT error for a virtual item: it hands back
/// something that looks like an answer (an empty string, a bare display name, a `::{GUID}`
/// shell-namespace string), so every item gets a slot in `raw` — an empty string on a failed or
/// empty `Path()` call — and classification runs on the CONTENT, never on whether the COM call
/// itself succeeded.
unsafe fn paths_from_view(view: &IShellFolderViewDual) -> SelectionOutcome {
    let Ok(items) = view.SelectedItems() else {
        return SelectionOutcome::Empty;
    };
    let n = items.Count().unwrap_or(0);
    let mut raw = Vec::with_capacity(n.max(0) as usize);
    for j in 0..n {
        let s = items
            .Item(&VARIANT::from(j))
            .ok()
            .and_then(|item| item.Path().ok())
            .map(|bstr| bstr.to_string())
            .unwrap_or_default();
        raw.push(s);
    }
    classify_selection(&raw, |p| std::path::Path::new(p).exists())
}

/// Classifies the raw `Path()` strings gathered for one selection (one entry per selected item;
/// see [`paths_from_view`] for why a failed/empty `Path()` still gets an empty-string entry
/// rather than being dropped) into [`Empty`](SelectionOutcome::Empty),
/// [`VirtualOnly`](SelectionOutcome::VirtualOnly), or real
/// [`Paths`](SelectionOutcome::Paths). A pure decision over a list of strings, kept separate
/// from the COM walk so it's unit-testable without a live shell selection.
///
/// A candidate counts as a real path only if it's ROOTED ([`is_rooted_path`]) AND `exists`
/// confirms something is actually there — syntax alone isn't enough, since a stale/moved path
/// can be rooted-looking and still have nothing behind it, which is exactly the "not actually on
/// disk" case this must also treat as virtual.
fn classify_selection(raw: &[String], exists: impl Fn(&str) -> bool) -> SelectionOutcome {
    if raw.is_empty() {
        return SelectionOutcome::Empty;
    }
    let paths: Vec<String> = raw
        .iter()
        .filter(|p| is_rooted_path(p) && exists(p))
        .cloned()
        .collect();
    if paths.is_empty() {
        SelectionOutcome::VirtualOnly
    } else {
        SelectionOutcome::Paths(paths)
    }
}

/// Whether the foreground window is the desktop (its class is `Progman` or a `WorkerW`). Gates
/// the desktop-selection probe so an empty Explorer selection never silently grabs the desktop's.
unsafe fn is_desktop_foreground() -> bool {
    let fg = GetForegroundWindow();
    if fg.0.is_null() {
        return false;
    }
    let cls = class_name(fg);
    cls == "Progman" || cls == "WorkerW"
}

/// A window's class name (best-effort; empty string on failure). Shared with the Space hook,
/// which classifies the same foreground window one layer up.
pub(crate) unsafe fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 128];
    let n = GetClassNameW(hwnd, &mut buf);
    if n <= 0 {
        String::new()
    } else {
        String::from_utf16_lossy(&buf[..n as usize])
    }
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

#[cfg(test)]
mod tests;
