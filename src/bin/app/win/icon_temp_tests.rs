#![cfg(test)]

use super::*;

/// `claim_icon_temp_file` must never write through an already-claimed name: a second
/// call while the first candidate is still held must land on a DIFFERENT path, proving
/// `create_new` (not `std::fs::write`) is what decides the name — the guard against a
/// pre-planted hard link or reparse point at the predictable-looking first candidate.
#[test]
fn claim_icon_temp_file_never_reuses_a_held_name() {
    let first = claim_icon_temp_file().expect("must claim a %TEMP% name");
    assert_eq!(
        std::fs::read(&first).expect("claimed file must be readable"),
        APP_ICO,
        "the claimed temp file must hold exactly the embedded icon bytes"
    );

    let second = claim_icon_temp_file().expect("must fall through to the next candidate");
    assert_ne!(
        first, second,
        "a still-held name must never be reused/overwritten by a later claim"
    );
    assert_eq!(
        std::fs::read(&first).expect("the first file must be untouched"),
        APP_ICO,
        "the first claim's file must be unaffected by the second claim"
    );

    let _ = std::fs::remove_file(&first);
    let _ = std::fs::remove_file(&second);
}
