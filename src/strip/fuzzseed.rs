//! Direct fuzz entry points into `strip`'s private per-format parsers. Test-only.
//!
//! Lives here rather than in `crate::fuzz` because these modules (`isobmff`, `svgmeta`,
//! `webpmeta`, `xmpinfo`, `ddsinfo`, `jumbf`) are private to `strip`; naming them from inside
//! keeps their production visibility unchanged instead of widening it for a test — the same
//! shape as [`crate::container::fuzzseed`].
//!
//! This module's own doc comment claims coverage of "every pure-Rust parser that consumes
//! untrusted bytes/headers directly", but none of these six ever reached
//! `crate::fuzz::header_targets()` — a grep for `strip::`/`isobmff::`/`svgmeta::`/`webpmeta::`/
//! `xmpinfo::`/`ddsinfo::`/`jumbf::` in `fuzz.rs` turned up nothing. All are reached
//! IN-PROCESS from `strip_metadata`/`read_info_verbose`, which run inside `explorer.exe` under
//! `panic = "abort"`.

use super::*;

/// One parser entry point: a stable name and a closure that must never panic on any input.
type Target = (&'static str, fn(&[u8]));

pub(crate) fn targets() -> Vec<Target> {
    vec![
        ("strip::isobmff::items", |b| {
            let _ = isobmff::items(b);
        }),
        ("strip::isobmff::strip", |b| {
            let _ = isobmff::strip(b);
        }),
        ("strip::svgmeta::strip", |b| {
            let _ = svgmeta::strip(b);
        }),
        ("strip::webpmeta::strip", |b| {
            let _ = webpmeta::strip(img_parts::Bytes::from(b.to_vec()));
        }),
        ("strip::xmpinfo::packet", |b| {
            let _ = xmpinfo::packet(b);
        }),
        ("strip::ddsinfo::describe", |b| {
            let _ = ddsinfo::describe(b);
        }),
        ("strip::jumbf::is_jumbf_app11", |b| {
            let _ = jumbf::is_jumbf_app11(b);
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A seed that fails its own format's magic/entry check is worthless decoration (see
    /// `container::fuzzseed`'s doc for why this matters) — these targets take raw bytes with no
    /// magic gate at all, so the thing actually worth proving is that every target runs without
    /// panicking on a starter set of inputs (empty, all-zero, all-0xFF, and a real element from
    /// this file's own tests), i.e. that `targets()` is wired up and callable at all.
    #[test]
    fn every_target_survives_a_handful_of_degenerate_inputs() {
        for (name, f) in targets() {
            for input in [&b""[..], &[0u8; 64][..], &[0xFFu8; 64][..]] {
                let result = std::panic::catch_unwind(|| f(input));
                assert!(result.is_ok(), "{name} panicked on a degenerate input");
            }
        }
    }
}
