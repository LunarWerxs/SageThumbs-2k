//! The off-thread, budgeted menu-preview decode: worker accounting and the decode itself.
//!
//! Split out of `contextmenu.rs` 2026-07-31 (pure move).

use super::*;

/// The `Send` result of the off-thread menu-preview decode: the scaled RGBA thumbnail (the GDI
/// DIB is created on the caller's UI thread) plus the file's true source dimensions.
pub(crate) struct MenuThumb {
    pub(crate) rgba: Vec<u8>,
    pub(crate) w: i32,
    pub(crate) h: i32,
    pub(crate) ow: u32,
    pub(crate) oh: u32,
}

/// Wall-clock budget for the off-thread menu-preview decode. A shell menu callback must feel
/// immediate; if the cheap decoder cannot finish inside this small allowance, show the
/// caption-only tile instead of making Explorer wait.
pub(crate) const MENU_PREVIEW_BUDGET: std::time::Duration = std::time::Duration::from_millis(125);
/// Timed-out workers finish in the background. Bound their count so repeated right-clicks on a
/// pathological image cannot accumulate an unbounded number of decoders inside Explorer.
pub(crate) const MAX_MENU_PREVIEW_WORKERS: usize = 2;

/// How long one detached worker may hold its slot before the slot is reclaimed for a new
/// preview request. This used to be a plain counter released only by the worker's own
/// `Drop`, which never runs for a worker blocked forever (a OneDrive online-only
/// placeholder, a dropped SMB share stalling `std::fs::read`) - two such hangs would
/// permanently disable the in-menu preview for the rest of the process's life. A lease
/// makes the failure self-healing instead of terminal: the hung thread keeps running in
/// the background, but its slot becomes claimable again once the lease expires. Mirrors
/// `propstore::PROBE_LEASE_MS`/`acquire_probe_slot`, the same fix for the same shape of bug.
///
/// `usize`, not `u64`: this crate ships x64/ARM64 only (no 32-bit target), so `usize` is
/// always 64-bit here, and reusing it keeps the slot array on the same atomic type
/// `contextmenu.rs` already imports rather than pulling in a second one for this file alone.
const MENU_PREVIEW_LEASE_MS: usize = 30_000;

/// Lease expiry per slot, in [`safety::elapsed_ms`] units (truncated to `usize`, see
/// [`MENU_PREVIEW_LEASE_MS`]). `0` = free.
static MENU_PREVIEW_SLOTS: [AtomicUsize; MAX_MENU_PREVIEW_WORKERS] =
    [const { AtomicUsize::new(0) }; MAX_MENU_PREVIEW_WORKERS];

/// Claim a slot, returning its index. `None` when every slot holds an unexpired lease.
/// Pure in `now_ms` so the policy is unit-testable without sleeping.
fn acquire_menu_preview_slot(now_ms: usize) -> Option<usize> {
    let expiry = now_ms.saturating_add(MENU_PREVIEW_LEASE_MS);
    for (i, slot) in MENU_PREVIEW_SLOTS.iter().enumerate() {
        let held = slot.load(Ordering::Acquire);
        // Free, or the previous holder's lease has run out and we may take it over.
        if (held == 0 || held <= now_ms)
            && slot
                .compare_exchange(held, expiry, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            return Some(i);
        }
    }
    None
}

/// Release a slot claimed by [`acquire_menu_preview_slot`], unless the lease already
/// expired and another request took it over (in which case `expiry` no longer matches
/// and we must not clear someone else's claim).
fn release_menu_preview_slot(index: usize, expiry: usize) {
    if let Some(slot) = MENU_PREVIEW_SLOTS.get(index) {
        let _ = slot.compare_exchange(expiry, 0, Ordering::AcqRel, Ordering::Acquire);
    }
}

/// Whether `len` bytes is small enough for the in-process menu-preview worker to read -
/// the same two-part budget `contextmenu::build_preview` checks before composing the
/// tile at all, re-applied here immediately before the worker's own read so a file that
/// grows or gets replaced after that first gate can't slip an oversized read in.
fn preview_len_ok(len: u64) -> bool {
    len <= PREVIEW_MAX_BYTES && len <= settings::max_file_size_bytes()
}

pub(crate) struct MenuPreviewWorker {
    index: usize,
    expiry: usize,
}

impl Drop for MenuPreviewWorker {
    fn drop(&mut self) {
        release_menu_preview_slot(self.index, self.expiry);
    }
}

/// Start reading + decoding `path` to a scaled menu thumbnail on a detached worker. Mirrors
/// `propstore::probe_budgeted` / `decode_svg`: the worker holds a `crate::ModuleRef` and inits
/// COM (the WIC HEIC/AVIF/RAW tier needs an apartment). Uses only the cheap in-process tiers
/// (`decode_menu_preview` — container covers, fast image/WIC tiers, and pure-Rust resvg for
/// SVG; no magick/video/pdf), so the worker is fast and bundled-byte-free.
pub(crate) fn start_menu_thumb(path: &str) -> Option<std::sync::mpsc::Receiver<Option<MenuThumb>>> {
    let now = safety::elapsed_ms() as usize;
    let index = acquire_menu_preview_slot(now)?;
    let expiry = now.saturating_add(MENU_PREVIEW_LEASE_MS);

    let path = path.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::Builder::new()
        .name("st2k-menu-preview".into())
        .spawn(move || {
            let _worker = MenuPreviewWorker { index, expiry };
            #[allow(clippy::default_constructed_unit_structs)]
            let _module = crate::ModuleRef::default();
            let inited = unsafe {
                windows::Win32::System::Com::CoInitializeEx(
                    None,
                    windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
                )
            }
            .is_ok();
            let out = (|| {
                // Re-check size right before the read, not just once back in Initialize: a
                // file that grows or gets replaced between that gate and this worker running
                // (download in progress, rename onto a bigger file) must not be read in full
                // into explorer.exe unbounded. Same two-part budget `build_preview` re-checks.
                let meta = std::fs::metadata(&path).ok()?;
                if !preview_len_ok(meta.len()) {
                    return None;
                }
                let bytes = std::fs::read(&path).ok()?;
                let img = crate::decode::decode_menu_preview(&bytes).ok()?;
                let (ow, oh) =
                    crate::container::real_dims(&bytes).unwrap_or((img.width(), img.height()));
                // Width up to PREVIEW_WIDE, height up to PREVIEW_BOX: wide images render wide,
                // normal/tall ones stay capped at the 88px height.
                //
                // SHRINKING goes through the one shared reduction, so this tile is the same
                // picture the thumbnail provider draws instead of a second, softer filter.
                // ENLARGING deliberately stays on `DynamicImage::thumbnail`: the shared
                // reduction never enlarges, so routing the small case through it would leave a
                // 32px icon drawn 32px wide in a menu that has always filled the 88px cell.
                // That is a visible layout change, not a quality one, so it is not smuggled in
                // with a filter swap.
                let thumb = if img.width() > PREVIEW_WIDE || img.height() > PREVIEW_BOX {
                    crate::decode::reduce_to_fit(img, PREVIEW_WIDE, PREVIEW_BOX)
                } else {
                    img.thumbnail(PREVIEW_WIDE, PREVIEW_BOX)
                };
                let rgba = thumb.to_rgba8();
                let (w, h) = (rgba.width() as i32, rgba.height() as i32);
                Some(MenuThumb {
                    rgba: rgba.into_raw(),
                    w,
                    h,
                    ow,
                    oh,
                })
            })();
            if inited {
                unsafe { windows::Win32::System::Com::CoUninitialize() };
            }
            let _ = tx.send(out);
        });
    if worker.is_err() {
        release_menu_preview_slot(index, expiry);
        return None;
    }
    Some(rx)
}

/// Finish a previously-started decode, or start one on demand for diagnostic
/// callers. The shell path normally supplies a prefetched receiver, hiding most
/// or all of this bounded wait behind Explorer's own menu construction.
pub(crate) fn decode_menu_thumb_budgeted(
    path: &str,
    prefetched: Option<std::sync::mpsc::Receiver<Option<MenuThumb>>>,
) -> Option<MenuThumb> {
    let rx = prefetched.or_else(|| start_menu_thumb(path))?;
    rx.recv_timeout(MENU_PREVIEW_BUDGET).ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The slots must be a LEASE, not a permanent claim. Two files whose reads hang forever
    /// (a OneDrive placeholder, a dropped SMB share) used to hold both slots for the life of
    /// the process — after that, right-click preview was silently caption-only forever, no
    /// error, no recovery. Drives the pure slot policy directly with an injected clock, so
    /// it asserts the real behaviour without sleeping or spawning a thread. Single test (like
    /// `propstore`'s sibling) so it owns the shared static slots start-to-finish; splitting it
    /// across `#[test]` fns would race under cargo's default parallel test runner.
    #[test]
    fn a_hung_workers_slot_is_reclaimed_after_its_lease_expires() {
        // Start from a clean slate - other tests in this binary don't touch the slots, but
        // be explicit so ordering can never matter.
        for slot in MENU_PREVIEW_SLOTS.iter() {
            slot.store(0, Ordering::Release);
        }
        let t0 = 1_000_000usize;

        // Fill every slot (never released - simulating permanently hung reads) and confirm
        // the cap actually holds.
        let claimed: Vec<usize> = (0..MAX_MENU_PREVIEW_WORKERS)
            .map(|_| acquire_menu_preview_slot(t0).expect("slot available"))
            .collect();
        assert_eq!(claimed.len(), MAX_MENU_PREVIEW_WORKERS);
        assert_eq!(
            acquire_menu_preview_slot(t0 + 1),
            None,
            "the concurrency cap must still bound live workers"
        );
        assert_eq!(
            acquire_menu_preview_slot(t0 + MENU_PREVIEW_LEASE_MS - 1),
            None,
            "must still be refused one millisecond before the lease is up"
        );

        // Past the lease, a fresh request must be able to take a slot over even though the
        // original "worker" never released it - the whole point of the fix.
        assert!(
            acquire_menu_preview_slot(t0 + MENU_PREVIEW_LEASE_MS + 1).is_some(),
            "an expired lease must be reclaimable, or the outage is permanent"
        );

        // A late release from the FIRST generation must not free the slot its successor now
        // owns - release is keyed to the exact lease it claimed, not just the index.
        let successor = MENU_PREVIEW_SLOTS[claimed[0]].load(Ordering::Acquire);
        release_menu_preview_slot(claimed[0], t0.saturating_add(MENU_PREVIEW_LEASE_MS));
        assert_eq!(
            MENU_PREVIEW_SLOTS[claimed[0]].load(Ordering::Acquire),
            successor,
            "a stale release must not steal the current holder's slot"
        );

        // Leave the slots clean for whichever test runs next in this binary.
        for slot in MENU_PREVIEW_SLOTS.iter() {
            slot.store(0, Ordering::Release);
        }
    }

    #[test]
    fn preview_len_ok_rejects_anything_over_the_menu_preview_cap() {
        assert!(
            preview_len_ok(1024),
            "a 1 KiB file must be well within budget"
        );
        assert!(
            !preview_len_ok(100 * 1024 * 1024),
            "a 100 MiB file must exceed the 32 MiB menu-preview cap regardless of settings"
        );
    }
}
