//! The context-menu settings: the gate, which items show, and their order.

use super::*;

/// Show the right-click "SageThumbs 2K" menu.
pub fn menu_enabled() -> bool {
    get_dword("EnableMenu", 1) != 0
}

/// Show the menu on ANY file (not just supported images/audio). When on, an UNSUPPORTED
/// selection still gets a CONDENSED menu — only the file-agnostic utilities (Files to
/// folder · Sort into folders · Rename · Pick color) + Settings (see
/// [`crate::verbs::condensed_top_level`]). OFF by default — the menu stays on supported
/// formats only unless the user wants it everywhere.
pub fn menu_all_file_types() -> bool {
    get_dword("MenuAllFileTypes", 0) != 0
}

/// Thumbnail preview inside the classic right-click menu (single image
/// selection): 0 = off, 1 = at the top of the SageThumbs submenu,
/// 2 = directly on the main context menu.
///
/// Default: 1 (at the top of the SageThumbs submenu) — this is how the original
/// SageThumbs showed its preview, so long-time users get the familiar behavior and
/// we don't crowd the main right-click menu out of the box. It's owner-drawn (the
/// only way to make a menu row tall enough for the image) but the menu still renders
/// in the system theme (dark stays dark); see [`crate::contextmenu`]. Users who want
/// it directly on the main menu (2) or off (0) can change it in Settings.
pub fn menu_preview() -> u32 {
    get_dword("MenuPreview", DEFAULT_MENU_PREVIEW).min(2)
}

/// Surface the most-used verbs (Convert into / Resize / Rotate) directly on the
/// MAIN right-click menu (above the SageThumbs submenu), so they're one click
/// instead of two. OFF by default — the original SageThumbs kept everything inside
/// its submenu, so we don't crowd the main menu unless the user opts in.
pub fn menu_quick_verbs() -> bool {
    get_dword("MenuQuickVerbs", 0) != 0
}

/// A snapshot of the three menu-gate settings ([`menu_enabled`], [`menu_all_file_types`],
/// [`menu_quick_verbs`]), read with a SINGLE HKCU key open instead of one open per getter —
/// the same collapsing [`ThumbSettings`]/[`thumb_settings`] already does for the per-thumbnail
/// settings. `explorer.exe`'s modern-menu `GetState`/`EnumSubCommands` calls all three
/// separately today, once per top-level menu item per right-click (item 132).
#[derive(Clone, Copy, Debug)]
pub struct MenuGate {
    /// `EnableMenu` — master on/off for the right-click menu — AND the business-licence
    /// lock (`licence_state::shell_locked`): a locked copy reads as menu-off on both the
    /// classic and the modern menu, so neither shows a verb the app would then refuse. The
    /// Settings checkbox itself reads [`menu_enabled`], which stays the user's own switch.
    pub enabled: bool,
    /// `MenuAllFileTypes` — show a condensed menu on unsupported selections too.
    pub all_file_types: bool,
    /// `MenuQuickVerbs` — surface the top verbs directly on the main right-click menu.
    pub quick_verbs: bool,
}

/// Read the menu-gate settings in one HKCU key open. Missing values fall back to the same
/// defaults the individual getters use, so the result is identical to calling
/// [`menu_enabled`]/[`menu_all_file_types`]/[`menu_quick_verbs`] separately — just without the
/// repeated opens.
pub fn menu_gate() -> MenuGate {
    let gopt = snapshot_u32_getter();
    let g = |name: &str, default: u32| gopt(name).unwrap_or(default);
    MenuGate {
        enabled: g("EnableMenu", 1) != 0 && !crate::licence_state::shell_locked(),
        all_file_types: g("MenuAllFileTypes", 0) != 0,
        quick_verbs: g("MenuQuickVerbs", 0) != 0,
    }
}

/// Whether a top-level context-menu item (by its MENU title key, e.g.
/// `menu_convert_into`) is shown. All shown by default; the Settings checklist
/// can hide ones the user never uses. Stored under `…\SageThumbs2K\MenuItems\<key>`.
pub fn menu_item_shown(key: &str) -> bool {
    if store::portable() {
        return store::get_u32(Some(MENU_ITEMS), key)
            .map(|v| v != 0)
            .unwrap_or(true);
    }
    CURRENT_USER
        .open(format!(r"{}\MenuItems", hkcu_root()))
        .and_then(|k| k.get_u32(key))
        .map(|v| v != 0)
        .unwrap_or(true)
}

/// Persist a top-level menu item's visibility (used by the Options dialog).
pub fn set_menu_item_shown(key: &str, shown: bool) -> windows_registry::Result<()> {
    if store::portable() {
        return io_result(store::set_u32(Some(MENU_ITEMS), key, shown as u32));
    }
    CURRENT_USER
        .create(format!(r"{}\MenuItems", hkcu_root()))?
        .set_u32(key, shown as u32)
}

/// The user's custom top-level menu order — a list of menu-item title keys, top to
/// bottom — or empty for the default tree order. Stored comma-joined under
/// `…\SageThumbs2K\MenuOrder` (the keys are `menu_*` identifiers, never contain a
/// comma). The classic menu builder applies it via `verbs::ordered_top_level`.
pub fn menu_order() -> Vec<String> {
    let stored = if store::portable() {
        store::get_string(None, "MenuOrder")
    } else {
        CURRENT_USER
            .open(hkcu_root())
            .and_then(|k| k.get_string("MenuOrder"))
            .ok()
    };
    stored
        .filter(|s| !s.is_empty())
        .map(|s| s.split(',').map(str::to_string).collect())
        .unwrap_or_default()
}

/// Persist the custom menu order (comma-joined keys); an empty slice clears it
/// (= back to the default tree order).
pub fn set_menu_order(keys: &[&str]) -> windows_registry::Result<()> {
    if store::portable() {
        return io_result(store::set_string(None, "MenuOrder", &keys.join(",")));
    }
    CURRENT_USER
        .create(hkcu_root())?
        .set_string("MenuOrder", keys.join(","))
}

/// A one-shot snapshot of the menu-item visibility subkey. Building the right-click
/// menu calls [`menu_item_shown`] once per node (~one HKCU open + `format!` alloc
/// each); on a per-right-click hot path inside explorer.exe that adds up. Open
/// `…\MenuItems` ONCE at the top of `QueryContextMenu` / `EnumSubCommands` and ask
/// [`MenuVisibility::shown`] per item instead — same semantics, ~N opens collapse
/// to one. A fresh snapshot per menu build keeps the live-toggle contract (§ module
/// docs) intact — we don't cache across builds.
pub struct MenuVisibility(pub(super) MenuVisibilitySource);

/// Which backing store the snapshot came from. The portable arm holds the parsed section
/// outright — same "read once per menu build" contract, no file touched per item.
pub(super) enum MenuVisibilitySource {
    Registry(Option<windows_registry::Key>),
    Portable(std::collections::HashMap<String, String>),
}

/// Open the menu-visibility subkey once for the current menu build. An absent subkey
/// (nothing ever hidden) makes every [`MenuVisibility::shown`] return true.
pub fn menu_visibility() -> MenuVisibility {
    MenuVisibility(if store::portable() {
        MenuVisibilitySource::Portable(
            store::section_values(Some(MENU_ITEMS))
                .into_iter()
                .collect(),
        )
    } else {
        MenuVisibilitySource::Registry(
            CURRENT_USER
                .open(format!(r"{}\MenuItems", hkcu_root()))
                .ok(),
        )
    })
}

impl MenuVisibility {
    /// Whether `key` (a top-level menu item title) is shown — default true unless an
    /// explicit `0` is stored. Identical to [`menu_item_shown`], reusing the snapshot.
    pub fn shown(&self, key: &str) -> bool {
        // Shown by default; hidden only when an explicit `0` is stored. (`matches!`
        // keeps this MSRV-1.80-safe — `is_none_or` would need 1.82.)
        match &self.0 {
            MenuVisibilitySource::Registry(k) => {
                !matches!(k.as_ref().and_then(|k| k.get_u32(key).ok()), Some(0))
            }
            MenuVisibilitySource::Portable(m) => {
                // Numeric, like the registry arm above and like `menu_item_shown` (this
                // function's own doc claims "identical" to it) — a literal string match
                // against "0" disagreed on a non-canonical stored value like "00", which
                // `menu_item_shown`'s `get_u32` parses as 0 (hidden) but this used to keep
                // as "shown" since "00" != "0".
                !matches!(m.get(key).and_then(|v| v.parse::<u32>().ok()), Some(0))
            }
        }
    }
}
