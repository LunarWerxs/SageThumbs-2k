//! Audit F29 (2026-09-06) source contract: the screenshot editor's tool labels, toolbar
//! tooltips, selection hints and text-flyout captions, plus the Quick preview's outline
//! header, must be localized rather than hardcoded English.
//!
//! Two independent checks, neither of which a build or the existing test suite could catch on
//! its own:
//!
//! 1. **Literal removal** — the exact English sentences these five files used to hardcode must
//!    be GONE from the source (a translator fixing the locale tables cannot fix code that still
//!    ignores them). `tools.rs` gets a narrower rule: its hint-strip words must vanish from the
//!    localized `hint_label` function specifically, while the deliberately-unlocalized
//!    `label()` (the `--screenshot-automation` window-title identifier `tests/screenshot_automation.rs`
//!    parses) keeps them — see that function's doc comment for why.
//! 2. **Real translation, not a copy** — for a sample of the new keys, `assets/locales/fr.toml`
//!    must carry a DIFFERENT value than `assets/locales/en.toml`. A locale file that just
//!    copy-pasted the English text would pass every other gate (key parity, placeholder parity,
//!    `cargo check`'s duplicate-key guard) while shipping unfixed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

fn repo_path(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(rel)
}

fn read(rel: &str) -> String {
    std::fs::read_to_string(repo_path(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"))
}

/// Parse a locale TOML's flat `key = "value"` lines into a map (same shape
/// `scripts/check-locale-keys.ps1` assumes: no nesting, no multiline values).
fn locale_map(rel: &str) -> HashMap<String, String> {
    let text = read(rel);
    let mut map = HashMap::new();
    for line in text.lines() {
        let line = line.trim_end();
        let Some(eq) = line.find(" = \"") else {
            continue;
        };
        let key = line[..eq].trim();
        if key.is_empty() || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            continue;
        }
        let Some(rest) = line.strip_prefix(&format!("{key} = \"")) else {
            continue;
        };
        let Some(value) = rest.strip_suffix('"') else {
            continue;
        };
        map.insert(key.to_string(), value.to_string());
    }
    map
}

/// Assert none of `literals` appear anywhere in the file at `rel`.
fn assert_literals_absent(rel: &str, literals: &[&str]) {
    let text = read(rel);
    for lit in literals {
        assert!(
            !text.contains(lit),
            "{rel} still contains the pre-fix hardcoded literal {lit:?} — audit F29 requires \
             this string to come from the locale table instead"
        );
    }
}

#[test]
fn toolbar_tooltips_no_longer_hardcode_english() {
    assert_literals_absent(
        "src/bin/app/screenshot/toolbar.rs",
        &[
            "Rectangle (R) — drag to draw",
            "Ellipse (O) — drag to draw",
            "Arrow (A) — drag tail to head",
            "Line (L) — drag to draw",
            "Pen (P) — freehand draw",
            "Text (T) — click then type",
            "Number (N) — click to drop 1, 2, 3…",
            "Highlight (H) — translucent marker",
            "Pixelate (B) — blur/blockify a region",
            "Invert (I) — invert a region's colours",
            "Pick colour (E) — click a pixel to copy its hex",
            "Move (M) — drag a shape",
            "Colour (K) — cycle the palette",
            "Undo (Ctrl+Z)",
            "Redo (Ctrl+Y / Ctrl+Shift+Z)",
            "Copy to the clipboard (Ctrl+C / Enter)",
            "Copy text (OCR) (Ctrl+T) — read the words in the region",
            "Save a PNG (Ctrl+S)",
            "Upload & copy the link (Ctrl+U)",
            "Close (Esc)",
        ],
    );
}

#[test]
fn selection_hint_strip_no_longer_hardcodes_english() {
    // Precise old CODE shapes (a quoted literal immediately where the removed source had one),
    // not bare words — a bare "Ctrl-drag moves" or "Shift snaps 45°" could coincidentally match
    // a doc comment or a test fixture using an unrelated marker string, which would make this
    // test cry wolf on unrelated, legitimate text.
    assert_literals_absent(
        "src/bin/app/screenshot/overlay/paint.rs",
        &[
            "Ctrl-drag moves  ·  Enter copy  ·  Ctrl+T text  ·  Ctrl+S save  ·  Esc close",
            "\"  ·  F8 snap 45° ON\"",
            "\"  ·  F8 snap 45° OFF\"",
            "\"  ·  Shift snaps 45°\"",
            "format!(\"size {}\", s.thickness)",
            "format!(\"text {}\", -s.text_font.lfHeight)",
        ],
    );
}

#[test]
fn text_flyout_captions_no_longer_hardcode_english() {
    assert_literals_absent(
        "src/bin/app/screenshot/toolbar/textflyout.rs",
        &[
            "[x]  Bold",
            "[  ]  Bold",
            "[x]  Underline",
            "[  ]  Underline",
            "Font\\u{2026} (more)",
        ],
    );
}

#[test]
fn preview_outline_header_no_longer_hardcodes_contents() {
    // The precise old code shape, not the bare word: a bare "CONTENTS" would also match this
    // very fix's own doc comments and the necessary `assert_eq!(..., "CONTENTS")` in
    // `outline_header_matches_the_locale_tables_english_value` (paint.rs's own test module),
    // which legitimately still needs the English value as a literal to compare against.
    assert_literals_absent(
        "src/bin/app/preview/paint.rs",
        &["\"CONTENTS\".encode_utf16()"],
    );
}

/// `tools.rs` is special: `label()` MUST keep the bare English words (the
/// `--screenshot-automation` window-title contract depends on it staying fixed), so this checks
/// the narrower claim — the localized `hint_label` function's own body is free of them. Extracted
/// by locating the function's signature and its balanced closing brace rather than a fixed line
/// count, so a later edit that shifts the function doesn't silently stop checking anything.
#[test]
fn tool_hint_label_body_has_no_hardcoded_display_words() {
    let text = read("src/bin/app/screenshot/tools.rs");
    let start = text
        .find("fn hint_label(self) -> &'static str {")
        .expect("hint_label must exist");
    let body_start = start + text[start..].find('{').expect("opening brace");
    let mut depth = 0i32;
    let mut end = body_start;
    for (i, ch) in text[body_start..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end = body_start + i + 1;
                    break;
                }
            }
            _ => {}
        }
    }
    let body = &text[body_start..end];
    assert!(
        !body.is_empty() && body.len() < text.len(),
        "brace matching failed"
    );
    for word in [
        "\"Rect\"",
        "\"Ellipse\"",
        "\"Arrow\"",
        "\"Line\"",
        "\"Pen\"",
        "\"Text\"",
        "\"Number\"",
        "\"Highlight\"",
        "\"Pixelate\"",
        "\"Invert\"",
        "\"Pick\"",
        "\"Move\"",
    ] {
        assert!(
            !body.contains(word),
            "hint_label's body still contains the hardcoded literal {word} — it must look the \
             string up via t(\"shot_tool_short_...\") instead"
        );
    }
    // And it must actually call into the locale system at all — an empty/no-op body would
    // trivially pass the check above too.
    assert!(
        body.contains("t(match self"),
        "hint_label no longer routes through t()"
    );
}

/// A sample of the new F29 keys must carry a REAL French translation, not an English copy —
/// key/placeholder parity (`scripts/check-locale-keys.ps1`) cannot see this, and neither can
/// `cargo check`'s duplicate-key guard.
#[test]
fn a_sample_of_the_new_keys_are_actually_translated_in_french() {
    let en = locale_map("assets/locales/en.toml");
    let fr = locale_map("assets/locales/fr.toml");
    let sample = [
        "shot_tool_short_rect",
        "shot_tool_short_move",
        "shot_tip_rect",
        "shot_tip_close",
        "shot_hint_active",
        "shot_hint_snap_shift",
        "shot_text_bold",
        "shot_text_underline",
        "shot_text_more_fonts",
        "preview_outline_header",
    ];
    for key in sample {
        let en_val = en
            .get(key)
            .unwrap_or_else(|| panic!("en.toml missing {key}"));
        let fr_val = fr
            .get(key)
            .unwrap_or_else(|| panic!("fr.toml missing {key}"));
        assert_ne!(
            en_val, fr_val,
            "fr.toml's {key} is identical to en.toml's — looks like an untranslated copy"
        );
    }
}

/// Every new key this finding introduced must exist, with matching `{token}` placeholders, in
/// EVERY shipped locale — the mechanical half `scripts/check-locale-keys.ps1` already owns, run
/// here too so `cargo test` alone (no PowerShell) still catches a locale a fan-out batch missed.
#[test]
fn every_new_key_exists_with_matching_placeholders_in_every_locale() {
    let en = locale_map("assets/locales/en.toml");
    let new_keys = [
        "shot_tool_short_rect",
        "shot_tool_short_ellipse",
        "shot_tool_short_arrow",
        "shot_tool_short_line",
        "shot_tool_short_pen",
        "shot_tool_short_text",
        "shot_tool_short_number",
        "shot_tool_short_highlight",
        "shot_tool_short_pixelate",
        "shot_tool_short_invert",
        "shot_tool_short_eyedropper",
        "shot_tool_short_move",
        "shot_tip_rect",
        "shot_tip_ellipse",
        "shot_tip_arrow",
        "shot_tip_line",
        "shot_tip_pen",
        "shot_tip_text",
        "shot_tip_number",
        "shot_tip_highlight",
        "shot_tip_pixelate",
        "shot_tip_invert",
        "shot_tip_eyedropper",
        "shot_tip_move",
        "shot_tip_color",
        "shot_tip_undo",
        "shot_tip_redo",
        "shot_tip_copy",
        "shot_tip_ocr",
        "shot_tip_save",
        "shot_tip_upload",
        "shot_tip_close",
        "shot_hint_size_generic",
        "shot_hint_size_text",
        "shot_hint_snap_on",
        "shot_hint_snap_off",
        "shot_hint_snap_shift",
        "shot_hint_active",
        "shot_text_bold",
        "shot_text_underline",
        "shot_text_more_fonts",
        "shot_text_size_down",
        "shot_text_size_up",
        "shot_text_more_options",
        "shot_text_font_field",
        "preview_outline_header",
    ];
    for key in new_keys {
        assert!(
            en.contains_key(key),
            "en.toml is missing its own new key {key}"
        );
    }

    let placeholders = |v: &str| -> Vec<String> {
        let mut out = Vec::new();
        let mut rest = v;
        while let Some(open) = rest.find('{') {
            let Some(close) = rest[open..].find('}') else {
                break;
            };
            out.push(rest[open..open + close + 1].to_string());
            rest = &rest[open + close + 1..];
        }
        out.sort();
        out
    };

    let locale_dir = repo_path("assets/locales");
    let mut checked_locales = 0usize;
    for entry in std::fs::read_dir(&locale_dir).expect("read assets/locales") {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if name == "en.toml" {
            continue;
        }
        checked_locales += 1;
        let rel = format!("assets/locales/{name}");
        let map = locale_map(&rel);
        for key in new_keys {
            let Some(got) = map.get(key) else {
                panic!("{name} is missing the new F29 key {key}");
            };
            let want_slots = placeholders(en[key].as_str());
            let got_slots = placeholders(got);
            assert_eq!(
                want_slots, got_slots,
                "{name}'s {key} placeholder set {got_slots:?} does not match en.toml's \
                 {want_slots:?}"
            );
        }
    }
    assert_eq!(checked_locales, 35, "expected 35 non-English locale files");
}
