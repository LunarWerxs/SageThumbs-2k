//! The Quick preview's view-source toggle (`Btn::Source` / Ctrl+U / the headless `--source` flag).
//!
//! Locks the two properties that make the toggle meaningful, using the documented headless
//! `--shot --window preview` harness (see CLAUDE.md §6) — capture the same file with and without
//! `--source` and compare the PNGs:
//!
//!  * a file that RENDERS (markdown, csv) must look DIFFERENT in source mode — that's the whole
//!    feature. A regression that silently ignored the flag, or that lost the rendered path, shows
//!    up here as two identical captures.
//!  * a file that has only ONE view (a plain `.txt` — already source; a `.png` — no source at all)
//!    must be BYTE-IDENTICAL either way. `source_capable` gates on this, and the toolbar hides the
//!    button; if the gate ever loosened, a `.txt` would re-read into a subtly different pane and
//!    these captures would diverge.
//!
//! Byte comparison is safe here because the shot harness is deterministic: same off-screen size,
//! same content, no timestamps in the frame (the caption shows the file NAME, not a date), and
//! the capture itself is SETTLED — `capture_hwnd_bgra` re-grabs until two sentinel-backed
//! `PrintWindow` passes agree byte-for-byte, because under full-suite CPU load a single
//! `PW_RENDERFULLCONTENT` grab can come back PARTIAL (an uninitialized white band where the
//! render didn't finish; that flaked these tests twice in four full runs on 2026-07-21).
//!
//! Scratch dirs are removed only when a test PASSES — a failure leaves its PNG pair on disk
//! (%TEMP%\st2k_src_shot_<pid>_<case>), which is exactly what makes the flake diagnosable.
//!
//! Needs a window station (real GDI + `PrintWindow`), like the other headless shot tooling.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Write `body` as `name` in a per-case scratch dir; returns the dir and the file path.
fn sample(case: &str, name: &str, body: &str) -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!("st2k_src_shot_{}_{case}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let doc = dir.join(name);
    std::fs::write(&doc, body).expect("write sample");
    (dir, doc)
}

/// Drop `case`'s scratch dir. Call AFTER the asserts — a failing case must keep its PNGs on
/// disk (they are the evidence), only a passing one cleans up.
fn cleanup(case: &str) {
    let _ = std::fs::remove_dir_all(
        std::env::temp_dir().join(format!("st2k_src_shot_{}_{case}", std::process::id())),
    );
}

/// Capture `body` twice: rendered, and with `--source`. Returns the two PNGs' bytes.
fn shot_both(case: &str, name: &str, body: &str) -> (Vec<u8>, Vec<u8>) {
    let (dir, doc) = sample(case, name, body);
    (shot(&dir, &doc, "rendered", &[]), shot(&dir, &doc, "source", &["--source"]))
}

/// One headless capture of `doc` with `extra` flags appended. Panics unless the child exits clean
/// AND writes a non-empty PNG (either failure would make a byte comparison meaningless).
fn shot(dir: &Path, doc: &Path, tag: &str, extra: &[&str]) -> Vec<u8> {
    let out = dir.join(format!("{tag}.png"));
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_SageThumbs2K"));
    cmd.arg("--shot").arg(&out).args(["--window", "preview", "--file"]).arg(doc).args(extra);
    let status = cmd.status().expect("spawn SageThumbs2K --shot");
    assert!(
        status.success(),
        "{tag} shot of {doc:?} failed: exit {:?} (0xC000041D = abort(), e.g. a RefCell \
         BorrowMutError under panic=abort)",
        status.code(),
    );
    let bytes = std::fs::read(&out).unwrap_or_else(|e| panic!("{tag} shot wrote no PNG: {e}"));
    assert!(!bytes.is_empty(), "{tag} shot wrote an empty PNG");
    bytes
}

/// A rendered document must actually CHANGE when `--source` is passed.
#[test]
fn source_mode_changes_a_rendered_document() {
    // (case, filename, body) — one markdown file, one CSV (which rides the markdown pipeline via
    // `docconv`, so its raw text is not even what the rendered view holds).
    let cases = [
        ("md", "doc.md", "# Heading\n\nSome **bold** body text and a list:\n\n- one\n- two\n"),
        ("csv", "table.csv", "name,role\nAda,Analyst\nGrace,Admiral\n"),
    ];
    for (case, name, body) in cases {
        let (rendered, sourced) = shot_both(case, name, body);
        assert_ne!(
            rendered, sourced,
            "{case}: --source produced the same image as the rendered view — the view-source \
             toggle is not taking effect (check loader::source_capable + the src_view branch \
             in loader::load / load_static)",
        );
        cleanup(case);
    }
}

/// A file with only one view must ignore the flag entirely — the toolbar hides the button for it,
/// so honouring `--source` there would be a state the UI can't reach.
#[test]
fn source_mode_is_a_noop_without_a_rendered_view() {
    // A .txt is already shown as source; a .rs likewise. Both must be untouched by the flag.
    let cases = [
        ("txt", "notes.txt", "plain text, already the source view\nsecond line\n"),
        ("rs", "lib.rs", "fn main() {\n    println!(\"hi\");\n}\n"),
    ];
    for (case, name, body) in cases {
        let (rendered, sourced) = shot_both(case, name, body);
        assert_eq!(
            rendered, sourced,
            "{case}: --source changed a file that has no rendered view — source_capable is \
             too loose, so the toolbar would offer a toggle that does nothing visible",
        );
        cleanup(case);
    }
}

/// PRESSING the button (the real `do_action` → `toggle_source` → reload path) must reach the same
/// place as opening with `--source`.
///
/// This is the test that matters most: `--source` presets the mode and never calls
/// `toggle_source`, so it cannot catch a bug in the toggle itself — and one shipped-then-caught
/// bug lived exactly there (an `if let Some(p) = st.path.borrow().clone()` held its `Ref` across
/// `load`'s `borrow_mut` on edition 2021, so every real click aborted the process while every
/// `--source` shot stayed green).
#[test]
fn pressing_the_button_matches_opening_in_source_mode() {
    let (dir, doc) = sample("press", "doc.md", "# Heading\n\nbody **text** here\n\n- a\n- b\n");
    let pressed = shot(&dir, &doc, "pressed", &["--toggle-source"]);
    let preset = shot(&dir, &doc, "preset", &["--source"]);
    assert_eq!(
        pressed, preset,
        "clicking the view-source button did not land in the same state as --source \
         (the click path goes through do_action -> toggle_source -> request_load; the preset \
         does not, so they diverge if the toggle's reload is broken)",
    );
    cleanup("press");
}

/// And pressing it AGAIN must come back to the rendered view — the toggle has to round-trip, not
/// just latch on. `--source --toggle-source` = start in source, press once, expect rendered.
#[test]
fn pressing_the_button_twice_round_trips_to_rendered() {
    let (dir, doc) = sample("round", "doc.md", "# Heading\n\nbody **text** here\n\n- a\n- b\n");
    let rendered = shot(&dir, &doc, "rendered", &[]);
    let back = shot(&dir, &doc, "back", &["--source", "--toggle-source"]);
    assert_eq!(
        rendered, back,
        "toggling out of source mode did not restore the rendered view",
    );
    cleanup("round");
}

/// The `--hot N` button indices are positional (`BTNS`), and the source toggle sits at index 1.
/// Hovering it must render (this is also the cheapest guard that adding the variant kept every
/// `Btn` match arm — glyph, tooltip key, visibility — in agreement).
#[test]
fn hovering_the_source_button_renders() {
    let dir = std::env::temp_dir().join(format!("st2k_src_hot_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let doc = dir.join("doc.md");
    std::fs::write(&doc, "# Heading\n\nbody\n").expect("write markdown");
    let out: PathBuf = dir.join("hot.png");
    let status = Command::new(env!("CARGO_BIN_EXE_SageThumbs2K"))
        .arg("--shot")
        .arg(&out)
        .args(["--window", "preview", "--file"])
        .arg(&doc)
        .args(["--hot", "1"])
        .status()
        .expect("spawn SageThumbs2K --shot");
    assert!(status.success(), "hover shot failed: exit {:?}", status.code());
    assert!(out.is_file(), "hover shot wrote no PNG");
    let _ = std::fs::remove_dir_all(&dir);
}
