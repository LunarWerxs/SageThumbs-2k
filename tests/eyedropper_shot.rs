//! The `--shot --window eyedropper` harness must finish and write a PNG.
//!
//! It runs the SAME window procedure and paint path as the live colour picker, with the loupe
//! parked at a real (non-sentinel) position so the very first paint draws the magnifier. That
//! paint deadlocked the message thread in 2.5.0: `eye_paint` held the snapshot mutex for the
//! whole `if let` body while `eye_draw_loupe` -> `eye_sample` re-locked it, so the picker froze
//! on its first mouse move and this harness never returned (2026-09-05 audit, F26). A deadlock
//! does not FAIL a test, it hangs it, so the child is killed at a bound and the bound is the
//! assertion. The existing snapshot lifecycle unit tests never reach this nested lock.
//!
//! The capture is of whatever the primary monitor shows, so nothing about its pixels is
//! asserted beyond "a PNG was written"; the existence of the file is what proves the paint
//! path ran to completion.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn the_eyedropper_shot_paints_its_loupe_and_exits() {
    let out: PathBuf =
        std::env::temp_dir().join(format!("st2k_eyedropper_shot_{}.png", std::process::id()));
    let _ = std::fs::remove_file(&out);

    let mut child = Command::new(env!("CARGO_BIN_EXE_SageThumbs2K"))
        .arg("--shot")
        .arg(&out)
        .args(["--window", "eyedropper"])
        .stdin(Stdio::null())
        .spawn()
        .expect("spawn SageThumbs2K --shot --window eyedropper");

    // Generous for a debug build on a loaded machine, tiny next to "forever".
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break Some(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let status = status.expect(
        "the eyedropper shot hung past its bound: the loupe paint path is deadlocked (F26)",
    );
    assert!(
        status.success(),
        "eyedropper shot failed: exit {:?} (0xC000041D = abort(), e.g. a panic under panic=abort)",
        status.code()
    );
    let bytes = std::fs::read(&out).expect("the shot exited clean but wrote no PNG");
    assert!(
        bytes.starts_with(&[0x89, b'P', b'N', b'G']),
        "the written file is not a PNG ({} bytes)",
        bytes.len()
    );
    let _ = std::fs::remove_file(&out);
}
