//! The fallback when nothing is selected: a file picker with an owner window of its own.

use super::*;

/// Create an invisible, unowned top-level window to own the file-picker dialog, and give
/// it the foreground + keyboard focus before the dialog shows (issue #42/P42).
///
/// A dialog shown with no owner (`dlg.Show(None)`), from a process spawned by the
/// background hotkey daemon — which never itself received the triggering keypress —
/// can be refused the foreground grant Windows requires: it still appears (topmost),
/// but reads as "the hotkey did nothing" because it opens BEHIND the user's current
/// window with only a taskbar flash. [`crate::win::force_foreground`] is the same fix
/// already used for the eyedropper/capture overlay, spawned from this same context.
///
/// Uses the built-in `STATIC` class: this window is never shown, so it needs no
/// window procedure of its own — it exists only to be an owner HWND and a foreground
/// grant target, and is destroyed by the caller once the dialog closes.
pub(super) unsafe fn create_picker_owner() -> Option<HWND> {
    let hinst: HINSTANCE = windows::Win32::System::LibraryLoader::GetModuleHandleW(None)
        .ok()?
        .into();
    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("STATIC"),
        PCWSTR::null(),
        WS_POPUP,
        0,
        0,
        0,
        0,
        None,
        None,
        Some(hinst),
        None,
    )
    .ok()?;
    crate::win::force_foreground(hwnd);
    Some(hwnd)
}

/// A multi-select "open files" dialog. `images_only` restricts the filter to image types.
/// Returns the chosen paths, or `None` if the user cancelled. COM is already initialised by
/// the caller ([`selection_or_pick`]).
pub(super) unsafe fn pick_files(images_only: bool) -> Option<Vec<String>> {
    let dlg: IFileOpenDialog =
        CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
    if let Ok(opts) = dlg.GetOptions() {
        let _ =
            dlg.SetOptions(opts | FOS_ALLOWMULTISELECT | FOS_FILEMUSTEXIST | FOS_FORCEFILESYSTEM);
    }
    let name = wide("Images");
    let spec =
        wide("*.png;*.jpg;*.jpeg;*.gif;*.bmp;*.tif;*.tiff;*.webp;*.avif;*.heic;*.heif;*.ico;*.tga");
    if images_only {
        let specs = [COMDLG_FILTERSPEC {
            pszName: PCWSTR(name.as_ptr()),
            pszSpec: PCWSTR(spec.as_ptr()),
        }];
        let _ = dlg.SetFileTypes(&specs);
    }
    let owner = create_picker_owner();
    let shown = dlg.Show(owner);
    if let Some(o) = owner {
        let _ = DestroyWindow(o);
    }
    shown.ok()?;
    let results: IShellItemArray = dlg.GetResults().ok()?;
    let n = results.GetCount().ok()?;
    let mut out = Vec::with_capacity(n as usize);
    for i in 0..n {
        let Ok(item): windows::core::Result<IShellItem> = results.GetItemAt(i) else {
            continue;
        };
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
