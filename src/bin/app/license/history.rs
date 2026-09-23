//! The licence history file and the lock that serialises its writers.

use super::*;

/// How many times [`HistoryLock::acquire`] retries before giving up, and how long it waits
/// between tries. `LockFile` fails immediately rather than blocking when the region is
/// already held (blocking needs `LockFileEx` plus an `OVERLAPPED`, which this one call
/// site does not otherwise need), so the ~2s budget the old session-local mutex gave
/// callers via `WaitForSingleObject` is reproduced here as bounded polling instead. Short
/// under `cfg(test)` so a test that deliberately holds the lock costs milliseconds, not
/// the production budget.
#[cfg(not(test))]
pub(super) const LOCK_ATTEMPTS: u32 = 20;

#[cfg(test)]
pub(super) const LOCK_ATTEMPTS: u32 = 6;

#[cfg(not(test))]
pub(super) const LOCK_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(100);

#[cfg(test)]
pub(super) const LOCK_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(25);

/// Serialises the breadcrumb's read-modify-write across every process that touches it, on
/// this MACHINE rather than this logon session (2026-09-05 audit, F18). The lock this
/// replaces (`Local\SageThumbs2K.LicenceHistory`) named a kernel object scoped to the
/// caller's session, so two Windows sessions on one box - two users, or RDP layered over
/// the console - each got their OWN mutex and could both believe they held exclusive
/// access to the SAME shared ProgramData file, the second write silently discarding the
/// first's. A file lock has no session namespace: `LockFile` contends on the file itself,
/// and a file has exactly one identity no matter which session opened it.
pub(super) struct HistoryLock {
    pub(super) file: std::fs::File,
}

impl HistoryLock {
    /// Open (creating if needed) the lock file beside the breadcrumb and take an exclusive
    /// whole-file lock on it, retrying up to [`LOCK_ATTEMPTS`] times. `None` once that
    /// budget is spent - the caller's contract is to write NOTHING in that case (see
    /// [`update_history_at`]), never to fall back to writing unlocked the way the old
    /// mutex path did on a timed-out wait.
    pub(super) fn acquire(lock_path: &std::path::Path) -> Option<Self> {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::Storage::FileSystem::LockFile;

        if let Some(dir) = lock_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            // The lock file carries no content that matters - it exists only to be
            // locked - but a truncate on every acquire would still be a pointless write
            // to a file another process might be mid-open on. Never truncate it.
            .truncate(false)
            .open(lock_path)
            .ok()?;
        let handle = HANDLE(file.as_raw_handle());
        for attempt in 0..LOCK_ATTEMPTS {
            // SAFETY: `file` outlives this call and is kept alive inside the returned
            // `HistoryLock` for as long as the lock must hold; a whole-file range (offset
            // 0, length u32::MAX/u32::MAX) is the standard Windows idiom for "lock the
            // file" regardless of its actual length.
            let locked = unsafe { LockFile(handle, 0, 0, u32::MAX, u32::MAX) }.is_ok();
            if locked {
                return Some(HistoryLock { file });
            }
            if attempt + 1 < LOCK_ATTEMPTS {
                std::thread::sleep(LOCK_RETRY_DELAY);
            }
        }
        None
    }
}

impl Drop for HistoryLock {
    fn drop(&mut self) {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::Storage::FileSystem::UnlockFile;

        let handle = HANDLE(self.file.as_raw_handle());
        // SAFETY: the same handle and region locked in `acquire`; unlocking a region this
        // handle does not hold is a documented failure return, never undefined behaviour.
        unsafe {
            let _ = UnlockFile(handle, 0, 0, u32::MAX, u32::MAX);
        }
    }
}

/// Read-modify-write the breadcrumb through one closure, so every network/decision
/// function below shares one place that knows how to load-or-default and save. Silent
/// on a missing `%ProgramData%` (portable / hand-deleted, same as [`write_history`]'s
/// own fail-open) - a machine that can't remember this reminder still isn't broken.
pub(super) fn update_history(mutate: impl FnOnce(&mut History)) {
    let Some(path) = history_path() else {
        return;
    };
    update_history_at(&path, mutate);
}

/// The testable core of [`update_history`]: an explicit path, and a return value saying
/// whether the update actually happened, so a test can tell "lost the lock race" apart
/// from "wrote, and it happened to be a no-op".
///
/// 2026-09-05 audit, F18: no lock now means no write, not a write proceeding unlocked. The
/// old code called `HistoryLock::acquire`, ignored a `None`, and wrote anyway - which is
/// exactly how two sessions each racing their own separate `Local\` mutex could both
/// believe they held exclusive access and clobber each other's write. Losing one update (a
/// stale nag count, a downgrade notice shown once more than it should be) is a cosmetic
/// cost; silently replacing a newer write from another session is the bug this function
/// now refuses to reproduce.
pub(super) fn update_history_at(path: &std::path::Path, mutate: impl FnOnce(&mut History)) -> bool {
    let Some(_lock) = HistoryLock::acquire(&lock_path(path)) else {
        st2k_base::safety::log_debug(
            "license: history lock unavailable after retrying, skipping this update rather than writing over a possibly newer file",
        );
        return false;
    };
    let mut h = read_history(path).unwrap_or_default();
    mutate(&mut h);
    write_history(path, &h)
}

/// Where the machine-wide lock lives: a sibling of the breadcrumb, never the breadcrumb
/// file itself, so taking the lock can never race the JSON file's own atomic replace.
pub(super) fn lock_path(path: &std::path::Path) -> std::path::PathBuf {
    path.with_extension("lock")
}
