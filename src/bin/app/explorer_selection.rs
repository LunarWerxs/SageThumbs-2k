//! Resolve the file(s) a global-hotkey action should operate on.
//!
//! A hotkey has no shell selection of its own, so we read the CURRENT selection of the
//! foreground Explorer window via the shell automation interfaces
//! (`IShellWindows` → `IWebBrowser2` → `IShellFolderViewDual` → `FolderItems`). If that
//! yields nothing (no Explorer focused, or an empty selection), we fall back to a
//! multi-select file picker so the action still works (the owner's chosen behaviour).

use core::ffi::c_void;

use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::{
    FileOpenDialog, IFileOpenDialog, IShellFolderViewDual, IShellItem, IShellItemArray,
    IShellWindows, IWebBrowser2, ShellWindows, FOS_ALLOWMULTISELECT, FOS_FILEMUSTEXIST,
    FOS_FORCEFILESYSTEM, SIGDN_FILESYSPATH,
};
use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

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

/// The file paths currently selected in the FOREGROUND Explorer window, or an empty Vec if the
/// foreground window isn't an Explorer view (or has no selection). Best-effort: any COM failure
/// degrades to empty, which the caller turns into a picker prompt.
unsafe fn foreground_explorer_selection() -> Vec<String> {
    let fg = GetForegroundWindow();
    if fg.0.is_null() {
        return Vec::new();
    }
    let shell_windows: IShellWindows = match CoCreateInstance(&ShellWindows, None, CLSCTX_ALL) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let count = shell_windows.Count().unwrap_or(0);
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
        let Ok(items) = view.SelectedItems() else { continue };
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
        return out;
    }
    Vec::new()
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
