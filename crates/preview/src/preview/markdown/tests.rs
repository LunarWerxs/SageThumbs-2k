#![cfg(test)]

use super::*;

/// `y_for_offset` must return the CONTAINING block's own top, not merely some earlier
/// block that also starts before `off` — this is the exact-jump path `ensure_visible`'s
/// Find-hit scrolling depends on to reach a match many screens away in one hop.
#[test]
fn y_for_offset_returns_the_target_blocks_own_top() {
    let l = MdLayout {
        ready: true,
        heights: vec![10, 20, 30],
        bases: vec![
            DocBase::Runs(vec![0]),  // block 0: doc offsets starting at 0
            DocBase::Runs(vec![10]), // block 1: starts at 10
            DocBase::Code(30),       // block 2: starts at 30
        ],
        ..Default::default()
    };
    assert_eq!(y_for_offset(&l, 0), Some(0));
    assert_eq!(y_for_offset(&l, 5), Some(0)); // still inside block 0
    assert_eq!(y_for_offset(&l, 10), Some(10)); // exactly block 1's start
    assert_eq!(y_for_offset(&l, 25), Some(10)); // inside block 1
    assert_eq!(y_for_offset(&l, 30), Some(30)); // block 2
    assert_eq!(y_for_offset(&l, 999), Some(30)); // past everything: last block wins
}

/// A block with no selectable text (`DocBase::None` — a rule or image) must not break the
/// scan: its height still counts toward later blocks' y, and it neither confirms nor rules
/// out the running `best` match.
#[test]
fn y_for_offset_skips_over_offsetless_blocks_without_losing_its_place() {
    let l = MdLayout {
        ready: true,
        heights: vec![10, 5, 20], // middle block is a rule/image
        bases: vec![
            DocBase::Runs(vec![0]),
            DocBase::None,
            DocBase::Runs(vec![10]),
        ],
        ..Default::default()
    };
    assert_eq!(y_for_offset(&l, 12), Some(15)); // block 2's top: 10 (b0) + 5 (rule)
}

/// A block that was never measured (`h == -1`, nothing has painted that far yet) makes the
/// exact jump untrustworthy past that point — `None`, so the caller falls back to the fine
/// stepper instead of scrolling to a wrong y.
#[test]
fn y_for_offset_is_none_before_the_target_block_is_measured() {
    let l = MdLayout {
        ready: true,
        heights: vec![10, -1, 30], // block 1 never measured
        bases: vec![
            DocBase::Runs(vec![0]),
            DocBase::Runs(vec![10]),
            DocBase::Code(30),
        ],
        ..Default::default()
    };
    assert_eq!(y_for_offset(&l, 25), None);
}

/// The real render, `has_headings`, and `has_remote_images` used to each build their
/// own copy of the same 3-line `Options` block, so a flag added to one could silently
/// desync from the other two (the toolbar would disagree with what actually renders).
/// Locks the shared [`md_options`] to exactly the flags the renderer needs.
#[test]
fn md_options_matches_the_flags_the_renderer_needs() {
    let opts = md_options();
    assert!(opts.contains(Options::ENABLE_TABLES));
    assert!(opts.contains(Options::ENABLE_STRIKETHROUGH));
    assert!(opts.contains(Options::ENABLE_TASKLISTS));
    // Nothing else snuck in - a stray extra/missing bit here is exactly the kind of
    // silent drift the shared helper exists to make impossible.
    assert_eq!(
        opts,
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS
    );
}

/// Collect linkified runs as (text, is_link, dest) triples for assertions.
fn linkify(s: &str) -> Vec<(String, Option<String>)> {
    let mut runs = Vec::new();
    linkify_into(&mut runs, s, false, false, false);
    runs.into_iter().map(|r| (r.text, r.link)).collect()
}

/// What Ctrl+C would put on the clipboard for `md` (pre-CRLF-normalisation).
fn copied(md: &str) -> String {
    build_doc(&parse_blocks(md, false)).0
}

/// Structured Markdown must survive a copy/paste round trip. Every assertion here is a way
/// the old flattening broke it: nesting depth was dropped, blocks were joined with a single
/// newline (so paragraphs merged), and headings/quotes/fences lost their markers entirely.
#[test]
fn copy_preserves_document_structure() {
    let out = copied(
        "# Title\n\nIntro para.\n\nAnother para.\n\n\
         - top\n  - nested\n    - deeper\n- second\n\n\
         1. one\n2. two\n\n> quoted\n\n```rust\nfn f() {}\n```\n\n---\n",
    );
    // Blocks are separated by a BLANK line, so they don't merge into one paragraph.
    assert!(
        out.contains("# Title\n\nIntro para.\n\nAnother para.\n\n"),
        "got:\n{out}"
    );
    // Nesting survives, with a real Markdown bullet rather than the display glyph.
    assert!(
        out.contains("- top\n  - nested\n    - deeper\n- second"),
        "got:\n{out}"
    );
    assert!(
        !out.contains('\u{2022}'),
        "display bullet leaked into the copy:\n{out}"
    );
    // Consecutive items stay TIGHT (no blank line) or the list re-renders loose.
    assert!(!out.contains("- top\n\n"), "list went loose:\n{out}");
    // Ordered lists keep their numbers; quotes and fences keep their markers.
    assert!(out.contains("1. one\n2. two"), "got:\n{out}");
    // A DIFFERENT list still gets its blank line, or the two run together as one mangled list.
    assert!(
        out.contains("- second\n\n1. one"),
        "lists butted together:\n{out}"
    );
    assert!(out.contains("> quoted"), "got:\n{out}");
    assert!(out.contains("```rust\nfn f() {}\n```"), "got:\n{out}");
    assert!(out.contains("---"), "got:\n{out}");
}

/// GFM order is marker THEN checkbox. Emitting the box in place of the bullet produced
/// "[x] done", which no Markdown renderer treats as a task list.
#[test]
fn copy_emits_valid_gfm_task_items() {
    let out = copied("- [x] done\n- [ ] todo\n");
    assert!(out.contains("- [x] done"), "got:\n{out}");
    assert!(out.contains("- [ ] todo"), "got:\n{out}");
}

/// A block that appends nothing must not leave its separator behind as a stray blank line,
/// and the document must never start with one.
#[test]
fn copy_has_no_stray_blank_lines() {
    let out = copied("para one\n\npara two\n");
    assert!(!out.starts_with('\n'), "leading blank line:\n{out}");
    assert!(!out.contains("\n\n\n"), "doubled separator:\n{out}");
    assert!(out.ends_with('\n'));
}

#[test]
fn bare_https_becomes_a_link() {
    let r = linkify("see https://example.com/x now");
    assert_eq!(
        r,
        vec![
            ("see ".into(), None),
            (
                "https://example.com/x".into(),
                Some("https://example.com/x".into())
            ),
            (" now".into(), None),
        ]
    );
}

#[test]
fn www_gets_https_scheme() {
    let r = linkify("go www.example.com today");
    assert_eq!(
        r[1],
        (
            "www.example.com".into(),
            Some("https://www.example.com".into())
        )
    );
}

#[test]
fn trailing_punctuation_trimmed_but_url_kept() {
    // sentence-ending period is not part of the link
    let r = linkify("visit https://example.com.");
    assert_eq!(
        r[1],
        (
            "https://example.com".into(),
            Some("https://example.com".into())
        )
    );
    assert_eq!(r[2].0, ".");
}

#[test]
fn balanced_paren_kept_unbalanced_trimmed() {
    let kept = url_at("https://en.wikipedia.org/wiki/Foo_(bar)", 0).unwrap();
    assert_eq!(kept.1, "https://en.wikipedia.org/wiki/Foo_(bar)");
    // a wrapping paren is NOT swallowed: "(https://x.com)" trims the trailing ')'
    let wrapped = url_at("(https://x.com)", 1).unwrap();
    assert_eq!(wrapped.1, "https://x.com");
}

#[test]
fn no_match_mid_word_or_without_dot() {
    assert!(url_at("foohttps://x.com", 3).is_none()); // 'o' precedes → not a boundary
    assert!(url_at("https://localhost", 0).is_none()); // no dot in host
    assert!(url_at("https://", 0).is_none()); // bare scheme
}

#[test]
fn multibyte_after_url_is_safe() {
    // a CJK period right after the URL must not panic on a non-char-boundary slice
    let r = linkify("https://example.com。あと");
    assert_eq!(r[0].1, Some("https://example.com".into()));
    assert!(r[1].0.starts_with('。'));
}
