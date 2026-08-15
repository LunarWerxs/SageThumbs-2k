//! The hidden video-decode child verbs of `st2k` — codecs Windows cannot decode, decoded
//! by pure-Rust crates that are not allowed inside the shell extension:
//!
//! * `st2k flv-frame` ([`flv`], feature `flash-video`): FLV bytes on STDIN → the first
//!   VP6/Sorenson-Spark keyframe as a PNG on STDOUT (nihav + h263-rs, the Ruffle Flash
//!   codecs — issue #26's first half).
//! * `st2k vp9-frame` ([`vp9`], feature `vp9-video`): one raw VP9 keyframe on STDIN → a
//!   PNG on STDOUT (`vp9dec` — Profile 2/3 10/12-bit, which Media Foundation cannot
//!   decode at all; issue #26's last open codec).
//!
//! Both are the CHILD side of a `sagethumbs2k_core` spawner (`flv::flash_frame` /
//! `vp9::vp9_frame`, sharing one harness: `flv::child_frame_png`). The decoders here
//! either PANIC on malformed input (nihav/h263) or carry a 0.1.x-sized unsafe surface
//! (vp9dec's SIMD), and the workspace builds `panic = "abort"`, so they may only ever run
//! in this throwaway process: a crash is a non-zero exit the parent shrugs at, never a
//! dead Explorer. That is also why these modules are compiled ONLY into the `st2k`
//! console binary behind EXE-only features — `cargo tree -p sagethumbs2k-dll` must never
//! list nihav, h263, or vp9dec (mirrors the webview2 arrangement).
//!
//! Stdin/stdout on purpose (matching the ImageMagick child): a decoded frame of someone's
//! video never touches the disk.

use std::io::{Read, Write};

#[cfg(feature = "flash-video")]
mod flv;
#[cfg(feature = "vp9-video")]
mod vp9;

/// Hard cap on how much memory this child may commit, enforced by Windows itself.
///
/// A subprocess contains a CRASH. It does not, on its own, contain a MEMORY BOMB: a child
/// quietly committing tens of gigabytes hurts the whole machine long before the CPU watchdog
/// notices, because it burns almost no CPU doing it.
///
/// And that is reachable. h263-rs allocates the full luma + chroma planes the moment it parses
/// a picture header, straight from a 16-bit custom width/height that a crafted Sorenson header
/// may declare as 65535x65535 — roughly 8.6 GB from a few hundred bytes of input, before the
/// FLV module's own `check_dims` ever gets to run. (vp9dec fares better — it refuses headers
/// past 2^27 luma samples before allocating, and the vp9 module pre-parses the header dims
/// itself — but this cap is the guard that needs no audit of either.)
///
/// The obvious fix, pre-parsing those dimensions ourselves and refusing early, is the WRONG
/// one for h263: the size fields sit at a bit offset that depends on `recognize_start_code`'s
/// scan for the picture start code, so a pre-check means re-implementing their bitstream
/// search and getting it exactly right forever. Instead this bounds the whole process by
/// mechanism, which also covers every other allocation site in every decoder here — including
/// the ones nobody has audited, which is the point.
///
/// Over the limit, the commit fails, Rust's allocator error handler aborts the child, and the
/// parent sees a dead child and falls back to the stock icon: the same outcome as any other
/// decode failure. 512 MiB is far above a legitimate frame and far below anything that
/// troubles the machine.
const CHILD_MEMORY_CAP: usize = 512 * 1024 * 1024;

/// Put THIS process under a job-object memory cap, before a single byte of input is read.
///
/// Self-assignment is deliberate: capping from the parent would leave a window between
/// `CreateProcess` and `AssignProcessToJobObject` in which the child is already running and
/// could allocate, and `std::process::Command` cannot spawn suspended. Doing it here, first
/// thing, closes that window entirely. Best-effort: on the (unexpected) failure path we carry
/// on, because a missing cap is a smaller problem than refusing to thumbnail anything.
fn cap_own_memory() {
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_PROCESS_MEMORY,
    };
    use windows::Win32::System::Threading::GetCurrentProcess;

    unsafe {
        let Ok(job) = CreateJobObjectW(None, None) else {
            return;
        };
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_PROCESS_MEMORY;
        info.ProcessMemoryLimit = CHILD_MEMORY_CAP;
        let ok = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            core::ptr::addr_of!(info).cast(),
            u32::try_from(core::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                .unwrap_or(0),
        )
        .is_ok();
        if ok {
            let _ = AssignProcessToJobObject(job, GetCurrentProcess());
        }
    }
}

/// The shared child shell: memory cap FIRST, then capped stdin → `frame_png` → stdout.
/// Exit 0 with output, non-zero with none. Both verbs run through this so the containment
/// mechanics (cap-before-read, over-cap detection, binary stdout) cannot drift apart.
fn run_child(verb: &str, input_cap: usize, frame_png: fn(&[u8]) -> Result<Vec<u8>, String>) -> i32 {
    // FIRST, before any input is read: see `CHILD_MEMORY_CAP`.
    cap_own_memory();
    let mut input = Vec::new();
    // Cap + 1 so an over-cap stream is detected as such instead of being silently
    // truncated into a "valid" but different file.
    if std::io::stdin()
        .lock()
        .take((input_cap + 1) as u64)
        .read_to_end(&mut input)
        .is_err()
    {
        eprintln!("st2k {verb}: could not read stdin");
        return 1;
    }
    if input.len() > input_cap {
        eprintln!("st2k {verb}: input exceeds the {input_cap}-byte cap");
        return 1;
    }
    match frame_png(&input) {
        Ok(png) => {
            let mut out = std::io::stdout().lock();
            if out.write_all(&png).and_then(|()| out.flush()).is_err() {
                return 1;
            }
            0
        }
        Err(why) => {
            eprintln!("st2k {verb}: {why}");
            1
        }
    }
}

/// Entry point for `st2k flv-frame`: returns the process exit code.
#[cfg(feature = "flash-video")]
pub fn run_flv() -> i32 {
    run_child(
        "flv-frame",
        sagethumbs2k_core::flv::FLASH_INPUT_CAP,
        flv::frame_png,
    )
}

/// Entry point for `st2k vp9-frame`: returns the process exit code.
#[cfg(feature = "vp9-video")]
pub fn run_vp9() -> i32 {
    run_child(
        "vp9-frame",
        sagethumbs2k_core::vp9::VP9_INPUT_CAP,
        vp9::frame_png,
    )
}
