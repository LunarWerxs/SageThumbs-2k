use std::ffi::c_void;
use std::sync::OnceLock;

// kernel32 is always linked; declaring these here avoids enabling the `windows`
// crate's `Win32_System_Threading` feature just for four calls (kept off
// deliberately — see the CREATE_NO_WINDOW note in lib.rs).
#[link(name = "kernel32")]
extern "system" {
    fn CreateMutexW(attrs: *const c_void, initial_owner: i32, name: *const u16) -> *mut c_void;
    fn WaitForMultipleObjects(
        count: u32,
        handles: *const *mut c_void,
        wait_all: i32,
        millis: u32,
    ) -> u32;
    fn ReleaseMutex(handle: *mut c_void) -> i32;
}

/// Max concurrent magick children. 4 × ~512 MiB ≈ 2 GiB worst case — safe on any
/// modern machine, still ~4× faster than serial on the exotic long tail.
const MAX: usize = 4;
/// Bounded acquire deadline (ms) for a THUMBNAIL caller: a tile must never block the shell's
/// thread for long on a gate that is busy with LIVE decodes. Past it the caller proceeds
/// UNCAPPED. 5s is ample for a slot to free on that tier (its decode is ≤20s of CPU but
/// usually <3s).
const GATE_WAIT_MS: u32 = 5_000;
/// The same deadline for a FULL-FIDELITY caller, which is a different trade in both
/// directions: its decode can legitimately hold a slot for a minute or more (see
/// `magick::Fidelity`), so a 5 s wait would send every sibling in a Convert batch
/// straight past the cap and run them all UNCAPPED — the memory bound this gate exists
/// for, gone exactly when the documents are largest. And it is never a shell thread:
/// it is a Convert/Resize worker in our own EXE, behind a progress dialog with a Cancel
/// button, so waiting is cheap where blocking Explorer would not be.
const FULL_FIDELITY_GATE_WAIT_MS: u32 = 90_000;
const WAIT_ABANDONED_0: u32 = 0x80;
/// The slots' shared name stem, suffixed 0..MAX. `Local\` = one gate per logon session,
/// shared by every process (the DLL in each shell host, the EXE, the st2k children).
const SLOT_NAME: &str = "Local\\SageThumbs2K_MagickSlot";

/// The gate is MAX named MUTEXES, one per slot, never a counting semaphore. A semaphore's
/// count is not given back when its holder dies, and the shell kills its thumbnail host
/// whenever an extraction runs past its patience — mid-decode, with a permit held. Four such
/// kills left the old semaphore at zero for as long as ANY process kept a handle to it (the
/// resident helper, Explorer itself after a menu preview), so from then on every magick tile
/// waited its full five seconds, which made the shell time out and kill the host again: the
/// "after a few minutes all thumbnails stop, only a restart fixes it" report (2026-09-27).
/// A mutex whose owner dies is ABANDONED, and Windows hands it to the next waiter, so a dead
/// holder costs nothing. A mutex belongs to the thread that took it, which suits every caller:
/// each holds its [`Permit`] on the acquiring thread until the child is reaped, and `Permit`
/// is not `Send`, so the compiler keeps it there.
pub(crate) struct Gate {
    slots: [usize; MAX],
}

impl Gate {
    /// Open (or create) the slot mutexes named `stem0..stem{MAX-1}`. `None` when any of them
    /// cannot be created, in which case callers run uncapped.
    fn open(stem: &str) -> Option<Gate> {
        let mut slots = [0usize; MAX];
        for (i, slot) in slots.iter_mut().enumerate() {
            let name: Vec<u16> = format!("{stem}{i}\0").encode_utf16().collect();
            let h = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
            if h.is_null() {
                return None;
            }
            *slot = h as usize;
        }
        Some(Gate { slots })
    }

    /// Take any free slot within `ms`. A slot whose holder died is taken like a free one.
    fn acquire(&self, ms: u32) -> Option<Permit> {
        let handles: [*mut c_void; MAX] = self.slots.map(|h| h as *mut c_void);
        let r = unsafe { WaitForMultipleObjects(MAX as u32, handles.as_ptr(), 0, ms) };
        let i = match r {
            // WAIT_OBJECT_0 is 0, so a free slot's index is the return value itself.
            r if r < MAX as u32 => r,
            r if (WAIT_ABANDONED_0..WAIT_ABANDONED_0 + MAX as u32).contains(&r) => {
                st2k_base::safety::log_debugf!(
                    "magick gate: reclaimed a slot whose holder died mid-decode"
                );
                r - WAIT_ABANDONED_0
            }
            _ => return None,
        };
        Some(Permit(handles[i as usize]))
    }
}

/// The process-wide gate (created once, kept for the process lifetime — the OS reclaims
/// the handles on exit).
fn gate() -> Option<&'static Gate> {
    static G: OnceLock<Option<Gate>> = OnceLock::new();
    G.get_or_init(|| Gate::open(SLOT_NAME)).as_ref()
}

/// Held while a magick child runs; releases its slot on drop.
pub(crate) struct Permit(*mut c_void);
impl Drop for Permit {
    fn drop(&mut self) {
        unsafe { ReleaseMutex(self.0) };
    }
}

/// Acquire a magick slot, waiting at most this caller's deadline ([`GATE_WAIT_MS`] for a
/// tile, [`FULL_FIDELITY_GATE_WAIT_MS`] for a user-chosen decode). Returns `None` if the
/// slots couldn't be created, the wait timed out, or it otherwise failed — in every such
/// case the caller proceeds UNCAPPED (best-effort: a missing or busy cap must never block
/// decoding, only bound its memory).
pub(crate) fn acquire_for(fidelity: super::Fidelity) -> Option<Permit> {
    let ms = match fidelity {
        super::Fidelity::Tile => GATE_WAIT_MS,
        super::Fidelity::Full => FULL_FIDELITY_GATE_WAIT_MS,
    };
    gate()?.acquire(ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The regression: holders that die without ever releasing (a shell host killed
    /// mid-decode) must not shrink the gate. Every slot is taken by a thread that then
    /// exits without dropping its permit, and the next caller must still get one at once.
    /// On the old counting semaphore this waited out the whole deadline and got nothing.
    #[test]
    fn slots_whose_holders_died_are_reclaimed_immediately() {
        let stem = format!("Local\\SageThumbs2K_MagickSlotTest{}_", std::process::id());
        let gate = Gate::open(&stem).expect("slot mutexes");
        let slots = gate.slots;
        // One holder per slot (a thread re-taking a mutex it owns gets the same one back),
        // all holding at once, then all exiting without a release.
        let all_held = std::sync::Arc::new(std::sync::Barrier::new(MAX));
        let holders: Vec<_> = (0..MAX)
            .map(|_| {
                let all_held = all_held.clone();
                std::thread::spawn(move || {
                    let g = Gate { slots };
                    std::mem::forget(g.acquire(0).expect("a free slot"));
                    all_held.wait();
                })
            })
            .collect();
        for h in holders {
            h.join().unwrap();
        }
        let t = std::time::Instant::now();
        let permit = gate.acquire(2_000);
        assert!(permit.is_some(), "a dead holder's slot must be reclaimed");
        assert!(
            t.elapsed() < std::time::Duration::from_millis(1_000),
            "reclaiming must not wait out the deadline"
        );
    }
}
