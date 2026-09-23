//! The ONE place test code learns where the sample corpus is.
//!
//! `..\test-corpus` (and `..\test-corpus-real`) are siblings of the repo, built by
//! `scripts\build-corpus.ps1`, never in git, so a CI checkout has neither. Every test that
//! reads a sample has to tolerate that, and until 2026-09-19 each test spelled the path for
//! itself and decided for itself whether absence was a skip or a panic. Four tests that day
//! chose `unwrap()`, passed on the one machine that has the files, and painted three CI runs
//! red in a row - the exact round trip a pre-push gate exists to prevent, and one the gate
//! could not see because the gate runs on the machine that HAS the corpus.
//!
//! So: the path is spelled here and nowhere else (a test in this module scans `src/` and
//! `tests/` and fails on any other spelling), and the gate can now make the corpus VANISH.
//! With `ST2K_CORPUS_ABSENT=1` every accessor answers a path that does not exist, which is
//! precisely what CI sees, on the machine that has the files. `scripts\preflight.ps1` runs
//! the suite once normally with `ST2K_CORPUS_TOUCH_LOG` set (each call records the test that
//! made it - libtest names the thread after the test) and then re-runs exactly those tests
//! with the corpus absent. A test that cannot survive without its sample fails there, before
//! the push, in seconds.
//!
//! Public only because the integration tests and the `vdec` bin need it; not an API.

use std::path::{Path, PathBuf};

/// Set (to anything but empty or `0`) to make every accessor answer a path that does not
/// exist: the CI shape, reproduced on a machine that has the corpus.
pub const ABSENT_VAR: &str = "ST2K_CORPUS_ABSENT";
/// A file to append the calling test's name to on every access, so a gate can learn which
/// tests read the corpus without parsing their source.
pub const TOUCH_LOG_VAR: &str = "ST2K_CORPUS_TOUCH_LOG";
/// What a call from a thread libtest did not name records (a worker a test spawned).
pub const UNNAMED: &str = "<unnamed>";

fn absent() -> bool {
    absent_value(std::env::var_os(ABSENT_VAR).as_deref())
}

fn absent_value(v: Option<&std::ffi::OsStr>) -> bool {
    v.is_some_and(|v| !v.is_empty() && v != "0")
}

fn touch() {
    let Some(log) = std::env::var_os(TOUCH_LOG_VAR) else {
        return;
    };
    // One write per line: tests run concurrently and append to the same file, and Windows
    // keeps each append-mode write whole, so a whole line per write keeps the names apart
    // (`writeln!` issues the newline as a second write and the names ran into each other).
    let line = format!("{}\n", std::thread::current().name().unwrap_or(UNNAMED));
    // A log that cannot be written must never fail a test: the gate notices an empty log.
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
    {
        use std::io::Write;
        let _ = f.write_all(line.as_bytes());
    }
}

/// The workspace root (the `app` checkout): this crate sits in `crates/base`.
pub fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

/// Every library crate's `src`: the core crate's (which also holds the binaries, under
/// `src/bin`) and each layer's under `crates/`, for tests that read the source tree.
pub fn library_sources() -> Vec<PathBuf> {
    [
        "src",
        "crates/base/src",
        "crates/codecs/src",
        "crates/actions/src",
    ]
    .into_iter()
    .map(|d| workspace().join(d))
    .filter(|p| p.is_dir())
    .collect()
}

/// [`library_sources`] plus the app's own crates (the window kit, the viewer, the screenshot
/// tool) and the three shims (`dll`, `dlghook`, `build-support`): every workspace crate's `src`,
/// for tests that police the whole tree. The root `tests/` and `examples/` are not included;
/// a caller that wants them adds them.
pub fn all_sources() -> Vec<PathBuf> {
    let mut v = library_sources();
    v.extend(
        [
            "crates/appkit/src",
            "crates/preview/src",
            "crates/screenshot/src",
            "crates/dll/src",
            "crates/dlghook/src",
            "crates/build-support/src",
        ]
        .into_iter()
        .map(|d| workspace().join(d))
        .filter(|p| p.is_dir()),
    );
    v
}

fn root(name: &str) -> PathBuf {
    touch();
    let base = workspace().join("..");
    if absent() {
        // The process id keeps it unique and impossible to have been created by anyone.
        return base.join(format!("{name}.absent-{}", std::process::id()));
    }
    base.join(name)
}

/// `..\test-corpus`. The directory may not exist; callers that read from it already handle
/// that (`let Ok(bytes) = std::fs::read(dir().join(..)) else { return }`).
pub fn dir() -> PathBuf {
    root("test-corpus")
}

/// `..\test-corpus-real`: the real-content twin (RAW files with embedded previews, real
/// videos). Same contract as [`dir`].
pub fn real_dir() -> PathBuf {
    root("test-corpus-real")
}

/// One sample by name, when it is present.
pub fn path(name: &str) -> Option<PathBuf> {
    let p = dir().join(name);
    p.is_file().then_some(p)
}

/// One sample's bytes by name. Absence prints the standing NOT MEASURED line (a skipped
/// measurement is reported, never folded into green) and answers `None`, so the caller's
/// whole handling is `let Some(bytes) = read("x") else { return };`.
pub fn read(name: &str) -> Option<Vec<u8>> {
    match std::fs::read(dir().join(name)) {
        Ok(bytes) => Some(bytes),
        Err(_) => {
            eprintln!("NOT MEASURED: test-corpus/{name} is absent");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The corpus path is spelled in this file and nowhere else. Any other spelling is a
    /// test that the absent-corpus gate cannot make vanish, i.e. the 2026-09-19 CI red
    /// waiting to happen again. Comment lines and message strings may mention the folder;
    /// a string literal that IS the path (`"test-corpus"`, `"../test-corpus/x"`) may not.
    #[test]
    fn the_corpus_path_is_spelled_only_here() {
        let mut offenders = Vec::new();
        for top in all_sources().into_iter().chain([workspace().join("tests")]) {
            walk(&top, &mut |p| {
                let is_this_file = p.file_name().is_some_and(|n| n == "testcorpus.rs")
                    && p.parent().is_some_and(|d| d.ends_with("src"));
                if p.extension().is_none_or(|e| e != "rs") || is_this_file {
                    return;
                }
                let Ok(text) = std::fs::read_to_string(p) else {
                    return;
                };
                for (i, line) in text.lines().enumerate() {
                    if line.trim_start().starts_with("//") {
                        continue;
                    }
                    if spells_the_path(line) {
                        offenders.push(format!("{}:{}: {}", p.display(), i + 1, line.trim()));
                    }
                }
            });
        }
        assert!(
            offenders.is_empty(),
            "the corpus path belongs in crates/base/src/testcorpus.rs only (use testcorpus::dir(), \
             read() or path()); found:\n{}",
            offenders.join("\n")
        );
    }

    /// A string literal beginning with the folder name, with or without `../` / `..\`.
    fn spells_the_path(line: &str) -> bool {
        line.match_indices("test-corpus").any(|(at, _)| {
            let before = &line[..at];
            before.ends_with('"')
                || before.ends_with("\"../")
                || before.ends_with("\"..\\\\")
                || before.ends_with("\"..\\")
        })
    }

    fn walk(dir: &Path, f: &mut dyn FnMut(&Path)) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, f);
            } else {
                f(&p);
            }
        }
    }

    #[test]
    fn the_scan_recognises_a_path_literal_and_ignores_a_message() {
        assert!(spells_the_path(r#"let d = Path::new("../test-corpus");"#));
        assert!(spells_the_path(r#".join("test-corpus")"#));
        assert!(spells_the_path(
            r#"std::fs::read("../test-corpus/real.pix")"#
        ));
        assert!(!spells_the_path(
            r#"eprintln!("NOT MEASURED: test-corpus/x is absent");"#
        ));
        assert!(!spells_the_path(r#"#[ignore = "needs ../test-corpus"]"#));
        assert!(!spells_the_path("// the corpus lives in ../test-corpus"));
    }

    /// The switch the pre-push gate relies on: set, the corpus is gone whatever the disk
    /// holds; unset, empty or `0`, the real sibling folder answers. Decided from the value
    /// alone so the test never has to touch the process environment other tests share.
    #[test]
    fn the_absent_switch_hides_a_corpus_that_exists() {
        use std::ffi::OsStr;
        assert!(absent_value(Some(OsStr::new("1"))));
        assert!(absent_value(Some(OsStr::new("yes"))));
        assert!(!absent_value(Some(OsStr::new("0"))));
        assert!(!absent_value(Some(OsStr::new(""))));
        assert!(!absent_value(None));
        // And the path the accessors answer follows the same decision.
        let shown = dir();
        assert_eq!(
            shown.to_string_lossy().contains(".absent-"),
            absent(),
            "{}",
            shown.display()
        );
        assert!(shown.ends_with("test-corpus") || absent());
    }
}
