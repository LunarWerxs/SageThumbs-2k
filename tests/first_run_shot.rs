//! The first-run welcome window renders in BOTH of its shapes.
//!
//! A portable copy gets one extra row — the Explorer-thumbnails offer — because that is the
//! single switch a zip cannot turn on for itself. It shipped after a tester reported "thumbnails
//! are not working" on the portable build: 1.8.1 made them possible, but nothing on screen said
//! so, and the welcome window actively said the opposite (`fr_intro` claims thumbnails are
//! ALREADY being added, which is true of an installed copy and false of an unpacked zip).
//!
//! This drives the documented headless harness (`--shot --window firstrun`) rather than poking
//! at layout constants, so it fails the same way a user would see it fail. Portable mode is
//! forced with `ST2K_PORTABLE_INI`, the same override `tests/portable_settings.rs` uses — the
//! shot path only READS settings, so nothing is registered and no window is ever shown.
//!
//! Needs a window station (real GDI + `PrintWindow`), like the other headless shot tooling.

use std::path::PathBuf;
use std::process::Command;

/// Big-endian `u32` at `off` — PNG stores IHDR width/height that way.
fn be32(b: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

/// `(width, height)` from a PNG's IHDR, which always sits at a fixed offset.
fn png_size(bytes: &[u8]) -> (u32, u32) {
    assert!(bytes.len() > 24, "not a PNG (too short: {} B)", bytes.len());
    assert_eq!(&bytes[1..4], b"PNG", "not a PNG");
    (be32(bytes, 16), be32(bytes, 20))
}

/// A directory of this CASE's own. Per-case rather than per-process because cargo runs the
/// tests in this file on parallel threads, and a shared directory means one test's cleanup
/// deletes the other's output from under it.
fn scratch(case: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("st2k_firstrun_{}_{case}", std::process::id()));
    std::fs::create_dir_all(&d).expect("scratch dir");
    d
}

/// Capture the welcome window. `portable` points the settings layer at a throwaway ini, which
/// is the ENTIRE switch that makes a build portable — see `settings.rs`.
fn shot(case: &str, portable: bool) -> Vec<u8> {
    let dir = scratch(case);
    let out = dir.join(format!("{case}.png"));
    let _ = std::fs::remove_file(&out);

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_SageThumbs2K"));
    cmd.arg("--shot").arg(&out).args(["--window", "firstrun"]);
    if portable {
        // Must not exist as a real settings store with content; an absent file is a valid
        // portable ini (every getter answers with its default).
        cmd.env("ST2K_PORTABLE_INI", dir.join("SageThumbs2K.ini"));
    } else {
        // A parent shell that already had it set would otherwise leak into the "installed" case.
        cmd.env_remove("ST2K_PORTABLE_INI");
    }
    let status = cmd.status().expect("spawn SageThumbs2K --shot");
    assert!(
        status.success(),
        "{case} shot failed: exit {:?} (0xC000041D = abort())",
        status.code()
    );
    let bytes = std::fs::read(&out).unwrap_or_else(|e| panic!("{case} wrote no PNG: {e}"));
    assert!(!bytes.is_empty(), "{case} wrote an empty PNG");
    bytes
}

/// Both shapes render, and the portable one is genuinely TALLER — the thumbnails row is real
/// layout, not a control created off the bottom of a fixed-height window where nobody can see
/// or reach it.
#[test]
fn portable_welcome_adds_the_thumbnails_row() {
    let installed = shot("installed", false);
    let portable = shot("portable", true);

    let (iw, ih) = png_size(&installed);
    let (pw, ph) = png_size(&portable);

    assert_eq!(iw, pw, "width should not change between the two shapes");
    assert!(
        ph > ih,
        "the portable welcome must be taller to fit the thumbnails row \
         (installed {iw}x{ih}, portable {pw}x{ph}) — if these match, `dlg_h()` is not \
         reacting to settings::portable() and the row is being drawn off-window"
    );
    assert_ne!(
        installed, portable,
        "the two shapes rendered identically — the portable copy is still showing the \
         installed intro ('thumbnails are already being added'), which is the exact false \
         claim this row exists to correct"
    );

    let _ = std::fs::remove_dir_all(scratch("installed"));
    let _ = std::fs::remove_dir_all(scratch("portable"));
}

/// Control: the installed shape is unchanged by the flag being absent vs the window simply
/// being built twice. If this fails the harness is non-deterministic and the assertions above
/// prove nothing.
#[test]
fn installed_welcome_is_stable_across_runs() {
    let a = shot("stable_a", false);
    let b = shot("stable_b", false);
    assert_eq!(
        png_size(&a),
        png_size(&b),
        "the welcome window changed size between two identical runs"
    );
    let _ = std::fs::remove_dir_all(scratch("stable_a"));
    let _ = std::fs::remove_dir_all(scratch("stable_b"));
}
