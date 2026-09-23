//! Budgeted workers: run a decode on its own thread, abandon it past the deadline, and cap how many abandoned threads may exist.

use super::*;

/// `std::thread::spawn` for the short helper threads that are not budgeted workers (pipe
/// feeders and drainers around a child process): `None` when the OS refuses the thread,
/// where `std::thread::spawn` panics, and `panic = "abort"` turns that into a dead host
/// inside Explorer. The closure, and everything it captured, is dropped on `None`.
pub fn try_spawn<T, F>(thread_name: &str, f: F) -> Option<std::thread::JoinHandle<T>>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    std::thread::Builder::new()
        .name(thread_name.to_string())
        .spawn(f)
        .ok()
}

/// Spawn a detached worker that holds a DLL pin ([`crate::ModuleRef`]) for its WHOLE life:
/// taken here, BEFORE `spawn`, and moved into the closure. `spawn` only schedules the thread,
/// so a pin taken as the closure's first line leaves a window in which the host could unload
/// the DLL under a thread about to touch it, and a pin taken inside a helper the closure calls
/// ends before the closure's own last writes. On `Err` the OS refused the thread; the closure,
/// and the pin, are dropped with it, and nothing panics (`std::thread::spawn` would).
pub fn spawn_pinned<F>(thread_name: &str, f: F) -> std::io::Result<()>
where
    F: FnOnce() + Send + 'static,
{
    #[allow(clippy::default_constructed_unit_structs)]
    let module = crate::ModuleRef::default();
    std::thread::Builder::new()
        .name(thread_name.to_string())
        .spawn(move || {
            let _module = module;
            f();
        })
        .map(drop)
}

/// Start a child process's two pipe threads: one feeding `input` to its stdin (the pipe
/// closes when that thread finishes, so the child sees EOF) and one running `read` over its
/// stdout, each on its own thread so a full pipe can never deadlock the caller. `None` when the
/// child has no stdin or stdout pipe, or the OS refuses a thread; the child is then killed and
/// reaped (and a feeder that did start is joined), so nothing is left running.
pub fn start_child_pipes<T, R>(
    child: &mut std::process::Child,
    input: Vec<u8>,
    read: R,
) -> Option<(std::thread::JoinHandle<()>, std::thread::JoinHandle<T>)>
where
    T: Send + 'static,
    R: FnOnce(std::process::ChildStdout) -> T + Send + 'static,
{
    let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    };
    let Some(writer) = try_spawn("st2k-child-stdin", move || {
        use std::io::Write;
        let _ = stdin.write_all(&input);
    }) else {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    };
    let Some(reader) = try_spawn("st2k-child-stdout", move || read(stdout)) else {
        let _ = child.kill();
        let _ = writer.join();
        let _ = child.wait();
        return None;
    };
    Some((writer, reader))
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
pub(super) static ABANDONED_WORKERS: AtomicU64 = AtomicU64::new(0);

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
pub(super) const WORKER_RUNNING: u8 = 0;

pub(super) const WORKER_DONE: u8 = 1;

pub(super) const WORKER_ABANDONED: u8 = 2;

/// Caller side: mark the worker abandoned. True when it was still running, so the caller
/// owns the increment; false when the worker had already finished (nothing to count).
///
/// A compare-exchange from RUNNING, not a swap: a swap would write ABANDONED over a worker
/// that had already marked itself DONE, so the state would read "counted" for a worker that
/// never was. The accounting did not care (the swap's return value still said "not owned"),
/// but the state is what a test and any future reader of it must be able to trust, so
/// ABANDONED now means exactly "the caller gave up while the worker was still running, and
/// the worker has not finished since".
pub(super) fn worker_abandoned(state: &AtomicU8) -> bool {
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
pub(super) fn worker_finished(state: &AtomicU8) -> bool {
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
pub(super) fn reserve_abandoned(count: &AtomicU64) {
    count.fetch_add(1, Ordering::AcqRel);
}

/// Caller side, step 2 of 2: publish the abandonment. If the worker had already finished
/// (it saw RUNNING and counted nothing), the reservation from step 1 is undone here; the
/// count then reads exactly as if the worker had never been late. Calling this twice for one
/// worker is harmless: the second swap does not see RUNNING either, so it undoes its own
/// reservation and nets to zero.
pub(super) fn publish_abandoned(state: &AtomicU8, count: &AtomicU64) {
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
pub(super) fn finish_worker(state: &AtomicU8, count: &AtomicU64) {
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
    pub(super) state: Arc<AtomicU8>,
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
    pub(super) slots: [AtomicU64; N],
    pub(super) lease_ms: u64,
}

/// An acquired slot in a [`LeasePool`]. Dropping it frees the slot, unless the lease already
/// expired and another worker took the slot over (then the stored expiry no longer matches
/// and the drop leaves the successor's claim alone).
pub struct Lease {
    pub(super) slot: &'static AtomicU64,
    pub(super) expiry: u64,
}

/// Claim one lease slot for the window starting at `$now_ms`: `true` when `$slot` was free, or
/// its previous holder's lease had expired at `$now_ms`, and this call's single compare-exchange
/// put `$expiry` in its place; `false` means another holder's unexpired lease is still there.
///
/// A macro rather than a `fn` because the two pools it serves hold different atomic integer
/// types - [`crate::safety::LeasePool`]'s `AtomicU64` and `contextmenu::thumb`'s `AtomicUsize`.
/// `$slot` must be a plain place expression (`slot`), as it is read both for the load and by
/// the compare-exchange.
#[macro_export]
macro_rules! try_claim_slot {
    ($slot:expr, $now_ms:expr, $expiry:expr) => {{
        let held = $slot.load(core::sync::atomic::Ordering::Acquire);
        (held == 0 || held <= $now_ms)
            && $slot
                .compare_exchange(
                    held,
                    $expiry,
                    core::sync::atomic::Ordering::AcqRel,
                    core::sync::atomic::Ordering::Acquire,
                )
                .is_ok()
    }};
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
            // Free, or the previous holder's lease has run out and may be taken over.
            let claimed = try_claim_slot!(slot, now_ms, expiry);
            if claimed {
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
