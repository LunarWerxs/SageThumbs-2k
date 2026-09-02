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
//! # The handshake
//!
//! The requester (`src/bin/app/dialog_hook.rs`) creates the section and the event with a
//! DACL that admits only its own user, refuses to proceed if either name already existed,
//! writes a [`Slot`] carrying the dialog `HWND` and a fresh random nonce, installs the hook,
//! and then SENDS the registered `SageThumbs2K.DlgSel.Request` message to the dialog. The
//! hook ignores every other message outright; on that one it maps the section, claims the
//! request with a compare-exchange on `state`, answers only if the armed `HWND` is the
//! window the message went to AND that window belongs to the current thread, copies the
//! nonce back into `ack`, and unmaps again. Nothing stays mapped between requests, so the
//! section dies with the requester's handles and the next request can create it afresh.
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
//!   every window on that thread. The hook compares the message id against the registered
//!   request message and drops straight back out on a mismatch, so the host pays one atomic
//!   load and one integer compare per message and nothing else.
//! - **Nothing is logged, stored or sent.** The only output is one path, written into a
//!   shared section the requester created.

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, Ordering};

use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Memory::{
    MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_ALL_ACCESS,
    MEMORY_MAPPED_VIEW_ADDRESS,
};
use windows::Win32::System::Threading::{
    GetCurrentThreadId, OpenEventW, SetEvent, EVENT_MODIFY_STATE,
};
use windows::Win32::UI::Shell::{
    IFolderView2, IShellBrowser, IShellItem, IShellItemArray, SIGDN_FILESYSPATH,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetWindowThreadProcessId, RegisterWindowMessageW, SendMessageTimeoutW,
    CWPSTRUCT, SMTO_ABORTIFHUNG, WM_USER,
};

/// The undocumented "give me your `IShellBrowser`" message a common file dialog answers.
/// Same constant QuickLook's `DialogHook` uses; it is the only route in (see the module docs).
const WM_GETISHELLBROWSER: u32 = WM_USER + 7;

/// How long the `WM_USER + 7` probe may take. The dialog is on this very thread, so the
/// send is a direct call and this is a backstop, not a latency budget.
const PROBE_TIMEOUT_MS: u32 = 1000;

/// Names of the two kernel objects the REQUESTER creates before arming a request, and of
/// the window message it sends to the dialog to make the hook run. Kept in step with
/// `src/bin/app/dialog_hook.rs` by hand — this crate deliberately shares no code with the
/// app, so that the app's dependency tree never reaches a DLL that loads into other
/// processes. `slot_layout_and_constants_match_the_app_side` pins the numbers on both sides.
const SECTION_NAME: PCWSTR = windows::core::w!("Local\\SageThumbs2K.DlgSel.Section");
const EVENT_NAME: PCWSTR = windows::core::w!("Local\\SageThumbs2K.DlgSel.Done");
const REQUEST_MESSAGE_NAME: PCWSTR = windows::core::w!("SageThumbs2K.DlgSel.Request");

/// Longest path we will hand back, in UTF-16 units. Comfortably past `MAX_PATH` so long
/// paths survive, and small enough that the whole slot is one page-ish.
pub const PATH_CAP: usize = 1024;

/// `Slot::state` values. The requester writes `REQUESTED`; we move it to `BUSY` with a
/// compare-exchange (so two hooked windows cannot both answer), then to `DONE` or `FAILED`,
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
    /// One of the `STATE_*` values above. Only ever moved with atomic operations.
    state: u32,
    /// The dialog `HWND` to interrogate, as a `u64` so the layout is bitness-stable.
    dialog: u64,
    /// Per-request random value written by the requester.
    nonce: u64,
    /// `nonce`, copied back by the hook when it finishes; the requester accepts an answer
    /// only when this equals the nonce it armed.
    ack: u64,
    /// UTF-16 units written into `path`.
    len: u32,
    /// The resolved filesystem path, NOT NUL-terminated (`len` bounds it).
    path: [u16; PATH_CAP],
}

/// `ST2D`, little-endian.
const MAGIC: u32 = 0x4432_5453;

/// The registered id of `REQUEST_MESSAGE_NAME` in this process; `0` until first use.
static REQUEST_MESSAGE: AtomicU32 = AtomicU32::new(0);

/// The `WH_CALLWNDPROC` callback. Exported by name; the app resolves it with
/// `GetProcAddress` and hands it to `SetWindowsHookExW` against the dialog's thread.
///
/// # Safety
/// Called by Windows with the documented hook-callback contract.
#[no_mangle]
pub unsafe extern "system" fn st2k_dlg_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 && lparam.0 != 0 {
        // SAFETY: for HC_ACTION Windows passes a CWPSTRUCT pointer in lparam.
        let cwp: CWPSTRUCT = core::ptr::read(lparam.0 as *const CWPSTRUCT);
        if Some(cwp.message) == request_message() {
            serve(cwp.hwnd);
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

/// The id of the requester's message, registered once per process. `None` when user32
/// refuses to register it, in which case this hook never serves.
unsafe fn request_message() -> Option<u32> {
    let cached = REQUEST_MESSAGE.load(Ordering::Relaxed);
    if cached != 0 {
        return Some(cached);
    }
    let id = RegisterWindowMessageW(REQUEST_MESSAGE_NAME);
    if id == 0 {
        return None;
    }
    REQUEST_MESSAGE.store(id, Ordering::Relaxed);
    Some(id)
}

/// Move `state` from `REQUESTED` to `BUSY`. Exactly one caller wins; every other hooked
/// window that sees the same request loses the exchange and leaves the slot alone.
fn claim(state: &AtomicU32) -> bool {
    state
        .compare_exchange(
            STATE_REQUESTED,
            STATE_BUSY,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_ok()
}

/// Whether `hwnd` is a live window owned by the thread this hook is running on.
unsafe fn owned_by_this_thread(hwnd: HWND) -> bool {
    let tid = GetWindowThreadProcessId(hwnd, None);
    tid != 0 && tid == GetCurrentThreadId()
}

/// Answer the pending request, if there is one, for the window the request message was
/// sent to. Maps the requester's section for the duration of the call only.
unsafe fn serve(target: HWND) {
    let Some(view) = MappedSlot::open() else {
        return;
    };
    let slot = view.slot();
    if (*slot).magic != MAGIC {
        return;
    }
    // SAFETY: `state` is a 4-byte-aligned u32 inside a page-aligned mapped view, and every
    // party touches it through atomics only.
    let state = AtomicU32::from_ptr(&raw mut (*slot).state);
    if !claim(state) {
        return;
    }
    let dialog = HWND((*slot).dialog as *mut c_void);
    let nonce = (*slot).nonce;
    let ok = std::ptr::eq(dialog.0, target.0)
        && owned_by_this_thread(dialog)
        && write_selected_path(dialog, slot);
    (*slot).ack = nonce;
    state.store(
        if ok { STATE_DONE } else { STATE_FAILED },
        Ordering::Release,
    );
    signal_done();
}

/// Resolve the dialog's selection straight into the slot's `path`, setting `len`. `false`
/// when nothing is selected, the item has no filesystem path, or the path does not fit
/// [`PATH_CAP`]. The slot's `len` is left at what the requester armed (zero) on failure.
unsafe fn write_selected_path(hwnd: HWND, slot: *mut Slot) -> bool {
    // A raw pointer into the mapped view, never a `&mut [u16; N]`: the section is shared
    // with the requester, so a reference would assert an aliasing guarantee we cannot make.
    let dst = (&raw mut (*slot).path).cast::<u16>();
    match selected_path_into(hwnd, dst) {
        Some(n) => {
            (*slot).len = n;
            true
        }
        None => false,
    }
}

/// Copy the full filesystem path of the first SELECTED item in the dialog into `dst`, which
/// holds [`PATH_CAP`] units, and return how many units were written. `None` when there is
/// nothing to hand back.
///
/// Runs on the dialog's own thread (that is the whole point of this DLL), so the
/// apartment-bound `IShellBrowser` from `WM_USER + 7` is legal to call here. The caller has
/// already checked that `hwnd` belongs to this thread. Nothing is allocated: the string goes
/// from the shell's buffer into the mapped slot and the shell's buffer is freed.
unsafe fn selected_path_into(hwnd: HWND, dst: *mut u16) -> Option<u32> {
    let mut raw = 0usize;
    let sent = SendMessageTimeoutW(
        hwnd,
        WM_GETISHELLBROWSER,
        WPARAM(0),
        LPARAM(0),
        SMTO_ABORTIFHUNG,
        PROBE_TIMEOUT_MS,
        Some(&mut raw),
    );
    if sent.0 == 0 || raw == 0 {
        return None;
    }
    // BORROWED, never owned: `WM_USER + 7` returns the pointer without an `AddRef`, so
    // taking ownership here would over-release the dialog's own browser.
    let ptr = raw as *mut c_void;
    let browser = IShellBrowser::from_raw_borrowed(&ptr)?;
    let view = browser.QueryActiveShellView().ok()?;
    let folder = view.cast::<IFolderView2>().ok()?;
    let items: IShellItemArray = folder.GetSelection(false).ok()?;
    if items.GetCount().ok()? == 0 {
        return None;
    }
    let item: IShellItem = items.GetItemAt(0).ok()?;
    let pw = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
    let n = copy_pwstr(pw.0, dst);
    CoTaskMemFree(Some(pw.0 as *const c_void));
    n
}

/// Copy the NUL-terminated shell string at `p` into `dst` (at least [`PATH_CAP`] units) and
/// return the number of units copied, NUL excluded. Reads at most `PATH_CAP` units of `p`, so
/// a hostile/garbage pointer cannot walk memory forever. `None` for null, empty, or a string
/// with no NUL within `PATH_CAP` units — the last case means the real path is longer than we
/// can carry, and handing back a silently truncated path is worse than the caller's existing
/// `STATE_FAILED` handling (a truncated path can resolve to a different, real file).
unsafe fn copy_pwstr(p: *const u16, dst: *mut u16) -> Option<u32> {
    if p.is_null() {
        return None;
    }
    let mut i = 0usize;
    while i < PATH_CAP {
        let c = *p.add(i);
        if c == 0 {
            return if i == 0 { None } else { Some(i as u32) };
        }
        *dst.add(i) = c;
        i += 1;
    }
    None
}

/// A view of the requester's section, unmapped when dropped. Nothing is cached across
/// calls: a view held open would keep the section object (and its name) alive after the
/// requester released it, and the requester refuses a name that already exists.
struct MappedSlot(MEMORY_MAPPED_VIEW_ADDRESS);

impl MappedSlot {
    /// Open and map the section. `None` when it does not exist (nobody is asking) or the
    /// DACL the requester put on it keeps this process out.
    unsafe fn open() -> Option<Self> {
        let mapping = OpenFileMappingW(FILE_MAP_ALL_ACCESS.0, false, SECTION_NAME).ok()?;
        let view = MapViewOfFile(
            mapping,
            FILE_MAP_ALL_ACCESS,
            0,
            0,
            core::mem::size_of::<Slot>(),
        );
        // The view survives the handle; close it straight away.
        let _ = CloseHandle(mapping);
        if view.Value.is_null() {
            return None;
        }
        Some(Self(view))
    }

    fn slot(&self) -> *mut Slot {
        self.0.Value as *mut Slot
    }
}

impl Drop for MappedSlot {
    fn drop(&mut self) {
        unsafe {
            let _ = UnmapViewOfFile(self.0);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The app mirrors this layout by hand (`src/bin/app/dialog_hook.rs`); its test pins the
    /// same numbers, so a change on one side fails one of the two.
    #[test]
    fn slot_layout_and_constants_match_the_app_side() {
        assert_eq!(core::mem::size_of::<Slot>(), 2088);
        assert_eq!(core::mem::align_of::<Slot>(), 8);
        assert_eq!(core::mem::offset_of!(Slot, magic), 0);
        assert_eq!(core::mem::offset_of!(Slot, state), 4);
        assert_eq!(core::mem::offset_of!(Slot, dialog), 8);
        assert_eq!(core::mem::offset_of!(Slot, nonce), 16);
        assert_eq!(core::mem::offset_of!(Slot, ack), 24);
        assert_eq!(core::mem::offset_of!(Slot, len), 32);
        assert_eq!(core::mem::offset_of!(Slot, path), 36);
        assert_eq!(PATH_CAP, 1024);
        assert_eq!(MAGIC, 0x4432_5453);
        assert_eq!(STATE_REQUESTED, 1);
        assert_eq!(STATE_BUSY, 2);
        assert_eq!(STATE_DONE, 3);
        assert_eq!(STATE_FAILED, 4);
        assert_eq!(WM_GETISHELLBROWSER, WM_USER + 7);
    }

    /// Two hooked windows seeing the same request: the first claim wins, the second is
    /// refused, and a slot that is not `REQUESTED` is never claimed at all.
    #[test]
    fn claim_is_won_exactly_once() {
        let state = AtomicU32::new(STATE_REQUESTED);
        assert!(claim(&state), "the first claimant must win");
        assert_eq!(state.load(Ordering::Relaxed), STATE_BUSY);
        assert!(!claim(&state), "a second claimant must lose");
        for other in [0, STATE_DONE, STATE_FAILED] {
            let state = AtomicU32::new(other);
            assert!(!claim(&state), "state {other} is not claimable");
            assert_eq!(state.load(Ordering::Relaxed), other);
        }
    }

    /// A NUL-terminated string within the cap is copied verbatim.
    #[test]
    fn copy_pwstr_returns_the_terminated_string() {
        let src: Vec<u16> = "C:\\pics\\a.png\0".encode_utf16().collect();
        let mut dst = [0u16; PATH_CAP];
        let n = unsafe { copy_pwstr(src.as_ptr(), dst.as_mut_ptr()) }
            .expect("terminated string must copy");
        let want: Vec<u16> = "C:\\pics\\a.png".encode_utf16().collect();
        assert_eq!(n as usize, want.len());
        let got: Vec<u16> = dst.iter().take(n as usize).copied().collect();
        assert_eq!(got, want);
    }

    /// A string with no NUL within `PATH_CAP` units used to be silently truncated and handed
    /// back as `Some(out)` — the caller then reported `STATE_DONE` with a cut-off path instead
    /// of the `STATE_FAILED` this exact case is meant to produce. Must now be `None`.
    #[test]
    fn copy_pwstr_rejects_a_string_with_no_nul_within_the_cap_instead_of_truncating() {
        let src: Vec<u16> = core::iter::repeat_n(b'x' as u16, PATH_CAP).collect();
        let mut dst = [0u16; PATH_CAP];
        assert!(
            unsafe { copy_pwstr(src.as_ptr(), dst.as_mut_ptr()) }.is_none(),
            "an unterminated string at the cap boundary must be rejected, not truncated"
        );
    }

    /// A NUL landing on the very last index the bounded loop checks (`PATH_CAP - 1`) is still
    /// a real, legitimate terminated string and must not be caught by the new truncation
    /// guard — only a string with NO NUL anywhere in range is a truncation.
    #[test]
    fn copy_pwstr_accepts_a_string_terminated_exactly_at_the_cap_boundary() {
        let mut src: Vec<u16> = core::iter::repeat_n(b'x' as u16, PATH_CAP - 1).collect();
        src.push(0); // NUL at index PATH_CAP - 1, the last index the loop reads
        let mut dst = [0u16; PATH_CAP];
        let n = unsafe { copy_pwstr(src.as_ptr(), dst.as_mut_ptr()) }
            .expect("boundary-terminated string");
        assert_eq!(n as usize, PATH_CAP - 1);
        assert!(dst.iter().take(PATH_CAP - 1).all(|&c| c == b'x' as u16));
    }

    /// An empty string (NUL at index 0) is not a path and is refused, not reported as a
    /// zero-length success.
    #[test]
    fn copy_pwstr_rejects_an_empty_string() {
        let src = [0u16; 1];
        let mut dst = [0u16; PATH_CAP];
        assert!(unsafe { copy_pwstr(src.as_ptr(), dst.as_mut_ptr()) }.is_none());
    }

    #[test]
    fn copy_pwstr_rejects_null_pointer() {
        let mut dst = [0u16; PATH_CAP];
        assert!(unsafe { copy_pwstr(core::ptr::null(), dst.as_mut_ptr()) }.is_none());
    }
}
