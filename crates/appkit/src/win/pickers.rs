//! File-dialog pickers + clipboard (extracted from win.rs; behavior unchanged).

use core::ffi::c_void;

use windows::core::PCWSTR;
use windows::Win32::Foundation::HWND;

use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER};
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::{
    FOLDERID_Desktop, FileOpenDialog, FileSaveDialog, IFileDialog, IFileOpenDialog,
    IFileSaveDialog, IShellItem, SHCreateItemFromParsingName, SHGetKnownFolderPath,
    FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, FOS_STRICTFILETYPES, KF_FLAG_DEFAULT, SIGDN_FILESYSPATH,
};

use st2k_base::fsutil::parsing_path;
use st2k_base::parallel::ComGuard;

use super::wide;

pub unsafe fn desktop_dir() -> String {
    match SHGetKnownFolderPath(&FOLDERID_Desktop, KF_FLAG_DEFAULT, None) {
        Ok(pw) => {
            let s = pw.to_string().unwrap_or_default();
            CoTaskMemFree(Some(pw.0 as *const c_void));
            s
        }
        Err(_) => String::new(),
    }
}

/// Show `dlg` on `owner` and take the picked item's filesystem path. The tail every picker
/// here ends with; four copies of it until 2026-09-19.
unsafe fn shown_path(dlg: &IFileDialog, owner: HWND) -> Option<String> {
    dlg.Show(Some(owner)).ok()?;
    let item: IShellItem = dlg.GetResult().ok()?;
    let pw = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
    let s = pw.to_string().ok();
    CoTaskMemFree(Some(pw.0 as *const c_void));
    s
}

/// One file-type filter (`name`, `spec` such as `*.png`) and, for a save dialog, the default
/// extension appended to a typed name.
unsafe fn set_single_filter(dlg: &IFileDialog, name: &str, spec: &str, default_ext: Option<&str>) {
    let spec_name = wide(name);
    let spec_ext = wide(spec);
    let specs = [COMDLG_FILTERSPEC {
        pszName: PCWSTR(spec_name.as_ptr()),
        pszSpec: PCWSTR(spec_ext.as_ptr()),
    }];
    let _ = dlg.SetFileTypes(&specs);
    if let Some(ext) = default_ext {
        let ext = wide(ext);
        let _ = dlg.SetDefaultExtension(PCWSTR(ext.as_ptr()));
    }
}

/// The open-dialog options the folder picker adds to whatever the dialog already had:
/// `FOS_PICKFOLDERS` turns the file dialog into a folder chooser, and `FOS_FORCEFILESYSTEM`
/// keeps the result a real filesystem path.
fn folder_pick_options(
    opts: windows::Win32::UI::Shell::FILEOPENDIALOGOPTIONS,
) -> windows::Win32::UI::Shell::FILEOPENDIALOGOPTIONS {
    opts | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM
}

/// Folder picker via IFileOpenDialog (FOS_PICKFOLDERS).
pub unsafe fn pick_folder(owner: HWND) -> Option<String> {
    let _com = ComGuard::sta();
    let dlg: IFileOpenDialog =
        CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
    let opts = dlg.GetOptions().ok()?;
    dlg.SetOptions(folder_pick_options(opts)).ok()?;
    shown_path(&dlg, owner)
}

/// Create an `IFileSaveDialog` on a freshly entered STA; the returned guard must be kept
/// alive (bound, not `_`) for as long as the dialog is used.
unsafe fn save_dialog() -> Option<(Option<ComGuard>, IFileSaveDialog)> {
    let com = ComGuard::sta();
    let dlg: IFileSaveDialog =
        CoCreateInstance(&FileSaveDialog, None, CLSCTX_INPROC_SERVER).ok()?;
    Some((com, dlg))
}

/// The save-dialog options `pick_save_png` forces on top of the dialog's own:
/// `FOS_STRICTFILETYPES` keeps a typed `shot.jpg` from coming back as-is (2026-09-19 audit
/// F18), and `FOS_FORCEFILESYSTEM` keeps the result a real filesystem path.
fn png_save_options(
    opts: windows::Win32::UI::Shell::FILEOPENDIALOGOPTIONS,
) -> windows::Win32::UI::Shell::FILEOPENDIALOGOPTIONS {
    opts | FOS_STRICTFILETYPES | FOS_FORCEFILESYSTEM
}

/// PNG "Save as" dialog via IFileSaveDialog. Unlike the classic GetSaveFileNameW — which
/// drifts to the top-left / behind a fullscreen owner like the capture overlay — this
/// centres itself on the owner, so it can't get lost. Seeds the dialog with folder `dir`
/// and default file `name`. Returns the chosen path (a `.png`), or None if cancelled.
pub unsafe fn pick_save_png(owner: HWND, dir: &str, name: &str) -> Option<String> {
    let (_com, dlg) = save_dialog()?;
    set_single_filter(&dlg, "PNG image", "*.png", Some("png"));
    // The dialog itself keeps the chosen name on the PNG filter: without FOS_STRICTFILETYPES a
    // typed `shot.jpg` came back as-is and the save then had to cope with a name that lied
    // about the format (2026-09-19 audit F18).
    if let Ok(opts) = dlg.GetOptions() {
        let _ = dlg.SetOptions(png_save_options(opts));
    }
    let nm = wide(name);
    let _ = dlg.SetFileName(PCWSTR(nm.as_ptr()));
    if !dir.is_empty() {
        // `SHCreateItemFromParsingName` wants the extended-length-prefix-free absolute form
        // (see `parsing_path`'s doc); a raw path here silently fails to resolve on some
        // inputs (G86 in the paired review docs).
        let dw = wide(&parsing_path(dir));
        if let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(dw.as_ptr()), None)
        {
            let _ = dlg.SetFolder(&item);
        }
    }
    shown_path(&dlg, owner)
}

/// "Save settings as" dialog (a `.json` file) via IFileSaveDialog — centres on `owner`
/// like [`pick_save_png`]. Seeds the default file `name`; returns the chosen path or None.
pub unsafe fn pick_save_settings(owner: HWND, name: &str) -> Option<String> {
    let (_com, dlg) = save_dialog()?;
    set_single_filter(&dlg, "SageThumbs 2K settings", "*.json", Some("json"));
    let nm = wide(name);
    let _ = dlg.SetFileName(PCWSTR(nm.as_ptr()));
    shown_path(&dlg, owner)
}

/// "Open settings" dialog (a `.json` file) via IFileOpenDialog. Returns the chosen path
/// or None. Open dialogs default to file-must-exist, so a bad pick can't reach us.
pub unsafe fn pick_open_settings(owner: HWND) -> Option<String> {
    pick_open_file(owner, "SageThumbs 2K settings", "*.json")
}

/// The open-dialog options `pick_open_file` forces on top of the dialog's own: only
/// `FOS_FORCEFILESYSTEM`, so the picked item is a real filesystem path.
fn open_file_options(
    opts: windows::Win32::UI::Shell::FILEOPENDIALOGOPTIONS,
) -> windows::Win32::UI::Shell::FILEOPENDIALOGOPTIONS {
    opts | FOS_FORCEFILESYSTEM
}

/// An IFileOpenDialog with one file-type filter (`filter_name`, `filter_spec` such as
/// `*.png;*.jpg`), forced to filesystem paths. The chosen path, or None on cancel/failure.
pub unsafe fn pick_open_file(owner: HWND, filter_name: &str, filter_spec: &str) -> Option<String> {
    let _com = ComGuard::sta();
    let dlg: IFileOpenDialog =
        CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
    if let Ok(opts) = dlg.GetOptions() {
        let _ = dlg.SetOptions(open_file_options(opts));
    }
    set_single_filter(&dlg, filter_name, filter_spec, None);
    shown_path(&dlg, owner)
}

/// Put `text` on the clipboard as Unicode text. Best-effort. Delegates the unsafe
/// HGLOBAL ownership dance to the one shared writer in the lib's `clipboard` module.
pub unsafe fn set_clipboard_text(text: &str) -> bool {
    let bytes = st2k_base::clipboard::utf16_nul_bytes(text);
    st2k_base::clipboard::set_clipboard(st2k_base::clipboard::CF_UNICODETEXT, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::Shell::{FILEOPENDIALOGOPTIONS, FOS_ALLNONSTORAGEITEMS};

    /// The folder picker must actually pick folders: without `FOS_PICKFOLDERS` the dialog is
    /// an ordinary file chooser and the user can never satisfy "choose a folder".
    #[test]
    fn folder_pick_options_force_the_folder_chooser_flag() {
        let got = folder_pick_options(FILEOPENDIALOGOPTIONS(0));
        assert!(got.contains(FOS_PICKFOLDERS));
        assert!(got.contains(FOS_FORCEFILESYSTEM));
    }

    /// A folder pick is meaningless unless it resolves to a real filesystem path, which is
    /// what `FOS_FORCEFILESYSTEM` guarantees — but the picker must not, like the save dialog,
    /// start enforcing file types (there is no file type to enforce).
    #[test]
    fn folder_pick_options_do_not_impose_strict_file_types() {
        let got = folder_pick_options(FILEOPENDIALOGOPTIONS(0));
        assert!(!got.contains(FOS_STRICTFILETYPES));
    }

    /// 2026-09-19 audit F18: without `FOS_STRICTFILETYPES` a typed `shot.jpg` came back as-is
    /// on the PNG filter and the save then wrote PNG bytes under a name that lied about the
    /// format. The flag is the whole fix, so it must stay forced.
    #[test]
    fn png_save_options_force_strict_file_types() {
        let got = png_save_options(FILEOPENDIALOGOPTIONS(0));
        assert!(got.contains(FOS_STRICTFILETYPES));
        assert!(got.contains(FOS_FORCEFILESYSTEM));
    }

    /// The open dialog is a plain file chooser (an open dialog defaults to file-must-exist),
    /// so it must add only the filesystem bit — turning on the folder chooser here would
    /// change what the settings picker can return.
    #[test]
    fn open_file_options_add_only_the_filesystem_bit() {
        let got = open_file_options(FILEOPENDIALOGOPTIONS(0));
        assert_eq!(got, FOS_FORCEFILESYSTEM);
        assert!(!got.contains(FOS_PICKFOLDERS));
        assert!(!got.contains(FOS_STRICTFILETYPES));
    }

    /// These helpers are applied to `GetOptions`' result, so they may only ADD bits: dropping
    /// a flag the dialog already carried would silently reverse some other caller's choice.
    #[test]
    fn every_options_helper_preserves_the_flags_the_dialog_already_had() {
        let existing = FOS_ALLNONSTORAGEITEMS;
        assert!(folder_pick_options(existing).contains(existing));
        assert!(png_save_options(existing).contains(existing));
        assert!(open_file_options(existing).contains(existing));
    }
}
