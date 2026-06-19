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
    }

    /// Every CLSID literal in the package manifest must be the Explorer-command
    /// GUID from this file. We ship classic-menu-only now (no modern verb), so the
    /// only CLSID is the dormant `com:Class Id` surrogate declaration; if a future
    /// build re-adds `desktop5:Verb Clsid="…"` entries (see AppxManifest.xml note),
    /// this pins them too. A regenerated GUID that updated guids.rs but not the
    /// hand-maintained manifest would silently break COM activation with no
    /// compile error.
    #[test]
    fn manifest_clsids_match_explorer_command() {
        let want = bare(CLSID_EXPLORER_COMMAND);
        // The com:Class Id (the COM surrogate server) must always match.
        assert!(
            MANIFEST.to_uppercase().contains(&format!("ID=\"{want}\"")),
            "com:Class Id does not match CLSID_EXPLORER_COMMAND",
        );
        // Any modern-menu verb Clsid (absent in the classic-only build) must match too.
        for seg in MANIFEST.split("Clsid=\"").skip(1) {
            let val = seg.split('"').next().unwrap();
            assert!(val.eq_ignore_ascii_case(&want), "manifest Clsid `{val}` != `{want}`");
        }
    }

    /// Any modern-menu ItemType extension must be a real FORMATS entry, so the
    /// manifest can't pin the flyout to a type we don't actually handle. The
    /// classic-only build has no ItemType entries; this guards a future re-add.
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
