//! File-dialog pickers + clipboard (extracted from win.rs; behavior unchanged).

use core::ffi::c_void;

use windows::core::PCWSTR;
use windows::Win32::Foundation::HWND;

use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::{
    FileOpenDialog, FileSaveDialog, IFileOpenDialog, IFileSaveDialog, IShellItem,
    SHCreateItemFromParsingName, SHGetKnownFolderPath, FOLDERID_Desktop, FOS_FORCEFILESYSTEM,
    FOS_PICKFOLDERS, KF_FLAG_DEFAULT, SIGDN_FILESYSPATH,
};

use super::wide;

pub(crate) unsafe fn desktop_dir() -> String {
    match SHGetKnownFolderPath(&FOLDERID_Desktop, KF_FLAG_DEFAULT, None) {
        Ok(pw) => {
            let s = pw.to_string().unwrap_or_default();
            CoTaskMemFree(Some(pw.0 as *const c_void));
            s
        }
        Err(_) => String::new(),
    }
}

/// Folder picker via IFileOpenDialog (FOS_PICKFOLDERS).
pub(crate) unsafe fn pick_folder(owner: HWND) -> Option<String> {
    struct ComGuard(bool);
    impl Drop for ComGuard {
        fn drop(&mut self) {
            if self.0 {
                unsafe { CoUninitialize() };
            }
        }
    }

    let _com = ComGuard(CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok());
    let dlg: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
    let opts = dlg.GetOptions().ok()?;
    dlg.SetOptions(opts | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM).ok()?;
    dlg.Show(Some(owner)).ok()?;
    let item: IShellItem = dlg.GetResult().ok()?;
    let pw = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
    let s = pw.to_string().ok();
    CoTaskMemFree(Some(pw.0 as *const c_void));
    s
}

/// PNG "Save as" dialog via IFileSaveDialog. Unlike the classic GetSaveFileNameW — which
/// drifts to the top-left / behind a fullscreen owner like the capture overlay — this
/// centres itself on the owner, so it can't get lost. Seeds the dialog with folder `dir`
/// and default file `name`. Returns the chosen path (a `.png`), or None if cancelled.
pub(crate) unsafe fn pick_save_png(owner: HWND, dir: &str, name: &str) -> Option<String> {
    struct ComGuard(bool);
    impl Drop for ComGuard {
        fn drop(&mut self) {
            if self.0 {
                unsafe { CoUninitialize() };
            }
        }
    }

    let _com = ComGuard(CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok());
    let dlg: IFileSaveDialog = CoCreateInstance(&FileSaveDialog, None, CLSCTX_INPROC_SERVER).ok()?;
    let spec_name = wide("PNG image");
    let spec_ext = wide("*.png");
    let specs = [COMDLG_FILTERSPEC {
        pszName: PCWSTR(spec_name.as_ptr()),
        pszSpec: PCWSTR(spec_ext.as_ptr()),
    }];
    let _ = dlg.SetFileTypes(&specs);
    let ext = wide("png");
    let _ = dlg.SetDefaultExtension(PCWSTR(ext.as_ptr()));
    let nm = wide(name);
    let _ = dlg.SetFileName(PCWSTR(nm.as_ptr()));
    if !dir.is_empty() {
        let dw = wide(dir);
        if let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(dw.as_ptr()), None) {
            let _ = dlg.SetFolder(&item);
        }
    }
    dlg.Show(Some(owner)).ok()?;
    let item: IShellItem = dlg.GetResult().ok()?;
    let pw = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
    let s = pw.to_string().ok();
    CoTaskMemFree(Some(pw.0 as *const c_void));
    s
}

/// "Save settings as" dialog (a `.json` file) via IFileSaveDialog — centres on `owner`
/// like [`pick_save_png`]. Seeds the default file `name`; returns the chosen path or None.
pub(crate) unsafe fn pick_save_settings(owner: HWND, name: &str) -> Option<String> {
    struct ComGuard(bool);
    impl Drop for ComGuard {
        fn drop(&mut self) {
            if self.0 {
                unsafe { CoUninitialize() };
            }
        }
    }

    let _com = ComGuard(CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok());
    let dlg: IFileSaveDialog = CoCreateInstance(&FileSaveDialog, None, CLSCTX_INPROC_SERVER).ok()?;
    let spec_name = wide("SageThumbs 2K settings");
    let spec_ext = wide("*.json");
    let specs = [COMDLG_FILTERSPEC {
        pszName: PCWSTR(spec_name.as_ptr()),
        pszSpec: PCWSTR(spec_ext.as_ptr()),
    }];
    let _ = dlg.SetFileTypes(&specs);
    let ext = wide("json");
    let _ = dlg.SetDefaultExtension(PCWSTR(ext.as_ptr()));
    let nm = wide(name);
    let _ = dlg.SetFileName(PCWSTR(nm.as_ptr()));
    dlg.Show(Some(owner)).ok()?;
    let item: IShellItem = dlg.GetResult().ok()?;
    let pw = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
    let s = pw.to_string().ok();
    CoTaskMemFree(Some(pw.0 as *const c_void));
    s
}

/// "Open settings" dialog (a `.json` file) via IFileOpenDialog. Returns the chosen path
/// or None. Open dialogs default to file-must-exist, so a bad pick can't reach us.
pub(crate) unsafe fn pick_open_settings(owner: HWND) -> Option<String> {
    struct ComGuard(bool);
    impl Drop for ComGuard {
        fn drop(&mut self) {
            if self.0 {
                unsafe { CoUninitialize() };
            }
        }
    }

    let _com = ComGuard(CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok());
    let dlg: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
    if let Ok(opts) = dlg.GetOptions() {
        let _ = dlg.SetOptions(opts | FOS_FORCEFILESYSTEM);
    }
    let spec_name = wide("SageThumbs 2K settings");
    let spec_ext = wide("*.json");
    let specs = [COMDLG_FILTERSPEC {
        pszName: PCWSTR(spec_name.as_ptr()),
        pszSpec: PCWSTR(spec_ext.as_ptr()),
    }];
    let _ = dlg.SetFileTypes(&specs);
    dlg.Show(Some(owner)).ok()?;
    let item: IShellItem = dlg.GetResult().ok()?;
    let pw = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
    let s = pw.to_string().ok();
    CoTaskMemFree(Some(pw.0 as *const c_void));
    s
}

/// Put `text` on the clipboard as Unicode text. Best-effort. Delegates the unsafe
/// HGLOBAL ownership dance to the one shared writer in the lib's `clipboard` module.
pub(crate) unsafe fn set_clipboard_text(text: &str) -> bool {
    let bytes = sagethumbs2k_core::clipboard::utf16_nul_bytes(text);
    sagethumbs2k_core::clipboard::set_clipboard(sagethumbs2k_core::clipboard::CF_UNICODETEXT, &bytes)
}
