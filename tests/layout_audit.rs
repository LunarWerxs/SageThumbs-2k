//! Every window the headless `--shot` harness can build, checked for what a person SEES: no
//! control past its window's edge, none covering another, and every label and button with room
//! for its own text. `st2k_appkit::win::audit_layout` does the looking; `--audit-layout` builds
//! every dialog and every Settings page in one process and records what it finds.
//!
//! Why this exists: the suite checked that the code ran, never what the dialogs looked like.
//! Rename with pattern shipped with its Rename/Cancel row below the window's bottom edge (issue
//! #48), and the first sweep of all 36 languages at 100/150/200% then found translated buttons
//! and labels cut off in a dozen more places. `scripts/check-layout.ps1` runs that full sweep;
//! this runs the three cases that caught the most (English at 96 dpi, Bulgarian at 192,
//! Filipino at 144), one process each, side by side.
//!
//! Settings go to a throwaway portable ini per case (`ST2K_PORTABLE_INI`), so nothing touches
//! the developer's own settings. Needs a window station, like the other `--shot` tests here.
#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command};

/// (language, dpi) cases: English as designed, and the two languages whose translations ran
/// longest against the fixed layouts in the full sweep, at the scalings they broke at.
const CASES: &[(&str, u32)] = &[("en", 96), ("bg", 192), ("fil", 144)];

fn scratch() -> PathBuf {
    let d = std::env::temp_dir().join(format!("st2k-layout-audit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("scratch dir");
    d
}

/// Start one `--audit-layout` run of every window in `lang` at `dpi`, into `<lang>_<dpi>.jsonl`.
fn spawn(dir: &Path, lang: &str, dpi: u32) -> (PathBuf, Child) {
    let ini = dir.join(format!("{lang}.ini"));
    std::fs::write(
        &ini,
        format!("[Settings]\nLang={lang}\nPreviewEnabled=1\nNavDotsSeen=2047\n"),
    )
    .expect("ini");
    let out = dir.join(format!("{lang}_{dpi}.jsonl"));
    let child = Command::new(env!("CARGO_BIN_EXE_SageThumbs2K"))
        .arg("--audit-layout")
        .arg(&out)
        .args(["--dpi", &dpi.to_string()])
        .env("ST2K_PORTABLE_INI", &ini)
        .spawn()
        .expect("spawn SageThumbs2K --audit-layout");
    (out, child)
}

/// What the runs reported: findings, windows that failed, and how many windows were audited.
#[derive(Default)]
struct Tally {
    findings: Vec<String>,
    failed: Vec<String>,
    audited: usize,
}

/// Wait for one run and add what its file says to `t`.
fn tally_run(t: &mut Tally, out: &Path, mut child: Child) {
    let status = child.wait().expect("wait for the audit run");
    let tag = out.file_stem().unwrap().to_string_lossy().into_owned();
    let text = std::fs::read_to_string(out).unwrap_or_default();
    for line in text.lines() {
        match ["kind", "error", "audited"]
            .into_iter()
            .find(|k| line.contains(&format!("\"{k}\":")))
        {
            Some("kind") => t.findings.push(format!("{tag}: {line}")),
            Some("error") => t.failed.push(format!("{tag}: {line}")),
            Some(_) => t.audited += 1,
            None => {}
        }
    }
    if !status.success() {
        t.failed.push(format!("{tag}: exit {:?}", status.code()));
    }
}

#[test]
fn no_window_cuts_off_a_control_in_english_or_the_longest_languages() {
    let dir = scratch();
    let running: Vec<_> = CASES
        .iter()
        .map(|&(lang, dpi)| spawn(&dir, lang, dpi))
        .collect();
    let mut t = Tally::default();
    for (out, child) in running {
        tally_run(&mut t, &out, child);
    }
    let _ = std::fs::remove_dir_all(&dir);
    let Tally {
        findings,
        failed,
        audited,
    } = t;

    assert!(
        failed.is_empty(),
        "windows that did not build or showed nothing: {failed:#?}"
    );
    assert!(audited > 0, "the runs audited no window at all");
    assert!(
        findings.is_empty(),
        "{} control(s) cut off, off the edge or overlapping:\n{}",
        findings.len(),
        findings.join("\n")
    );
}
