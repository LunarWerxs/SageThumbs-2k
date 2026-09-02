//! Shared helpers for the integration tests under `tests/`. Each test file that needs one
//! adds `mod common;` and calls through `common::` — not every helper here is used by every
//! test file (each integration test is its own binary crate), hence the blanket allow below
//! rather than one per unused item per file.
#![cfg(windows)]
#![allow(dead_code)]

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;

/// The built cdylib sits one directory above the test exe
/// (`target/<profile>/sagethumbs2k.dll` vs `target/<profile>/deps/<test>.exe`).
pub fn dll_path() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    exe.parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("sagethumbs2k.dll")
}

/// UTF-16, NUL-terminated — the shape every `PCWSTR`-taking Win32 call in these tests needs.
pub fn to_wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// Set a process environment variable for a test. `std::env::set_var` carries an `unsafe`
/// signature (mutating process-global state is unsound to race against a read on another
/// thread), so every integration test routes through this ONE wrapper instead of
/// re-acknowledging that soundness contract, inconsistently, at each call site.
///
/// # Safety
/// The caller must ensure no other thread is reading or writing the process environment
/// concurrently with this call — in practice: call it before spawning any thread that reads
/// settings, and before the first settings read this process performs (the DLL's own
/// `OnceLock`-cached reads included).
pub unsafe fn set_test_env(key: &str, value: impl AsRef<OsStr>) {
    // SAFETY: forwarded to the caller's own obligation, documented above.
    unsafe { std::env::set_var(key, value) };
}

/// Remove a process environment variable for a test — the `remove_var` counterpart to
/// [`set_test_env`], with the identical soundness obligation.
///
/// # Safety
/// The caller must ensure no other thread is reading or writing the process environment
/// concurrently with this call.
pub unsafe fn remove_test_env(key: &str) {
    // SAFETY: forwarded to the caller's own obligation, documented above.
    unsafe { std::env::remove_var(key) };
}
