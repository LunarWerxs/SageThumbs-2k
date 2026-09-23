#![cfg(test)]

use super::FontKey;

// `FontKey` equality is what makes cache lookups hit/miss correctly; it's the one part of
// `FontCache` that's pure enough to test without a live HWND. The `get`/`clear`/`Drop`
// behavior around it is exercised indirectly by every existing `--shot --window preview`
// markdown capture, same as `Fonts` always was.
#[test]
fn same_style_is_the_same_key() {
    let a = FontKey {
        px: 16,
        bold: false,
        italic: false,
    };
    let b = FontKey {
        px: 16,
        bold: false,
        italic: false,
    };
    assert!(a == b);
}

#[test]
fn px_bold_and_italic_each_change_the_key() {
    let base = FontKey {
        px: 16,
        bold: false,
        italic: false,
    };
    assert!(
        base != FontKey {
            px: 17,
            bold: false,
            italic: false
        }
    );
    assert!(
        base != FontKey {
            px: 16,
            bold: true,
            italic: false
        }
    );
    assert!(
        base != FontKey {
            px: 16,
            bold: false,
            italic: true
        }
    );
}
