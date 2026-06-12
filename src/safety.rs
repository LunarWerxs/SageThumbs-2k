//! Crash-safety boundary.
//!
//! Unwinding a Rust panic across the COM ABI (an `extern "system"`
//! non-unwinding boundary) is undefined behavior, and windows-rs's
//! `#[implement]` macro does NOT wrap method bodies for us. So every COM
//! method funnels through one of these guards. In release we also build
//! with `panic = "abort"` (see Cargo.toml), which makes a panic terminate
//! the disposable surrogate host instead of corrupting Explorer.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::OnceLock;
use windows::core::{Error, Result, HRESULT};
use windows::Win32::Foundation::E_FAIL;
use windows_registry::CURRENT_USER;

/// Wrap a COM method body that returns a raw `HRESULT`.
pub fn guard_hr<F: FnOnce() -> HRESULT>(f: F) -> HRESULT {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(hr) => hr,
        Err(_) => {
            log("panic crossed a COM boundary -> E_FAIL");
            E_FAIL
        }
    }
}

/// Wrap a COM method body that returns `windows::core::Result<()>`.
pub fn guard<F: FnOnce() -> Result<()>>(f: F) -> Result<()> {
    guard_val(f)
}

/// Wrap a COM method body that returns `windows::core::Result<T>`.
pub fn guard_val<T, F: FnOnce() -> Result<T>>(f: F) -> Result<T> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(_) => {
            log("panic crossed a COM boundary -> E_FAIL");
            Err(Error::from(E_FAIL))
        }
    }
}

/// Opt-in verbose logging. Set `HKCU\Software\SageThumbs2K\Debug = 1` (DWORD)
/// to trace Initialize/GetThumbnail calls; off by default so production is
/// silent. Read once and cached. `dev-register.ps1 -Debug` sets the flag.
pub fn log_debug(msg: &str) {
    static DEBUG_ON: OnceLock<bool> = OnceLock::new();
    let on = *DEBUG_ON.get_or_init(|| {
        CURRENT_USER
            .open(r"Software\SageThumbs2K")
            .and_then(|k| k.get_u32("Debug"))
            .map(|v| v == 1)
            .unwrap_or(false)
    });
    if on {
        log(msg);
    }
}

/// Append a line to `%LOCALAPPDATA%\SageThumbs2K.log`. Handlers run inside
/// `dllhost.exe`, so there is no console — a file is the only sink.
pub fn log(msg: &str) {
    use std::io::Write;
    if let Ok(dir) = std::env::var("LOCALAPPDATA") {
        let path = std::path::Path::new(&dir).join("SageThumbs2K.log");
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{msg}");
        }
    }
}
