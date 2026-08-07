//! The Quick preview's SQLite database view (`preview::dbdoc`), driven through the real window.
//!
//! `dbdoc`'s own unit tests check the markdown it produces. This checks the parts they can't: that
//! the loader hook actually FIRES for a `.db`, that a file wearing the extension without being a
//! database still falls through to its old behaviour, and that a corrupt one neither crashes nor
//! hangs the viewer. All through the documented headless `--shot --window preview` harness
//! (CLAUDE.md §6), same as `preview_view_source`.
//!
//! Byte comparison is safe for the same reasons spelled out there: the harness is deterministic
//! and the capture is settled. The two files being compared are deliberately given the SAME
//! FILENAME in different directories, because the caption bar shows the name — comparing
//! `a.db` against `a.txt` would differ in the caption no matter what the pane held.
//!
//! Scratch dirs are removed only when a test PASSES; a failure leaves its PNGs as evidence.
//!
//! Needs a window station (real GDI + `PrintWindow`), like the other headless shot tooling.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The committed fixture — a real database written by SQLite (see
/// `scripts/make-sqlite-fixture.py`).
fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("sqlite")
        .join("sample.db")
}

/// Per-case scratch dir.
fn scratch(case: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("st2k_db_shot_{}_{case}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn cleanup(case: &str) {
    let _ = std::fs::remove_dir_all(
        std::env::temp_dir().join(format!("st2k_db_shot_{}_{case}", std::process::id())),
    );
}

/// One headless capture of `doc`. Panics unless the child exits clean AND writes a non-empty PNG.
fn shot(dir: &Path, doc: &Path, tag: &str, extra: &[&str]) -> Vec<u8> {
    let out = dir.join(format!("{tag}.png"));
    let status = Command::new(env!("CARGO_BIN_EXE_SageThumbs2K"))
        .arg("--shot")
        .arg(&out)
        .args(["--window", "preview", "--file"])
        .arg(doc)
        .args(extra)
        .status()
        .expect("spawn SageThumbs2K --shot");
    assert!(
        status.success(),
        "{tag} shot of {doc:?} failed: exit {:?} (0xC000041D = abort(), e.g. a panic inside the \
         database reader under panic=abort)",
        status.code(),
    );
    let bytes = std::fs::read(&out).unwrap_or_else(|e| panic!("{tag} shot wrote no PNG: {e}"));
    assert!(!bytes.is_empty(), "{tag} shot wrote an empty PNG");
    bytes
}

/// A real database must render as the database view — i.e. differently from a same-named file
/// that merely wears the extension. Identical captures would mean the `dbdoc` hook in
/// `loader::load` never fired and the file fell through to the plain-text sniff.
#[test]
fn a_real_database_renders_as_a_database() {
    let dir = scratch("real");
    let (real_dir, fake_dir) = (dir.join("real"), dir.join("fake"));
    std::fs::create_dir_all(&real_dir).expect("dir");
    std::fs::create_dir_all(&fake_dir).expect("dir");
    // SAME file name on both sides so the caption bar cannot be the thing that differs.
    let real = real_dir.join("sample.db");
    let fake = fake_dir.join("sample.db");
    std::fs::copy(fixture(), &real).expect("copy fixture");
    std::fs::write(&fake, "not a database, just text\n".repeat(40)).expect("write fake");

    let a = shot(&dir, &real, "real", &[]);
    let b = shot(&dir, &fake, "fake", &[]);
    assert_ne!(
        a, b,
        "a real SQLite file rendered identically to a plain-text file with the same name — the \
         dbdoc hook in loader::load did not fire",
    );
    cleanup("real");
}

/// `.db` is a generic extension (`Thumbs.db` is a compound file, not SQLite). A file that isn't a
/// database must keep the behaviour it had before this feature existed — the text sniff, showing
/// its actual contents. Two non-database `.db` files with the same NAME but different TEXT must
/// therefore render differently; an info card, an error placeholder, or an empty pane would make
/// both captures identical.
#[test]
fn a_non_database_still_shows_its_text() {
    let dir = scratch("fallthrough");
    let (one, two) = (dir.join("one"), dir.join("two"));
    std::fs::create_dir_all(&one).expect("dir");
    std::fs::create_dir_all(&two).expect("dir");
    // Same file name on both sides, so the caption cannot be what differs.
    let a = one.join("notes.db");
    let b = two.join("notes.db");
    std::fs::write(&a, "alpha alpha alpha\n".repeat(20)).expect("write");
    std::fs::write(&b, "beta beta beta\n".repeat(20)).expect("write");
    assert_ne!(
        shot(&dir, &a, "one", &[]),
        shot(&dir, &b, "two", &[]),
        "two different non-database .db files rendered identically — their text is not reaching \
         the pane, so the fall-through broke",
    );
    cleanup("fallthrough");
}

/// A corrupt database must degrade, never crash or hang: a valid header followed by garbage sends
/// the b-tree walk down page pointers that lead nowhere. Every one of those paths is bounds-checked
/// and budgeted; this proves it on the real thing rather than on a synthetic record.
#[test]
fn a_corrupt_database_does_not_crash_the_viewer() {
    let dir = scratch("corrupt");
    let mut bytes = std::fs::read(fixture()).expect("read fixture");
    // Deterministic scramble of everything past the 100-byte file header: page types, cell
    // pointers, payload lengths and overflow pointers all become nonsense, while the header keeps
    // the file recognisable as SQLite so the reader commits to parsing it.
    for (i, b) in bytes.iter_mut().enumerate().skip(100) {
        *b = ((i as u32).wrapping_mul(2654435761) >> 13) as u8;
    }
    let doc = dir.join("corrupt.db");
    std::fs::write(&doc, &bytes).expect("write corrupt db");
    shot(&dir, &doc, "corrupt", &[]);

    // Truncated mid-page: the pager's short-read path and the walk's missing-page path.
    let cut = dir.join("truncated.db");
    let good = std::fs::read(fixture()).expect("read fixture");
    std::fs::write(&cut, &good[..good.len() / 3]).expect("write truncated db");
    shot(&dir, &cut, "truncated", &[]);
    cleanup("corrupt");
}

/// The view-source toggle must ignore a database. `loader::source_capable` does not list the
/// database extensions, so the toolbar hides the button — honouring `--source` there would be a
/// state the UI cannot reach, and would show a binary file as garbage text.
#[test]
fn source_mode_is_a_noop_for_a_database() {
    let dir = scratch("source");
    let doc = dir.join("sample.db");
    std::fs::copy(fixture(), &doc).expect("copy fixture");
    let rendered = shot(&dir, &doc, "rendered", &[]);
    let sourced = shot(&dir, &doc, "sourced", &["--source"]);
    assert_eq!(
        rendered, sourced,
        "--source changed the database view — source_capable has loosened, so the toolbar would \
         offer a toggle that dumps binary bytes into the text pane",
    );
    cleanup("source");
}
