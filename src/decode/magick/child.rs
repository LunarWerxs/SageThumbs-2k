//! Watching a magick child: CPU time, bounded output drains and the reap.

use super::*;

/// Kill a magick child unconditionally, join its stdin-writer/stdout-reader threads and
/// reap it, returning its exit status and whatever its stderr drain collected.
pub(super) fn reap_magick_child(
    child: &mut std::process::Child,
    writer: std::thread::JoinHandle<()>,
    reader: std::thread::JoinHandle<()>,
    errdrain: Option<std::thread::JoinHandle<Vec<u8>>>,
) -> (Option<std::process::ExitStatus>, Vec<u8>) {
    let _ = child.kill();
    let _ = writer.join();
    let _ = reader.join();
    let err = errdrain.and_then(|h| h.join().ok()).unwrap_or_default();
    let status = child.wait().ok();
    (status, err)
}

/// Total CPU time (kernel + user) this child has consumed so far.
///
/// `None` when the OS won't say — the caller then falls back to the wall-clock backstop
/// alone, i.e. exactly the behaviour that predates the CPU budget.
pub(super) fn child_cpu_time(child: &std::process::Child) -> Option<Duration> {
    use std::os::windows::io::AsRawHandle;
    // FILETIME is two 32-bit halves and is only 4-byte aligned, so take it as a pair of
    // u32 and recombine rather than letting the OS write a u64 into a maybe-underaligned
    // slot.
    let (mut creation, mut exit, mut kernel, mut user) =
        ([0u32; 2], [0u32; 2], [0u32; 2], [0u32; 2]);
    let ok = unsafe {
        GetProcessTimes(
            child.as_raw_handle().cast(),
            creation.as_mut_ptr(),
            exit.as_mut_ptr(),
            kernel.as_mut_ptr(),
            user.as_mut_ptr(),
        )
    };
    if ok == 0 {
        return None;
    }
    let ticks = |v: [u32; 2]| (u64::from(v[1]) << 32) | u64::from(v[0]);
    // FILETIME counts 100-nanosecond intervals.
    Some(Duration::from_nanos(
        ticks(kernel)
            .saturating_add(ticks(user))
            .saturating_mul(100),
    ))
}

/// Wait for the child's PNG on `rx`, enforcing a CPU budget with a wall-clock backstop.
///
/// `Err` is the message to log; the caller kills and reaps. Two cases are deliberately NOT
/// failures, and both are why this is a loop instead of one `recv_timeout`:
///
///  * the child is alive but starved — it has burned almost no CPU, so it keeps its budget
///    however long the machine makes it wait;
///  * the child has already EXITED — its stdout is closed, so the pending `read_to_end`
///    returns as soon as that thread is scheduled, and killing at that point would throw
///    away a decode that already succeeded (issue #9 logged this as
///    `decode timed out (status Some(ExitStatus(0)))`).
pub(crate) fn await_magick_output(
    child: &mut std::process::Child,
    rx: &std::sync::mpsc::Receiver<Vec<u8>>,
    cpu_budget: Duration,
    wall_ceiling: Duration,
) -> std::result::Result<Vec<u8>, &'static str> {
    use std::sync::mpsc::RecvTimeoutError;
    let start = std::time::Instant::now();
    loop {
        match rx.recv_timeout(WATCHDOG_SLICE) {
            Ok(buf) => return Ok(buf),
            // The reader thread went away without sending: nothing more is coming. Report
            // it as empty output so the caller's existing `png.is_empty()` check handles it.
            Err(RecvTimeoutError::Disconnected) => return Ok(Vec::new()),
            Err(RecvTimeoutError::Timeout) => {}
        }
        let still_running = !matches!(child.try_wait(), Ok(Some(_)));
        if still_running && child_cpu_time(child).is_some_and(|cpu| cpu > cpu_budget) {
            return Err("decode exceeded its CPU budget");
        }
        if start.elapsed() > wall_ceiling {
            return Err("decode timed out");
        }
    }
}

/// Read a child pipe to EOF but keep at most ~4 KiB so a flood of magick warnings
/// can't balloon our memory; the captured head is plenty to diagnose a failure.
pub(super) fn drain_capped<R: Read>(mut r: R) -> Vec<u8> {
    const CAP: usize = 4 * 1024;
    let mut out = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        match r.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if out.len() < CAP {
                    let take = n.min(CAP - out.len());
                    out.extend_from_slice(&chunk[..take]);
                }
                // keep reading to EOF (drains the pipe) even once capped
            }
        }
    }
    out
}

/// Log a magick child-process failure: the captured (capped) stderr plus the
/// exit status, via `log_debug` so it's silent unless Debug is on.
pub(super) fn log_magick_failure(
    what: &str,
    status: Option<std::process::ExitStatus>,
    stderr: &[u8],
) {
    let err = String::from_utf8_lossy(stderr);
    let err = err.trim();
    crate::safety::log_debugf!(
        "magick {what} (status {status:?}): {}",
        if err.is_empty() { "<no stderr>" } else { err }
    );
}
