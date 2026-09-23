//! The diagnostics log: the debug switch, the rotating file, the session header and the panic hook that writes to it.

use super::*;

/// Opt-in verbose logging. Set `HKCU\Software\SageThumbs2K\Debug = 1` (DWORD)
/// to trace Initialize/GetThumbnail calls; off by default so production is
/// silent. `dev-register.ps1 -Debug` sets the flag.
///
/// Read with a short TTL rather than cached forever: settings' documented
/// contract is that toggles take effect immediately for new requests, so a live
/// `-Debug` flip must work WITHOUT restarting the Explorer/dllhost surrogate.
/// A blanket `OnceLock` cache violated that (the first read won forever). We
/// re-read the registry at most every `DEBUG_TTL_MS`, so a toggle is honored
/// within that window while a busy log loop still avoids a registry hit per line.
///
/// Callers that would `format!` a message first should use [`log_debugf!`] instead, which
/// only formats when the flag is on; this function is for messages that already exist.
pub fn log_debug(msg: &str) {
    if debug_logging_on() {
        log(msg);
    }
}

/// Whether `HKCU\Software\SageThumbs2K\Debug = 1` is set, cached for `DEBUG_TTL_MS`.
/// Shared by [`log_debug`] and [`log_debugf!`]; see [`log_debug`] for why it is a TTL and
/// not a one-time read.
pub fn debug_logging_on() -> bool {
    const DEBUG_TTL_MS: u64 = 1000;
    // Packed: high 63 bits = elapsed-ms timestamp of the last probe, low bit = on.
    // 0 means "never probed". Relaxed is fine: a stale read just costs one extra
    // registry probe or one extra/skipped line around a toggle — never UB.
    static CACHE: AtomicU64 = AtomicU64::new(0);

    let now_ms = elapsed_ms();
    let packed = CACHE.load(Ordering::Relaxed);
    let last_ms = packed >> 1;
    if packed == 0 || now_ms.wrapping_sub(last_ms) >= DEBUG_TTL_MS {
        let fresh = CURRENT_USER
            .open(crate::settings::ROOT)
            .and_then(|k| k.get_u32("Debug"))
            .map(|v| v == 1)
            .unwrap_or(false);
        CACHE.store((now_ms << 1) | (fresh as u64), Ordering::Relaxed);
        fresh
    } else {
        packed & 1 != 0
    }
}

/// Append a line to `%LOCALAPPDATA%\SageThumbs2K.log`. Handlers run inside
/// `dllhost.exe`, so there is no console — a file is the only sink.
///
/// Each line is prefixed with the process id and a millisecond elapsed counter
/// so the interleaved logs of Explorer, its throwaway `dllhost` surrogates, and
/// our helper EXEs (which all append to this one file) can be told apart and
/// time-ordered when read back.
pub fn log(msg: &str) {
    use std::io::Write;
    let Some(path) = log_file() else { return };
    maybe_rotate(&path);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(f, "[pid {} +{}ms] {msg}", std::process::id(), elapsed_ms());
    }
}

/// Always-on error logging — for genuine failures (a crash, a COM boundary panic, a
/// thumbnail that couldn't be produced), NOT the verbose `log_debug` traces. Prefixed
/// `ERROR` so a user-sent log is greppable.
pub fn log_error(msg: &str) {
    log(&format!("ERROR {msg}"));
}

/// The diagnostics log path (`%LOCALAPPDATA%\SageThumbs2K.log`), or None if
/// `LOCALAPPDATA` is unset. Public so the Options dialog's "Open log" button can
/// reveal it for the user to send in.
pub fn log_file() -> Option<std::path::PathBuf> {
    std::env::var("LOCALAPPDATA")
        .ok()
        .map(|d| std::path::Path::new(&d).join("SageThumbs2K.log"))
}

/// Cap the diagnostics log at ~1 MiB. Past that, best-effort + throttled (~every 64
/// writes): rename the current file to `SageThumbs2K.log.old` (one backup) so it can
/// never grow unbounded.
///
/// Accepted, documented race: the every-64-writes throttle is a per-process counter, not
/// coordinated across processes or with a file lock, and Explorer/dllhost/prevhost/our
/// helper EXEs all append to this one path concurrently (see `log`'s doc comment). So a
/// rotation here can race another process's concurrent `OpenOptions::append` — the rename
/// can land between that process's open and its write, silently dropping or truncating the
/// line it was about to append, not merely "skipping one rotation" cleanly. This is
/// acceptable for a best-effort diagnostics log (never fatal, never blocks a thumbnail) but
/// is NOT lock-safe; do not rely on this file for anything that needs a complete trace.
pub(super) fn maybe_rotate(path: &std::path::Path) {
    const LOG_CAP_BYTES: u64 = 1 << 20;
    static N: AtomicU64 = AtomicU64::new(0);
    if !N.fetch_add(1, Ordering::Relaxed).is_multiple_of(64) {
        return;
    }
    if std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) > LOG_CAP_BYTES {
        let _ = std::fs::rename(path, path.with_file_name("SageThumbs2K.log.old"));
    }
}

/// Write a one-line session header (version · artifact · OS build) the first time
/// this process logs, so a user-sent log says which build + Windows it came from.
pub(super) fn log_session_header(artifact: &str) {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        log(&format!(
            "==== SageThumbs2K {} [{artifact}] · {} ====",
            env!("CARGO_PKG_VERSION"),
            os_string()
        ));
    });
}

/// A short Windows version string for the log header, from `HKLM\…\CurrentVersion`.
/// `ProductName` still says "Windows 10" on 11, so promote by build number.
pub fn os_string() -> String {
    use windows_registry::LOCAL_MACHINE;
    let k = LOCAL_MACHINE
        .open(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion")
        .ok();
    let g = |n: &str| {
        k.as_ref()
            .and_then(|k| k.get_string(n).ok())
            .unwrap_or_default()
    };
    let build: u32 = g("CurrentBuild").parse().unwrap_or(0);
    let product = if build >= 22000 {
        "Windows 11".to_string()
    } else {
        g("ProductName")
    };
    format!("{product} {} (build {build})", g("DisplayVersion"))
}

/// Install a process-wide panic hook that writes the panic (message + `file:line`) to
/// the diagnostics log BEFORE the process aborts. The release profile is
/// `panic = "abort"`, so the COM `catch_unwind` guards above never actually run — this
/// hook is the ONLY way a crash leaves a trace. Idempotent (first call wins) and
/// chains to the previous hook. `artifact` tags which binary crashed (dll/app/st2k).
pub fn install_panic_hook(artifact: &'static str) {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        log_session_header(artifact);
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let loc = info
                .location()
                .map(|l| format!("{}:{}", l.file(), l.line()))
                .unwrap_or_else(|| "<unknown>".to_string());
            let msg = info
                .payload()
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| info.payload().downcast_ref::<String>().map(|s| s.as_str()))
                .unwrap_or("<non-string panic payload>");
            log_error(&format!("PANIC [{artifact}] at {loc}: {msg}"));
            prev(info);
        }));
    });
}
