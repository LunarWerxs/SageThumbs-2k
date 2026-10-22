//! The invisible, selectable text layer.

use super::*;

/// Build invisible text operators for the text layer.
pub(super) fn build_text_content(page: &DjVuPage, dpi: f32, pt_h: f32) -> String {
    let text_layer = match page.text_layer() {
        Ok(Some(tl)) => tl,
        _ => return String::new(),
    };

    let mut ops = String::new();
    // Begin text object
    ops.push_str("BT\n");
    // Set text rendering mode to invisible (mode 3)
    ops.push_str("3 Tr\n");
    // Set font — use a small size, we scale per-word
    ops.push_str("/F1 1 Tf\n");

    // Emit one positioned run per leaf word/character zone (shared zone-walk).
    for span in crate::export_common::word_spans(&text_layer) {
        emit_word_span(&mut ops, span.rect, span.text, dpi, pt_h);
    }

    ops.push_str("ET\n");

    if ops == "BT\n3 Tr\n/F1 1 Tf\nET\n" {
        // No actual text was emitted
        return String::new();
    }

    ops
}

/// Emit text positioning operators for one leaf word/character span.
///
/// `rect` is top-left-origin pixels; PDF uses bottom-left origin, so the
/// baseline is flipped in point space: `pdf_y = pt_h - (r.y + r.height) * 72/dpi`.
/// (This subtract-after-convert order is what produces byte-identical output;
/// see the note on [`crate::export_common::flip_y_bottom`].)
pub(super) fn emit_word_span(ops: &mut String, rect: &Rect, text: &str, dpi: f32, pt_h: f32) {
    let x = px_to_pt(rect.x as f32, dpi);
    let y = pt_h - px_to_pt((rect.y + rect.height) as f32, dpi);
    let w = px_to_pt(rect.width as f32, dpi);
    let h = px_to_pt(rect.height as f32, dpi);

    if w <= 0.0 || h <= 0.0 {
        return;
    }

    // Font size = zone height in points
    let font_size = h;
    if font_size < 0.5 {
        return;
    }

    // Horizontal scale to fit text width
    let text_escaped = pdf_escape_string(text);
    // Sum per-character advance widths using Helvetica metrics.
    let natural_width: f32 = text
        .chars()
        .map(|c| helvetica_advance(c) * font_size)
        .sum::<f32>()
        .max(0.01);
    let h_scale = if natural_width > 0.01 {
        (w / natural_width) * 100.0
    } else {
        100.0
    };

    ops.push_str(&format!(
        "{font_size:.2} 0 0 {font_size:.2} {x:.4} {y:.4} Tm\n"
    ));
    if (h_scale - 100.0).abs() > 1.0 {
        ops.push_str(&format!("{h_scale:.2} Tz\n"));
    }
    ops.push_str(&format!("({text_escaped}) Tj\n"));
}

/// Return the normalized advance width (fraction of em) for `c` in Helvetica.
///
/// Uses standard Helvetica metrics for ASCII, and Unicode-block heuristics
/// for non-ASCII ranges.  CJK, full-width, and Hangul characters are
/// treated as full-width (1.0).  Everything else falls back to 0.556 (the
/// Helvetica average for Latin lowercase).
pub(super) fn helvetica_advance(c: char) -> f32 {
    let cp = c as u32;
    match c {
        // ASCII control / non-printing — zero width
        '\x00'..='\x1f' | '\x7f' => 0.0,
        // Space
        ' ' => 0.278,
        // Digits
        '0'..='9' => 0.556,
        // Common punctuation
        ',' | '.' | ':' | ';' | '!' | '?' => 0.278,
        '\'' | '"' => 0.222,
        '(' | ')' | '[' | ']' | '{' | '}' => 0.333,
        '-' | '\u{2013}' | '\u{2014}' => 0.333,
        // Uppercase ASCII — broad average for Helvetica
        'A'..='Z' => 0.667,
        // Lowercase ASCII
        'a'..='z' => 0.556,
        _ => {
            // CJK Unified Ideographs and common CJK blocks → full-width
            if matches!(cp,
                0x1100..=0x11FF  // Hangul Jamo
                | 0x2E80..=0x2EFF  // CJK Radicals Supplement
                | 0x2F00..=0x2FDF  // Kangxi Radicals
                | 0x3000..=0x303F  // CJK Symbols and Punctuation
                | 0x3040..=0x309F  // Hiragana
                | 0x30A0..=0x30FF  // Katakana
                | 0x3100..=0x312F  // Bopomofo
                | 0x3130..=0x318F  // Hangul Compatibility Jamo
                | 0x3190..=0x31FF  // various CJK
                | 0x3200..=0x32FF  // Enclosed CJK
                | 0x3300..=0x33FF  // CJK Compatibility
                | 0x3400..=0x4DBF  // CJK Extension A
                | 0x4E00..=0x9FFF  // CJK Unified Ideographs
                | 0xA000..=0xA48F  // Yi Syllables
                | 0xA490..=0xA4CF  // Yi Radicals
                | 0xAC00..=0xD7AF  // Hangul Syllables
                | 0xF900..=0xFAFF  // CJK Compatibility Ideographs
                | 0xFE10..=0xFE1F  // Vertical Forms
                | 0xFE30..=0xFE4F  // CJK Compatibility Forms
                | 0xFF00..=0xFFEF  // Halfwidth and Fullwidth Forms
                | 0x1B000..=0x1B0FF // Kana Supplement
                | 0x20000..=0x2A6DF // CJK Extension B
                | 0x2A700..=0x2CEAF // CJK Extensions C/D/E
                | 0x2CEB0..=0x2EBEF // CJK Extension F
                | 0x30000..=0x3134F // CJK Extension G
            ) {
                1.0
            } else {
                // Latin Extended, Cyrillic, Greek, Arabic, Hebrew, etc.
                0.556
            }
        }
    }
}
