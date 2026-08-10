//! The Open/Save-dialog selection reader — the ONE piece of SageThumbs that runs inside
//! somebody else's process.
//!
//! # Why this exists at all
//!
//! Quick preview resolves "what is selected" through the shell automation interfaces
//! (`explorer_selection.rs`). A common file dialog is invisible to those: it is not in
//! `IShellWindows`, and every cross-process route to its shell view was probed and refused
//! (2026-08-10, on Win11 26200 with the dialog sitting on a KNOWN folder, which is the case
//! that matters because it is where people actually pick files):
//!
//! | route                                        | result                                    |
//! |----------------------------------------------|-------------------------------------------|
//! | `AccessibleObjectFromWindow(OBJID_NATIVEOM)`  | `E_FAIL` on the dialog, the `SHELLDLL_DefView` and its `DirectUIHWND` |
//! | UI Automation over the whole dialog           | 0 of 104 elements carried a filesystem path |
//! | window text / `WM_GETTEXT` over every child   | the breadcrumb reads "Address: Documents", a DISPLAY name, not a path |
//! | MSAA `accSelection` on the `SHELLDLL_DefView` | `null` (the selection lives in the DirectUI child) |
//!
//! What DOES work is the undocumented `WM_USER + 7`, which returns the dialog's live
//! `IShellBrowser`. That pointer is apartment-bound: calling it from another thread returns
//! `RPC_E_WRONG_THREAD` (measured, not assumed — an in-process dialog reproduced it). So the
//! call has to be made ON the dialog's own thread, and the only way to get there is to be
//! loaded into its process by a `WH_CALLWNDPROC` hook. That is this DLL, and it is the
//! entire reason it is separate from `sagethumbs2k.dll`.
//!
//! # The rules this file lives by — do not relax any of them
//!
//! - **PANIC-FREE.** The workspace release profile is `panic = "abort"`. A panic in here
//!   aborts the HOST application (Word, a browser, the user's editor), not us. So: no
//!   `unwrap`, no `expect`, no `[]` indexing, no slicing, no arithmetic that can overflow in
//!   debug. Everything is `Option`/`Result` + `?`, and every buffer write is length-checked.
//! - **No allocation, no I/O, no COM init.** The dialog's thread is already an initialised
//!   STA; we borrow its objects and leave. We never call `CoInitialize`/`CoUninitialize`
//!   (that would corrupt the host's apartment refcount) and we never `Release` the browser
//!   (`WM_USER + 7` hands it out WITHOUT an `AddRef`, so we borrow it).
//! - **Answer once, then go quiet.** A `WH_CALLWNDPROC` hook fires for every message sent to
//!   every window on that thread. The state machine below does its work only when the
//!   requesting side has armed a request, and drops straight back out otherwise, so the host
//!   pays one atomic load per message and nothing else.
//! - **Nothing is logged, stored or sent.** The only output is one path, written into a
//!   shared section the requester created.

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use core::ffi::c_void;
use core::sync::atomic::{AtomicIsize, Ordering};

use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Memory::{
    MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_ALL_ACCESS,
    MEMORY_MAPPED_VIEW_ADDRESS,
};
use windows::Win32::System::Threading::{OpenEventW, SetEvent, EVENT_MODIFY_STATE};
use windows::Win32::UI::Shell::{
    IFolderView2, IShellBrowser, IShellItem, IShellItemArray, SIGDN_FILESYSPATH,
};
use windows::Win32::UI::WindowsAndMessaging::{CallNextHookEx, SendMessageW, WM_USER};

/// The undocumented "give me your `IShellBrowser`" message a common file dialog answers.
/// Same constant QuickLook's `DialogHook` uses; it is the only route in (see the module docs).
const WM_GETISHELLBROWSER: u32 = WM_USER + 7;

/// Names of the two kernel objects the REQUESTER creates before arming a request. Kept in
/// step with `explorer_selection.rs::dialog` by hand — this crate deliberately shares no code
/// with the app, so that the app's dependency tree never reaches a DLL that loads into other
/// processes.
const SECTION_NAME: PCWSTR = windows::core::w!("Local\\SageThumbs2K.DlgSel.Section");
const EVENT_NAME: PCWSTR = windows::core::w!("Local\\SageThumbs2K.DlgSel.Done");

/// Longest path we will hand back, in UTF-16 units. Comfortably past `MAX_PATH` so long
/// paths survive, and small enough that the whole slot is one page-ish.
pub const PATH_CAP: usize = 1024;

/// `Slot::state` values. The requester writes `REQUESTED`; we move it to `DONE` or `FAILED`
/// and never touch it again.
const STATE_REQUESTED: u32 = 1;
const STATE_BUSY: u32 = 2;
const STATE_DONE: u32 = 3;
const STATE_FAILED: u32 = 4;

/// The shared handshake block. `#[repr(C)]` because two independently compiled binaries map
/// it; never reorder or resize a field without changing both sides.
#[repr(C)]
struct Slot {
    /// `ST2D` — guards against mapping something else's section of the same name.
    magic: u32,
    /// One of the `STATE_*` values above.
    state: u32,
    /// The dialog `HWND` to interrogate, as a `u64` so the layout is bitness-stable.
    dialog: u64,
    /// UTF-16 units written into `path`.
    len: u32,
    /// The resolved filesystem path, NOT NUL-terminated (`len` bounds it).
    path: [u16; PATH_CAP],
}

/// `ST2D`, little-endian.
const MAGIC: u32 = 0x4432_5453;

/// Cached mapped view for this process, so a hook that fires thousands of times opens the
/// section once. Stored as a raw address; `0` means "not mapped yet / unavailable".
static VIEW: AtomicIsize = AtomicIsize::new(0);

/// The `WH_CALLWNDPROC` callback. Exported by name; the app resolves it with
/// `GetProcAddress` and hands it to `SetWindowsHookExW` against the dialog's thread.
///
/// # Safety
/// Called by Windows with the documented hook-callback contract.
#[no_mangle]
pub unsafe extern "system" fn st2k_dlg_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        serve();
    }
    CallNextHookEx(None, code, wparam, lparam)
}

/// Answer a pending request, if there is one. Returns immediately when there is not, which
/// is the overwhelmingly common case (this runs for EVERY message sent on the host's thread).
unsafe fn serve() {
    let Some(slot) = slot() else { return };
    if (*slot).magic != MAGIC || (*slot).state != STATE_REQUESTED {
        return;
    }
    // Claim it before doing any work, so a re-entrant message can't start a second attempt.
    (*slot).state = STATE_BUSY;

    let ok = match selected_path((*slot).dialog) {
        Some(p) => write_path(slot, &p),
        None => false,
    };
    (*slot).state = if ok { STATE_DONE } else { STATE_FAILED };
    signal_done();
}

/// Copy `path` into the slot, truncating to [`PATH_CAP`]. Returns false on an empty path.
/// Length-checked rather than sliced — this file must not panic (see the module docs).
unsafe fn write_path(slot: *mut Slot, path: &[u16]) -> bool {
    let n = if path.len() > PATH_CAP {
        PATH_CAP
    } else {
        path.len()
    };
    if n == 0 {
        return false;
    }
    core::ptr::copy_nonoverlapping(path.as_ptr(), (*slot).path.as_mut_ptr(), n);
    (*slot).len = n as u32;
    true
}

/// The full filesystem path of the first SELECTED item in the dialog, or `None`.
///
/// Runs on the dialog's own thread (that is the whole point of this DLL), so the
/// apartment-bound `IShellBrowser` from `WM_USER + 7` is legal to call here.
unsafe fn selected_path(dialog: u64) -> Option<Vec<u16>> {
    let hwnd = HWND(dialog as *mut c_void);
    let raw = SendMessageW(hwnd, WM_GETISHELLBROWSER, None, None);
    if raw.0 == 0 {
        return None;
    }
    // BORROWED, never owned: `WM_USER + 7` returns the pointer without an `AddRef`, so
    // taking ownership here would over-release the dialog's own browser.
    let ptr = raw.0 as *mut c_void;
    let browser = IShellBrowser::from_raw_borrowed(&ptr)?;
    let view = browser.QueryActiveShellView().ok()?;
    let folder = view.cast::<IFolderView2>().ok()?;
    let items: IShellItemArray = folder.GetSelection(false).ok()?;
    if items.GetCount().ok()? == 0 {
        return None;
    }
    let item: IShellItem = items.GetItemAt(0).ok()?;
    let pw = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
    let out = copy_pwstr(pw.0);
    CoTaskMemFree(Some(pw.0 as *const c_void));
    out
}

/// Copy a NUL-terminated shell string into an owned buffer, bounded by [`PATH_CAP`] so a
/// hostile/garbage pointer cannot walk memory forever. `None` for null or empty.
unsafe fn copy_pwstr(p: *const u16) -> Option<Vec<u16>> {
    if p.is_null() {
        return None;
    }
    let mut out: Vec<u16> = Vec::new();
    if out.try_reserve(PATH_CAP).is_err() {
        return None;
    }
    let mut i = 0usize;
    while i < PATH_CAP {
        let c = *p.add(i);
        if c == 0 {
            break;
        }
        out.push(c);
        i += 1;
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// The mapped handshake block for this process, opening the section on first use. `None`
/// when the requester's section does not exist (i.e. nobody is asking).
unsafe fn slot() -> Option<*mut Slot> {
    let cached = VIEW.load(Ordering::Relaxed);
    if cached != 0 {
        return Some(cached as *mut Slot);
    }
    let mapping = OpenFileMappingW(FILE_MAP_ALL_ACCESS.0, false, SECTION_NAME).ok()?;
    let view: MEMORY_MAPPED_VIEW_ADDRESS = MapViewOfFile(
        mapping,
        FILE_MAP_ALL_ACCESS,
        0,
        0,
        core::mem::size_of::<Slot>(),
    );
    // The view survives the handle; close it straight away so we leak nothing in the host.
    let _ = CloseHandle(mapping);
    if view.Value.is_null() {
        return None;
    }
    // Another thread may have won the race; keep the first mapping and drop ours.
    let addr = view.Value as isize;
    match VIEW.compare_exchange(0, addr, Ordering::Relaxed, Ordering::Relaxed) {
        Ok(_) => Some(addr as *mut Slot),
        Err(existing) => {
            let _ = UnmapViewOfFile(view);
            Some(existing as *mut Slot)
        }
    }
}

/// Wake the requester. Best-effort: it also polls, so a lost signal only costs latency.
unsafe fn signal_done() {
    let Ok(ev) = OpenEventW(EVENT_MODIFY_STATE, false, EVENT_NAME) else {
        return;
    };
    let _ = SetEvent(ev);
    let _ = CloseHandle(HANDLE(ev.0));
}
