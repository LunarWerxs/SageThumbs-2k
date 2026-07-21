//! Dialog control IDs + layout constants (extracted from settings_dlg.rs, pure data).

// ---- Control IDs --------------------------------------------------------
pub(super) const ID_ENABLE_THUMBS: i32 = 1001;
pub(super) const ID_USE_EMBEDDED: i32 = 1002;
pub(super) const ID_ENABLE_MENU: i32 = 1003;
pub(super) const ID_MAXSIZE: i32 = 1004;
pub(super) const ID_SIZE: i32 = 1005;
pub(super) const ID_JPEG: i32 = 1006;
pub(super) const ID_PNG: i32 = 1007;
pub(super) const ID_LIST: i32 = 1008;
pub(super) const ID_SELECT_ALL: i32 = 1009;
pub(super) const ID_CLEAR_ALL: i32 = 1010;
pub(super) const ID_DEFAULTS: i32 = 1011;
// Translatable static labels (need IDs so the language picker can relabel live).
pub(super) const ID_LBL_THUMBS: i32 = 1100;
pub(super) const ID_LBL_LIMITS: i32 = 1101;
pub(super) const ID_LBL_MAXFILE: i32 = 1102;
pub(super) const ID_LBL_MAXTHUMB: i32 = 1103;
pub(super) const ID_LBL_JPEG: i32 = 1104;
pub(super) const ID_LBL_PNG: i32 = 1105;
pub(super) const ID_LBL_FORMATS: i32 = 1106;
pub(super) const ID_LBL_LANG: i32 = 1107;
pub(super) const ID_LANG: i32 = 1108;
// Ebook/comic archive cover options.
pub(super) const ID_LBL_EBOOK: i32 = 1109;
pub(super) const ID_C_SORT: i32 = 1110;
pub(super) const ID_C_PREFER_COVER: i32 = 1111;
pub(super) const ID_C_SKIP_SCAN: i32 = 1112;
// Sponsor promotion (footer link + clickable banner + About box).
pub(super) const ID_ABOUT: i32 = 1113;
pub(super) const ID_PROMO_LINK: i32 = 1114;
pub(super) const ID_BANNER: i32 = 1115;
// Context-menu preview placement (Off / submenu / main menu).
pub(super) const ID_LBL_PREVIEW: i32 = 1116;
pub(super) const ID_MENU_PREVIEW: i32 = 1117;
// Quick verbs directly on the main right-click menu.
pub(super) const ID_MENU_QUICK: i32 = 1118;
// Show the menu on ALL file types (a condensed file-utility set on unsupported files).
pub(super) const ID_MENU_ALL_TYPES: i32 = 1119;
// Subtle checkerboard behind the menu preview's transparent areas.
pub(super) const ID_MENU_CHECKER: i32 = 1120;
// "Keep original date on saved files" — preserve source mtime on Convert/Resize/Rotate output.
pub(super) const ID_PRESERVE_DATE: i32 = 1121;

// Settings-sync (optional Connections account) — the opt-in row. IDs 1200-1202 are free
// (control IDs stop at 1187; nav IDs start at 1700).
pub(super) const ID_LBL_SYNC: i32 = 1200;
pub(super) const ID_SYNC_BTN: i32 = 1201;
// Live "● Synced · up to date" status line beside the sync button (green when signed in,
// muted when signed out) — replaces baking the raw account id into the button label.
pub(super) const ID_SYNC_STATUS: i32 = 1202;
// Left-column scroll plumbing: a vertical scrollbar + an opaque mask that hides
// controls scrolled below the viewport (so the left options can grow/scroll
// without making the window taller).
pub(super) const ID_SCROLLBAR: i32 = 1131;
pub(super) const ID_LEFT_MASK: i32 = 1132;
// Live search box that filters the supported-file-types list.
pub(super) const ID_SEARCH: i32 = 1133;
// Screenshot capture service: an enable toggle + a hotkey preset picker (the
// opt-in tray daemon's global hotkey, configurable here instead of via the tray).
pub(super) const ID_LBL_SHOT: i32 = 1134;
pub(super) const ID_SHOT_ENABLE: i32 = 1135;
pub(super) const ID_LBL_SHOT_HK: i32 = 1136;
pub(super) const ID_SHOT_HOTKEY: i32 = 1137;
// Live daemon status line + a Start/Restart button (the hotkey only fires while the
// tray daemon is alive; this surfaces its state + lets you recover it).
pub(super) const ID_SHOT_STATUS: i32 = 1139;
pub(super) const ID_SHOT_RESTART: i32 = 1140;
// Settings checkbox: hide the daemon's notification-area (tray) icon.
pub(super) const ID_SHOT_HIDE_TRAY: i32 = 1141;
// Optional second "quick-save" hotkey (full-screen → clipboard+PNG, no editor):
// an enable checkbox that gates the hotkey-picker combo.
pub(super) const ID_SHOT_QUICK_ENABLE: i32 = 1144;
pub(super) const ID_LBL_SHOT_QUICK_HK: i32 = 1142;
pub(super) const ID_SHOT_QUICK_HOTKEY: i32 = 1143;
// Ctrl+S save destination: a "use a fixed folder" toggle, a folder-picker button, and a
// read-only display of the current folder (the Desktop known folder by default).
pub(super) const ID_SHOT_USE_DIR: i32 = 1169;
pub(super) const ID_SHOT_SET_DIR: i32 = 1170;
pub(super) const ID_SHOT_DIR: i32 = 1171;
// "General" section header (right-click-menu settings + UI language).
pub(super) const ID_LBL_GENERAL: i32 = 1138;
// "Menu items" checklist header (per-item context-menu visibility).
pub(super) const ID_LBL_MENU_ITEMS: i32 = 1164;
// The "Menu items" visibility checklist — a compact checkbox ListView (like the
// Supported File Types list) instead of ~14 stacked checkboxes.
pub(super) const ID_MENU_ITEMS_LIST: i32 = 1165;
// "Reset order" button under the checklist — restores the default drag-reorder order.
pub(super) const ID_MENU_RESET: i32 = 1145;
// "Reset all settings" button (left column, end of Diagnostics) — factory reset of the
// whole dialog. (The top-right "Defaults" resets only the file-type list — see its tip.)
pub(super) const ID_RESET_ALL: i32 = 1146;

// Diagnostics section (error/crash log).
pub(super) const ID_LBL_DIAG: i32 = 1166;
pub(super) const ID_VERBOSE_LOG: i32 = 1167;
pub(super) const ID_OPEN_LOG: i32 = 1168;
// Import / Export settings — they share the Reset row at the end of Diagnostics
// (1169–1171 are the Ctrl+S save-dir controls above).
pub(super) const ID_IMPORT: i32 = 1172;
pub(super) const ID_EXPORT: i32 = 1173;
// Diagnostics actions: clear Windows' thumbnail cache + check GitHub for a newer release.
pub(super) const ID_REBUILD_CACHE: i32 = 1174;
pub(super) const ID_CHECK_UPDATES: i32 = 1175;
// Re-register all enabled formats (fixes thumbnails stolen by another app).
pub(super) const ID_REPAIR_ASSOC: i32 = 1176;
// Toggle the background update check (the one the resident hotkey helper runs).
pub(super) const ID_UPDATE_AUTO: i32 = 1177;
// Custom action hotkey (the user-assignable "action -> hotkey" binding): an action
// picker + a hotkey-chord picker, both under the Screenshots section.
pub(super) const ID_LBL_SHOT_ACTION: i32 = 1178;
pub(super) const ID_SHOT_ACTION: i32 = 1179;
pub(super) const ID_LBL_SHOT_ACTION_HK: i32 = 1180;
pub(super) const ID_SHOT_ACTION_HK: i32 = 1181;
// v3 reorg: an explicit enable toggle for the custom action (gates the two combos),
// plus group sub-headers for the reorganized General / Advanced pages.
pub(super) const ID_CUSTOM_ACTION_ENABLE: i32 = 1182;
pub(super) const ID_LBL_UPDATES: i32 = 1184;
pub(super) const ID_LBL_BACKUP: i32 = 1185;
pub(super) const ID_LBL_HOTKEY_SVC: i32 = 1186;
// "Edit upload hosts…" — opens the user-editable upload-hosts config file
// (%APPDATA%\SageThumbs2K\upload-hosts.conf) in the default text editor.
pub(super) const ID_EDIT_UPLOAD_HOSTS: i32 = 1187;
// Quick preview (QuickLook-style Space-to-preview) — the master toggle drives daemon
// residency (like the screenshot service); the three below are viewer-behavior prefs.
// Control IDs stop at 1202 (ID_SYNC_STATUS); nav IDs start at 1700, so 1203+ is free.
pub(super) const ID_PREVIEW_ENABLED: i32 = 1203;
pub(super) const ID_PREVIEW_HOLD_PEEK: i32 = 1204;
pub(super) const ID_PREVIEW_CLOSE_FOCUS: i32 = 1205;
pub(super) const ID_PREVIEW_TOPMOST: i32 = 1206;
// Phase 3: preview text/code + render markdown.
pub(super) const ID_PREVIEW_TEXT: i32 = 1207;
pub(super) const ID_PREVIEW_MARKDOWN: i32 = 1208;
// Markdown remote-image fetch (opt-in; not feature-gated — markdown always ships).
pub(super) const ID_PREVIEW_MD_REMOTE: i32 = 1211;
#[cfg(feature = "html-preview")]
pub(super) const ID_PREVIEW_HTML: i32 = 1209;
#[cfg(feature = "html-preview")]
pub(super) const ID_PREVIEW_URL_LIVE: i32 = 1210;
// Generic-archive (.zip/.rar/.7z) contact-sheet thumbnails (Ebook/comic tab).
pub(super) const ID_C_ARCHIVE_SHEET: i32 = 1212;

/// Per-item menu-visibility checkboxes (XnShell-style "Displayed menu items").
/// Each (control id, MENU title key); the checkbox LABEL reuses the menu item's
/// own translated name via `t(key)`. `menu_settings` is intentionally absent — the
/// Settings entry is always shown so the dialog stays reachable.
pub(super) const MENU_ITEM_TOGGLES: &[(i32, &str)] = &[
    (1150, "menu_convert_into"),
    (1151, "menu_convert_dialog"),
    (1152, "menu_combine_pdf"),
    (1153, "menu_combine_cbz"),
    (1154, "menu_resize"),
    (1155, "menu_email"),
    (1156, "menu_rotate"),
    (1157, "menu_rename"),
    (1158, "menu_files_to_folder"),
    (1159, "menu_sort"),
    // "Tools" is now four individually-toggleable top-level entries (was one submenu).
    (1160, "menu_copy_text"),
    (1147, "menu_image_info"),
    (1148, "menu_pick_color"),
    (1149, "menu_strip_meta"),
    (1161, "menu_copy"),
    (1183, "menu_upload"),
    (1162, "menu_set_folder_icon"),
    (1163, "menu_wallpaper"),
];

/// Capture-hotkey presets offered in the Settings dropdown, each paired with its
/// packed HOTKEYF/VK value (high byte = HOTKEYF_* modifiers, low byte = virtual
/// key) — the same packing `settings::screenshot_hotkey` stores. Curated to safe,
/// non-conflicting chords (no bare letters that would hijack a global key, and
/// avoiding Win+Shift+S / Alt+PrtScn which the OS already claims).
pub(crate) const SHOT_PRESETS: &[(&str, u32)] = &[
    ("Ctrl + PrtScn", (0x02 << 8) | 0x2C),
    ("PrtScn", 0x2C),
    ("Ctrl + Shift + S", ((0x02 | 0x01) << 8) | 0x53),
    ("Ctrl + Shift + A", ((0x02 | 0x01) << 8) | 0x41),
    ("Ctrl + Shift + 4", ((0x02 | 0x01) << 8) | 0x34),
    ("Ctrl + Alt + S", ((0x02 | 0x04) << 8) | 0x53),
    ("F9", 0x78),
    ("Ctrl + F12", (0x02 << 8) | 0x7B),
];
/// Default chord pre-selected in the quick-save combo when none is saved yet —
/// deliberately NOT the main `Ctrl + PrtScn` default, so enabling the instant
/// screenshot doesn't try to grab a chord already owned by the editor hotkey.
pub(super) const QUICK_DEFAULT_LABEL: &str = "Ctrl + Shift + S";

// Left-column scroll geometry (96-dpi design px). The viewport is the visible
// band of the left options; content taller than it scrolls.
pub(super) const LEFT_VIEW_TOP: i32 = 6;
pub(super) const LEFT_VIEW_BOTTOM: i32 = 442;
pub(super) const LEFT_RIGHT_EDGE: i32 = 340; // x past which a control is "right column" (not scrolled)
