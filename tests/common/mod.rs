//! Shared helpers for the integration tests under `tests/`. Each test file that needs one
//! adds `mod common;` and calls through `common::` — not every helper here is used by every
//! test file (each integration test is its own binary crate), hence the blanket allow below
//! rather than one per unused item per file.
#![cfg(windows)]
#![allow(dead_code)]

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;

/// The built cdylib sits one directory above the test exe
/// (`target/<profile>/sagethumbs2k.dll` vs `target/<profile>/deps/<test>.exe`).
pub fn dll_path() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    exe.parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("sagethumbs2k.dll")
}

/// UTF-16, NUL-terminated — the shape every `PCWSTR`-taking Win32 call in these tests needs.
pub fn to_wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// Set a process environment variable for a test. `std::env::set_var` carries an `unsafe`
/// signature (mutating process-global state is unsound to race against a read on another
/// thread), so every integration test routes through this ONE wrapper instead of
/// re-acknowledging that soundness contract, inconsistently, at each call site.
///
/// # Safety
/// The caller must ensure no other thread is reading or writing the process environment
/// concurrently with this call — in practice: call it before spawning any thread that reads
/// settings, and before the first settings read this process performs (the DLL's own
/// `OnceLock`-cached reads included).
pub unsafe fn set_test_env(key: &str, value: impl AsRef<OsStr>) {
    // SAFETY: forwarded to the caller's own obligation, documented above.
    unsafe { std::env::set_var(key, value) };
}

/// Remove a process environment variable for a test — the `remove_var` counterpart to
/// [`set_test_env`], with the identical soundness obligation.
///
/// # Safety
/// The caller must ensure no other thread is reading or writing the process environment
/// concurrently with this call.
pub unsafe fn remove_test_env(key: &str) {
    // SAFETY: forwarded to the caller's own obligation, documented above.
    unsafe { std::env::remove_var(key) };
}

/// Every entry of a zip as `(name, decompressed bytes)`, for the doctor-bundle scrub tests
/// (2026-09-05 audit, E01). Decompressed on purpose: a deflated entry never contains its
/// plaintext, so a scan of the raw archive bytes would pass with the secret right there.
pub fn zip_entries(path: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    let f = std::fs::File::open(path).expect("open the bundle");
    let mut zip = zip::ZipArchive::new(f).expect("a zip");
    (0..zip.len())
        .map(|i| {
            let mut entry = zip.by_index(i).expect("entry");
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut entry, &mut bytes).expect("read entry");
            (entry.name().to_string(), bytes)
        })
        .collect()
}

/// The doctor-bundle contract shared by both storage backends: none of `secrets` appears in
/// ANY entry, byte for byte; the four entries are all there; and the ordinary preferences the
/// tests seed (`Theme=1`, `MaxSize=200`, the `[jpg]` section) survive in `settings.txt`, so
/// a scrub that simply dropped the whole file would fail here too.
pub fn assert_bundle_is_scrubbed(entries: &[(String, Vec<u8>)], secrets: &[&str]) {
    let contains = |hay: &[u8], needle: &[u8]| hay.windows(needle.len()).any(|w| w == needle);
    for (name, bytes) in entries {
        for secret in secrets {
            assert!(
                !contains(bytes, secret.as_bytes()),
                "{name} carries the sign-in state {secret:?}:\n{}",
                String::from_utf8_lossy(bytes)
            );
        }
    }
    let names: Vec<&str> = entries.iter().map(|(n, _)| n.as_str()).collect();
    for expect in [
        "doctor-report.txt",
        "formats.json",
        "log-tail.txt",
        "settings.txt",
    ] {
        assert!(names.contains(&expect), "missing {expect}: {names:?}");
    }
    let settings = entries
        .iter()
        .find(|(n, _)| n == "settings.txt")
        .map(|(_, b)| String::from_utf8_lossy(b).into_owned())
        .expect("settings.txt");
    for kept in ["Theme=1", "MaxSize=200", "[jpg]", "Enabled=0"] {
        assert!(
            settings.contains(kept),
            "{kept} missing: the preferences must survive the scrub:\n{settings}"
        );
    }
    assert!(
        !settings.contains("[OAuth]") && !settings.contains("OAuth_"),
        "the credential container itself must not be listed:\n{settings}"
    );
}
