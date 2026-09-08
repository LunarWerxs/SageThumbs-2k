//! Guard: keep the IFF emission seam closed (#367, #376).
//!
//! `crates/djvu-iff` is the single owner of the on-disk `AT&T`/`FORM` framing.
//! Every other module must build DjVu containers through the emission seam
//! (`iff::emit` / `iff::partial_emit` / `iff::partial_emit_with_offsets`) and
//! the `iff::MAGIC` constant — never by appending the raw magic/`FORM` tag to a
//! buffer itself. This test fails if a hand-rolled FORM writer reappears, so
//! the seam stops eroding between reviews instead of needing to be re-found.
//!
//! The flagged idiom is *assembly* — `extend_from_slice(b"AT&T")` /
//! `extend_from_slice(b"FORM")`. Reading/comparing the magic (`== b"AT&T"`,
//! `find_first(b"FORM")`, naming a `Chunk` id `*b"FORM"`) is fine. A line that
//! must genuinely hand-assemble framing (none today) can opt out with a
//! trailing `// iff-raw-ok` marker.

use std::fs;
use std::path::{Path, PathBuf};

/// The two assembly idioms that route around the seam.
const FORBIDDEN: [&str; 2] = [
    "extend_from_slice(b\"AT&T\")",
    "extend_from_slice(b\"FORM\")",
];

const OPT_OUT: &str = "// iff-raw-ok";

#[test]
fn no_raw_form_framing_outside_iff_crate() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut offenders = Vec::new();

    for dir in ["src", "crates"] {
        scan(&root.join(dir), root, &mut offenders);
    }

    assert!(
        offenders.is_empty(),
        "raw IFF `AT&T`/`FORM` assembly found outside `djvu-iff`. Route the \
         framing through `iff::partial_emit` / `iff::emit` (or `iff::MAGIC`), \
         or annotate the line with `{OPT_OUT}` if it is intentional:\n  {}",
        offenders.join("\n  "),
    );
}

/// Recursively scan `dir` for `.rs` files, skipping the owner crate.
fn scan(dir: &Path, root: &Path, offenders: &mut Vec<String>) {
    // `djvu-iff` is the seam's home — it may spell the framing literally.
    if dir.ends_with("djvu-iff") {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // Skip build output and nested target dirs.
            if path.ends_with("target") {
                continue;
            }
            scan(&path, root, offenders);
        } else if path.extension().is_some_and(|e| e == "rs") {
            check_file(&path, root, offenders);
        }
    }
}

fn check_file(path: &Path, root: &Path, offenders: &mut Vec<String>) {
    let Ok(text) = fs::read_to_string(path) else {
        return;
    };
    let rel: PathBuf = path.strip_prefix(root).unwrap_or(path).to_path_buf();
    for (i, line) in text.lines().enumerate() {
        if line.contains(OPT_OUT) {
            continue;
        }
        if FORBIDDEN.iter().any(|pat| line.contains(pat)) {
            offenders.push(format!("{}:{}: {}", rel.display(), i + 1, line.trim()));
        }
    }
}
