//! Portable-mode settings: the ini backend behind `settings.rs`.
//!
//! The whole promise of the portable drop is "settings live in a file next to the exe and
//! the registry is never touched." Unit tests in `settings::store` cover the ini parser;
//! this covers the claim itself, through the PUBLIC accessors the app actually calls, and
//! asserts the negative half (HKCU is byte-for-byte unchanged) that no parser test can.
//!
//! ONE test function on purpose. `settings::portable()` resolves through a `OnceLock`, so
//! the env var has to be set before the first settings call in the process; and every case
//! here read-modify-writes the same ini, which parallel tests would race on. A single test
//! that runs sections in order is the honest shape, not a limitation worked around.

use sagethumbs2k_core::settings;

/// A snapshot of `HKCU\Software\SageThumbs2K` — root values plus one level of subkeys —
/// rendered as sorted `path\name=value` strings so two snapshots compare with `assert_eq!`
/// and a failure prints exactly which value moved.
fn hkcu_snapshot() -> Vec<String> {
    fn values(key: &windows_registry::Key, prefix: &str) -> Vec<String> {
        let Ok(vals) = key.values() else {
            return Vec::new();
        };
        vals.map(|(name, value)| {
            // Only the two types we ever store need to render exactly; anything else just
            // needs to be stable across the two snapshots.
            let rendered = u32::try_from(value.clone())
                .map(|n| n.to_string())
                .or_else(|_| String::try_from(value.clone()))
                .unwrap_or_else(|_| "<other>".to_string());
            format!("{prefix}{name}={rendered}")
        })
        .collect()
    }

    let Ok(root) = windows_registry::CURRENT_USER.open(settings::ROOT) else {
        return Vec::new(); // key absent is itself a valid state to compare against
    };
    let mut out = values(&root, "");
    if let Ok(names) = root.keys() {
        for name in names {
            if let Ok(sub) = root.open(&name) {
                out.extend(values(&sub, &format!("{name}\\")));
            }
        }
    }
    out.sort();
    out
}

#[test]
fn portable_mode_uses_the_ini_and_never_touches_the_registry() {
    let ini = std::env::temp_dir().join(format!("st2k-portable-{}.ini", std::process::id()));
    let _ = std::fs::remove_file(&ini);
    // Must happen before ANY settings call — see the module docs.
    std::env::set_var("ST2K_PORTABLE_INI", &ini);

    assert!(
        settings::portable(),
        "ST2K_PORTABLE_INI should put settings in portable mode"
    );
    assert_eq!(settings::ini_path(), Some(&ini));

    // The registry state we must not disturb, captured before the first write.
    let before = hkcu_snapshot();

    // ---- defaults come through with no file on disk -------------------------
    // A portable drop ships an empty (or absent) ini; every getter must still answer.
    assert!(!ini.exists(), "no file should exist yet");
    assert_eq!(settings::max_thumb_size(), settings::DEFAULT_THUMB_SIZE);
    assert_eq!(settings::jpeg_quality(), settings::DEFAULT_JPEG as u8);
    assert_eq!(settings::lang_override(), None);
    assert!(
        settings::format_enabled("psd"),
        "unset format defaults to on"
    );
    assert!(settings::menu_item_shown("menu_convert_into"));
    assert!(settings::menu_order().is_empty());

    // ---- round-trip every shape the store has to handle ---------------------
    // Root DWORD, root string, a subkey DWORD, the menu-visibility subkey, and a
    // comma-joined list — between them these cover every accessor family in settings.rs.
    // Width AND Height: `max_thumb_size` clamps the LARGER of the pair, so writing one
    // alone would still read back the other's 1024 default.
    settings::set_dword("Width", 512).unwrap();
    settings::set_dword("Height", 512).unwrap();
    settings::set_dword("JPEG", 71).unwrap();
    settings::set_lang("fr").unwrap();
    settings::set_screenshot_save_dir(r"D:\shots").unwrap();
    settings::set_string("ScreenshotCustomColors", "FF0000,00FF00").unwrap();
    settings::set_format_enabled("psd", false).unwrap();
    settings::set_menu_item_shown("menu_convert_into", false).unwrap();
    settings::set_menu_order(&["menu_resize", "--", "menu_convert_into"]).unwrap();

    assert_eq!(settings::max_thumb_size(), 512);
    assert_eq!(settings::jpeg_quality(), 71);
    assert_eq!(settings::lang_override().as_deref(), Some("fr"));
    assert_eq!(settings::screenshot_save_dir(), r"D:\shots");
    assert_eq!(
        settings::get_string_opt("ScreenshotCustomColors").as_deref(),
        Some("FF0000,00FF00")
    );
    assert!(!settings::format_enabled("psd"));
    assert!(settings::format_enabled("jpg"), "untouched format stays on");
    assert!(!settings::menu_item_shown("menu_convert_into"));
    assert!(settings::menu_item_shown("menu_resize"));
    assert_eq!(
        settings::menu_order(),
        vec!["menu_resize", "--", "menu_convert_into"]
    );

    // The batched readers must agree with the individual getters — they take a different
    // path through the store (one section snapshot instead of a value at a time).
    let thumb = settings::thumb_settings();
    assert_eq!(thumb.max_thumb, 512);
    assert!(thumb.enabled, "EnableThumbs was never written → default on");
    let vis = settings::menu_visibility();
    assert!(!vis.shown("menu_convert_into"));
    assert!(vis.shown("menu_resize"));

    // ---- the file is the documented, hand-editable shape --------------------
    let text = std::fs::read_to_string(&ini).expect("portable ini should exist by now");
    assert!(text.contains("[Settings]"), "{text}");
    assert!(text.contains("Width=512"), "{text}");
    assert!(text.contains("Lang=fr"), "{text}");
    assert!(text.contains("[psd]"), "{text}");
    assert!(text.contains("[MenuItems]"), "{text}");

    // ---- a hand edit takes effect without restarting anything ---------------
    // settings.rs promises reads aren't cached across a change, so an external edit (a user
    // with Notepad open) has to be picked up live. This case is deliberately the NASTY one
    // and must stay that way: the replacement is the SAME BYTE LENGTH as what it replaces,
    // and it lands immediately after our own writes. An `(mtime, len)` cache — which this
    // store briefly had — passes every other assertion here and fails this one, because
    // neither half of that key moves. Do not "optimise" the re-read back in.
    let edited = text
        .replace("Width=512", "Width=256")
        .replace("Height=512", "Height=256");
    std::fs::write(&ini, edited).unwrap();
    assert_eq!(
        settings::max_thumb_size(),
        256,
        "an external edit must be visible without a restart"
    );

    // ---- the negative half: HKCU is exactly as we found it ------------------
    assert_eq!(
        before,
        hkcu_snapshot(),
        "portable mode wrote to the registry — that is the one thing it must never do"
    );

    let _ = std::fs::remove_file(&ini);
}
