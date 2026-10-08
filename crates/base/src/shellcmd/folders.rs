//! The folder windows open before Explorer restarts, reopened after it.
//!
//! Restart Manager starts Explorer again as `explorer.exe /LOADSAVEDWINDOWS`, but on Windows 11
//! that reopened none of the folder windows it had closed (measured 2026-10-08: one window open
//! before, none after, a full minute later). So the windows are read here before the restart,
//! by their shell location (a PIDL, so This PC, Home or a library come back too, not only
//! drive paths), and any Explorer has not restored itself a moment after the taskbar returns
//! is opened again.

use windows::core::{w, Interface};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, IServiceProvider, CLSCTX_ALL,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    IFolderView, ILGetSize, ILIsEqual, IPersistFolder2, IShellBrowser, IShellWindows,
    SID_STopLevelBrowser, ShellExecuteExW, ShellWindows, SEE_MASK_FLAG_NO_UI, SEE_MASK_IDLIST,
    SHELLEXECUTEINFOW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// One folder window's location: its ITEMIDLIST bytes, terminator included. Bytes, because a
/// PIDL pointer cannot cross to the thread that reopens it.
pub(super) type Pidl = Vec<u8>;

/// Run `f` on a fresh single-threaded apartment: the shell's window list and `ShellExecuteEx`
/// want one, and callers arrive on threads in any state (a Settings worker, a toast callback,
/// a bare `main`). A panic inside is the empty answer.
fn with_sta<T: Send + Default>(f: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|s| {
        s.spawn(|| unsafe {
            let initialized = CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok();
            let out = f();
            if initialized {
                CoUninitialize();
            }
            out
        })
        .join()
        .unwrap_or_default()
    })
}

/// The folder each open Explorer window shows. Empty when there are none, or when the shell
/// will not say: bringing windows back is a courtesy, never a reason to fail the restart.
pub(super) fn open_folders() -> Vec<Pidl> {
    with_sta(|| unsafe { folder_windows() })
}

/// After the restart: give Explorer `settle` to restore its own windows, then open each folder
/// from `saved` that no window shows, once. Matching is by shell identity (`ILIsEqual`), so a
/// window Explorer did restore is never opened a second time.
pub(super) fn reopen_missing(saved: Vec<Pidl>, settle: std::time::Duration) {
    if saved.is_empty() {
        return;
    }
    with_sta(move || unsafe {
        let deadline = std::time::Instant::now() + settle;
        let mut missing = still_missing(&saved, &folder_windows());
        while !missing.is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(250));
            missing = still_missing(&missing, &folder_windows());
        }
        for pidl in &missing {
            open_folder(pidl);
        }
    });
}

/// The folders in `saved` that no entry of `open` shows, each once (two windows on one folder
/// come back as one).
unsafe fn still_missing(saved: &[Pidl], open: &[Pidl]) -> Vec<Pidl> {
    let mut missing: Vec<Pidl> = Vec::new();
    for s in saved {
        if !open
            .iter()
            .chain(&missing)
            .any(|o| unsafe { same_folder(s, o) })
        {
            missing.push(s.clone());
        }
    }
    missing
}

unsafe fn same_folder(a: &Pidl, b: &Pidl) -> bool {
    unsafe {
        ILIsEqual(
            a.as_ptr().cast::<ITEMIDLIST>(),
            b.as_ptr().cast::<ITEMIDLIST>(),
        )
        .as_bool()
    }
}

/// Copy a shell-allocated PIDL into bytes and free it.
unsafe fn take_pidl(pidl: *mut ITEMIDLIST) -> Pidl {
    unsafe {
        let len = ILGetSize(Some(pidl)) as usize;
        let bytes = std::slice::from_raw_parts(pidl.cast::<u8>(), len).to_vec();
        CoTaskMemFree(Some(pidl.cast()));
        bytes
    }
}

/// Every Explorer folder window's location, read from the shell's window list.
unsafe fn folder_windows() -> Vec<Pidl> {
    unsafe {
        let Ok(windows) = CoCreateInstance::<_, IShellWindows>(&ShellWindows, None, CLSCTX_ALL)
        else {
            return Vec::new();
        };
        let count = windows.Count().unwrap_or(0);
        (0..count)
            .filter_map(|i| window_folder(&windows, i))
            .collect()
    }
}

/// Window `i`'s folder: its browser, the active view, the view's folder, that folder's PIDL.
unsafe fn window_folder(windows: &IShellWindows, i: i32) -> Option<Pidl> {
    unsafe {
        let window = windows.Item(&VARIANT::from(i)).ok()?;
        let browser: IShellBrowser = window
            .cast::<IServiceProvider>()
            .ok()?
            .QueryService(&SID_STopLevelBrowser)
            .ok()?;
        let view: IFolderView = browser.QueryActiveShellView().ok()?.cast().ok()?;
        let folder: IPersistFolder2 = view.GetFolder().ok()?;
        Some(take_pidl(folder.GetCurFolder().ok()?))
    }
}

/// Open `pidl` in a folder window, the way double-clicking it would.
unsafe fn open_folder(pidl: &Pidl) {
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_IDLIST | SEE_MASK_FLAG_NO_UI,
        lpVerb: w!("open"),
        lpIDList: pidl.as_ptr() as *mut core::ffi::c_void,
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    let _ = unsafe { ShellExecuteExW(&mut info) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::core::HSTRING;
    use windows::Win32::UI::Shell::SHParseDisplayName;

    fn pidl_of(path: &str) -> Pidl {
        let mut pidl = std::ptr::null_mut();
        unsafe {
            SHParseDisplayName(&HSTRING::from(path), None, &mut pidl, 0, None)
                .expect("parse a folder every Windows has");
            take_pidl(pidl)
        }
    }

    /// The decision after the restart: a folder Explorer restored on its own is not opened a
    /// second time, one it lost is, and two windows on one folder come back as one. Matched by
    /// the shell's own comparison on PIDLs parsed separately, as the before and after lists are.
    #[test]
    fn a_restored_folder_is_not_reopened_and_a_lost_one_is_once() {
        let (restored, lost) = with_sta(|| {
            let windows = pidl_of(r"C:\Windows");
            let system = pidl_of(r"C:\Windows\System32");
            let saved = vec![
                windows.clone(),
                system.clone(),
                pidl_of(r"C:\Windows\System32"),
            ];
            let missing = unsafe { still_missing(&saved, &[pidl_of(r"C:\Windows")]) };
            (
                missing.iter().any(|m| unsafe { same_folder(m, &windows) }),
                missing
                    .iter()
                    .filter(|m| unsafe { same_folder(m, &system) })
                    .count(),
            )
        });
        assert!(
            !restored,
            "a window Explorer restored must not be opened again"
        );
        assert_eq!(lost, 1, "a lost folder comes back exactly once");
    }
}
