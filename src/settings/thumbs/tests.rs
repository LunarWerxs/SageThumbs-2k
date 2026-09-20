#![cfg(test)]

use super::*;

/// A188: [`FormatEnabledSnapshot`]'s portable arm must agree with [`format_enabled`] for
/// every case that matters — an explicit `0` (disabled), any other stored value (enabled),
/// and an extension nobody configured at all (enabled by default) — since it exists purely
/// to replace repeated `format_enabled` calls with one parse, not to change the answer.
#[test]
fn format_enabled_snapshot_portable_arm_matches_format_enabled_semantics() {
    let mut doc = std::collections::BTreeMap::new();
    let mut psd = std::collections::BTreeMap::new();
    psd.insert("Enabled".to_string(), "0".to_string());
    doc.insert(".psd".to_string(), psd);
    let mut heic = std::collections::BTreeMap::new();
    heic.insert("Enabled".to_string(), "1".to_string());
    doc.insert(".heic".to_string(), heic);

    let snap = FormatEnabledSnapshot(FormatEnabledSource::Portable(doc));
    assert!(!snap.enabled(".psd"), "explicit 0 must read as disabled");
    assert!(snap.enabled(".heic"), "explicit 1 must read as enabled");
    assert!(
        snap.enabled(".never_configured"),
        "an extension with no stored value defaults enabled, matching format_enabled"
    );
}

/// A187: the portable arm of `MenuVisibility::shown` used to literal-string-match
/// `"0"`, disagreeing with `menu_item_shown`'s numeric `get_u32` parse (which reads
/// "00" as 0) despite `shown`'s own doc comment calling the two "identical".
#[test]
fn menu_visibility_portable_arm_parses_stored_value_numerically() {
    let mut m = std::collections::HashMap::new();
    m.insert("menu_convert_into".to_string(), "00".to_string());
    let mv = MenuVisibility(MenuVisibilitySource::Portable(m));
    assert!(
        !mv.shown("menu_convert_into"),
        "a non-canonical \"00\" must be treated as 0 (hidden), matching menu_item_shown"
    );
    // Absent / non-numeric stored values stay shown (the documented default).
    assert!(mv.shown("menu_never_configured"));
}

/// The stored DWORD round-trips, the default is the shipped look, and an unreadable
/// value falls back to it rather than to the largest mark. The combo's option ORDER is
/// this mapping (`build.rs` seeds it in `as_dword` order), so a change here silently
/// re-points every stored value - which is exactly what this locks.
#[test]
fn badge_size_round_trips_through_its_dword() {
    for s in [BadgeSize::Small, BadgeSize::Medium, BadgeSize::Large] {
        assert_eq!(BadgeSize::from_dword(s.as_dword()), s);
    }
    assert_eq!(BadgeSize::Small.as_dword(), 0);
    assert_eq!(BadgeSize::default(), BadgeSize::Small);
    assert_eq!(BadgeSize::from_dword(DEFAULT_BADGE_SIZE), BadgeSize::Small);
    assert_eq!(BadgeSize::from_dword(99), BadgeSize::Small);
    // Bigger step, smaller divisor - the ordering the badge geometry depends on.
    assert!(BadgeSize::Medium.divisor() < BadgeSize::Small.divisor());
    assert!(BadgeSize::Large.divisor() < BadgeSize::Medium.divisor());
    assert_eq!(
        BadgeSize::Small.divisor(),
        110,
        "the shipped look must not move"
    );
}
