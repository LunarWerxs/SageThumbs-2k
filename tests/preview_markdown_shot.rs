//! Regression: a Markdown heading with EMPTY text must not kill the Quick preview viewer.
//!
//! The bug (found 2026-07-17, present since the outline sidebar landed): a bare `#` parses to
//! `Block::Heading` with zero runs, which yields an empty outline (ToC) label. `paint_toc` turned
//! that into an empty `Vec<u16>` and handed it straight to `DrawTextW`. An empty `Vec`'s
//! `as_ptr()` is a DANGLING (alignment-valued) pointer, and user32 probes the text pointer even
//! when `cchText` is 0 — so the process died with `STATUS_FATAL_APP_EXIT` (0xC000041D, the CRT
//! `abort()`), no Rust panic and no log line. A two-character `.md` file was a full viewer crash.
//!
//! The fix is `preview::paint::draw_text`, which no-ops on an empty buffer. This test locks the
//! BEHAVIOUR rather than the guard: it drives the real render path through the documented headless
//! `--shot --window preview` harness (the same one the crash was found with) and asserts the
//! process exits cleanly and writes a PNG. Passing a VALID pointer with a 0 length is harmless, so
//! only buffers that can be an empty `Vec` are at risk — see `draw_text`'s comment.
//!
//! Needs a window station (real GDI + `PrintWindow`), like the other headless shot tooling.

use std::path::PathBuf;
use std::process::Command;

/// Render `md` through the headless preview harness; returns the child's exit status.
fn shot_markdown(case: &str, md: &str) -> (std::process::ExitStatus, PathBuf) {
    let dir = std::env::temp_dir().join(format!("st2k_md_shot_{}_{case}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let doc = dir.join("doc.md");
    let out = dir.join("out.png");
    std::fs::write(&doc, md).expect("write markdown");

    let status = Command::new(env!("CARGO_BIN_EXE_SageThumbs2K"))
        .arg("--shot")
        .arg(&out)
        .args(["--window", "preview", "--file"])
        .arg(&doc)
        .status()
        .expect("spawn SageThumbs2K --shot");
    (status, out)
}

/// Every shape that produces a heading with no text. Each one aborted the process before the fix.
#[test]
fn empty_heading_does_not_crash_the_viewer() {
    // (case name, markdown) — the trailing-space and content-after variants are distinct parses.
    let cases = [
        ("bare", "#"),
        ("trailing_space", "## "),
        ("content_after", "#\n\nbody text\n"),
        ("image_only", "# ![](nope.png)\n"),
        ("deep_level", "###### \n"),
    ];
    for (case, md) in cases {
        let (status, out) = shot_markdown(case, md);
        assert!(
            status.success(),
            "empty heading ({case:?}, {md:?}) crashed the viewer: exit {:?} \
             (0xC000041D = abort() -> the DrawTextW empty-buffer bug is back)",
            status.code(),
        );
        assert!(out.is_file(), "no PNG written for {case:?} — the shot did not complete");
        // Passing cases clean their scratch dir; a failure keeps its PNG as evidence.
        let _ = std::fs::remove_dir_all(out.parent().expect("scratch dir"));
    }
}

/// Control: a heading WITH text always worked. If this fails, the harness itself is broken and the
/// assertions above prove nothing.
#[test]
fn heading_with_text_still_renders() {
    let (status, out) = shot_markdown("control", "# heading\n\nbody\n");
    assert!(status.success(), "control render failed: exit {:?}", status.code());
    assert!(out.is_file(), "control wrote no PNG — the shot harness is broken");
    let _ = std::fs::remove_dir_all(out.parent().expect("scratch dir"));
}
