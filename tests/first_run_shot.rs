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

use windows_registry::CURRENT_USER;

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
    shot_window(case, portable, "firstrun")
}

/// [`shot`] with the page chosen: `firstrun` is page 1, `firstrun2` flips to page 2 first.
fn shot_window(case: &str, portable: bool, window: &str) -> Vec<u8> {
    let dir = scratch(case);
    let out = dir.join(format!("{case}.png"));
    let _ = std::fs::remove_file(&out);

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_SageThumbs2K"));
    cmd.arg("--shot").arg(&out).args(["--window", window]);
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

/// Page 2 carries THREE opt-ins now, which is one more than `DLG_H` was sized for, so
/// `flip_to_page2` grows the window. If that resize is ever dropped, the third row and the
/// button end up sharing the same strip of pixels — and because the controls are still
/// created, nothing errors and no other test notices. The height IS the assertion.
#[test]
fn page_two_grows_to_fit_its_third_opt_in() {
    let page1 = shot_window("p1", false, "firstrun");
    let page2 = shot_window("p2", false, "firstrun2");

    let (w1, h1) = png_size(&page1);
    let (w2, h2) = png_size(&page2);

    assert_eq!(w1, w2, "width must not change between pages");
    assert!(
        h2 > h1,
        "page 2 must be taller than page 1 to fit its third opt-in \
         (page 1 {w1}x{h1}, page 2 {w2}x{h2}) — equal heights mean `grow_for_page2` is not \
         running and the last row is drawn under the Get started button"
    );

    let _ = std::fs::remove_dir_all(scratch("p1"));
    let _ = std::fs::remove_dir_all(scratch("p2"));
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

// ---- Locale + DPI coverage (2026-09-05 audit finding F36) ---------------------------
//
// The portable-mode explanation (`fr_intro_portable`, or `fr_intro` on an installed copy)
// used to get a flat 34px box no matter which language was active; see `first_run.rs`'s
// `intro_h` for the measurement that replaced it. These captures are the acceptance bar
// itself: French and German (two of the longer-running shipped translations) at 96 AND 192
// DPI, non-portable so the window shows `fr_intro`, the exact string the finding names.

/// Throwaway HKCU subkey this file's language override writes to, named by case + this TEST
/// PROCESS's pid so parallel `cargo test` runs and other test binaries never collide, never
/// the real `Software\SageThumbs2K` a developer's own Explorer reads (same isolation
/// `tests/settings_gate.rs` uses for its own scratch key).
fn scratch_reg_root(case: &str) -> String {
    format!(
        r"Software\SageThumbs2K\__test_firstrunshot_{}_{case}",
        std::process::id()
    )
}

/// [`shot_window`], but with `lang` (a shipped locale code) forced via a scratch HKCU key and
/// `dpi` forced via the `--dpi` override `main.rs::run_shot_mode` now applies to every
/// `--shot` window (previously wired for `preview` only). Always non-portable, so the
/// window shows `fr_intro`, not `fr_intro_portable`.
fn shot_locale(case: &str, lang: &str, dpi: u32) -> Vec<u8> {
    let root = scratch_reg_root(case);
    CURRENT_USER
        .create(&root)
        .and_then(|k| k.set_string("Lang", lang))
        .unwrap_or_else(|e| panic!("{case}: failed to write scratch Lang={lang}: {e}"));

    let dir = scratch(case);
    let out = dir.join(format!("{case}.png"));
    let _ = std::fs::remove_file(&out);

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_SageThumbs2K"));
    cmd.arg("--shot")
        .arg(&out)
        .args(["--window", "firstrun", "--dpi", &dpi.to_string()])
        .env("ST2K_SETTINGS_ROOT", &root)
        // A parent shell with a leaked portable ini would otherwise force portable mode
        // (and `fr_intro_portable`) regardless of the HKCU override above.
        .env_remove("ST2K_PORTABLE_INI");
    let status = cmd.status().expect("spawn SageThumbs2K --shot");
    assert!(
        status.success(),
        "{case} shot failed: exit {:?} (0xC000041D = abort())",
        status.code()
    );
    let bytes = std::fs::read(&out).unwrap_or_else(|e| panic!("{case} wrote no PNG: {e}"));
    assert!(!bytes.is_empty(), "{case} wrote an empty PNG");

    let _ = CURRENT_USER.remove_tree(&root);
    let _ = std::fs::remove_dir_all(&dir);
    bytes
}

/// French and German (`fr_intro`, `de_intro`) render successfully at both 96 and 192 DPI:
/// the harness itself previously had no `--dpi` wiring for this window at all, so this is
/// also the regression guard for `main.rs::run_shot_mode`'s `--dpi` parsing and
/// `win::create_shot_window`'s window-frame DPI fix (both added by this finding).
#[test]
fn welcome_renders_in_long_text_locales_at_96_and_192_dpi() {
    for lang in ["fr", "de"] {
        for dpi in [96u32, 192] {
            let case = format!("locale_{lang}_{dpi}");
            let (w, h) = png_size(&shot_locale(&case, lang, dpi));
            assert!(w > 0 && h > 0, "{case}: decoded to a zero-size image");
        }
    }
}

/// A 192-DPI capture must be visibly larger than a 96-DPI one of the SAME language: the
/// window frame has to scale with the override, not just the controls inside it (a bug this
/// finding found and fixed in `create_shot_window`: the frame used to stay at the real
/// monitor's DPI while children laid out for the forced one, so every control past the
/// top-left corner rendered outside the captured window).
#[test]
fn a_192_dpi_welcome_capture_is_larger_than_a_96_dpi_one() {
    let (w96, h96) = png_size(&shot_locale("dpi_fr_96", "fr", 96));
    let (w192, h192) = png_size(&shot_locale("dpi_fr_192", "fr", 192));
    assert!(
        w192 > w96 && h192 > h96,
        "192 DPI ({w192}x{h192}) must be larger than 96 DPI ({w96}x{h96}); if it isn't, the \
         --dpi override never reached the window frame"
    );
}
