use std::ffi::c_void;
use std::sync::OnceLock;

// kernel32 is always linked; declaring these here avoids enabling the `windows`
// crate's `Win32_System_Threading` feature just for three calls (kept off
// deliberately — see the CREATE_NO_WINDOW note in lib.rs).
#[link(name = "kernel32")]
extern "system" {
    fn CreateSemaphoreW(
        attrs: *const c_void,
        initial: i32,
        max: i32,
        name: *const u16,
    ) -> *mut c_void;
    fn WaitForSingleObject(handle: *mut c_void, millis: u32) -> u32;
    fn ReleaseSemaphore(handle: *mut c_void, count: i32, prev: *mut i32) -> i32;
}

/// Max concurrent magick children. 4 × ~512 MiB ≈ 2 GiB worst case — safe on any
/// modern machine, still ~4× faster than serial on the exotic long tail.
const MAX: i32 = 4;
/// Bounded acquire deadline (ms) for a THUMBNAIL caller. A LEAKED permit — a host
/// process hard-killed mid-decode never runs `Permit::drop`, and Windows does NOT
/// restore a semaphore count when a holder dies (semaphores have no abandoned-state,
/// unlike a mutex) — would otherwise wedge the gate to 0 for the whole logon session,
/// so every later magick decode blocks forever (a must-kill/reboot hang in
/// prevhost/dllhost). With a finite wait we fall back to UNCAPPED instead of blocking
/// the calling (often a shell/host) thread indefinitely. 5s is ample for a slot to free
/// on that tier (its decode is ≤20s of CPU but usually <3s) yet self-heals fast.
const GATE_WAIT_MS: u32 = 5_000;
/// The same deadline for a FULL-FIDELITY caller, which is a different trade in both
/// directions: its decode can legitimately hold a slot for a minute or more (see
/// `magick::Fidelity`), so a 5 s wait would send every sibling in a Convert batch
/// straight past the cap and run them all UNCAPPED — the memory bound this gate exists
/// for, gone exactly when the documents are largest. And it is never a shell thread:
/// it is a Convert/Resize worker in our own EXE, behind a progress dialog with a Cancel
/// button, so waiting is cheap where blocking Explorer would not be. Still finite, so a
/// leaked permit self-heals here too.
const FULL_FIDELITY_GATE_WAIT_MS: u32 = 90_000;
const WAIT_OBJECT_0: u32 = 0;

/// The shared semaphore handle (created once, kept for the process lifetime —
/// the OS reclaims it on exit). Stored as `usize` because the raw `HANDLE`
/// pointer is not `Send`/`Sync`.
fn handle() -> Option<*mut c_void> {
    static H: OnceLock<usize> = OnceLock::new();
    let h = *H.get_or_init(|| {
        // A stable Local\ name → per-logon-session sharing across every process
        // (the DLL + all the st2k.exe children it spawns). An anonymous (null
        // name) semaphore would NOT be shared, defeating the cross-process cap.
        let name: Vec<u16> = "Local\\SageThumbs2K_MagickGate\0".encode_utf16().collect();
        unsafe { CreateSemaphoreW(std::ptr::null(), MAX, MAX, name.as_ptr()) as usize }
    });
    (h != 0).then_some(h as *mut c_void)
}

/// Held while a magick child runs; releases one slot on drop.
pub(crate) struct Permit(*mut c_void);
impl Drop for Permit {
    fn drop(&mut self) {
        unsafe { ReleaseSemaphore(self.0, 1, std::ptr::null_mut()) };
    }
}

/// Acquire a magick slot, waiting at most this caller's deadline ([`GATE_WAIT_MS`] for a
/// tile, [`FULL_FIDELITY_GATE_WAIT_MS`] for a user-chosen decode). Returns `None` if the
/// semaphore couldn't be created, the wait timed out, or it otherwise failed — in
/// every such case the caller proceeds UNCAPPED (best-effort: a missing or wedged
/// cap must never block decoding, only bound its memory). A genuine permit is always
/// released on drop; a timed-out wait acquired nothing, so there is nothing to
/// release. This finite wait is what prevents a leaked permit (see [`GATE_WAIT_MS`])
/// from turning into an indefinite host-process hang.
pub(crate) fn acquire_for(fidelity: super::Fidelity) -> Option<Permit> {
    let ms = match fidelity {
        super::Fidelity::Tile => GATE_WAIT_MS,
        super::Fidelity::Full => FULL_FIDELITY_GATE_WAIT_MS,
    };
    let h = handle()?;
    (unsafe { WaitForSingleObject(h, ms) } == WAIT_OBJECT_0).then(|| Permit(h))
}
