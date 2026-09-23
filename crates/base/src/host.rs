//! The DLL's own module: its live-reference count and handle, its path and the files beside
//! it, and the small helpers everything that spawns a process or names a Win32 string shares.
//! The bottom layer of the library, so every part of it can reach these without reaching up
//! into the COM entry points in `lib.rs`.

use core::ffi::c_void;
use std::sync::atomic::{AtomicI64, AtomicIsize, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use windows::core::{Error, PWSTR};
use windows::Win32::Foundation::{E_FAIL, E_OUTOFMEMORY, HMODULE};
use windows::Win32::System::Com::CoTaskMemAlloc;
use windows::Win32::System::LibraryLoader::GetModuleFileNameW;

/// Live-object + lock count. `DllCanUnloadNow` returns S_OK only at zero.
static MODULE_REFS: AtomicI64 = AtomicI64::new(0);
/// This DLL's HMODULE, captured in DllMain, used to resolve our own path.
static HMODULE_PTR: AtomicIsize = AtomicIsize::new(0);

/// Uptime at the most recent add-ref, in ms: the last time anyone asked this host for an
/// object. Read by [`dll_can_unload_now`]'s wedged-host exit, which must never fire while
/// the host is still being used.
static LAST_ADD_REF_MS: AtomicU64 = AtomicU64::new(0);

/// Time since this module first counted a reference (a monotonic clock that needs no
/// system-time assumptions, for the idle measurement above).
fn uptime() -> Duration {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed()
}

pub fn dll_add_ref() {
    MODULE_REFS.fetch_add(1, Ordering::SeqCst);
    LAST_ADD_REF_MS.store(uptime().as_millis() as u64, Ordering::Relaxed);
}

pub fn dll_release() {
    // Clamp at zero: a stray/unbalanced release must NOT push the count negative,
    // or it could cancel a live object's reference and let the DLL unload while in
    // use. `fetch_update` leaves a zero count untouched and only ever decrements.
    let prev = MODULE_REFS.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
        if n > 0 {
            Some(n - 1)
        } else {
            None // already zero — refuse to underflow
        }
    });
    debug_assert!(
        prev.is_ok(),
        "MODULE_REFS underflow: unbalanced LockServer(FALSE)/release"
    );
}

/// Record this DLL's `HMODULE` (DllMain, on process attach).
pub fn set_module(hmodule: isize) {
    HMODULE_PTR.store(hmodule, Ordering::SeqCst);
}

/// Live objects and locks: `DllCanUnloadNow` may say yes only at zero.
pub fn live_refs() -> i64 {
    MODULE_REFS.load(Ordering::SeqCst)
}

/// How long since anyone last asked this host for an object.
pub fn idle_for() -> Duration {
    uptime().saturating_sub(Duration::from_millis(
        LAST_ADD_REF_MS.load(Ordering::Relaxed),
    ))
}

/// RAII module-reference guard. Constructing one (via `Default`) bumps the
/// live-object count; dropping it releases. Each COM coclass carries a
/// `_ref: ModuleRef` field instead of hand-writing an add-ref in its
/// constructor and a matching `impl Drop` — six identical pairs collapse to
/// this one type. (The factory's `LockServer` count is a separate add/release
/// path and intentionally does NOT use this.)
pub struct ModuleRef;

impl Default for ModuleRef {
    fn default() -> Self {
        dll_add_ref();
        ModuleRef
    }
}

impl Drop for ModuleRef {
    fn drop(&mut self) {
        dll_release();
    }
}

/// `CREATE_NO_WINDOW` process-creation flag. Every helper we spawn from a GUI/shell
/// host (magick, st2k, self) passes it so no console window flashes. Defined here
/// once — a mistyped copy (`0x0080_0000`) would pop a console inside Explorer. (The
/// `windows` crate's `Threading` feature IS enabled now — for `CreateMutexW` — but
/// `std::process::CommandExt::creation_flags` wants a bare `u32` anyway.)
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// NUL-terminated UTF-16 for Win32 `*W` APIs. Was independently re-typed as
/// `s.encode_utf16().chain(once(0)).collect()` across half a dozen files (command /
/// contextmenu / propstore / actions / cli / container::select) — one shared helper
/// means the pattern can't drift and reads as intent at the call site. (The app bin
/// has its own `win::wide` twin; bins keep using that one.)
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(core::iter::once(0)).collect()
}

/// File names of the artifacts this crate builds, all installed side-by-side. The
/// co-located layout is an install contract; keeping the names here means a rename
/// is one edit and a typo can't silently break a spawn/icon lookup with no error.
pub const APP_EXE: &str = "SageThumbs2K.exe";
pub const CLI_EXE: &str = "st2k.exe";

/// Resolve a sibling file next to OUR DLL (whatever directory the install used).
/// Uses [`module_path`] — NEVER `current_exe()`, which inside the shell host is
/// `explorer.exe`/`dllhost.exe`. Returns the path only if it actually exists, so a
/// DLL-only install (no companion EXE) cleanly yields `None`.
pub fn sibling_of_dll(name: &str) -> Option<std::path::PathBuf> {
    let dll = module_path().ok()?;
    let p = std::path::Path::new(&dll).parent()?.join(name);
    p.exists().then_some(p)
}

/// This DLL's `HMODULE` (captured in `DllMain`), for use as the `hInstance` of
/// windows/classes we create — e.g. the preview handler's child window.
pub fn dll_hmodule() -> HMODULE {
    HMODULE(HMODULE_PTR.load(Ordering::SeqCst) as *mut c_void)
}

pub fn module_path() -> windows::core::Result<String> {
    unsafe {
        let h = HMODULE(HMODULE_PTR.load(Ordering::SeqCst) as *mut c_void);
        let mut buf = vec![0u16; 260];
        loop {
            let n = GetModuleFileNameW(Some(h), &mut buf) as usize;
            if n == 0 {
                return Err(Error::from_thread()); // GetLastError; includes HMODULE-missing
            }
            // n < len is the documented "it fit" signal; n == len means truncated.
            if n < buf.len() {
                return Ok(String::from_utf16_lossy(&buf[..n]));
            }
            if buf.len() >= 32_768 {
                return Err(Error::from(E_FAIL));
            }
            buf.resize((buf.len() * 2).min(32_768), 0);
        }
    }
}

/// Best-effort backing file name/path of a shell-supplied `IStream` (via `IStream::Stat`,
/// which fills `pwcsName` under `STATFLAG_DEFAULT`).
///
/// Shared by the preview handler (which logs it, so a "white preview" report names the exact
/// file) and the thumbnail provider (which needs the extension for the optional format
/// badge). `pwcsName` is a CoTaskMem allocation we own and must free.
///
/// # Safety
/// `stream` must be a live COM `IStream` on a thread where COM is initialised.
pub unsafe fn stream_name(stream: &windows::Win32::System::Com::IStream) -> Option<String> {
    use windows::Win32::System::Com::{STATFLAG_DEFAULT, STATSTG};
    let mut stat = STATSTG::default();
    stream.Stat(&mut stat, STATFLAG_DEFAULT).ok()?;
    if stat.pwcsName.is_null() {
        return None;
    }
    let s = stat.pwcsName.to_string().ok();
    windows::Win32::System::Com::CoTaskMemFree(Some(stat.pwcsName.0 as *const core::ffi::c_void));
    s
}

/// Overflow-safe UTF-16 byte length (`len * size_of::<u16>()`, checked). Shared by
/// `alloc_pwstr` and `propstore::pv_lpwstr` so every wide-string builder rejects an overflowing allocation
/// size rather than wrapping into an under-sized `CoTaskMemAlloc`.
pub fn checked_utf16_byte_len(len: usize) -> Option<usize> {
    len.checked_mul(2)
}

/// Allocate a NUL-terminated wide string with CoTaskMemAlloc; the shell frees it.
///
/// The single implementation of the wide-string allocation idiom, shared by the
/// context-menu verbs (`command`) and `propstore::pv_lpwstr` (which maps the allocation failure
/// to an empty variant instead of `E_OUTOFMEMORY`).
pub fn alloc_pwstr(s: &str) -> windows::core::Result<PWSTR> {
    let wide = wide(s);
    // Overflow-safe byte count (len * size_of::<u16>()); can't actually overflow for
    // any real string, but keep the allocation provably sound rather than wrapping.
    let bytes = checked_utf16_byte_len(wide.len()).ok_or_else(|| Error::from(E_OUTOFMEMORY))?;
    let p = unsafe { CoTaskMemAlloc(bytes) } as *mut u16;
    if p.is_null() {
        return Err(Error::from(E_OUTOFMEMORY));
    }
    unsafe { std::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len()) };
    Ok(PWSTR(p))
}
