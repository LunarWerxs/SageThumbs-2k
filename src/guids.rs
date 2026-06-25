//! Stable COM identifiers for SageThumbs 2K.
//!
//! These are FRESH GUIDs for the 2K rewrite (not the legacy
//! {4A34B3E3-…} coclass), so a 2K install never clashes with an
//! installed legacy SageThumbs.

use windows::core::GUID;

/// SageThumbs 2K thumbnail provider (IThumbnailProvider + IInitializeWithStream).
pub const CLSID_THUMBNAIL_PROVIDER: GUID =
    GUID::from_u128(0x7B2E6A14_9C3D_4F8A_B1E7_2A5D9F0C6E31);

/// Same CLSID, string form, for registry writes.
pub const CLSID_THUMBNAIL_PROVIDER_STR: &str = "{7B2E6A14-9C3D-4F8A-B1E7-2A5D9F0C6E31}";

/// SageThumbs 2K context-menu command (IExplorerCommand). Registered via the
/// package manifest (`packaging/AppxManifest.xml`), NOT the registry — so there is
/// no string-form const here; the `manifest_clsids_match_explorer_command` test
/// pins the manifest's CLSID literals to this typed GUID instead.
pub const CLSID_EXPLORER_COMMAND: GUID =
    GUID::from_u128(0xD4F1C8A2_3E7B_4A96_8C0F_6B1E2D9A4C57);

/// The modern-menu "quick verbs" — Convert into ▸ / Convert… / Resize ▸ / Rotate ▸ surfaced
/// as their OWN top-level entries on the Windows 11 context menu (one per CLSID), so that
/// when `MenuQuickVerbs` is on they appear one click away instead of two levels deep inside
/// the (dormant) `CLSID_EXPLORER_COMMAND` flyout. Each is declared as a `desktop5:Verb` in
/// `packaging/AppxManifest.xml` pointing at the matching CLSID below; the surrogate hosts
/// all of them out-of-proc in dllhost. `command.rs::QUICK_VERBS` binds each CLSID to its
/// `MENU` key + manifest verb id (a test pins that table to `verbs::QUICK_KEYS`), and the
/// `manifest_clsids_match_known` test pins these GUIDs to the manifest literals. Like the
/// root command, these need NO registry registration — the package surrogate activates them.
pub const CLSID_QUICK_CONVERT_INTO: GUID =
    GUID::from_u128(0x1C7F4E2A_9D63_4B85_A0F1_7E2C5B9D4A60);
pub const CLSID_QUICK_CONVERT_DIALOG: GUID =
    GUID::from_u128(0x2D8A5F3B_0E74_4C96_B1A2_8F3D6CAE5B71);
pub const CLSID_QUICK_RESIZE: GUID =
    GUID::from_u128(0x3E9B6A4C_1F85_4DA7_C2B3_9A4E7DBF6C82);
pub const CLSID_QUICK_ROTATE: GUID =
    GUID::from_u128(0x4FAC7B5D_2096_4EB8_D3C4_AB5F8ECA7D93);

/// SageThumbs 2K classic context-menu handler (IContextMenu + IShellExtInit).
/// Needed for machines where the modern Win11 menu is replaced by the classic
/// menu (StartAllBack / ExplorerPatcher / registry tweak).
pub const CLSID_CONTEXT_MENU: GUID =
    GUID::from_u128(0x9F3A2B1C_5E8D_4A7F_9C2E_1B6D4F8A0E53);
pub const CLSID_CONTEXT_MENU_STR: &str = "{9F3A2B1C-5E8D-4A7F-9C2E-1B6D4F8A0E53}";

/// SageThumbs 2K preview-pane handler (IPreviewHandler + IInitializeWithStream +
/// IObjectWithSite + IPreviewHandlerVisuals). Loaded by the shell's out-of-process
/// preview host (`prevhost.exe`) via the surrogate AppID set in `register.rs`.
pub const CLSID_PREVIEW_HANDLER: GUID =
    GUID::from_u128(0x2C8F1A3D_6B4E_4D9C_A1F2_7E3B5C8D0A46);
pub const CLSID_PREVIEW_HANDLER_STR: &str = "{2C8F1A3D-6B4E-4D9C-A1F2-7E3B5C8D0A46}";

/// SageThumbs 2K property handler (IPropertyStore + IInitializeWithStream). Surfaces image
/// dimensions / EXIF camera / audio tags in Explorer's Details pane, info-tips, and columns.
/// Loads in-process into explorer.exe + SearchIndexer.exe (read-only, panic-guarded).
pub const CLSID_PROPERTY_STORE: GUID =
    GUID::from_u128(0x5E1A7C92_8F3D_4B6A_A0E4_3C7B9D2F1A68);
pub const CLSID_PROPERTY_STORE_STR: &str = "{5E1A7C92-8F3D-4B6A-A0E4-3C7B9D2F1A68}";

#[cfg(test)]
mod tests {
    use super::*;

    /// Canonical brace-less GUID string (the form the AppxManifest uses).
    fn bare(g: GUID) -> String {
        format!(
            "{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
            g.data1, g.data2, g.data3,
            g.data4[0], g.data4[1], g.data4[2], g.data4[3],
            g.data4[4], g.data4[5], g.data4[6], g.data4[7],
        )
    }

    const MANIFEST: &str = include_str!("../packaging/AppxManifest.xml");

    /// The registry string-form CLSIDs must equal their typed GUIDs — a hand typo
    /// in either would register the wrong coclass with no compile error.
    #[test]
    fn clsid_string_consts_match_typed_guids() {
        assert_eq!(CLSID_THUMBNAIL_PROVIDER_STR, format!("{{{}}}", bare(CLSID_THUMBNAIL_PROVIDER)));
        assert_eq!(CLSID_CONTEXT_MENU_STR, format!("{{{}}}", bare(CLSID_CONTEXT_MENU)));
        assert_eq!(CLSID_PREVIEW_HANDLER_STR, format!("{{{}}}", bare(CLSID_PREVIEW_HANDLER)));
        assert_eq!(CLSID_PROPERTY_STORE_STR, format!("{{{}}}", bare(CLSID_PROPERTY_STORE)));
    }

    /// Every CLSID literal in the package manifest must be one of the coclass GUIDs
    /// from this file: the dormant root `CLSID_EXPLORER_COMMAND` surrogate plus the four
    /// modern-menu quick-verb CLSIDs (`desktop5:Verb Clsid="…"` + their `com:Class Id`).
    /// A regenerated GUID that updated guids.rs but not the hand-maintained manifest would
    /// silently break COM activation with no compile error, and a stray Clsid the code
    /// doesn't hand out would be a dead verb — this pins both directions.
    #[test]
    fn manifest_clsids_match_known() {
        let known: Vec<String> = [
            CLSID_EXPLORER_COMMAND,
            CLSID_QUICK_CONVERT_INTO,
            CLSID_QUICK_CONVERT_DIALOG,
            CLSID_QUICK_RESIZE,
            CLSID_QUICK_ROTATE,
        ]
        .iter()
        .map(|g| bare(*g))
        .collect();
        let is_known = |val: &str| known.iter().any(|k| k.eq_ignore_ascii_case(val));

        // Each quick-verb CLSID must appear as a `com:Class Id` (the surrogate server entry)
        // AND the root command's too — otherwise the surrogate can't activate the coclass.
        let upper = MANIFEST.to_uppercase();
        for g in &known {
            assert!(
                upper.contains(&format!("ID=\"{g}\"")),
                "com:Class Id missing for coclass `{g}`",
            );
        }
        // Every verb Clsid in the manifest must be one we actually hand out.
        for seg in MANIFEST.split("Clsid=\"").skip(1) {
            let val = seg.split('"').next().unwrap();
            assert!(is_known(val), "manifest Clsid `{val}` is not a known coclass GUID");
        }
    }

    /// Any modern-menu `ItemType Type=".<ext>"` must be a real FORMATS entry, so the
    /// manifest can't pin a verb to a type we don't actually handle. The quick verbs use
    /// `ItemType Type="*"` (all files; the per-verb `GetState` gate hides them on anything
    /// that isn't a supported, non-audio image), which has no `.<ext>` to validate — this
    /// guards any future build that lists explicit extensions instead.
    #[test]
    fn manifest_item_types_are_known_formats() {
        for seg in MANIFEST.split("ItemType Type=\".").skip(1) {
            let ext = seg.split('"').next().unwrap().to_ascii_lowercase();
            assert!(crate::formats::is_known(&ext), "manifest ItemType `.{ext}` is not in FORMATS");
        }
    }

    /// The package version must track the crate version (the manifest carries a
    /// 4-part version; compare the leading major.minor.patch).
    #[test]
    fn manifest_version_tracks_crate_version() {
        let want = env!("CARGO_PKG_VERSION"); // e.g. "0.2.0"
        let ver = MANIFEST
            .split("<Identity")
            .nth(1)
            .and_then(|s| s.split("Version=\"").nth(1))
            .and_then(|s| s.split('"').next())
            .expect("manifest <Identity Version=...> not found");
        assert!(
            ver == want || ver.strip_prefix(want).is_some_and(|r| r.starts_with('.')),
            "manifest Version `{ver}` does not track crate version `{want}`",
        );
    }
}
