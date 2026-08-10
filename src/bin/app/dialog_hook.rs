//! Read the selection out of a common **Open/Save dialog** — the requester half of the
//! `st2k_dlghook.dll` handshake.
//!
//! A file dialog is not a shell view we can reach: it is absent from `IShellWindows`, and
//! every cross-process route to its selection was probed and refused (the table is in
//! `dlghook/src/lib.rs`, which is where the WHY lives — do not duplicate it here). The one
//! route in is `WM_USER + 7`, whose `IShellBrowser` is apartment-bound, so the read must
//! happen on the dialog's OWN thread. This module arms a shared handshake block, gets
//! `st2k_dlghook.dll` loaded onto that thread with a `WH_CALLWNDPROC` hook, pokes the dialog
//! so the hook runs once, and reads the answer back.
//!
//! Everything here is FAIL-CLOSED. A missing DLL, a bitness mismatch, an elevated dialog, a
//! hook Windows refuses to install, a timeout — every one of them returns `None`, and Space
//! then stays a space exactly as it did before this module existed. It never guesses.

use core::ffi::c_void;

use windows::core::{s, w, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, FreeLibrary, HANDLE, HMODULE, HWND, INVALID_HANDLE_VALUE, LPARAM, WPARAM,
};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_ALL_ACCESS, PAGE_READWRITE,
};
use windows::Win32::System::Threading::{
    CreateEventW, OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::System::Threading::{GetCurrentProcess, IsWow64Process};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, FindWindowExW, GetWindowThreadProcessId, SendMessageTimeoutW,
    SetWindowsHookExW, UnhookWindowsHookEx, HHOOK, HOOKPROC, SMTO_ABORTIFHUNG, WH_CALLWNDPROC,
    WM_NULL,
};

use crate::explorer_selection::class_name;

/// Kept in step BY HAND with `dlghook/src/lib.rs` — the hook DLL shares no code with the app
/// on purpose, so that nothing in the app's dependency graph can reach a binary that gets
/// loaded into other people's processes.
const SECTION_NAME: PCWSTR = w!("Local\\SageThumbs2K.DlgSel.Section");
const EVENT_NAME: PCWSTR = w!("Local\\SageThumbs2K.DlgSel.Done");
const PATH_CAP: usize = 1024;
const MAGIC: u32 = 0x4432_5453; // "ST2D"
const STATE_REQUESTED: u32 = 1;
const STATE_DONE: u32 = 3;

/// Mirror of `dlghook`'s `Slot`. `#[repr(C)]`; never reorder a field without changing both.
#[repr(C)]
struct Slot {
    magic: u32,
    state: u32,
    dialog: u64,
    len: u32,
    path: [u16; PATH_CAP],
}

/// How long to wait for the hook to answer. The work on the far side is three COM calls on
/// an already-live object, so this is a hang budget, not a latency budget.
const WAIT_MS: u32 = 700;

/// Whether `hwnd` is a Vista+ common file dialog — a `#32770` hosting the shell's view. Cheap
/// (two user32 calls, no messages), because the Space hook calls this on every keypress.
pub(crate) unsafe fn is_file_dialog(hwnd: HWND) -> bool {
    class_name(hwnd) == "#32770"
        && FindWindowExW(Some(hwnd), None, w!("DUIViewWndClassName"), PCWSTR::null()).is_ok()
}

/// The full path of the item selected in the foreground Open/Save dialog, or `None`.
///
/// `None` covers every failure the same way: not a dialog, a 32-bit host we cannot reach, a
/// dialog we are not allowed to hook, no `st2k_dlghook.dll` beside the exe, or no answer
/// inside [`WAIT_MS`]. Callers treat all of them as "nothing is selected".
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

    let shared = Shared::create()?;
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
    // A WH_CALLWNDPROC hook only fires on a message SENT to that thread, so send a harmless
    // one. SMTO_ABORTIFHUNG keeps a wedged dialog from parking us here.
    let mut result = 0usize;
    let _ = SendMessageTimeoutW(
        fg,
        WM_NULL,
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
}

/// Whether the window's process has the same bitness as us. A 32-bit process cannot load our
/// 64-bit hook DLL, and this project deliberately ships no x86 build (see docs/TODO.md §7),
/// so those dialogs are simply out of scope rather than half-supported.
unsafe fn same_bitness(hwnd: HWND) -> bool {
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == 0 {
        return false;
    }
    let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
        return false; // elevated or otherwise out of reach — fail closed
    };
    let mut target_wow = false.into();
    let mut self_wow = false.into();
    let ok = IsWow64Process(h, &mut target_wow).is_ok()
        && IsWow64Process(GetCurrentProcess(), &mut self_wow).is_ok();
    let _ = CloseHandle(h);
    ok && target_wow.as_bool() == self_wow.as_bool()
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

/// The section + event the hook answers through, released together.
struct Shared {
    mapping: HANDLE,
    view: *mut Slot,
    event: HANDLE,
}

impl Shared {
    unsafe fn create() -> Option<Self> {
        let mapping = CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            None,
            PAGE_READWRITE,
            0,
            core::mem::size_of::<Slot>() as u32,
            SECTION_NAME,
        )
        .ok()?;
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
        let Ok(event) = CreateEventW(None, false, false, EVENT_NAME) else {
            let _ = UnmapViewOfFile(view);
            let _ = CloseHandle(mapping);
            return None;
        };
        Some(Self {
            mapping,
            view: view.Value as *mut Slot,
            event,
        })
    }

    /// Publish the request: which dialog, and "go".
    unsafe fn arm(&self, dialog: HWND) {
        (*self.view).magic = MAGIC;
        (*self.view).len = 0;
        (*self.view).dialog = dialog.0 as u64;
        (*self.view).state = STATE_REQUESTED;
    }

    /// The answer, if the hook wrote one.
    unsafe fn read(&self) -> Option<String> {
        if (*self.view).state != STATE_DONE {
            return None;
        }
        let n = (*self.view).len as usize;
        if n == 0 || n > PATH_CAP {
            return None;
        }
        // Read through a raw pointer deliberately: the section is shared with another
        // process, so an implicit `&[u16; N]` would assert an aliasing guarantee we cannot make.
        let mut buf = [0u16; PATH_CAP];
        core::ptr::copy_nonoverlapping((*self.view).path.as_ptr(), buf.as_mut_ptr(), n);
        let s = String::from_utf16_lossy(&buf[..n]);
        (!s.is_empty()).then_some(s)
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        unsafe {
            let _ = UnmapViewOfFile(windows::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
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
