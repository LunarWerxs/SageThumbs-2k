#![cfg(test)]

use super::*;

/// The balanced-paren case the function exists for: a trailing `)` that closes an earlier
/// `(` inside the URL (a Wikipedia-style `Foo_(bar)` link) must survive trimming.
#[test]
fn a_balanced_trailing_paren_is_kept() {
    let (len, url) = url_at("https://en.wikipedia.org/wiki/Foo_(bar)", 0).unwrap();
    assert_eq!(len, "https://en.wikipedia.org/wiki/Foo_(bar)".len());
    assert_eq!(url, "https://en.wikipedia.org/wiki/Foo_(bar)");
}

/// An unbalanced trailing `)` (prose punctuation, not part of the URL) is trimmed.
#[test]
fn an_unbalanced_trailing_paren_is_trimmed() {
    let (len, url) = url_at("(see https://example.com/x)", 5).unwrap();
    assert_eq!(
        &"(see https://example.com/x)"[5..5 + len],
        "https://example.com/x"
    );
    assert_eq!(url, "https://example.com/x");
}

/// The bug this guards: `trim_trailing_punct` used to recount `(`/`)` over the whole
/// shrinking prefix on every trailing `)` it examined — O(k²) for k trailing close-parens,
/// so a URL followed by hundreds of thousands of `)` (an accepted URL byte) hung the paint
/// thread. The counts are now maintained incrementally, so this must return promptly.
#[test]
fn a_flood_of_trailing_close_parens_does_not_hang() {
    let mut s = String::from("https://a.a/");
    for _ in 0..500_000 {
        s.push(')');
    }
    let started = std::time::Instant::now();
    let (len, _) = url_at(&s, 0).unwrap();
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "trim_trailing_punct took too long on a flood of trailing ')'"
    );
    // None of the flood is balanced by an opening '(', so every one of them is trimmed.
    assert_eq!(len, "https://a.a/".len());
}
