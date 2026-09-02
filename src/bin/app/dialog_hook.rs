//! Read the selection out of a common **Open/Save dialog** — the requester half of the
//! `st2k_dlghook.dll` handshake.
//!
//! A file dialog is not a shell view we can reach: it is absent from `IShellWindows`, and
//! every cross-process route to its selection was probed and refused (the table is in
//! `dlghook/src/lib.rs`, which is where the WHY lives — do not duplicate it here). The one
//! route in is `WM_USER + 7`, whose `IShellBrowser` is apartment-bound, so the read must
//! happen on the dialog's OWN thread. This module arms a shared handshake block, gets
//! `st2k_dlghook.dll` loaded onto that thread with a `WH_CALLWNDPROC` hook, sends the
//! dialog the registered request message so the hook runs once, and reads the answer back.
//!
//! The handshake objects (section, event, and the mutex that serialises requesters) are
//! created with a DACL that admits only the current user's SID, and a section or event
//! whose name already existed is refused rather than opened. Each request carries a fresh
//! random nonce that the hook copies back; `read` accepts an answer only when the magic,
//! the armed dialog handle and the nonce all match.
//!
//! Everything here is FAIL-CLOSED. A missing DLL, a bitness mismatch, an elevated dialog, a
//! hook Windows refuses to install, a timeout, a pre-existing section — every one of them
//! returns `None`, and Space then stays a space exactly as it did before this module
//! existed. It never guesses.

use core::ffi::c_void;

use crate::win::with_user_only_dacl;
use core::sync::atomic::{AtomicU32, Ordering};
use std::hash::{BuildHasher, Hasher};

use windows::core::{s, w, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, FreeLibrary, GetLastError, SetLastError, ERROR_ALREADY_EXISTS, HANDLE, HMODULE,
    HWND, INVALID_HANDLE_VALUE, LPARAM, WAIT_ABANDONED, WAIT_OBJECT_0, WIN32_ERROR, WPARAM,
};
use windows::Win32::Security::SECURITY_ATTRIBUTES;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_ALL_ACCESS,
    MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
};
use windows::Win32::System::SystemInformation::{IMAGE_FILE_MACHINE, IMAGE_FILE_MACHINE_UNKNOWN};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, GetCurrentProcess, IsWow64Process2, OpenProcess, ReleaseMutex,
    WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, FindWindowExW, GetWindowThreadProcessId, RegisterWindowMessageW,
    SendMessageTimeoutW, SetWindowsHookExW, UnhookWindowsHookEx, HHOOK, HOOKPROC, SMTO_ABORTIFHUNG,
    WH_CALLWNDPROC,
};

use crate::explorer_selection::class_name;

/// Kept in step BY HAND with `dlghook/src/lib.rs` — the hook DLL shares no code with the app
/// on purpose, so that nothing in the app's dependency graph can reach a binary that gets
/// loaded into other people's processes. `slot_layout_and_constants_match_the_hook_side`
/// pins the numbers on both sides.
const SECTION_NAME: PCWSTR = w!("Local\\SageThumbs2K.DlgSel.Section");
const EVENT_NAME: PCWSTR = w!("Local\\SageThumbs2K.DlgSel.Done");
const REQUEST_MESSAGE_NAME: PCWSTR = w!("SageThumbs2K.DlgSel.Request");
const PATH_CAP: usize = 1024;
const MAGIC: u32 = 0x4432_5453; // "ST2D"
const STATE_REQUESTED: u32 = 1;
// The hook's own intermediate and failure states; the requester only writes REQUESTED and
// reads DONE, so these exist here to pin the wire protocol in the test below.
#[cfg(test)]
const STATE_BUSY: u32 = 2;
const STATE_DONE: u32 = 3;
#[cfg(test)]
const STATE_FAILED: u32 = 4;

/// Serialises requesters in this session: the resident daemon's poll thread and the
/// `--explorer-selection` diagnostic must not arm the same section at once. App-side only;
/// the hook never opens it.
const LOCK_NAME: PCWSTR = w!("Local\\SageThumbs2K.DlgSel.Lock");

/// Mirror of `dlghook`'s `Slot`. `#[repr(C)]`; never reorder a field without changing both.
#[repr(C)]
struct Slot {
    magic: u32,
    state: u32,
    dialog: u64,
    nonce: u64,
    ack: u64,
    len: u32,
    path: [u16; PATH_CAP],
}

/// How long to wait for the hook to answer. The work on the far side is three COM calls on
/// an already-live object, so this is a hang budget, not a latency budget.
const WAIT_MS: u32 = 700;

/// How long to wait for another requester to finish before giving up on this request.
const LOCK_WAIT_MS: u32 = 1500;

/// Whether `hwnd` is a Vista+ common file dialog — a `#32770` hosting the shell's view. Cheap
/// (two user32 calls, no messages), because the Space hook calls this on every keypress.
pub(crate) unsafe fn is_file_dialog(hwnd: HWND) -> bool {
    class_name(hwnd) == "#32770"
        && FindWindowExW(Some(hwnd), None, w!("DUIViewWndClassName"), PCWSTR::null()).is_ok()
}

/// The full path of the item selected in the foreground Open/Save dialog, or `None`.
///
/// `None` covers every failure the same way: not a dialog, a 32-bit host we cannot reach, a
/// dialog we are not allowed to hook, no `st2k_dlghook.dll` beside the exe, another request
/// still in flight, a handshake object that already existed, or no answer inside
/// [`WAIT_MS`]. Callers treat all of them as "nothing is selected".
pub(crate) unsafe fn dialog_selection(fg: HWND) -> Option<String> {
    if !is_file_dialog(fg) {
        return None;
    }
    if !same_bitness(fg) {
        return None; // a 32-bit host cannot load our 64-bit hook; there is no x86 build
    }
    let tid = GetWindowThreadProcessId(fg, None);
    if tid == 0 {
        return None;
    }
    let request = request_message()?;
    with_user_only_dacl(|sa| {
        let _lock = Lock::acquire(sa)?;
        let mut shared = Shared::create(sa)?;
        shared.arm(fg);

        let dll = hook_dll_path()?;
        let module = Module::load(&dll)?;
        let proc_addr = GetProcAddress(module.0, s!("st2k_dlg_hook"))?;
        // SAFETY: the export is declared `extern "system" fn(i32, WPARAM, LPARAM) -> LRESULT`,
        // which is exactly `HOOKPROC`'s shape.
        let hook_proc: HOOKPROC = Some(core::mem::transmute::<
            unsafe extern "system" fn() -> isize,
            unsafe extern "system" fn(
                i32,
                windows::Win32::Foundation::WPARAM,
                windows::Win32::Foundation::LPARAM,
            ) -> windows::Win32::Foundation::LRESULT,
        >(proc_addr));

        let hook = Hook::install(hook_proc, module.0, tid)?;
        // A WH_CALLWNDPROC hook only fires on a message SENT to that thread, and the hook
        // serves only on this registered message sent to the armed dialog. SMTO_ABORTIFHUNG
        // keeps a wedged dialog from parking us here.
        let mut result = 0usize;
        let _ = SendMessageTimeoutW(
            fg,
            request,
            WPARAM(0),
            LPARAM(0),
            SMTO_ABORTIFHUNG,
            WAIT_MS,
            Some(&mut result),
        );
        let _ = WaitForSingleObject(shared.event, WAIT_MS);
        drop(hook); // unhook BEFORE reading, so the DLL stops running in the host either way
        let answer = shared.read();
        drop(module);
        answer
    })
}

/// The registered id of the message that makes the hook run, or `None` if user32 refuses
/// to register it.
unsafe fn request_message() -> Option<u32> {
    let id = RegisterWindowMessageW(REQUEST_MESSAGE_NAME);
    (id != 0).then_some(id)
}

/// A fresh per-request value. The hook copies it back into `ack`; `read` accepts nothing
/// that does not carry it. Never zero, which is what a freshly created section holds.
fn fresh_nonce() -> u64 {
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u32(std::process::id());
    h.finish().max(1)
}

/// Whether the window's process runs the same machine architecture as us. A 32-bit process
/// cannot load our 64-bit hook DLL, and this project deliberately ships no x86 build (see
/// docs/TODO.md §7), so those dialogs are simply out of scope rather than half-supported.
/// On ARM64 an x64 process under emulation reports `IMAGE_FILE_MACHINE_AMD64`, so an x64
/// build of the app matches it and an ARM64 build does not.
unsafe fn same_bitness(hwnd: HWND) -> bool {
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == 0 {
        return false;
    }
    let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
        return false; // elevated or otherwise out of reach — fail closed
    };
    let target = machine_of(h);
    let _ = CloseHandle(h);
    match (target, machine_of(GetCurrentProcess())) {
        (Some(target), Some(own)) => target == own,
        _ => false,
    }
}

/// The architecture a process's code runs as: the emulated one under WOW64, the host's
/// native one otherwise. `None` when the query fails.
unsafe fn machine_of(process: HANDLE) -> Option<IMAGE_FILE_MACHINE> {
    let mut process_machine = IMAGE_FILE_MACHINE_UNKNOWN;
    let mut native_machine = IMAGE_FILE_MACHINE_UNKNOWN;
    IsWow64Process2(process, &mut process_machine, Some(&mut native_machine)).ok()?;
    let machine = if process_machine == IMAGE_FILE_MACHINE_UNKNOWN {
        native_machine
    } else {
        process_machine
    };
    (machine != IMAGE_FILE_MACHINE_UNKNOWN).then_some(machine)
}

/// `st2k_dlghook.dll`, which ships beside the exe. `None` when it is absent — a build or an
/// install without it simply has no dialog support.
fn hook_dll_path() -> Option<std::path::PathBuf> {
    let p = std::env::current_exe()
        .ok()?
        .parent()?
        .join("st2k_dlghook.dll");
    p.exists().then_some(p)
}

/// Ownership of the requester mutex, released on drop.
struct Lock(HANDLE);

impl Lock {
    /// Create or open the mutex and wait for it. `None` when it cannot be created or another
    /// requester holds it past [`LOCK_WAIT_MS`]. An abandoned mutex still counts as acquired:
    /// the previous holder died, and this request starts from a fresh section anyway.
    unsafe fn acquire(sa: &SECURITY_ATTRIBUTES) -> Option<Self> {
        let sa: *const SECURITY_ATTRIBUTES = sa;
        let h = CreateMutexW(Some(sa), false, LOCK_NAME).ok()?;
        let waited = WaitForSingleObject(h, LOCK_WAIT_MS);
        if waited != WAIT_OBJECT_0 && waited != WAIT_ABANDONED {
            let _ = CloseHandle(h);
            return None;
        }
        Some(Self(h))
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(self.0);
            let _ = CloseHandle(self.0);
        }
    }
}

/// The section + event the hook answers through, released together, plus what this request
/// armed so `read` can check the answer against it.
struct Shared {
    mapping: HANDLE,
    view: *mut Slot,
    event: HANDLE,
    dialog: u64,
    nonce: u64,
}

impl Shared {
    /// Create both objects under `sa`. A name that already exists — whoever holds it — is a
    /// failure, not something to open: `CreateFileMappingW`/`CreateEventW` report that
    /// through `ERROR_ALREADY_EXISTS` on an otherwise successful call.
    unsafe fn create(sa: &SECURITY_ATTRIBUTES) -> Option<Self> {
        let sa: *const SECURITY_ATTRIBUTES = sa;
        SetLastError(WIN32_ERROR(0));
        let mapping = CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            Some(sa),
            PAGE_READWRITE,
            0,
            core::mem::size_of::<Slot>() as u32,
            SECTION_NAME,
        )
        .ok()?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = CloseHandle(mapping);
            return None;
        }
        let view = MapViewOfFile(
            mapping,
            FILE_MAP_ALL_ACCESS,
            0,
            0,
            core::mem::size_of::<Slot>(),
        );
        if view.Value.is_null() {
            let _ = CloseHandle(mapping);
            return None;
        }
        SetLastError(WIN32_ERROR(0));
        let mut event = CreateEventW(Some(sa), false, false, EVENT_NAME).ok();
        if event.is_some() && GetLastError() == ERROR_ALREADY_EXISTS {
            if let Some(h) = event.take() {
                let _ = CloseHandle(h);
            }
        }
        let Some(event) = event else {
            let _ = UnmapViewOfFile(view);
            let _ = CloseHandle(mapping);
            return None;
        };
        Some(Self {
            mapping,
            view: view.Value as *mut Slot,
            event,
            dialog: 0,
            nonce: 0,
        })
    }

    /// Publish the request: which dialog, this request's nonce, and "go". The state store
    /// is the last write and is `Release`, so the hook's `Acquire` claim sees the rest.
    unsafe fn arm(&mut self, dialog: HWND) {
        self.dialog = dialog.0 as u64;
        self.nonce = fresh_nonce();
        (*self.view).magic = MAGIC;
        (*self.view).len = 0;
        (*self.view).ack = 0;
        (*self.view).dialog = self.dialog;
        (*self.view).nonce = self.nonce;
        let state = AtomicU32::from_ptr(&raw mut (*self.view).state);
        state.store(STATE_REQUESTED, Ordering::Release);
    }

    /// The answer, if the hook wrote one for THIS request: state `DONE`, the magic intact,
    /// the dialog still the one armed, and the nonce echoed back.
    unsafe fn read(&self) -> Option<String> {
        let state = AtomicU32::from_ptr(&raw mut (*self.view).state);
        if state.load(Ordering::Acquire) != STATE_DONE {
            return None;
        }
        if (*self.view).magic != MAGIC
            || (*self.view).dialog != self.dialog
            || (*self.view).ack != self.nonce
        {
            return None;
        }
        let n = (*self.view).len as usize;
        if n == 0 || n > PATH_CAP {
            return None;
        }
        // Read through a raw pointer deliberately: the section is shared with another
        // process, so a `&[u16; N]` would assert an aliasing guarantee we cannot make.
        let src = (&raw const (*self.view).path).cast::<u16>();
        let mut buf = [0u16; PATH_CAP];
        core::ptr::copy_nonoverlapping(src, buf.as_mut_ptr(), n);
        let s = String::from_utf16_lossy(&buf[..n]);
        (!s.is_empty()).then_some(s)
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        unsafe {
            let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.view as *mut c_void,
            });
            let _ = CloseHandle(self.mapping);
            let _ = CloseHandle(self.event);
        }
    }
}

/// A loaded module, freed on drop.
struct Module(HMODULE);
impl Module {
    unsafe fn load(path: &std::path::Path) -> Option<Self> {
        let wide = crate::win::wide(&path.to_string_lossy());
        LoadLibraryW(PCWSTR(wide.as_ptr())).ok().map(Self)
    }
}
impl Drop for Module {
    fn drop(&mut self) {
        unsafe {
            let _ = FreeLibrary(self.0);
        }
    }
}

/// An installed hook, removed on drop — including on every early return above, which is why
/// our DLL never outlives the one question it was loaded to answer.
struct Hook(HHOOK);
impl Hook {
    unsafe fn install(proc: HOOKPROC, module: HMODULE, tid: u32) -> Option<Self> {
        SetWindowsHookExW(
            WH_CALLWNDPROC,
            proc,
            Some(windows::Win32::Foundation::HINSTANCE(module.0)),
            tid,
        )
        .ok()
        .map(Self)
    }
}
impl Drop for Hook {
    fn drop(&mut self) {
        unsafe {
            let _ = UnhookWindowsHookEx(self.0);
        }
    }
}

/// Silences an unused-import warning in builds where the hook type alias resolves without it.
#[allow(dead_code)]
unsafe fn _keep_callnexthookex_linked() {
    let _ = CallNextHookEx;
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Security::SECURITY_DESCRIPTOR;

    /// The hook DLL mirrors this layout by hand (`crates/dlghook/src/lib.rs`); its test pins
    /// the same numbers, so a change on one side fails one of the two.
    #[test]
    fn slot_layout_and_constants_match_the_hook_side() {
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
    }

    /// A nonce is never the zero a fresh section holds, and two requests do not share one.
    #[test]
    fn fresh_nonce_is_nonzero_and_varies() {
        let a = fresh_nonce();
        let b = fresh_nonce();
        assert_ne!(a, 0);
        assert_ne!(b, 0);
        assert_ne!(a, b);
    }

    /// The user-only DACL builds on this machine and yields attributes that name a
    /// descriptor with a DACL present.
    #[test]
    fn user_only_dacl_builds() {
        let ok = unsafe {
            with_user_only_dacl(|sa| {
                assert_eq!(
                    sa.nLength as usize,
                    core::mem::size_of::<SECURITY_ATTRIBUTES>()
                );
                assert!(!sa.lpSecurityDescriptor.is_null());
                let sd = &*(sa.lpSecurityDescriptor as *const SECURITY_DESCRIPTOR);
                assert!(
                    !sd.Dacl.is_null(),
                    "the DACL must be present, not NULL (= everyone)"
                );
                assert_eq!((*sd.Dacl).AceCount, 1);
                Some(true)
            })
        };
        assert_eq!(ok, Some(true));
    }
}
