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
        .unwrap_or_else(|| leak_fallback(key))
}

fn lookup(idx: usize, key: &str) -> Option<&'static str> {
    LOCALES
        .get(idx)
        .and_then(|(_, pairs)| pairs.iter().find(|(k, _)| *k == key).map(|(_, v)| *v))
}

/// Last-resort: a key with no English entry. Returns a 'static echo of the key.
/// (Only reachable for a typo'd key; en.toml is the canonical key set.)
fn leak_fallback(key: &str) -> &'static str {
    Box::leak(key.to_string().into_boxed_str())
}

/// Switch language by code (e.g. "fr", "zh-TW"). Returns false if unknown.
pub fn set_locale(code: &str) -> bool {
    if let Some(i) = LOCALES.iter().position(|(c, _)| *c == code) {
        CURRENT.store(i, Ordering::Relaxed);
        true
    } else {
        false
    }
}

/// The active language code.
pub fn current_code() -> &'static str {
    LOCALES
        .get(CURRENT.load(Ordering::Relaxed))
        .map(|(c, _)| *c)
        .unwrap_or("en")
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

/// Map the current Windows UI language to one of our codes, or None.
fn system_ui_code() -> Option<&'static str> {
    let langid = unsafe { GetUserDefaultUILanguage() };
    let primary = langid & 0x03ff;
    let code = match primary {
        0x09 => "en",
        0x01 => "ar",
        0x02 => "bg",
        0x05 => "cs",
        0x06 => "da",
        0x07 => "de",
        0x08 => "el",
        0x0a => "es",
        0x29 => "fa",
        0x0b => "fi",
        0x0c => "fr",
        0x0d => "he",
        0x39 => "hi",
        // 0x1a is shared by Croatian/Serbian/Bosnian sublangs; Croatian is the
        // nearest locale we ship.
        0x1a => "hr",
        0x0e => "hu",
        0x21 => "id",
        0x10 => "it",
        0x11 => "ja",
        0x12 => "ko",
        0x3e => "ms",
        0x14 => "nb",
        0x13 => "nl",
        0x64 => "fil",
        0x15 => "pl",
        0x16 => "pt-BR",
        0x18 => "ro",
        0x19 => "ru",
        0x1b => "sk",
        0x24 => "sl",
        0x1d => "sv",
        0x1e => "th",
        0x1f => "tr",
        0x22 => "uk",
        0x2a => "vi",
        0x04 => {
            // Chinese: sublang 0x01 == Traditional (TW); everything else Simplified.
            if (langid >> 10) == 0x01 {
                "zh-TW"
            } else {
                "zh-CN"
            }
        }
        _ => return None,
    };
    Some(code)
}

/// Native (autonym) display name for the language picker.
pub fn native_name(code: &str) -> &'static str {
    match code {
        "en" => "English",
        "ar" => "العربية",
        "bg" => "Български",
        "cs" => "Čeština",
        "da" => "Dansk",
        "de" => "Deutsch",
        "el" => "Ελληνικά",
        "es" => "Español",
        "fa" => "فارسی",
        "fi" => "Suomi",
        "fil" => "Filipino",
        "fr" => "Français",
        "he" => "עברית",
        "hi" => "हिन्दी",
        "hr" => "Hrvatski",
        "hu" => "Magyar",
        "id" => "Bahasa Indonesia",
        "it" => "Italiano",
        "ja" => "日本語",
        "ko" => "한국어",
        "ms" => "Bahasa Melayu",
        "nb" => "Norsk",
        "nl" => "Nederlands",
        "pl" => "Polski",
        "pt-BR" => "Português (Brasil)",
        "ro" => "Română",
        "ru" => "Русский",
        "sk" => "Slovenčina",
        "sl" => "Slovenščina",
        "sv" => "Svenska",
        "th" => "ไทย",
        "tr" => "Türkçe",
        "uk" => "Українська",
        "vi" => "Tiếng Việt",
        "zh-CN" => "简体中文",
        "zh-TW" => "繁體中文",
        _ => "English", // unreachable for our shipped codes
    }
}
