#![cfg(test)]

use super::*;

pub(crate) fn dimensions(bytes: &[u8]) {
    let _ = super::dimensions(bytes);
}

pub(crate) fn decode_reduced(bytes: &[u8]) {
    let _ = super::decode_reduced(bytes, 64);
}

/// A codestream `dimensions`/`decode_reduced` actually decode, for
/// `crate::fuzz`'s reach assertion — the same shape `fuzz_tests::sane_header_passes_the_
/// walk_budget` already proves decodes cleanly.
pub(crate) fn seed() -> Vec<u8> {
    hostile_codestream(64, 2, 1, 0, None, &[0u8; 64])
}

/// Whether [`seed`] reaches both parsers, for `crate::fuzz`'s reach assertion.
pub(crate) fn seed_decodes() -> bool {
    let cs = seed();
    super::dimensions(&cs).is_some() && super::decode_reduced(&cs, 64).is_ok()
}
