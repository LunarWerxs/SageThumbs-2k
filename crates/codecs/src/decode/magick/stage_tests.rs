#![cfg(test)]

use super::*;

/// The invariant the leak fix is about: the guard owns the path from the moment the
/// file exists, so every exit unlinks it.
#[test]
fn the_staged_file_lives_exactly_as_long_as_its_guard() {
    let guard = NamedTemp::create(b"payload", "rla").expect("staging must succeed in %TEMP%");
    let path = guard.0.clone();
    assert!(
        path.is_file(),
        "the staged file should exist while the guard does"
    );
    assert_eq!(
        std::fs::read(&path).expect("staged file must be readable"),
        b"payload",
        "the staged bytes must be the ones handed to ImageMagick"
    );
    // create_new on the same name must now refuse — proof the file is really claimed,
    // which is what stops a pre-planted hard link or reparse point being written through.
    let second = std::fs::File::options()
        .write(true)
        .create_new(true)
        .open(&path);
    assert!(
        second.is_err_and(|e| e.kind() == std::io::ErrorKind::AlreadyExists),
        "the staged name must be exclusively held"
    );
    drop(guard);
    assert!(
        !path.exists(),
        "dropping the guard must remove the staged file, got a leftover at {path:?}"
    );
}

/// The counter must hand out distinct names, or two concurrent decodes would fight over
/// one file and the first to finish would delete the other's input.
#[test]
fn concurrently_staged_files_never_share_a_name() {
    let guards: Vec<_> = (0..8)
        .map(|_| NamedTemp::create(b"x", "tim").expect("staging must succeed"))
        .collect();
    let mut paths: Vec<_> = guards.iter().map(|g| g.0.clone()).collect();
    paths.sort();
    let unique = paths.len();
    paths.dedup();
    assert_eq!(paths.len(), unique, "staged names collided: {paths:?}");
    for g in &guards {
        assert!(g.0.is_file(), "every staged file should exist: {:?}", g.0);
    }
    drop(guards);
    for p in &paths {
        assert!(!p.exists(), "leftover after drop: {p:?}");
    }
}

/// The one with real teeth, and deterministic: the name is ours, so nothing else in the
/// process can consume it first.
///
/// This is the assertion that fails against `File::create`, which maps to Windows
/// CREATE_ALWAYS: that follows hard links and reparse points and truncates whatever the
/// name resolves to, so a planted name in %TEMP% received our image bytes.
#[test]
fn a_squatted_name_is_refused_rather_than_written_through() {
    const SENTINEL: &[u8] = b"do not clobber me";
    let path = std::env::temp_dir().join(format!(
        "st2k-coder-squat-{}-{:p}.rla",
        std::process::id(),
        &SENTINEL
    ));
    std::fs::write(&path, SENTINEL).expect("the test must be able to plant a file");

    let claimed = NamedTemp::claim(path.clone(), b"image bytes that must not land here");
    assert!(
        claimed.is_none(),
        "an already-existing name must be refused, not claimed"
    );
    assert_eq!(
        std::fs::read(&path).ok().as_deref(),
        Some(SENTINEL),
        "the existing file was written THROUGH instead of being left alone"
    );

    let _ = std::fs::remove_file(&path);
}

/// The other half: a name nobody holds is claimed, filled, and released on drop.
#[test]
fn a_free_name_is_claimed_filled_and_released() {
    let path = std::env::temp_dir().join(format!(
        "st2k-coder-free-{}-{:p}.rla",
        std::process::id(),
        &MAX_STAGE_ATTEMPTS
    ));
    let _ = std::fs::remove_file(&path);
    let guard = NamedTemp::claim(path.clone(), b"payload").expect("a free name must be claimed");
    assert_eq!(std::fs::read(&path).ok().as_deref(), Some(&b"payload"[..]));
    drop(guard);
    assert!(!path.exists(), "dropping the guard must remove {path:?}");
}

/// A refused extension must never reach the filesystem at all.
#[test]
fn a_refused_extension_stages_nothing() {
    for ext in ["../../evil", "png", "", "waytoolongextension"] {
        assert!(
            decode_named_extension(b"whatever", ext, None).is_err(),
            "{ext:?} must be refused"
        );
    }
}
