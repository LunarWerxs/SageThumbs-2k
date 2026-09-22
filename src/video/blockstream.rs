//! A frame from a block-mode stream: the global interface table hand-off and the bounded, pumping wait.

use super::*;

/// Grab a representative ~30 % frame for a video MF can demux but we have no bespoke index
/// parser for (AVI, WMV/ASF, …), by letting MF seek the file's real index over a
/// [`crate::vstream::BlockCacheStream`] wrapping the shell `IStream`. `size` is the stream
/// length (the caller already has it).
///
/// Runs on a detached worker under [`VIDEO_TIMEOUT`], like every other tier, and the calling
/// thread is ALWAYS free again once that budget is spent. It did not use to be: this tier ran
/// inline on the shell thread with a side watchdog that could only log, because the shell
/// `IStream` is bound to the caller's apartment and a worker's reads must marshal back to it.
/// Issue #35 is what that cost. Windows 10's H.264 decoder wedged inside `ReadSample` on a
/// 4:4:4 file, the call never returned, and Explorer's whole thumbnail pipeline sat behind
/// it until a reboot; even a new `explorer.exe` queued up behind the same stuck surrogate.
///
/// The worker now gets the stream through the Global Interface Table and the caller waits
/// with `CoWaitForMultipleHandles`, which dispatches the worker's marshaled reads on an STA
/// caller (a preview host) and is a plain wait on an MTA one (the thumbnail host), so the
/// marshaling that forced the inline design is serviced instead of deadlocked. On timeout the
/// worker is left to finish on its own and recorded as stranded ([`Strand`]); a strand that
/// outlives [`STRAND_GRACE`] turns the video tiers off for this host ([`mf_usable`]) and lets
/// `dll_can_unload_now` recycle the surrogate. Block-caching still collapses MF's thousands
/// of tiny reads into a handful of 1 MiB ones, and the stream's own deadline + byte budget
/// bound its I/O as before. Returns `None` (the caller falls back) on any failure.
pub fn frame_from_block_stream(shell: &IStream, size: u64, frac: f64) -> Option<DynamicImage> {
    if !mf_usable() {
        return None;
    }
    // The Global Interface Table is the one marshaling that is right whatever apartment this
    // thread is in: an MTA caller's worker gets the same pointer back, an STA caller's gets a
    // proxy whose calls land on this thread (served by the pumping wait). The worker fetches
    // and revokes the entry itself, so the cookie never outlives the grab.
    let git = unsafe { global_interface_table() }?;
    let cookie = unsafe { git.RegisterInterfaceInGlobal(shell, &IStream::IID) }.ok()?;
    let mut cookie = GitCookie(Some(cookie));
    let seek = Seek {
        frac,
        cap_hns: None,
    };
    run_bounded_pumping(
        VIDEO_TIMEOUT,
        "the block-stream frame grab",
        move || unsafe {
            let entry = cookie.0?;
            // A `?` here drops `cookie`, whose Drop revokes the entry.
            let git = global_interface_table()?;
            let mut raw: *mut core::ffi::c_void = std::ptr::null_mut();
            let fetched = git
                .GetInterfaceFromGlobal(entry, &IStream::IID, &mut raw)
                .is_ok();
            cookie.revoke();
            if !fetched || raw.is_null() {
                return None;
            }
            // SAFETY: a live, AddRef'd IStream the table just handed this apartment.
            let inner = IStream::from_raw(raw);
            grab_block_stream(inner, size, seek)
        },
    )
}

/// `CLSID_StdGlobalInterfaceTable`, {00000323-0000-0000-C000-000000000046}.
pub(super) const CLSID_STD_GLOBAL_INTERFACE_TABLE: GUID =
    GUID::from_u128(0x00000323_0000_0000_c000_000000000046);

/// The process-wide Global Interface Table (a COM singleton; creating it is a lookup).
/// Shared with `command.rs`, which parks the modern menu's `IShellItemArray` in it so the
/// selection walk happens on the verb's worker instead of the shell thread.
pub(crate) unsafe fn global_interface_table() -> Option<IGlobalInterfaceTable> {
    CoCreateInstance(
        &CLSID_STD_GLOBAL_INTERFACE_TABLE,
        None,
        CLSCTX_INPROC_SERVER,
    )
    .ok()
}

/// RAII owner of a [`frame_from_block_stream`] GIT cookie. It revokes the table entry on drop
/// unless [`GitCookie::revoke`] already did, so a cookie whose worker never starts (the
/// `spawn` in `run_bounded_pumping` failed, dropping the closure) is still revoked instead of
/// pinning the shell `IStream` for the host's lifetime.
struct GitCookie(Option<u32>);

impl GitCookie {
    /// Revoke the entry now (whatever happens next) and disarm the drop.
    fn revoke(&mut self) {
        if let Some(cookie) = self.0.take() {
            unsafe {
                if let Some(git) = global_interface_table() {
                    let _ = git.RevokeInterfaceFromGlobal(cookie);
                }
            }
        }
    }
}

impl Drop for GitCookie {
    fn drop(&mut self) {
        self.revoke();
    }
}

/// Run `f` on a detached worker and wait for it at most `timeout`, dispatching this
/// apartment's incoming COM calls meanwhile ([`wait_pumping`]) so a worker whose reads
/// marshal back to an STA caller is serviced rather than deadlocked. `f` runs under its own
/// MTA apartment and a `ModuleRef`, like every other budgeted worker. On timeout the worker
/// is left running (never killed, see [`Strand`]), its strand is recorded, and `None` comes
/// back at once: the CALLER's thread is free again after `timeout`, whatever `f` is doing.
pub(super) fn run_bounded_pumping<T, F>(timeout: Duration, what: &'static str, f: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce() -> Option<T> + Send + 'static,
{
    MF_GRAB_ATTEMPTS.fetch_add(1, Ordering::SeqCst);
    // Manual-reset, and shared: a worker finishing after the timeout signals a handle both
    // sides still hold, which closes with the last of them, never one already recycled.
    let raw = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }.ok()?;
    // SAFETY: a fresh event handle this function owns; wrapped so it is closed exactly once.
    let event = Arc::new(unsafe { OwnedHandle::from_raw_handle(raw.0) });
    let slot: Arc<Mutex<Option<T>>> = Arc::new(Mutex::new(None));
    let done = Arc::new(AtomicBool::new(false));
    let (w_event, w_slot, w_done) = (event.clone(), slot.clone(), done.clone());
    // Pin the DLL for this detached worker's whole lifetime (see `grab_budgeted`): taken
    // BEFORE `spawn` and moved in, so it covers the slot store and `SetEvent` after
    // `with_mta_apartment` has dropped its own pin, and is released if `spawn` fails.
    #[allow(clippy::default_constructed_unit_structs)]
    let module = crate::ModuleRef::default();
    let spawned = std::thread::Builder::new()
        .name("st2k-video-worker".into())
        .spawn(move || {
            let _module = module;
            let r = crate::pdf::with_mta_apartment(f);
            *w_slot.lock().unwrap_or_else(|p| p.into_inner()) = r;
            // Last act, after the apartment is gone: "done" means done with Media Foundation.
            w_done.store(true, Ordering::SeqCst);
            let _ = unsafe { SetEvent(HANDLE(w_event.as_raw_handle())) };
        });
    if spawned.is_err() {
        return None;
    }
    if wait_pumping(HANDLE(event.as_raw_handle()), timeout) {
        slot.lock().unwrap_or_else(|p| p.into_inner()).take()
    } else {
        note_strand(done, what);
        None
    }
}

/// Wait for `h` up to `timeout`, dispatching this apartment's incoming COM calls while
/// waiting: an STA caller's marshaled stream reads land on this thread and MUST be served
/// for the worker to make progress, while an MTA caller simply waits. A thread with no
/// apartment at all falls back to a plain wait. `true` when the handle was signaled in time.
pub(super) fn wait_pumping(h: HANDLE, timeout: Duration) -> bool {
    let ms = timeout.as_millis().min(u32::MAX as u128) as u32;
    let flags = (COWAIT_DEFAULT | COWAIT_DISPATCH_CALLS).0 as u32;
    let waited = unsafe { CoWaitForMultipleHandles(flags, ms, &[h]) };
    match waited {
        Ok(_signaled_index) => true,
        Err(e) if e.code() == RPC_S_CALLPENDING => false,
        Err(_) => (unsafe { WaitForSingleObject(h, ms) }) == WAIT_OBJECT_0,
    }
}

/// Wrap `inner` (an `IStream` valid on the current thread) in a block-caching stream and grab.
/// The block stream carries a [`VIDEO_TIMEOUT`] wall-clock deadline so its source reads are
/// bounded even when this runs inline (no worker thread).
pub(super) unsafe fn grab_block_stream(
    inner: IStream,
    size: u64,
    seek: Seek,
) -> Option<DynamicImage> {
    let _session = MfSession::start()?;
    let deadline = std::time::Instant::now() + VIDEO_TIMEOUT;
    let bcs: IStream = crate::vstream::BlockCacheStream::new(inner, size, deadline).into();
    let bs = MFCreateMFByteStreamOnStream(&bcs).ok()?;
    let attrs = grab_attrs()?;
    let reader = MFCreateSourceReaderFromByteStream(&bs, &attrs).ok()?;
    grab_reader(&reader, seek)
}

/// Grab one frame at `frac` (a fraction of the duration) over the block-caching path, from a
/// PATH rather than a shell `IStream` — a file-backed `IStream` opened on the worker, so no
/// Global-Interface-Table marshaling is needed. Mirrors `frame_from_block_stream`'s decode.
///
/// Was `#[cfg(test)]` until 2026-09-08, when the Quick preview's video Save-frame button became
/// its first real caller: the viewer knows the on-screen position, and this turns that position
/// into the exact frame the user is looking at. Everything below (the delay-load gate, the
/// budget, the byte ceiling) already held for the test caller and holds identically here.
pub fn frame_from_block_stream_file(path: &str, frac: f64) -> Option<DynamicImage> {
    // Media Foundation is delay-loaded; calling into it when absent would raise a
    // structured exception under `panic = "abort"`. See `media_foundation_available`, and
    // `mf_usable` for the wedged-host half of the gate (issue #35).
    if !mf_usable() {
        return None;
    }
    use windows::Win32::System::Com::{STATFLAG_NONAME, STATSTG, STGM_READ};
    use windows::Win32::UI::Shell::SHCreateStreamOnFileEx;
    let owned = path.to_string();
    grab_budgeted(move || unsafe {
        let inner =
            SHCreateStreamOnFileEx(&HSTRING::from(owned.as_str()), STGM_READ.0, 0, false, None)
                .ok()?;
        let mut stat = STATSTG::default();
        inner.Stat(&mut stat, STATFLAG_NONAME).ok()?;
        grab_block_stream(
            inner,
            stat.cbSize,
            Seek {
                frac,
                cap_hns: None,
            },
        )
    })
}
