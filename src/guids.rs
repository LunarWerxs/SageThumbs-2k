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

/// SageThumbs 2K context-menu command (IExplorerCommand).
pub const CLSID_EXPLORER_COMMAND: GUID =
    GUID::from_u128(0xD4F1C8A2_3E7B_4A96_8C0F_6B1E2D9A4C57);

/// Same CLSID, string form, for the package manifest / registration.
#[allow(dead_code)]
pub const CLSID_EXPLORER_COMMAND_STR: &str = "{D4F1C8A2-3E7B-4A96-8C0F-6B1E2D9A4C57}";

/// SageThumbs 2K classic context-menu handler (IContextMenu + IShellExtInit).
/// Needed for machines where the modern Win11 menu is replaced by the classic
/// menu (StartAllBack / ExplorerPatcher / registry tweak).
pub const CLSID_CONTEXT_MENU: GUID =
    GUID::from_u128(0x9F3A2B1C_5E8D_4A7F_9C2E_1B6D4F8A0E53);
pub const CLSID_CONTEXT_MENU_STR: &str = "{9F3A2B1C-5E8D-4A7F-9C2E-1B6D4F8A0E53}";
