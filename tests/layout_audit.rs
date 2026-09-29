//! Every window the headless `--shot` harness can build, checked for what a person SEES: no
//! control past its window's edge, none covering another, and every label and button with room
//! for its own text. `st2k_appkit::win::audit_layout` does the looking; the capture step runs it
//! when `ST2K_LAYOUT_AUDIT` names a file.
//!
//! Why this exists: the suite checked that the code ran, never what the dialogs looked like.
//! Rename with pattern shipped with its Rename/Cancel row below the window's bottom edge (issue
//! #48), and the first sweep of all 36 languages at 100/150/200% then found translated buttons
//! and labels cut off in a dozen more places. The full sweep takes ~40 minutes; this runs the
//! three cases that caught the most (English at 96 dpi, Bulgarian at 192, Filipino at 144) over
//! every window, in parallel, in about a minute.
//!
//! Settings go to a throwaway portable ini per case (`ST2K_PORTABLE_INI`), so nothing touches
//! the developer's own settings. Needs a window station, like the other `--shot` tests here.
#![cfg(windows)]

use std::path::PathBuf;
use std::process::{Child, Command};

/// Every `--window` capture that is a dialog, plus each Settings page (`--tab N`).
const WINDOWS: &[&str] = &[
    "rename",
    "files-to-folder",
    "tags-to-folders",
    "convert",
    "convert-report",
    "feedback",
    "about",
    "doctor",
    "upload",
    "uploads",
    "firstrun",
    "firstrun2",
    "ocr",
];
const SETTINGS_PAGES: u32 = 11;

/// (language, dpi) cases: English as designed, and the two languages whose translations ran
/// longest against the fixed layouts in the full sweep, at the scalings they broke at.
const CASES: &[(&str, u32)] = &[("en", 96), ("bg", 192), ("fil", 144)];

/// How many captures run at once.
const PARALLEL: usize = 6;

fn scratch() -> PathBuf {
    let d = std::env::temp_dir().join(format!("st2k-layout-audit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("scratch dir");
    d
}

/// Start one capture of `window` (a `--window` name or `settings:N`) in `lang` at `dpi`,
/// recording its findings to `<tag>.jsonl` under `dir`.
fn spawn(dir: &std::path::Path, lang: &str, dpi: u32, window: &str) -> (String, Child) {
    let tag = format!("{lang}_{dpi}_{}", window.replace(':', "-"));
    let ini = dir.join(format!("{lang}.ini"));
    if !ini.exists() {
        std::fs::write(
            &ini,
            format!("[Settings]\nLang={lang}\nPreviewEnabled=1\nNavDotsSeen=2047\n"),
        )
        .expect("ini");
    }
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_SageThumbs2K"));
    cmd.arg("--shot").arg(dir.join(format!("{tag}.png")));
    match window.strip_prefix("settings:") {
        Some(tab) => cmd.args(["--tab", tab]),
        None => cmd.args(["--window", window]),
    };
    cmd.args(["--dpi", &dpi.to_string()])
        .env("ST2K_PORTABLE_INI", &ini)
        .env("ST2K_LAYOUT_AUDIT", dir.join(format!("{tag}.jsonl")));
    (tag, cmd.spawn().expect("spawn SageThumbs2K --shot"))
}

#[test]
fn no_window_cuts_off_a_control_in_english_or_the_longest_languages() {
    let dir = scratch();
    let windows: Vec<String> = WINDOWS
        .iter()
        .map(|w| w.to_string())
        .chain((0..SETTINGS_PAGES).map(|n| format!("settings:{n}")))
        .collect();
    let jobs: Vec<(&str, u32, &String)> = CASES
        .iter()
        .flat_map(|&(lang, dpi)| windows.iter().map(move |w| (lang, dpi, w)))
        .collect();

    let mut failed_runs = Vec::new();
    for batch in jobs.chunks(PARALLEL) {
        let running: Vec<_> = batch
            .iter()
            .map(|&(lang, dpi, w)| spawn(&dir, lang, dpi, w))
            .collect();
        for (tag, mut child) in running {
            let status = child.wait().expect("wait for capture");
            if !status.success() {
                failed_runs.push(format!("{tag}: exit {:?}", status.code()));
            }
        }
    }

    let mut findings = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("read scratch") {
        let path = entry.expect("entry").path();
        if path.extension().is_some_and(|e| e == "jsonl") {
            let tag = path.file_stem().unwrap().to_string_lossy().into_owned();
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            findings.extend(text.lines().map(|l| format!("{tag}: {l}")));
        }
    }
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        failed_runs.is_empty(),
        "captures that did not complete: {failed_runs:#?}"
    );
    assert!(
        findings.is_empty(),
        "{} control(s) cut off, off the edge or overlapping:\n{}",
        findings.len(),
        findings.join("\n")
    );
}
