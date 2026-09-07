//! The Convert dialog renders across locales and DPIs (2026-09-05 audit finding F36).
//!
//! `CID_RESIZE_CHK`/`CID_RESIZE_PAD`/`CID_RESIZE_ALL` used to be squeezed into flat 90px /
//! 172px boxes that clipped longer translations (French, German, ...); see `convert.rs`'s
//! module-level comment on `CV_RESIZE_RIGHT` for the fix. This drives the documented
//! headless harness (`--shot --window convert`) with the language forced via a scratch HKCU
//! key (the same isolation `tests/settings_gate.rs` uses) and the DPI forced via the
//! `--dpi` override `main.rs::run_shot_mode` now applies to every `--shot` window
//! (previously wired for `--window preview` only).

use std::path::PathBuf;
use std::process::Command;

use windows_registry::CURRENT_USER;

/// Big-endian `u32` at `off`: PNG stores IHDR width/height that way.
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
    let d = std::env::temp_dir().join(format!("st2k_convertshot_{}_{case}", std::process::id()));
    std::fs::create_dir_all(&d).expect("scratch dir");
    d
}

/// Throwaway HKCU subkey this file's language override writes to, named by case + this TEST
/// PROCESS's pid, never the real `Software\SageThumbs2K` a developer's own Explorer reads.
fn scratch_reg_root(case: &str) -> String {
    format!(
        r"Software\SageThumbs2K\__test_convertshot_{}_{case}",
        std::process::id()
    )
}

/// Capture the Convert dialog with `lang` (a shipped locale code) forced via a scratch HKCU
/// key and `dpi` forced via `--dpi`.
fn shot(case: &str, lang: &str, dpi: u32) -> Vec<u8> {
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
        .args(["--window", "convert", "--dpi", &dpi.to_string()])
        .env("ST2K_SETTINGS_ROOT", &root)
        // A parent shell with a leaked portable ini carries its own Lang value, which would
        // silently override the HKCU one this test just wrote.
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

/// French and German (two of the audit's flagged longer-running translations) render
/// successfully at both 96 and 192 DPI. `--dpi` was previously unwired for this window
/// entirely, so this also guards `main.rs::run_shot_mode`'s `--dpi` parsing and
/// `win::create_shot_window`'s window-frame DPI fix (both added by this finding); without
/// the frame fix, a 192-DPI capture renders only the top-left quarter of the dialog with
/// everything past it laid out outside the captured window.
#[test]
fn convert_dialog_renders_in_long_text_locales_at_96_and_192_dpi() {
    for lang in ["fr", "de"] {
        for dpi in [96u32, 192] {
            let case = format!("locale_{lang}_{dpi}");
            let (w, h) = png_size(&shot(&case, lang, dpi));
            assert!(w > 0 && h > 0, "{case}: decoded to a zero-size image");
        }
    }
}

/// The single-column layout the fix moved to (see `convert.rs`) gives every checkbox a
/// generously fixed-width column rather than growing the dialog per locale, so English,
/// French and German must all render at the IDENTICAL design size at a given DPI. A
/// regression that went back to sizing the dialog by locale would change this.
#[test]
fn convert_dialog_size_does_not_depend_on_locale() {
    for dpi in [96u32, 192] {
        let (ew, eh) = png_size(&shot(&format!("size_en_{dpi}"), "en", dpi));
        for lang in ["fr", "de"] {
            let case = format!("size_{lang}_{dpi}");
            let (w, h) = png_size(&shot(&case, lang, dpi));
            assert_eq!(
                (w, h),
                (ew, eh),
                "{case}: dialog size must match English at {dpi} DPI ({w}x{h} vs {ew}x{eh})"
            );
        }
    }
}

/// A 192-DPI capture must be visibly larger than a 96-DPI one of the same language: the
/// window frame has to scale with the override, not just the controls inside it.
#[test]
fn a_192_dpi_convert_capture_is_larger_than_a_96_dpi_one() {
    let (w96, h96) = png_size(&shot("dpi_en_96", "en", 96));
    let (w192, h192) = png_size(&shot("dpi_en_192", "en", 192));
    assert!(
        w192 > w96 && h192 > h96,
        "192 DPI ({w192}x{h192}) must be larger than 96 DPI ({w96}x{h96}); if it isn't, the \
         --dpi override never reached the window frame"
    );
}
