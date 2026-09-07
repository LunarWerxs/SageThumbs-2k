//! Crash-safety boundary.
//!
//! Unwinding a Rust panic across the COM ABI (an `extern "system"`
//! non-unwinding boundary) is undefined behavior, and windows-rs's
//! `#[implement]` macro does NOT wrap method bodies for us. So every COM
//! method funnels through one of these guards.
//!
//! **Important caveat about the release build.** `catch_unwind` only catches
//! *unwinding* panics; with `panic = "abort"` (our release profile, see
//! Cargo.toml) a panic aborts the process *before* any catch — so in release
//! these guards are effectively a debug aid, and the real release behavior is:
//! a panic terminates the host process. The blast radius depends on which
//! coclass panicked:
//!   - **Thumbnail provider** — runs in Explorer's throwaway `dllhost` surrogate,
//!     so an abort there is contained (the surrogate is disposable; Explorer
//!     respawns it). This is the "safe" case the design leans on.
//!   - **Classic context menu / modern `IExplorerCommand`** — these run
//!     **in-process inside `explorer.exe`**, so a panic there aborts the user's
//!     whole shell. Those code paths must therefore be written to *not panic*
//!     (checked indexing, no `unwrap` on attacker-influenced data); the guard is
//!     not a real net for them in release.

use core::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Once, OnceLock};
use std::time::{Duration, Instant};
use windows::core::{Error, Result, HRESULT};
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Graphics::Gdi::{
    CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, HBITMAP,
};
use windows_registry::CURRENT_USER;

/// Longest edge the Explorer preview pane renders at, and the ceiling handed to the decoders
/// on that path. The stream cascade scales to the same value, so the two cannot drift.
pub const PREVIEW_TARGET_EDGE: u32 = 1024;

/// Wall-clock budget for one preview decode, enforced off the host thread (see
/// `previewhandler::decode_preview_budgeted` and the Quick preview's `content.rs`) so a
/// slow decode never freezes the host's message pump. Sized above a typical ImageMagick
/// decode (1-4 s) and well under the ~20 s the host could otherwise be frozen for.
pub const PREVIEW_DECODE_BUDGET: Duration = Duration::from_secs(12);

/// ─── The Quick preview responsiveness contract (audit E02, 2026-09-07) ───
///
/// Slow files must not be able to block the viewer's message pump, and switching away from a
/// slow file must never let its stale output land on screen. That contract is:
///
/// 1. **Show within [`PREVIEW_APPEARANCE_BUDGET`].** `preview::loader::load` shows the window
///    in its `Loading` state before it reads a single byte of the target file (2026-09-05 audit,
///    F10); everything that can block (archive listing, DB/mail markdown, the text/markdown
///    read) runs on a worker thread instead. `tests/preview_async_load.rs` proves this against a
///    4 s slow-read seam and asserts this exact budget as its bound.
/// 2. **No single UI-thread pipeline stage may run past [`PREVIEW_UI_STAGE_BUDGET`]** without
///    it being logged as a stall (`stage_stall_report`, `safety::log_debug`): `apply_resolved`
///    and the render post-back handler (`on_render`) are the two stages this covers. This is a
///    DIAGNOSTIC ceiling, not an enforced one: a stage that legitimately needs longer (e.g.
///    compositing a very large decoded bitmap) still runs to completion, it is just recorded.
/// 3. **A decode worker is abandoned after [`PREVIEW_DECODE_BUDGET`]** (12 s, above) if it has
///    not returned by then; the caller stops waiting and the worker, if it ever finishes, throws
///    its result away. The same budget is used as the "worth logging as slow" threshold for the
///    worker-side prepare/decode stages, since neither has a tighter enforced ceiling of its own.
/// 4. **At most [`MAX_ABANDONED_WORKERS`] abandoned workers may be alive at once.** Past that,
///    [`spawn_budgeted`] and every other [`AbandonTicket`] user refuse to start another one; the
///    Quick preview's `spawn_prepare_load` is one such caller, and past the cap it leaves the window
///    in its `Loading` state (falling through to the fallback card) rather than piling on more
///    blocked threads. Every abandonment that actually counts against this budget is observable:
///    a debug line naming the live count and the cap (rate-limited, since a held arrow key can
///    abandon a worker on every repeat).
///
/// `PREVIEW_UI_STAGE_BUDGET < PREVIEW_APPEARANCE_BUDGET < PREVIEW_DECODE_BUDGET` is an invariant
/// (see `safety::tests::responsiveness_budgets_are_ordered`): the window must appear before a
/// slow decode could possibly finish, and any one UI-thread stage must be far cheaper than the
/// whole appearance budget, or it alone could blow it.
///
/// Wall-clock budget for the viewer to become VISIBLE after `load()` is called on a slow file,
/// measured from process/window-message start (see `tests/preview_async_load.rs`). 1.5 s is
/// generous for a cold/loaded CI box (a real appearance is typically tens of milliseconds, this
/// is showing the `Loading` state, not decoding) while remaining far under the seconds a slow
/// network read or the 12 s decode budget could otherwise stall for.
pub const PREVIEW_APPEARANCE_BUDGET: Duration = Duration::from_millis(1500);

/// Longest a single UI-thread stage of the preview pipeline (`apply_resolved`, the render
/// post-back handler) may take before it is logged as a stall via [`log_debug`]. 100 ms is the
/// commonly-cited threshold past which a UI action stops reading as instantaneous to a human, and
/// every UI-thread stage here is bookkeeping over an already-decoded result (never a decode
/// itself), so it should stay well under it in the overwhelming majority of cases. Exceeding it
/// is diagnostic, not fatal: the stage still runs to completion.
pub const PREVIEW_UI_STAGE_BUDGET: Duration = Duration::from_millis(100);

/// Pure decision + formatting for one pipeline stage's timing, shared by the prepare/decode
/// worker stages and the UI-thread apply/render stages: `Some(line)` (shaped for
/// [`log_debug`]) when `elapsed` exceeded `budget`, `None` when the stage was within budget and
/// nothing should be logged.
///
/// Kept pure (no clock, no I/O) so the log line's SHAPE is unit-tested directly here rather than
/// parsed by any test or script downstream (see `docs/DEVELOPMENT_GOTCHAS.md`, "a string a test
/// parses is an API": this is deliberately not that kind of interface; nothing outside these
/// unit tests should ever match against its text).
pub fn stage_stall_report(
    stage: &str,
    elapsed: Duration,
    budget: Duration,
    generation: u64,
    path: &str,
) -> Option<String> {
    if elapsed <= budget {
        return None;
    }
    Some(format!(
        "preview stage '{stage}' took {}ms (budget {}ms) for generation {generation}: {path}",
        elapsed.as_millis(),
        budget.as_millis(),
    ))
}

/// Wrap a COM method body that returns a raw `HRESULT`.
pub fn guard_hr<F: FnOnce() -> HRESULT>(f: F) -> HRESULT {
    install_panic_hook("dll");
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(hr) => hr,
        Err(_) => {
            log_error("panic crossed a COM boundary -> E_FAIL");
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
    install_panic_hook("dll");
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(_) => {
            log_error("panic crossed a COM boundary -> E_FAIL");
            Err(Error::from(E_FAIL))
        }
    }
}

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
pub fn log_debug(msg: &str) {
    const DEBUG_TTL_MS: u64 = 1000;
    // Packed: high 63 bits = elapsed-ms timestamp of the last probe, low bit = on.
    // 0 means "never probed". Relaxed is fine: a stale read just costs one extra
    // registry probe or one extra/skipped line around a toggle — never UB.
    static CACHE: AtomicU64 = AtomicU64::new(0);

    let now_ms = elapsed_ms();
    let packed = CACHE.load(Ordering::Relaxed);
    let last_ms = packed >> 1;
    let on = if packed == 0 || now_ms.wrapping_sub(last_ms) >= DEBUG_TTL_MS {
        let fresh = CURRENT_USER
            .open(crate::settings::ROOT)
            .and_then(|k| k.get_u32("Debug"))
            .map(|v| v == 1)
            .unwrap_or(false);
        CACHE.store((now_ms << 1) | (fresh as u64), Ordering::Relaxed);
        fresh
    } else {
        packed & 1 != 0
    };
    if on {
        log(msg);
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
pub(crate) fn log_error(msg: &str) {
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
fn maybe_rotate(path: &std::path::Path) {
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
fn log_session_header(artifact: &str) {
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

/// Milliseconds since the first logging call in this process — a cheap, monotonic
/// tick that lets lines from one process be ordered without pulling in wall-clock
/// formatting. Saturates to `u64` (decades), so the `<< 1` packing above is safe.
pub(crate) fn elapsed_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// True when the OS *app* theme is dark (`AppsUseLightTheme == 0` under
/// `HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize`), defaulting to light
/// (`false`) when the key/value is missing or unreadable.
///
/// A raw, uncached, un-overridden probe — every call re-reads the registry. Shared by
/// `contextmenu::paint::menu_dark` (the classic context-menu preview tile), `previewhandler`'s
/// `theme_default_bg` / `SetBackgroundColor` (the Explorer preview pane) and, via a thin
/// wrapper, the app EXE's `dark::is_dark` (which layers a `ST2K_THEME=light|dark` test
/// override and a process-lifetime `OnceLock` cache on top; that layering is call-site-specific
/// and deliberately NOT duplicated in here, only the raw registry read is shared).
pub fn apps_use_dark_theme() -> bool {
    CURRENT_USER
        .open(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize")
        .and_then(|k| k.get_u32("AppsUseLightTheme"))
        .map(|v| v == 0)
        .unwrap_or(false)
}

/// Run `op` on a fresh, DETACHED OS thread that pins the DLL for its whole lifetime, returning
/// its result only if it arrives within `timeout`. `None` on timeout OR if the OS refuses to
/// create the thread — the two are collapsed on purpose: a timed-out worker cannot be cancelled
/// safely (there is no way to abort a thread mid-decode/mid-probe), so either way the caller is
/// blocked for at most `timeout` and gets nothing back. A worker that times out keeps running —
/// it sends into a now-dropped channel (the send simply errors) and exits on its own.
///
/// The DLL pin happens BEFORE spawning, not as `op`'s first line: a `Builder::spawn` that fails
/// to create the OS thread never runs `op` at all, and pinning only on entry would leave a
/// narrow window, right after OS thread creation, during which nothing pins the DLL. On a
/// timeout the worker thread outlives this call, and `DllCanUnloadNow` must not think the DLL is
/// free to unload while that thread is still running — a `ModuleRef` moved into the SAME closure
/// as `op` (rather than acquired inside it) means it is held for the worker's entire run either
/// way.
///
/// Any per-call resource `op` needs to release when the worker finishes — a concurrency-limiting
/// slot lease, for instance — should be an RAII guard captured by `op` itself (constructed by
/// the CALLER, before this is invoked, then moved in). That guard then drops correctly on every
/// exit path: normal completion, timeout-but-still-running, AND a failed `Builder::spawn` (Rust
/// drops an unstarted thread closure, and everything it captured, when `spawn` returns `Err`) —
/// no separate "release on spawn failure" branch needed at the call site.
///
/// This does NOT initialize COM for `op` — callers whose work needs an apartment (the WIC/WinRT
/// decode tiers) must `CoInitializeEx`/`CoUninitialize` inside `op` themselves, since the
/// current callers genuinely disagree on whether they need one (the property-store probe
/// deliberately does not, to stay cheap on Explorer's/SearchIndexer's UI/indexing paths).
///
/// Shared by the preview-pane decode (`previewhandler::decode_preview_budgeted`), the property
/// probe (`propstore::probe_budgeted`), screen/file OCR (`ocr::recognize_bytes`) and the
/// metadata probe (`decode::metadata_budgeted`).
///
/// Abandoned workers are counted process-wide (see [`abandoned_workers`]): a worker that ran
/// past its budget cannot be cancelled, so each one is a thread, its stack, and a `ModuleRef`
/// pin held for as long as its read stays blocked. Past [`MAX_ABANDONED_WORKERS`] live ones
/// this refuses to start another (returning `None`, and logging once per process) until some
/// of them finish, so a tree of cloud placeholders or a dropped share cannot grow the host's
/// thread count without bound.
pub fn spawn_budgeted<R, F>(thread_name: &str, timeout: Duration, op: F) -> Option<R>
where
    R: Send + 'static,
    F: FnOnce() -> R + Send + 'static,
{
    if abandoned_budget_exhausted() {
        static LOGGED: Once = Once::new();
        LOGGED.call_once(|| {
            log_error(&format!(
                "spawn_budgeted: {MAX_ABANDONED_WORKERS} workers are still running past their \
                 budget; refusing new '{thread_name}' workers until some finish"
            ));
        });
        return None;
    }
    #[allow(clippy::default_constructed_unit_structs)]
    let module = crate::ModuleRef::default();
    let (tx, rx) = std::sync::mpsc::channel();
    let ticket = AbandonTicket::new();
    let worker_ticket = ticket.clone();
    let worker = std::thread::Builder::new()
        .name(thread_name.to_string())
        .spawn(move || {
            let _module = module;
            let _ = tx.send(op());
            worker_ticket.worker_finished();
        });
    // The OS refusing a new thread is the same terminal state as a timeout: no result, and
    // (per the doc above) any guard `op` captured has already been dropped by `spawn` itself.
    worker.ok()?;
    match rx.recv_timeout(timeout) {
        Ok(r) => Some(r),
        Err(_) => {
            ticket.caller_gave_up();
            None
        }
    }
}

/// Live workers that ran past their budget and have not finished yet: every
/// [`spawn_budgeted`] worker, plus any detached worker whose caller holds an
/// [`AbandonTicket`] for it (the menu-preview decode in `contextmenu::thumb`).
static ABANDONED_WORKERS: AtomicU64 = AtomicU64::new(0);

/// Once this many abandoned workers are alive in the process, [`spawn_budgeted`] (and every
/// other [`AbandonTicket`] user) refuses to start more. Each one is a blocked thread pinning
/// the DLL; eight is well past what a healthy host ever accumulates and small enough that a
/// hung share cannot exhaust the host.
pub const MAX_ABANDONED_WORKERS: u64 = 8;

/// The number of budgeted workers currently running past their budget.
pub fn abandoned_workers() -> u64 {
    ABANDONED_WORKERS.load(Ordering::Acquire)
}

/// Whether the process has already accumulated [`MAX_ABANDONED_WORKERS`] live abandoned
/// workers, so no caller should start another detached worker until some finish. The one
/// gate every detached-worker entry point consults; a path that spawns its own thread
/// without asking this is a path the budget does not cover.
pub fn abandoned_budget_exhausted() -> bool {
    abandoned_workers() >= MAX_ABANDONED_WORKERS
}

// Per-worker handshake between the caller (which may give up waiting) and the worker (which
// may finish before or after that). Exactly one of the two `swap`s sees the other's mark, so
// the abandoned count is incremented and decremented for the same worker, never twice and
// never for a worker that finished first.
const WORKER_RUNNING: u8 = 0;
const WORKER_DONE: u8 = 1;
const WORKER_ABANDONED: u8 = 2;

/// Caller side: mark the worker abandoned. True when it was still running, so the caller
/// owns the increment; false when the worker had already finished (nothing to count).
///
/// A compare-exchange from RUNNING, not a swap: a swap would write ABANDONED over a worker
/// that had already marked itself DONE, so the state would read "counted" for a worker that
/// never was. The accounting did not care (the swap's return value still said "not owned"),
/// but the state is what a test and any future reader of it must be able to trust, so
/// ABANDONED now means exactly "the caller gave up while the worker was still running, and
/// the worker has not finished since".
fn worker_abandoned(state: &AtomicU8) -> bool {
    state
        .compare_exchange(
            WORKER_RUNNING,
            WORKER_ABANDONED,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_ok()
}

/// Worker side: mark the worker done. True when the caller had already abandoned it, so the
/// worker owns the decrement; false when it finished in time (nothing was counted).
fn worker_finished(state: &AtomicU8) -> bool {
    state.swap(WORKER_DONE, Ordering::AcqRel) == WORKER_ABANDONED
}

/// Caller side of the accounting, step 1 of 2: reserve the count BEFORE publishing the
/// abandonment. Split from step 2 so a test can interleave the worker between them.
///
/// The order is the whole fix. The first version published the state first and incremented
/// afterwards, and the worker could finish in that gap: it saw ABANDONED, ran its decrement
/// against a count that did not yet include it (a `checked_sub` at zero is a no-op), and the
/// caller then incremented a count nothing would ever decrement again. Eight such phantoms
/// and [`spawn_budgeted`] refused every worker for the life of the host, with every real
/// worker long finished. Reserving first means the worker's decrement can only ever run
/// AFTER the increment it undoes: the decrement is gated on seeing ABANDONED, ABANDONED is
/// written only in step 2, and step 2 runs after this on the same thread.
fn reserve_abandoned(count: &AtomicU64) {
    count.fetch_add(1, Ordering::AcqRel);
}

/// Caller side, step 2 of 2: publish the abandonment. If the worker had already finished
/// (it saw RUNNING and counted nothing), the reservation from step 1 is undone here; the
/// count then reads exactly as if the worker had never been late. Calling this twice for one
/// worker is harmless: the second swap does not see RUNNING either, so it undoes its own
/// reservation and nets to zero.
fn publish_abandoned(state: &AtomicU8, count: &AtomicU64) {
    if !worker_abandoned(state) {
        count.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Worker side: mark done and, if the caller had already abandoned this worker, release the
/// count the caller reserved for it. `checked_sub` is defence in depth only: the ordering
/// above guarantees the reservation precedes this, so the count is never zero here for a
/// correctly paired ticket, and a bug that broke the pairing must not wrap the counter to
/// `u64::MAX` (which would refuse every worker forever, the exact failure this exists to
/// prevent).
fn finish_worker(state: &AtomicU8, count: &AtomicU64) {
    if worker_finished(state) {
        let _ = count.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1));
    }
}

/// The accounting handshake for ONE detached worker against the process-wide abandoned
/// count, for callers that cannot route their worker through [`spawn_budgeted`] (the
/// menu-preview decode hands its receiver to a later shell callback, so the timeout is not
/// observed where the thread is spawned).
///
/// Clone it once: the caller keeps one half and the worker's closure the other. The worker
/// calls [`worker_finished`](Self::worker_finished) as its last act; the caller calls
/// [`caller_gave_up`](Self::caller_gave_up) when it stops waiting, whether by timeout or by
/// never collecting the result. Both are idempotent, and the pairing guarantees the count
/// rises by exactly one for a worker that outlives its caller and returns to its baseline
/// when that worker eventually finishes, on every interleaving of the two sides.
#[derive(Clone)]
pub struct AbandonTicket {
    state: Arc<AtomicU8>,
}

impl AbandonTicket {
    pub fn new() -> Self {
        Self {
            state: Arc::new(AtomicU8::new(WORKER_RUNNING)),
        }
    }

    /// The caller has stopped waiting for the worker's result.
    pub fn caller_gave_up(&self) {
        reserve_abandoned(&ABANDONED_WORKERS);
        publish_abandoned(&self.state, &ABANDONED_WORKERS);
    }

    /// The worker has produced its result (or given up) and is about to exit.
    pub fn worker_finished(&self) {
        finish_worker(&self.state, &ABANDONED_WORKERS);
    }

    /// Whether this worker is counted against the budget right now: the caller gave up and
    /// the worker has not finished. Public (not test-only) so a caller outside this crate can
    /// observe its own ticket's state directly instead of racing a before/after read of the
    /// shared process-wide count against every other ticket's concurrent activity.
    pub fn is_counted(&self) -> bool {
        self.state.load(Ordering::Acquire) == WORKER_ABANDONED
    }
}

impl Default for AbandonTicket {
    fn default() -> Self {
        Self::new()
    }
}

/// A fixed number of concurrency slots, each held under a LEASE rather than a permanent
/// claim, for callers that start [`spawn_budgeted`] workers whose reads may never return.
///
/// A slot that is a plain counter decremented by the worker's own `Drop` is held for the
/// life of the process by a worker blocked forever (a OneDrive online-only placeholder, a
/// dropped SMB share). Two such files permanently exhausted the property store's two slots,
/// after which EVERY property query in that host returned nothing. A lease keeps the
/// original guarantee, at most `N` workers started in any lease window, while making the
/// failure self-healing: a worker that finishes normally releases its slot at once; one that
/// hangs loses it at expiry. `lease_ms` is generous on purpose, bounding the damage from a
/// hung read without cutting short a slow one that would have succeeded.
///
/// Time is [`elapsed_ms`]; `0` in a slot means free. Pools are `static`s so a [`Lease`] can
/// point straight at its slot.
pub struct LeasePool<const N: usize> {
    slots: [AtomicU64; N],
    lease_ms: u64,
}

/// An acquired slot in a [`LeasePool`]. Dropping it frees the slot, unless the lease already
/// expired and another worker took the slot over (then the stored expiry no longer matches
/// and the drop leaves the successor's claim alone).
pub struct Lease {
    slot: &'static AtomicU64,
    expiry: u64,
}

impl<const N: usize> LeasePool<N> {
    /// `lease_ms` must be non-zero, or a slot claimed at time 0 would read as free.
    pub const fn new(lease_ms: u64) -> Self {
        Self {
            slots: [const { AtomicU64::new(0) }; N],
            lease_ms,
        }
    }

    /// Claim a slot now. `None` when every slot holds an unexpired lease.
    pub fn acquire(&'static self) -> Option<Lease> {
        self.acquire_at(elapsed_ms())
    }

    /// [`acquire`](Self::acquire) with an injected clock, so the policy is testable
    /// without sleeping.
    pub fn acquire_at(&'static self, now_ms: u64) -> Option<Lease> {
        let expiry = now_ms.saturating_add(self.lease_ms.max(1));
        for slot in &self.slots {
            let held = slot.load(Ordering::Acquire);
            // Free, or the previous holder's lease has run out and may be taken over.
            if (held == 0 || held <= now_ms)
                && slot
                    .compare_exchange(held, expiry, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            {
                return Some(Lease { slot, expiry });
            }
        }
        None
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let _ = self
            .slot
            .compare_exchange(self.expiry, 0, Ordering::AcqRel, Ordering::Acquire);
    }
}

/// Build a top-down 32bpp DIB of `rgba` (straight, non-premultiplied) composited over the
/// opaque `bg` (`COLORREF` 0x00BBGGRR), so painting is a plain `StretchBlt`. `None` on a
/// malformed size / allocation failure — never panics on attacker-controlled dims, which
/// matters for the caller that runs this in-process in `prevhost` under `panic = "abort"`.
///
/// `opaque`: `Some(bool)` when the caller has already worked out whether every pixel is
/// fully opaque (skips the `O(px)` alpha scan below); `None` to have this function work it
/// out itself.
///
/// Shared by the preview-pane host (`previewhandler`'s `WM_PREVIEW_RENDER` arm, in-process
/// in `prevhost`, which passes `opaque: None`) and the Quick preview viewer EXE
/// (`bin/app/preview/content::make_dib`, which passes the opacity it already knows). Homed
/// here, not next to either caller, because `safety` is the one `pub` (crate-external-visible)
/// module this reaches: the app EXE is a SEPARATE crate (its own `[[bin]]`) that can only
/// call `pub` items, and neither `previewhandler` nor the app's own `preview` module is a
/// `pub mod` in `lib.rs`.
///
/// # Safety
/// Calls into GDI (`CreateDIBSection`), so this must run with a valid thread/GDI context, and
/// the caller owns the returned `HBITMAP` — it must eventually `DeleteObject` it, this function
/// does not track its lifetime. There is no other pointer/slice obligation on the caller:
/// `rgba`'s length and `iw`/`ih` are validated (via checked arithmetic) before any raw pointer
/// is touched, and a malformed input returns `None` rather than reading out of bounds.
pub unsafe fn composite_rgba_over_bg(
    iw: i32,
    ih: i32,
    rgba: &[u8],
    bg: u32,
    opaque: Option<bool>,
) -> Option<HBITMAP> {
    if iw <= 0 || ih <= 0 {
        return None;
    }
    let px = (iw as usize).checked_mul(ih as usize)?;
    if rgba.len() < px.checked_mul(4)? {
        return None;
    }
    let mut bmi = BITMAPINFO::default();
    bmi.bmiHeader.biSize = core::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bmi.bmiHeader.biWidth = iw;
    bmi.bmiHeader.biHeight = -ih; // top-down
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;
    bmi.bmiHeader.biCompression = 0; // BI_RGB

    let mut bits: *mut c_void = core::ptr::null_mut();
    let hbmp = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
    if bits.is_null() {
        _ = DeleteObject(hbmp.into());
        return None;
    }
    let (bg_r, bg_g, bg_b) = (bg & 0xFF, (bg >> 8) & 0xFF, (bg >> 16) & 0xFF);
    let dst = core::slice::from_raw_parts_mut(bits as *mut u8, px * 4);
    // "Opaque pixels copy through" was true of the arithmetic and false of the cost: the loop
    // below still ran three multiplies and three divides per pixel to arrive at its own input.
    // A photo is always fully opaque, so ask once (or trust a caller who already knows), then
    // take the plain swizzle when there is no transparency to honour.
    if opaque.unwrap_or_else(|| (0..px).all(|i| rgba[i * 4 + 3] == 255)) {
        for i in 0..px {
            dst[i * 4] = rgba[i * 4 + 2]; // B
            dst[i * 4 + 1] = rgba[i * 4 + 1]; // G
            dst[i * 4 + 2] = rgba[i * 4]; // R
            dst[i * 4 + 3] = 255;
        }
        return Some(hbmp);
    }
    for i in 0..px {
        let r = rgba[i * 4] as u32;
        let g = rgba[i * 4 + 1] as u32;
        let b = rgba[i * 4 + 2] as u32;
        let a = rgba[i * 4 + 3] as u32;
        // out = (src*a + bg*(255-a)) / 255, rounded.
        let comp = |s: u32, d: u32| (((s * a) + (d * (255 - a)) + 127) / 255) as u8;
        dst[i * 4] = comp(b, bg_b); // B
        dst[i * 4 + 1] = comp(g, bg_g); // G
        dst[i * 4 + 2] = comp(r, bg_r); // R
        dst[i * 4 + 3] = 255;
    }
    Some(hbmp)
}

#[cfg(test)]
mod dib_tests {
    use super::composite_rgba_over_bg;
    use windows::Win32::Graphics::Gdi::{DeleteObject, GetObjectW, BITMAP, HBITMAP};

    /// Read the top-left BGRA quad out of a DIB-section HBITMAP, then free it.
    unsafe fn first_px(hbmp: HBITMAP) -> [u8; 4] {
        let mut bm = BITMAP::default();
        let n = GetObjectW(
            hbmp.into(),
            core::mem::size_of::<BITMAP>() as i32,
            Some(&mut bm as *mut _ as *mut core::ffi::c_void),
        );
        assert!(n != 0 && !bm.bmBits.is_null());
        let px = core::slice::from_raw_parts(bm.bmBits as *const u8, 4);
        let out = [px[0], px[1], px[2], px[3]];
        let _ = DeleteObject(hbmp.into());
        out
    }

    /// The whole point of the `opaque` parameter: a caller that already knows the pixel is
    /// opaque can force the fast swizzle path via `Some(true)` and it must be HONORED (not
    /// silently re-derived by scanning alpha), even feeding it a genuinely translucent pixel.
    /// Without this, `previewhandler.rs` had no way to share the EXE viewer's opacity-hint
    /// optimization at all — this pins the parameter actually doing something.
    #[test]
    fn composite_rgba_over_bg_honors_a_forced_opaque_hint() {
        unsafe {
            // A 50%-alpha red pixel: under the real (computed) opacity it must blend with the
            // background; forced opaque, it must copy straight through instead and ignore bg.
            let translucent = [200u8, 0, 0, 128];
            let bg = 0x00FF_0000; // opaque blue, COLORREF 0x00BBGGRR

            let forced = composite_rgba_over_bg(1, 1, &translucent, bg, Some(true)).unwrap();
            assert_eq!(
                first_px(forced),
                [0, 0, 200, 255],
                "Some(true) must take the swizzle path even though the pixel is translucent"
            );

            let computed = composite_rgba_over_bg(1, 1, &translucent, bg, None).unwrap();
            assert_ne!(
                first_px(computed),
                [0, 0, 200, 255],
                "None must actually blend a translucent pixel with the background"
            );
        }
    }

    /// Untrusted decoded dimensions must be rejected (None), never deref/overflow — this runs
    /// in prevhost on attacker-influenced sizes.
    #[test]
    fn composite_rgba_over_bg_rejects_bad_dims_without_crashing() {
        unsafe {
            assert!(composite_rgba_over_bg(0, 5, &[0u8; 64], 0, None).is_none());
            assert!(composite_rgba_over_bg(5, 0, &[0u8; 64], 0, None).is_none());
            assert!(composite_rgba_over_bg(-3, 4, &[0u8; 64], 0, None).is_none());
            assert!(composite_rgba_over_bg(2, 2, &[0u8; 4], 0, None).is_none());
            assert!(composite_rgba_over_bg(i32::MAX, i32::MAX, &[0u8; 4], 0, None).is_none());
        }
    }

    /// The alpha-over-background compositing math the preview pane's `WM_PAINT` later
    /// StretchBlts (moved here from `previewhandler.rs` when its private `make_dib` copy was
    /// retired). `bg` is a COLORREF 0x00BBGGRR; 0x00FF_0000 is opaque blue.
    #[test]
    fn composite_rgba_over_bg_composites_alpha_over_background() {
        unsafe {
            // Opaque red over blue copies straight through -> BGRA [0,0,255,255].
            let red = composite_rgba_over_bg(1, 1, &[255, 0, 0, 255], 0x00FF_0000, None).unwrap();
            assert_eq!(first_px(red), [0, 0, 255, 255], "opaque red");

            // 50% red over blue: R ≈ 200*128/255 ≈ 100, B ≈ 255*127/255 ≈ 127.
            let half = composite_rgba_over_bg(1, 1, &[200, 0, 0, 128], 0x00FF_0000, None).unwrap();
            let [b, g, r, a] = first_px(half);
            assert_eq!((g, a), (0, 255), "no green; DIB opaque");
            assert!((r as i32 - 100).abs() <= 2, "R composited ~100, got {r}");
            assert!((b as i32 - 127).abs() <= 2, "B composited ~127, got {b}");
        }
    }
}

#[cfg(test)]
mod worker_tests {
    use super::*;

    /// The caller/worker handshake behind the abandoned-worker count: whichever side marks
    /// second sees the other's mark, so an increment is always paired with exactly one
    /// decrement, and a worker that finished before the caller gave up counts for nothing.
    #[test]
    fn abandoned_handshake_pairs_increment_with_decrement() {
        // Caller gives up first, worker finishes later: count, then uncount.
        let s = AtomicU8::new(WORKER_RUNNING);
        assert!(worker_abandoned(&s), "caller owns the increment");
        assert!(worker_finished(&s), "worker owns the decrement");

        // Worker finishes first: nothing to count on either side.
        let s = AtomicU8::new(WORKER_RUNNING);
        assert!(!worker_finished(&s));
        assert!(!worker_abandoned(&s));
    }

    /// Every interleaving of the worker's finish against the caller's two accounting steps
    /// must leave the count at its baseline once both sides are done, and must count the
    /// worker as abandoned for exactly the window in which it really is. Driven against a
    /// LOCAL counter with the steps called by hand, so each ordering is exercised
    /// deterministically rather than hoped for under a scheduler.
    ///
    /// The middle case is the one that used to fail: the caller published ABANDONED, the
    /// worker finished and decremented a count that was still zero (no-op), and the caller
    /// then incremented. That phantom was never removed, and eight of them shut every
    /// budgeted worker out of the host for good. The reproduction is `repros/
    /// f02_worker_counter_repro.rs` in the 2026-09-05 audit; this pins the fix.
    #[test]
    fn abandoned_count_returns_to_baseline_on_every_interleaving() {
        // Worker finishes BEFORE the caller gives up: nothing is ever counted.
        let count = AtomicU64::new(0);
        let s = AtomicU8::new(WORKER_RUNNING);
        finish_worker(&s, &count);
        reserve_abandoned(&count);
        publish_abandoned(&s, &count);
        assert_eq!(
            count.load(Ordering::Acquire),
            0,
            "worker-first must count nothing"
        );

        // Worker finishes BETWEEN the caller's reservation and its publication: the
        // reservation is undone, and the worker (which saw RUNNING) touched nothing.
        let count = AtomicU64::new(0);
        let s = AtomicU8::new(WORKER_RUNNING);
        reserve_abandoned(&count);
        finish_worker(&s, &count);
        publish_abandoned(&s, &count);
        assert_eq!(
            count.load(Ordering::Acquire),
            0,
            "a worker finishing inside the caller's gap must leave no phantom"
        );

        // Worker finishes AFTER the caller gave up: counted while late, uncounted when done.
        let count = AtomicU64::new(0);
        let s = AtomicU8::new(WORKER_RUNNING);
        reserve_abandoned(&count);
        publish_abandoned(&s, &count);
        assert_eq!(count.load(Ordering::Acquire), 1, "a late worker is counted");
        finish_worker(&s, &count);
        assert_eq!(
            count.load(Ordering::Acquire),
            0,
            "and uncounted when it finishes"
        );

        // Both sides repeated: a caller that times out and later drops its handle, a worker
        // path that reports twice. Neither may move the count a second time.
        let count = AtomicU64::new(0);
        let s = AtomicU8::new(WORKER_RUNNING);
        reserve_abandoned(&count);
        publish_abandoned(&s, &count);
        reserve_abandoned(&count);
        publish_abandoned(&s, &count);
        assert_eq!(
            count.load(Ordering::Acquire),
            1,
            "a second give-up is a no-op"
        );
        finish_worker(&s, &count);
        finish_worker(&s, &count);
        assert_eq!(
            count.load(Ordering::Acquire),
            0,
            "a second finish is a no-op"
        );
    }

    /// The refusal predicate over the same local counter: eight workers abandoned in the
    /// racy ordering used to leave eight phantoms and a permanent refusal. Now the same
    /// eight leave zero, the ninth start is allowed, and the threshold trips only while
    /// eight workers are GENUINELY still running late, recovering as they finish.
    #[test]
    fn eight_workers_finishing_inside_the_gap_do_not_exhaust_the_budget() {
        let count = AtomicU64::new(0);
        let exhausted = |c: &AtomicU64| c.load(Ordering::Acquire) >= MAX_ABANDONED_WORKERS;

        for _ in 0..MAX_ABANDONED_WORKERS {
            let s = AtomicU8::new(WORKER_RUNNING);
            reserve_abandoned(&count);
            finish_worker(&s, &count); // the worker slips in before the publication
            publish_abandoned(&s, &count);
        }
        assert_eq!(count.load(Ordering::Acquire), 0);
        assert!(
            !exhausted(&count),
            "no phantoms, so the budget must still be open"
        );

        // Eight genuinely late workers DO trip it, and finishing them reopens it.
        let states: Vec<AtomicU8> = (0..MAX_ABANDONED_WORKERS)
            .map(|_| AtomicU8::new(WORKER_RUNNING))
            .collect();
        for s in &states {
            reserve_abandoned(&count);
            publish_abandoned(s, &count);
        }
        assert!(
            exhausted(&count),
            "eight live late workers exhaust the budget"
        );
        finish_worker(&states[0], &count);
        assert!(!exhausted(&count), "one finishing reopens it");
        for s in &states[1..] {
            finish_worker(s, &count);
        }
        assert_eq!(count.load(Ordering::Acquire), 0, "back to baseline");
    }

    /// The public ticket wraps the same steps around the process-wide counter. The per-ticket
    /// state is asserted exactly; the shared count only relatively, since other tests in this
    /// binary run budgeted workers concurrently.
    #[test]
    fn abandon_ticket_counts_only_while_the_worker_is_genuinely_late() {
        let t = AbandonTicket::new();
        let w = t.clone();
        w.worker_finished();
        t.caller_gave_up();
        assert!(
            !t.is_counted(),
            "a worker that finished first must not be counted"
        );

        let t = AbandonTicket::new();
        let w = t.clone();
        t.caller_gave_up();
        assert!(t.is_counted(), "a late worker is counted");
        // Ours is live and counted, so the shared count is at least one whatever other tests'
        // workers do in the meantime.
        assert!(abandoned_workers() >= 1, "and the shared count saw it");
        w.worker_finished();
        assert!(!t.is_counted(), "and released on finish");
    }

    /// A worker that outlives its budget is counted while it runs and uncounted when it
    /// finishes, through the real `spawn_budgeted` path. Only relative facts are asserted
    /// (other tests in this binary may run budgeted workers concurrently).
    #[test]
    fn spawn_budgeted_counts_a_worker_that_outlives_its_budget() {
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let r = spawn_budgeted(
            "st2k-test-abandoned",
            Duration::from_millis(20),
            move || {
                let _ = release_rx.recv();
                let _ = done_tx.send(());
                7u8
            },
        );
        assert_eq!(r, None, "a blocked worker must time out");
        assert!(
            abandoned_workers() >= 1,
            "our still-blocked worker must be counted as abandoned"
        );
        let _ = release_tx.send(());
        assert!(
            done_rx.recv_timeout(Duration::from_secs(10)).is_ok(),
            "the abandoned worker must still run to completion on its own"
        );
    }

    /// A worker that finishes inside its budget hands back its result.
    #[test]
    fn spawn_budgeted_returns_a_prompt_result() {
        let r = spawn_budgeted("st2k-test-prompt", Duration::from_secs(10), || 42u32);
        assert_eq!(r, Some(42));
    }

    static POOL: LeasePool<2> = LeasePool::new(1_000);

    /// The slots must be a LEASE, not a permanent claim. Two files whose reads hang forever
    /// used to hold both property-probe slots for the life of the process, after which every
    /// property query in that host returned nothing. Driven with an injected clock, so it
    /// asserts the real policy without sleeping or spawning a thread.
    #[test]
    fn hung_holders_lose_their_slot_when_the_lease_expires() {
        let t0 = 1_000_000u64;
        let lease_ms = POOL.lease_ms;

        // Fill every slot, then confirm the cap actually holds.
        let first: Vec<Lease> = (0..2).map(|_| POOL.acquire_at(t0).expect("slot")).collect();
        assert!(
            POOL.acquire_at(t0).is_none(),
            "the cap must bound live holders"
        );

        // Still held part-way through the lease: a slow-but-progressing read keeps its slot.
        assert!(POOL.acquire_at(t0 + lease_ms - 1).is_none());

        // Past the lease, the slots are reclaimable even though the holders never finished.
        let second: Vec<Lease> = (0..2)
            .map(|_| {
                POOL.acquire_at(t0 + lease_ms + 1)
                    .expect("an expired lease must be reclaimable")
            })
            .collect();

        // A late release from the FIRST generation must not free the slot its successor now
        // owns: the drop is keyed to the exact expiry it claimed.
        let held: Vec<u64> = POOL
            .slots
            .iter()
            .map(|s| s.load(Ordering::Acquire))
            .collect();
        drop(first);
        let after: Vec<u64> = POOL
            .slots
            .iter()
            .map(|s| s.load(Ordering::Acquire))
            .collect();
        assert_eq!(
            held, after,
            "a stale release must not steal the current holder's slot"
        );

        // A holder that finishes normally frees its slot immediately.
        drop(second);
        assert!(
            POOL.slots.iter().all(|s| s.load(Ordering::Acquire) == 0),
            "released slots must read as free"
        );
    }
}

#[cfg(test)]
mod responsiveness_contract_tests {
    use super::*;

    /// The whole point of the ordering: the window must appear before a slow decode could
    /// possibly finish, and any single UI-thread stage must be far cheaper than the whole
    /// appearance budget, or it alone could blow it. See the doc comment above
    /// `PREVIEW_APPEARANCE_BUDGET` for the full contract this pins.
    #[test]
    fn responsiveness_budgets_are_ordered() {
        assert!(
            PREVIEW_UI_STAGE_BUDGET < PREVIEW_APPEARANCE_BUDGET,
            "a single UI-thread stage must be cheaper than the whole appearance budget"
        );
        assert!(
            PREVIEW_APPEARANCE_BUDGET < PREVIEW_DECODE_BUDGET,
            "the window must be able to appear well before a slow decode could time out"
        );
    }

    /// A stage that finished within its budget is silent: nothing worth logging happened.
    #[test]
    fn stage_stall_report_is_silent_within_budget() {
        assert_eq!(
            stage_stall_report(
                "prepare",
                Duration::from_millis(50),
                Duration::from_millis(100),
                7,
                "x.txt"
            ),
            None
        );
    }

    /// The boundary itself is still within budget (`elapsed <= budget`), not a stall: a stage
    /// that finishes exactly on the budget should not flap between logged/silent on jitter.
    #[test]
    fn stage_stall_report_treats_the_exact_budget_as_not_stalled() {
        assert_eq!(
            stage_stall_report(
                "decode",
                Duration::from_millis(100),
                Duration::from_millis(100),
                1,
                "a.png"
            ),
            None
        );
    }

    /// Over budget: a line naming the stage, elapsed/budget in ms, the generation, and the
    /// path. This is the shape this whole function exists to test (see the doc comment: this
    /// shape is asserted here and nowhere else, so localizing/reformatting it later cannot
    /// silently break a hidden parser).
    #[test]
    fn stage_stall_report_names_stage_elapsed_budget_generation_and_path() {
        let line = stage_stall_report(
            "apply",
            Duration::from_millis(250),
            Duration::from_millis(100),
            42,
            r"C:\slow\file.txt",
        )
        .expect("over budget must report");
        assert!(line.contains("apply"), "{line}");
        assert!(line.contains("250"), "{line}");
        assert!(line.contains("100"), "{line}");
        assert!(line.contains("42"), "{line}");
        assert!(line.contains(r"C:\slow\file.txt"), "{line}");
    }
}
