#![cfg(test)]

use super::*;

/// The doctor reports displaced handlers BY FORMAT, which means parsing the extension back
/// out of the key path `hook_ext` recorded. `thumb_keys` produces those paths in register.rs
/// and `displaced_key_ext` recovers the extension in displaced.rs; this test pins the two
/// together. If `thumb_keys` ever changes shape, this fails instead of the
/// report silently going blank (a `find` that matches nothing returns `None`, which the
/// doctor skips — a failure mode with no symptom at all).
#[test]
fn displaced_key_ext_matches_thumb_keys() {
    for (ext, _) in st2k_base::formats::FORMATS {
        for path in thumb_keys(ext) {
            assert_eq!(
                displaced_key_ext(&path),
                Some(format!(".{ext}").as_str()),
                "could not recover .{ext} from {path}"
            );
        }
    }
}

/// The `SystemFileAssociations` twin must not collide with the bare-extension key: they are
/// stored as two separate value names under `DISPLACED`, so a collision would mean one of
/// the two displaced handlers is silently forgotten and never restored.
#[test]
fn thumb_keys_are_distinct_per_extension() {
    let mut seen = std::collections::BTreeSet::new();
    for (ext, _) in st2k_base::formats::FORMATS {
        for path in thumb_keys(ext) {
            assert!(seen.insert(path.clone()), "duplicate displaced key {path}");
        }
    }
}
