//! Audit F29 (2026-09-06) source contract: the screenshot editor's tool labels, toolbar
//! tooltips, selection hints and text-flyout captions, plus the Quick preview's outline
//! header, must be localized rather than hardcoded English.
//!
//! Two independent checks, neither of which a build or the existing test suite could catch on
//! its own:
//!
//! 1. **Literal removal** - the exact English sentences these five files used to hardcode must
//!    be GONE from the source (a translator fixing the locale tables cannot fix code that still
//!    ignores them). `tools.rs` gets a narrower rule: its hint-strip words must vanish from the
//!    localized `hint_label` function specifically, while the deliberately-unlocalized
//!    `label()` (the `--screenshot-automation` window-title identifier `tests/screenshot_automation.rs`
//!    parses) keeps them - see that function's doc comment for why.
//! 2. **Real translation, not a copy** - for a sample of the new keys, `assets/locales/fr.toml`
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
    // not bare words - a bare "Ctrl-drag moves" or "Shift snaps 45°" could coincidentally match
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
/// the narrower claim - the localized `hint_label` function's own body is free of them. Extracted
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
    // And it must actually call into the locale system at all - an empty/no-op body would
    // trivially pass the check above too.
    assert!(
        body.contains("t(match self"),
        "hint_label no longer routes through t()"
    );
}

/// A sample of the new F29 keys must carry a REAL French translation, not an English copy -
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
/// EVERY shipped locale - the mechanical half `scripts/check-locale-keys.ps1` already owns, run
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
        // Found by `no_new_hardcoded_display_strings` below, not by the original audit sweep:
        // the Quick preview's decode placeholder was still a bare English literal.
        "preview_loading",
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

// ---------------------------------------------------------------------------------------
// Forward-looking guard: a string that NEVER became a key.
//
// The audit's own acceptance note for F29 says static key parity cannot detect this class,
// and it is right: `scripts/check-locale-keys.ps1` compares en.toml against the other 35
// files, so a sentence that only ever existed as a Rust literal is invisible to it, and so
// is every test above (they name the specific literals the fix removed, which catches a
// revert but not the NEXT hardcoded string somebody adds).
//
// So this scans the files F29 localized and requires every display-looking literal in them
// to be either routed through `t()` or listed in `ALLOWED_LITERALS` with a stated reason.
// Adding a line to that table is a deliberate, reviewable act; forgetting to localize is not.
//
// It deliberately does NOT assert that every allowed entry is still present. Deleting a
// literal is tidying, not a defect, and a gate that goes red on tidy-up gets ignored, which
// is how it would come to miss the real thing.
// ---------------------------------------------------------------------------------------

/// The paint/label files F29 localized. New user-visible text overwhelmingly lands here.
const SCANNED_FILES: [&str; 5] = [
    "src/bin/app/screenshot/tools.rs",
    "src/bin/app/screenshot/toolbar.rs",
    "src/bin/app/screenshot/overlay/paint.rs",
    "src/bin/app/screenshot/toolbar/textflyout.rs",
    "src/bin/app/preview/paint.rs",
];

/// `(file, literal, why it is not translatable)`. Every entry is a literal a human decided
/// must stay fixed, not a shape the scan happens to miss.
const ALLOWED_LITERALS: &[(&str, &str, &str)] = &[
    // `Tool::label()` is the fixed-English `--screenshot-automation` window-title identifier
    // that `tests/screenshot_automation.rs` parses (`tool=Rect`). Localizing it would make
    // that harness depend on the machine's UI language. Its localized twin is `hint_label`,
    // which `tool_hint_label_body_has_no_hardcoded_display_words` above guards.
    ("tools.rs", "Rect", "Tool::label automation identifier"),
    ("tools.rs", "Ellipse", "Tool::label automation identifier"),
    ("tools.rs", "Arrow", "Tool::label automation identifier"),
    ("tools.rs", "Line", "Tool::label automation identifier"),
    ("tools.rs", "Pen", "Tool::label automation identifier"),
    ("tools.rs", "Text", "Tool::label automation identifier"),
    ("tools.rs", "Number", "Tool::label automation identifier"),
    ("tools.rs", "Highlight", "Tool::label automation identifier"),
    ("tools.rs", "Pixelate", "Tool::label automation identifier"),
    ("tools.rs", "Invert", "Tool::label automation identifier"),
    ("tools.rs", "Pick", "Tool::label automation identifier"),
    ("tools.rs", "Move", "Tool::label automation identifier"),
    (
        "tools.rs",
        "Tool::DEFAULTABLE and settings::SHOT_TOOL_COUNT disagree",
        "compile-time assert message, seen by a developer building the crate, never by a user",
    ),
    // Windows font FACE names. These are looked up by name in the system font table, so a
    // translated value would silently fall back to a substitute face.
    ("tools.rs", "Segoe UI", "Windows font face name"),
    ("textflyout.rs", "Segoe UI", "Windows font face name"),
    ("textflyout.rs", "Arial", "Windows font face name"),
    ("textflyout.rs", "Calibri", "Windows font face name"),
    ("textflyout.rs", "Verdana", "Windows font face name"),
    ("textflyout.rs", "Tahoma", "Windows font face name"),
    ("textflyout.rs", "Consolas", "Windows font face name"),
    ("textflyout.rs", "Times New Roman", "Windows font face name"),
    ("textflyout.rs", "Comic Sans MS", "Windows font face name"),
    ("preview/paint.rs", "Consolas", "Windows font face name"),
    (
        "textflyout.rs",
        "{size} px",
        "px is the unit abbreviation, written px in every locale this app ships; a key here \
         would be the same string 36 times",
    ),
    // The `--screenshot-automation` stand-in desktop. It exists so the harness never captures
    // real screen pixels, and it is drawn only under that flag, so no shipped run can show it.
    (
        "overlay/paint.rs",
        "SYNTHETIC FULL-SCREEN AUTOMATION CANVAS",
        "test-harness canvas, only ever drawn under --screenshot-automation",
    ),
    (
        "overlay/paint.rs",
        "Safe test surface: no desktop pixels, clipboard, files, dialogs, or uploads",
        "test-harness canvas, only ever drawn under --screenshot-automation",
    ),
];

/// Overwrite `from..to` with spaces so later offsets and line numbers still line up.
fn blank(out: &mut [char], from: usize, to: usize) {
    for slot in out.iter_mut().take(to).skip(from) {
        *slot = ' ';
    }
}

/// Index just past the line comment starting at `i`.
fn line_comment_end(c: &[char], i: usize) -> usize {
    (i..c.len()).find(|&k| c[k] == '\n').unwrap_or(c.len())
}

/// Index just past the raw string (`r"..."`, `r#"..."#`) starting at `i`, or `None` if one
/// does not start there. Raw strings in these files are format/path helpers, never display
/// text, so their contents are skipped rather than scanned.
fn raw_string_end(c: &[char], i: usize) -> Option<usize> {
    if c[i] != 'r' {
        return None;
    }
    // `r` immediately after an identifier character is the tail of a name, not a prefix.
    if i > 0 && (c[i - 1].is_alphanumeric() || c[i - 1] == '_') {
        return None;
    }
    let mut j = i + 1;
    let mut hashes = 0usize;
    while c.get(j) == Some(&'#') {
        hashes += 1;
        j += 1;
    }
    if c.get(j) != Some(&'"') {
        return None;
    }
    let mut k = j + 1;
    while k < c.len() {
        if c[k] == '"' && c[k + 1..].iter().take(hashes).all(|ch| *ch == '#') {
            return Some(k + 1 + hashes);
        }
        k += 1;
    }
    Some(c.len())
}

/// Index just past the ordinary string literal starting at `i`.
fn string_end(c: &[char], i: usize) -> usize {
    let mut j = i + 1;
    while j < c.len() {
        match c[j] {
            '\\' => j += 2,
            '"' => return j + 1,
            _ => j += 1,
        }
    }
    c.len()
}

/// One pass over the source producing both halves this guard needs: every ordinary string
/// literal with its start offset, and a "skeleton" in which those literals, raw strings and
/// comments are blanked. The skeleton is what the `#[cfg(test)]` brace matching runs on, so a
/// stray `{` inside an assertion message cannot throw the brace count off.
fn scan_source(src: &str) -> (Vec<char>, Vec<(usize, String)>) {
    let c: Vec<char> = src.chars().collect();
    let mut skeleton = c.clone();
    let mut literals = Vec::new();
    let mut i = 0;
    while i < c.len() {
        if let Some(end) = raw_string_end(&c, i) {
            blank(&mut skeleton, i, end);
            i = end;
        } else if c[i] == '"' {
            let end = string_end(&c, i);
            // An UNTERMINATED literal ends at EOF with no closing quote to step back over, so
            // clamp rather than letting `end - 1` fall below the opening quote and panic.
            let inner = end.saturating_sub(1).max(i + 1);
            literals.push((i, c[i + 1..inner].iter().collect()));
            blank(&mut skeleton, i, end);
            i = end;
        } else if c[i] == '/' && c.get(i + 1) == Some(&'/') {
            let end = line_comment_end(&c, i);
            blank(&mut skeleton, i, end);
            i = end;
        } else {
            i += 1;
        }
    }
    (skeleton, literals)
}

/// First index of `needle` in `hay` at or after `from`.
fn find_chars(hay: &[char], needle: &str, from: usize) -> Option<usize> {
    let pat: Vec<char> = needle.chars().collect();
    let last = hay.len().checked_sub(pat.len())?;
    (from..=last).find(|&i| hay[i..i + pat.len()] == pat[..])
}

/// The char ranges covered by `#[cfg(test)]` modules.
///
/// Brace-matched rather than "cut the file at the first marker", which is the obvious
/// shortcut and is wrong here: `tools.rs` has TWO test modules and the first starts at line
/// 132 of 840, so cutting there would stop scanning three quarters of the production code and
/// report a clean file.
fn test_module_ranges(skeleton: &[char]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut from = 0;
    while let Some(at) = find_chars(skeleton, "#[cfg(test)]", from) {
        let Some(open) = (at..skeleton.len()).find(|&i| skeleton[i] == '{') else {
            ranges.push((at, skeleton.len()));
            break;
        };
        let mut depth = 0i32;
        let mut end = skeleton.len();
        for (i, ch) in skeleton.iter().enumerate().skip(open) {
            match ch {
                '{' => depth += 1,
                '}' => depth -= 1,
                _ => {}
            }
            if depth == 0 {
                end = i + 1;
                break;
            }
        }
        ranges.push((at, end));
        from = end;
    }
    ranges
}

/// The literal with every `{...}` group removed, so a pure substitution template such as
/// `"{tool}"` or an escape such as `"\u{25BE}"` is not mistaken for a sentence.
fn without_braced_groups(v: &str) -> String {
    let mut out = String::new();
    let mut depth = 0usize;
    for ch in v.chars() {
        match ch {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(ch),
            _ => {}
        }
    }
    out
}

/// Does this literal read as text a user would see, rather than a key, an identifier or a
/// format token? Two adjacent ASCII letters outside any `{...}` group is the "is it a word"
/// test; an all-lowercase-and-underscores value is a locale key or an ident, not prose.
fn looks_like_display_text(v: &str) -> bool {
    let bare = without_braced_groups(v);
    let letters: Vec<char> = bare.chars().collect();
    let has_word = letters
        .windows(2)
        .any(|w| w[0].is_ascii_alphabetic() && w[1].is_ascii_alphabetic());
    if !has_word {
        return false;
    }
    !v.chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
}

#[test]
fn no_new_hardcoded_display_strings() {
    let mut offenders: Vec<String> = Vec::new();
    for rel in SCANNED_FILES {
        let src = read(rel);
        let (skeleton, literals) = scan_source(&src);
        let skipped = test_module_ranges(&skeleton);
        let chars: Vec<char> = src.chars().collect();
        for (at, value) in literals {
            if skipped.iter().any(|(a, b)| at >= *a && at < *b) {
                continue;
            }
            if !looks_like_display_text(&value) {
                continue;
            }
            let allowed = ALLOWED_LITERALS
                .iter()
                .any(|(file, lit, _)| rel.ends_with(file) && *lit == value);
            if allowed {
                continue;
            }
            let line = 1 + chars[..at].iter().filter(|ch| **ch == '\n').count();
            offenders.push(format!("{rel}:{line}: {value:?}"));
        }
    }
    assert!(
        offenders.is_empty(),
        "user-visible text is hardcoded in these files instead of coming from the locale \
         tables (audit F29). Route each one through t(\"some_key\") and add the key to ALL \
         of assets/locales/*.toml, or, if it genuinely must not be translated, add it to \
         ALLOWED_LITERALS in this file with the reason:\n  {}",
        offenders.join("\n  ")
    );
}

/// The guard is only worth having if it actually fires, and a scan that silently matched
/// nothing would pass `no_new_hardcoded_display_strings` just as quietly as a clean tree.
/// This pins the machinery against fixtures instead of trusting the real files to exercise it.
#[test]
fn the_hardcoded_string_guard_detects_what_it_claims_to() {
    assert!(looks_like_display_text("Loading…"));
    assert!(looks_like_display_text("Rectangle (R) drag to draw"));
    // Not prose: a locale key, a bare substitution template, punctuation, a single letter.
    assert!(!looks_like_display_text("preview_loading"));
    assert!(!looks_like_display_text("{tool}"));
    assert!(!looks_like_display_text("{mark}  {}"));
    assert!(!looks_like_display_text("-"));

    // A literal inside a #[cfg(test)] module is skipped, one in the production code above it
    // is not, and the module is found by brace matching rather than by cutting to end of file.
    let src = "fn a() { m(\"Live text\"); }\n#[cfg(test)]\nmod t { fn b() { m(\"Test text\"); } }\nfn c() { m(\"After text\"); }\n";
    let (skeleton, literals) = scan_source(src);
    let skipped = test_module_ranges(&skeleton);
    let visible: Vec<String> = literals
        .into_iter()
        .filter(|(at, _)| !skipped.iter().any(|(a, b)| at >= a && at < b))
        .map(|(_, v)| v)
        .collect();
    assert_eq!(
        visible,
        vec!["Live text".to_string(), "After text".to_string()],
        "the test-module filter must skip only the #[cfg(test)] body"
    );

    // A comment that looks like code must not be scanned, and a raw string must be skipped.
    let (_, lits) = scan_source("// m(\"Commented out\");\nlet p = r\"C:\\Windows\\Fonts\";\n");
    assert!(
        lits.iter().all(|(_, v)| v != "Commented out"),
        "line comments must not be scanned"
    );
}
