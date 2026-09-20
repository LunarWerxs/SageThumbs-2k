#![cfg(test)]

use super::*;

/// Front matter with a closing fence becomes ONE preformatted Yaml code block ahead of the
/// document body, going through the same `Block::Code` path a fenced ```yaml block would.
#[test]
fn front_matter_present_becomes_a_code_block() {
    let blocks = parse_blocks("---\ntitle: Test\ndraft: false\n---\n\n# Body\n", false);
    match &blocks[0] {
        Block::Code(text, lang) => {
            assert_eq!(text, "title: Test\ndraft: false");
            assert!(matches!(lang, highlight::Lang::Yaml));
        }
        _ => panic!("expected a front-matter code block first"),
    }
    assert!(
        matches!(&blocks[1], Block::Heading(1, ..)),
        "body must follow the fenced-off front matter"
    );
}

/// No closing `---` means it is NOT front matter (maybe just forgotten, maybe the file was
/// never meant to have any) — the pre-pass must change nothing, so it renders exactly as it
/// did before: a rule, then the "fields" as a stray paragraph.
#[test]
fn unterminated_front_matter_is_left_alone() {
    let blocks = parse_blocks("---\ntitle: Test\ndraft: false\n\nBody text.\n", false);
    assert!(matches!(&blocks[0], Block::Rule));
    assert!(matches!(&blocks[1], Block::Para(..)));
}

/// A document that legitimately opens with a thematic break must keep rendering as one —
/// there is no closing `---` here either, so the leading rule is untouched.
#[test]
fn leading_rule_followed_by_paragraph_stays_a_rule() {
    let blocks = parse_blocks("---\n\nJust a normal paragraph.\n", false);
    assert!(matches!(&blocks[0], Block::Rule));
    assert!(matches!(&blocks[1], Block::Para(..)));
}
