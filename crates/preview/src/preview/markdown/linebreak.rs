//! Where a line is ALLOWED to break.
//!
//! Latin text breaks at whitespace, which is all the wrapper used to know. That treats a
//! Chinese, Japanese or Thai paragraph as one enormous word and runs it off the edge of
//! the pane, so those scripts break between characters instead - subject to the usual
//! typesetting rules: a closing bracket or full stop is never pushed to the start of a
//! line, an opening bracket is never stranded at the end of one, and a combining mark
//! never separates from its base.

/// Is there a legal line-break opportunity between `a` and `b`?
///
/// Only ever returns true when at least one side is from a script that doesn't delimit words
/// with spaces — Latin text keeps plain whitespace wrapping, unchanged. This is a deliberately
/// small subset of UAX #14: enough to stop CJK/Thai from overflowing the pane, without pulling
/// in a line-breaking table.
pub(super) fn can_break_between(a: char, b: char) -> bool {
    if a.is_whitespace() || b.is_whitespace() {
        return false; // the whitespace arms already break here
    }
    // A mark has to stay welded to the base character it decorates — most importantly the Thai
    // vowels/tone marks and the Japanese dakuten, which are separate codepoints.
    if is_combining(b) {
        return false;
    }
    if !is_spaceless_script(a) && !is_spaceless_script(b) {
        return false;
    }
    // Kinsoku: closers/terminators may not START a line, openers may not END one.
    !starts_line_never(b) && !ends_line_never(a)
}

/// Characters from scripts written without spaces between words: CJK ideographs and their
/// punctuation, kana, hangul, fullwidth forms, and Thai.
///
/// Thai genuinely needs dictionary-based segmentation, which we don't have — so breaks land at
/// arbitrary character boundaries. That is wrong at the word level and still far better than the
/// alternative, which was the entire paragraph running off-screen.
pub(super) fn is_spaceless_script(c: char) -> bool {
    matches!(c as u32,
        0x1100..=0x11FF     // Hangul Jamo
        | 0x3000..=0x303F   // CJK symbols and punctuation
        | 0x3040..=0x30FF   // Hiragana + Katakana
        | 0x3400..=0x4DBF   // CJK Extension A
        | 0x4E00..=0x9FFF   // CJK Unified Ideographs
        | 0xA000..=0xA4CF   // Yi
        | 0xAC00..=0xD7AF   // Hangul syllables
        | 0xF900..=0xFAFF   // CJK compatibility ideographs
        | 0xFF00..=0xFF60   // Fullwidth forms
        | 0xFFE0..=0xFFE6
        | 0x0E00..=0x0E7F   // Thai
        | 0x2_0000..=0x3_FFFF // CJK Extension B and beyond
    )
}

/// Combining marks that must never be separated from their base character.
pub(super) fn is_combining(c: char) -> bool {
    matches!(c as u32,
        0x0300..=0x036F                          // combining diacriticals
        | 0x0E31 | 0x0E34..=0x0E3A | 0x0E47..=0x0E4E // Thai vowels + tone marks
        | 0x3099..=0x309A                        // dakuten / handakuten
        | 0xFE00..=0xFE0F                        // variation selectors
    )
}

/// Closing/trailing punctuation that may not be pushed to the start of a line.
pub(super) fn starts_line_never(c: char) -> bool {
    matches!(
        c,
        '、' | '。'
            | '，'
            | '．'
            | '！'
            | '？'
            | '；'
            | '：'
            | '）'
            | '】'
            | '》'
            | '」'
            | '』'
            | '〕'
            | '〉'
            | '｝'
            | '〗'
            | '・'
            | 'ー'
            | '々'
            | '〜'
            | '％'
            | '"'
            | '\''
            | '…'
            | '‥'
            | ')'
            | ']'
            | '}'
            | ','
            | '.'
            | '!'
            | '?'
            | ';'
            | ':'
            // small kana conventionally stay with the preceding syllable
            | 'ぁ' | 'ぃ' | 'ぅ' | 'ぇ' | 'ぉ' | 'っ' | 'ゃ' | 'ゅ' | 'ょ'
            | 'ァ' | 'ィ' | 'ゥ' | 'ェ' | 'ォ' | 'ッ' | 'ャ' | 'ュ' | 'ョ'
    )
}

/// Opening punctuation that may not be left dangling at the end of a line.
pub(super) fn ends_line_never(c: char) -> bool {
    matches!(
        c,
        '（' | '【'
            | '《'
            | '「'
            | '『'
            | '〔'
            | '〈'
            | '｛'
            | '〖'
            | '"'
            | '\''
            | '('
            | '['
            | '{'
            | '＄'
            | '￥'
            | '＃'
    )
}

#[cfg(test)]
mod linebreak_tests {
    use super::*;

    #[test]
    fn latin_text_keeps_whitespace_only_wrapping() {
        // No break opportunity inside a Latin word — otherwise every English paragraph would
        // start wrapping mid-word.
        assert!(!can_break_between('h', 'e'));
        assert!(!can_break_between('e', 'l'));
        assert!(!can_break_between('.', 'c')); // "foo.com"
    }

    #[test]
    fn cjk_breaks_between_ideographs() {
        assert!(can_break_between('世', '界'));
        assert!(can_break_between('こ', 'れ'));
        assert!(can_break_between('한', '국'));
        // Latin↔CJK boundaries are break opportunities too.
        assert!(can_break_between('a', '世'));
        assert!(can_break_between('界', 'a'));
    }

    #[test]
    fn kinsoku_punctuation_is_respected() {
        // Closers may not be pushed to the start of the next line.
        assert!(!can_break_between('世', '。'));
        assert!(!can_break_between('世', '、'));
        assert!(!can_break_between('世', '」'));
        // Small kana stays put.
        assert!(!can_break_between('あ', 'っ'));
        // Openers may not be left dangling at the end of a line.
        assert!(!can_break_between('「', '世'));
        assert!(!can_break_between('（', '世'));
        // But a break AFTER a closer is fine.
        assert!(can_break_between('。', '世'));
    }

    #[test]
    fn combining_marks_stay_with_their_base() {
        // Thai vowel/tone marks are separate codepoints; orphaning one renders wrong.
        assert!(!can_break_between('ก', '\u{0E34}'));
        assert!(!can_break_between('ก', '\u{0E49}'));
        // Japanese dakuten likewise.
        assert!(!can_break_between('\u{304B}', '\u{3099}'));
        // Thai without a mark following is a legal (if dictionary-less) break.
        assert!(can_break_between('ก', 'ข'));
    }

    #[test]
    fn whitespace_is_left_to_the_whitespace_arms() {
        assert!(!can_break_between('世', ' '));
        assert!(!can_break_between(' ', '世'));
        assert!(!can_break_between('世', '\n'));
    }
}
