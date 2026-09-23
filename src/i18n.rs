//! Lightweight localization. Locale strings are compiled from `locales/*.toml`
//! into a static `LOCALES` table by build.rs (no runtime TOML parser), so the
//! shell-extension DLL stays self-contained. The active language follows the
//! Windows UI language by default, overridable via `HKCU\…\SageThumbs2K\Lang`
//! (set by the Options dialog's language picker).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Once;

use windows::Win32::Globalization::GetUserDefaultUILanguage;

// pub static LOCALES: &[(&str, &[(&str, &str)])] = &[ ("en", &[..]), .. ];
include!(concat!(env!("OUT_DIR"), "/i18n_gen.rs"));

/// Index into `LOCALES` of the active language. 0 == `en` (the fallback).
static CURRENT: AtomicUsize = AtomicUsize::new(0);
static INIT: Once = Once::new();

/// Translate `key` in the active language, falling back to English, then to the
/// key itself (so a missing string is visible, never a crash).
pub fn t(key: &str) -> &'static str {
    ensure_init();
    let idx = CURRENT.load(Ordering::Relaxed);
    lookup(idx, key)
        .or_else(|| lookup(0, key))
        .unwrap_or(MISSING_KEY)
}

fn lookup(idx: usize, key: &str) -> Option<&'static str> {
    LOCALES.get(idx).and_then(|(_, pairs)| {
        // build.rs emits each locale's pairs from a BTreeMap, so they're sorted by
        // key — binary-search instead of a linear scan (t() runs once per drawn menu
        // node, and the en-fallback path scans twice). The sort invariant is locked
        // by `locale_pairs_are_sorted_for_binary_search` below.
        pairs
            .binary_search_by(|(k, _)| (*k).cmp(key))
            .ok()
            .map(|i| pairs[i].1)
    })
}

/// Last-resort sentinel for a key absent from BOTH the active locale and `en`.
/// Only reachable for a typo'd key (en.toml is the canonical key set), so a fixed
/// `&'static str` is enough — and, unlike the old `Box::leak(key.to_string())`,
/// it can't leak unboundedly when the same bad key is looked up repeatedly.
const MISSING_KEY: &str = "\u{27e8}?\u{27e9}";

/// Switch language by code (e.g. "fr", "zh-TW"). Returns false if unknown.
///
/// Case-folded, and — failing an exact match — falls back to the code's primary subtag
/// (`"fr-CA"` still resolves to our `"fr"`). The override comes from a registry value a user
/// can hand-edit (`HKCU\…\Lang`), so `"FR"` or a region-tagged variant we don't ship exactly
/// must not silently fall all the way back to the system locale.
pub(crate) fn set_locale(code: &str) -> bool {
    if let Some(i) = LOCALES
        .iter()
        .position(|(c, _)| c.eq_ignore_ascii_case(code))
    {
        CURRENT.store(i, Ordering::Relaxed);
        return true;
    }
    let primary = code.split(['-', '_']).next().unwrap_or(code);
    if let Some(i) = LOCALES
        .iter()
        .position(|(c, _)| c.eq_ignore_ascii_case(primary))
    {
        CURRENT.store(i, Ordering::Relaxed);
        return true;
    }
    false
}

/// All available language codes, English first.
pub fn codes() -> impl Iterator<Item = &'static str> {
    LOCALES.iter().map(|(c, _)| *c)
}

/// Resolve the language once, from the HKCU override or the Windows UI language.
/// Idempotent (safe to call from every COM entry point and from `main`).
pub fn ensure_init() {
    INIT.call_once(|| {
        if let Some(code) = crate::settings::lang_override() {
            if set_locale(&code) {
                return;
            }
        }
        if let Some(code) = system_ui_code() {
            set_locale(code); // leaves index 0 (en) if we don't ship that language
        }
    });
}

/// Re-resolve after the user changes the override (the `Once` above only fires
/// the initial auto-detection).
pub fn apply_override_or_system(code: Option<&str>) {
    match code {
        Some(c) if set_locale(c) => {}
        _ => {
            if let Some(c) = system_ui_code() {
                set_locale(c);
            } else {
                set_locale("en");
            }
        }
    }
}

/// Map the current Windows UI language to one of our codes, or None. Split across two
/// lookup tables purely to keep each match's arm count under the complexity gate.
fn system_ui_code() -> Option<&'static str> {
    code_for_langid(unsafe { GetUserDefaultUILanguage() })
}

/// Every shipped language: its code, the Windows primary LANGID that selects it, and the name
/// the language picker shows (the autonym). One row per language, so auto-detection and the
/// picker cannot disagree about which languages exist. Chinese is two rows on one primary id,
/// told apart by sublang in [`code_for_langid`].
const LANGUAGES: &[(&str, u16, &str)] = &[
    ("en", 0x09, "English"),
    ("ar", 0x01, "العربية"),
    ("bg", 0x02, "Български"),
    ("cs", 0x05, "Čeština"),
    ("da", 0x06, "Dansk"),
    ("de", 0x07, "Deutsch"),
    ("el", 0x08, "Ελληνικά"),
    ("es", 0x0a, "Español"),
    ("fa", 0x29, "فارسی"),
    ("fi", 0x0b, "Suomi"),
    ("fil", 0x64, "Filipino"),
    ("fr", 0x0c, "Français"),
    ("he", 0x0d, "עברית"),
    ("hi", 0x39, "हिन्दी"),
    // 0x1a is shared by Croatian/Serbian/Bosnian sublangs; Croatian is the nearest we ship.
    ("hr", 0x1a, "Hrvatski"),
    ("hu", 0x0e, "Magyar"),
    ("id", 0x21, "Bahasa Indonesia"),
    ("it", 0x10, "Italiano"),
    ("ja", 0x11, "日本語"),
    ("ko", 0x12, "한국어"),
    ("ms", 0x3e, "Bahasa Melayu"),
    ("nb", 0x14, "Norsk"),
    ("nl", 0x13, "Nederlands"),
    ("pl", 0x15, "Polski"),
    ("pt-BR", 0x16, "Português (Brasil)"),
    ("ro", 0x18, "Română"),
    ("ru", 0x19, "Русский"),
    ("sk", 0x1b, "Slovenčina"),
    ("sl", 0x24, "Slovenščina"),
    ("sv", 0x1d, "Svenska"),
    ("th", 0x1e, "ไทย"),
    ("tr", 0x1f, "Türkçe"),
    ("uk", 0x22, "Українська"),
    ("vi", 0x2a, "Tiếng Việt"),
    ("zh-CN", 0x04, "简体中文"),
    ("zh-TW", 0x04, "繁體中文"),
];

/// The shipped language code a Windows LANGID selects, or `None` for one we do not ship.
fn code_for_langid(langid: u16) -> Option<&'static str> {
    let primary = langid & 0x03ff;
    if primary == 0x04 {
        return Some(zh_variant(langid));
    }
    LANGUAGES
        .iter()
        .find(|(_, p, _)| *p == primary)
        .map(|(code, _, _)| *code)
}

/// Which Chinese locale a Windows LANGID's sublang maps to. Sublangs 0x01 (Taiwan), 0x03
/// (Hong Kong) and 0x05 (Macao) are all Traditional-script; everything else (PRC mainland,
/// Singapore) is Simplified.
fn zh_variant(langid: u16) -> &'static str {
    match langid >> 10 {
        0x01 | 0x03 | 0x05 => "zh-TW",
        _ => "zh-CN",
    }
}

/// Native (autonym) display name for the language picker, from [`LANGUAGES`].
pub fn native_name(code: &str) -> &'static str {
    LANGUAGES
        .iter()
        .find(|(c, _, _)| *c == code)
        .map_or("English", |(_, _, name)| *name) // unreachable for our shipped codes
}

#[cfg(test)]
mod tests {
    use super::*;

    /// EVERY `t("…")` IN LIB CODE MUST USE A `menu_*` KEY, because the shipped DLL's locale
    /// table is FILTERED to exactly those (`dll-i18n-subset`, see build.rs). A lib-side lookup
    /// of any other key compiles, tests, and runs perfectly in every EXE — and then resolves to
    /// the [`MISSING_KEY`] sentinel `⟨?⟩` inside the real DLL, because that string was stripped
    /// out of the table the DLL ships with.
    ///
    /// That is not hypothetical. 1.12.0 added `foldermenu.rs`, which calls `t()` to get the
    /// caption for the "Build thumbnails here" static verb and WRITES IT INTO THE REGISTRY. The
    /// key was `pb_verb`, not `menu_pb_verb`, so every user got a right-click entry captioned
    /// literally `⟨?⟩` — and because the caption is baked into the registry at registration
    /// time, it stayed wrong until the next re-registration (issue #26.2). The build.rs comment
    /// asserting the DLL "only ever calls t() with menu_* keys" had quietly become false, and
    /// nothing checked it.
    ///
    /// This scans the lib sources rather than trusting that comment. `src/bin/` is excluded:
    /// those are the EXEs, which link the FULL table and may use any key.
    /// Scan one file's text for literal `t("…")` calls, appending a `menu_`-rename offender
    /// for every real, non-`menu_` translation key found. Returns how many literal
    /// translation-key calls it recognized (including ones that already pass).
    fn scan_file_t_calls(
        text: &str,
        display_path: &std::path::Path,
        offenders: &mut Vec<String>,
    ) -> usize {
        let mut checked = 0usize;
        // Match the literal-key form `t("…")`. The dynamic form `t(title)` is covered
        // separately by `every_menu_title_is_a_menu_key` in verbs.
        for (i, _) in text.match_indices("t(\"") {
            // Require the char before `t` to be a non-identifier one, so this does not fire
            // on `format!("…{}", other_fn_that("x"))`-style names ending in `t` (e.g.
            // `set(`, `get(`, `insert(`).
            if i > 0 && text.as_bytes()[i - 1].is_ascii_alphanumeric() {
                continue;
            }
            if i > 0 && text.as_bytes()[i - 1] == b'_' {
                continue;
            }
            let rest = &text[i + 3..];
            let Some(end) = rest.find('"') else { continue };
            let key = &rest[..end];
            // Only judge things that are actually translation keys: every key in en.toml is
            // snake_case ASCII. Anything else is some other `…t("…")`.
            if key.is_empty()
                || !key
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            {
                continue;
            }
            if lookup(0, key).is_none() {
                continue; // not a translation key at all
            }
            checked += 1;
            if !key.starts_with("menu_") {
                offenders.push(format!(
                    "{}: t(\"{key}\") — rename the key to `menu_{key}` in every \
                     assets/locales/*.toml, or the shipped DLL renders it as ⟨?⟩",
                    display_path.display(),
                ));
            }
        }
        checked
    }

    /// Walk `src` (minus `src/bin/`, which gets the full locale table) and scan every `.rs`
    /// file for lib-side literal `t("…")` calls. Returns the total recognized-call count and
    /// the accumulated `menu_`-rename offenders.
    fn scan_lib_t_calls(src: &std::path::Path) -> (usize, Vec<String>) {
        let mut checked = 0usize;
        let mut offenders: Vec<String> = Vec::new();

        // Small tree, plain recursion, no dev-dependency needed.
        let mut stack = vec![src.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    if p.file_name().is_some_and(|n| n == "bin") {
                        continue; // the EXEs get the full locale table
                    }
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    let Ok(text) = std::fs::read_to_string(&p) else {
                        continue;
                    };
                    let rel = p.strip_prefix(src).unwrap_or(&p).to_path_buf();
                    checked += scan_file_t_calls(&text, &rel, &mut offenders);
                }
            }
        }
        (checked, offenders)
    }

    #[test]
    fn lib_side_translation_keys_all_survive_the_dll_subset() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let (checked, offenders) = scan_lib_t_calls(&src);

        assert!(
            offenders.is_empty(),
            "lib-side t() calls using keys the slim DLL does not ship:\n  {}",
            offenders.join("\n  "),
        );
        // A scan that silently matched nothing would pass forever while proving nothing.
        assert!(
            checked > 0,
            "the scan found no lib-side t(\"literal\") calls at all — the matcher is broken, \
             not the code",
        );
    }

    /// The subset predicate in build.rs and the guard above have to agree, or the guard is
    /// checking a rule the build does not apply. `DLL_KEYS` is build.rs's own answer to
    /// "which keys does the DLL ship"; assert it really is exactly the `menu_*` set.
    #[test]
    fn dll_keys_is_exactly_the_menu_prefix_set() {
        assert!(
            DLL_KEYS.iter().all(|k| k.starts_with("menu_")),
            "DLL_KEYS contains a non-menu_ key — build.rs's is_dll_key predicate changed",
        );
        let en_menu = LOCALES[0]
            .1
            .iter()
            .filter(|(k, _)| k.starts_with("menu_"))
            .count();
        assert_eq!(
            DLL_KEYS.len(),
            en_menu,
            "DLL_KEYS and the en menu_* keys disagree; the guard above would check the wrong set",
        );
        assert!(
            DLL_KEYS.contains(&"menu_pb_verb"),
            "menu_pb_verb must ship in the DLL — foldermenu writes it into the registry as the \
             right-click caption (issue #26.2)",
        );
    }

    /// `lookup` binary-searches each locale's pairs, which is only correct if they
    /// are sorted by key. build.rs emits them from a BTreeMap (sorted), so this
    /// holds today — this test fails loudly if a future build.rs change breaks it.
    #[test]
    fn locale_pairs_are_sorted_for_binary_search() {
        for (code, pairs) in LOCALES {
            assert!(
                pairs.windows(2).all(|w| w[0].0 < w[1].0),
                "locale {code}: pairs are not strictly sorted by key — binary_search in lookup() would miss strings",
            );
        }
    }

    /// Every shipped locale carries EXACTLY en's key set, with the same `{placeholders}`
    /// in every value that has any. The build script already refuses to compile a gap
    /// (`enforce_locale_parity` in build.rs), so this cannot fail on a binary that exists;
    /// it is here so the invariant is also stated where `cargo test` reads it, and so a
    /// future "make the build lenient again" change trips a test rather than nothing
    /// (owner directive, Michael, 2026-09-13: no language may ever miss a string).
    #[test]
    fn every_locale_carries_exactly_the_english_key_set_with_the_same_placeholders() {
        fn slots(v: &str) -> std::collections::BTreeSet<&str> {
            let mut out = std::collections::BTreeSet::new();
            let mut rest = v;
            while let Some(start) = rest.find('{') {
                let after = &rest[start + 1..];
                match after.find('}') {
                    Some(end)
                        if end > 0
                            && after[..end]
                                .bytes()
                                .all(|b| b.is_ascii_lowercase() || b == b'_') =>
                    {
                        out.insert(&after[..end]);
                        rest = &after[end + 1..];
                    }
                    _ => rest = after,
                }
            }
            out
        }
        let en = LOCALES[0].1;
        assert_eq!(LOCALES[0].0, "en");
        for (code, pairs) in LOCALES.iter().skip(1) {
            let en_keys: Vec<&str> = en.iter().map(|(k, _)| *k).collect();
            let keys: Vec<&str> = pairs.iter().map(|(k, _)| *k).collect();
            assert_eq!(
                keys, en_keys,
                "locale {code}: key set differs from en (a missing key shows English inside a {code} UI; an extra one is a dead string)"
            );
            for ((k, en_v), (_, v)) in en.iter().zip(pairs.iter()) {
                assert_eq!(
                    slots(v),
                    slots(en_v),
                    "locale {code}, key {k}: placeholders differ from en - the runtime substitution would leave a hole"
                );
            }
        }
    }

    /// Every English key resolves to itself's value via the binary search (not the
    /// MISSING sentinel) — a smoke test that the search finds real keys.
    #[test]
    fn english_keys_resolve() {
        for (k, v) in LOCALES[0].1 {
            assert_eq!(
                lookup(0, k),
                Some(*v),
                "en key {k} not found by binary search"
            );
        }
    }

    /// Hong Kong (0x03) and Macao (0x05) are Traditional-script sublangs, same as Taiwan
    /// (0x01) — only PRC-mainland-style sublangs should fall to Simplified.
    #[test]
    fn zh_variant_covers_all_traditional_sublangs() {
        for sublang in [0x01u16, 0x03, 0x05] {
            assert_eq!(
                zh_variant(sublang << 10),
                "zh-TW",
                "sublang {sublang:#x} should be Traditional"
            );
        }
        assert_eq!(zh_variant(0x02 << 10), "zh-CN"); // PRC mainland
    }

    /// A hand-edited registry override is free-form text; the match must not silently
    /// discard a locale we do ship just because of letter case.
    #[test]
    fn locale_override_matches_case_insensitively() {
        assert!(set_locale("FR"), "uppercase override should match \"fr\"");
        assert_eq!(LOCALES[CURRENT.load(Ordering::Relaxed)].0, "fr");
        set_locale("en"); // restore the default so this doesn't leak into other tests
    }

    /// A region-tagged override we don't ship exactly (`"fr-CA"`) should still resolve to
    /// its primary subtag rather than falling all the way back to the system locale.
    #[test]
    fn locale_override_falls_back_to_primary_subtag() {
        assert!(
            set_locale("fr-CA"),
            "a region variant of a shipped locale should resolve via its primary subtag"
        );
        assert_eq!(LOCALES[CURRENT.load(Ordering::Relaxed)].0, "fr");
        set_locale("en");
    }

    /// Item 115: `codes()` is derived from the build-generated `LOCALES` table (i.e. from
    /// `assets/locales/*.toml`), but the picker's names and the auto-detection come from the
    /// hand-maintained [`LANGUAGES`] table. Add a locale `.toml` without a row and it lists in
    /// the dropdown as a second "English" and can never be auto-selected; a row with no
    /// `.toml` auto-selects a locale that is not built. Both sets must be the same.
    #[test]
    fn the_language_table_is_exactly_the_shipped_locale_set() {
        use std::collections::HashSet;
        let table: HashSet<&'static str> = LANGUAGES.iter().map(|(c, _, _)| *c).collect();
        let shipped: HashSet<&'static str> = codes().collect();
        assert_eq!(
            table, shipped,
            "LANGUAGES must list exactly the shipped locales"
        );
        assert_eq!(table.len(), LANGUAGES.len(), "a language code listed twice");
    }

    /// Every language is reachable from some Windows LANGID, and Chinese splits by sublang:
    /// Taiwan/Hong Kong/Macao are Traditional, everything else Simplified.
    #[test]
    fn every_language_is_selected_by_its_windows_language_id() {
        for (code, primary, _) in LANGUAGES {
            if *primary != 0x04 {
                assert_eq!(code_for_langid(*primary), Some(*code), "{code}");
            }
        }
        assert_eq!(code_for_langid((0x01 << 10) | 0x04), Some("zh-TW"));
        assert_eq!(code_for_langid((0x03 << 10) | 0x04), Some("zh-TW"));
        assert_eq!(code_for_langid((0x02 << 10) | 0x04), Some("zh-CN"));
        assert_eq!(code_for_langid(0x0409), Some("en"));
        assert_eq!(
            code_for_langid(0x7f),
            None,
            "an unshipped language falls back"
        );
        assert_eq!(native_name("pt-BR"), "Português (Brasil)");
    }
}
