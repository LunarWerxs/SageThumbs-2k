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
// "Keep EXIF and other metadata in converted files" - carry EXIF/XMP/IPTC through
// Convert/Resize instead of dropping it in the re-encode.
pub(super) const ID_KEEP_METADATA: i32 = 1122;
// "Add a margin to combined PDFs" - PdfLayout 1 vs 0. The A4/Letter sheet modes the
// engine also supports stay registry-only (see settings::pdf_page).
pub(super) const ID_PDF_MARGIN: i32 = 1123;

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
/// Which annotation tool the capture editor opens with (combo + its label).
pub(super) const ID_LBL_SHOT_TOOL: i32 = 1230;
pub(super) const ID_SHOT_TOOL: i32 = 1231;
/// Seconds to wait before a capture freezes the screen (combo + its label).
pub(super) const ID_LBL_SHOT_DELAY: i32 = 1237;
pub(super) const ID_SHOT_DELAY: i32 = 1238;
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
/// "Check for problems" - opens the doctor report (see `doctor_report.rs`).
/// 1183 was picked as the one free `const` slot left in this block. A first attempt reused
/// 1177, which is `ID_UPDATE_AUTO`: Win32 identifies a control by its id, so that did not
/// error, it silently REPLACED the "Automatically check for updates" switch on the Advanced
/// page. (1183 was never fully free even after that: `MENU_ITEM_TOGGLES` below also carried a
/// `(1183, "menu_upload")` entry, invisible to the test because it only parsed `const ID_*`
/// lines, not array literals - harmless in practice since nothing ever reads that field, but
/// it meant this comment's "free slot" claim was never quite true. Moved to 1234; the test now
/// scans `MENU_ITEM_TOGGLES` too.)
/// The duplicate-id test at the bottom of this file exists so the next one fails loudly.
pub(super) const ID_RUN_DOCTOR: i32 = 1183;
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
/// "Appearance:" on the Quick preview page: light/dark for SageThumbs' own windows, instead of
/// following the Windows app-colour setting. Label + combo, like `ID_LBL_PREVIEW`/`ID_MENU_PREVIEW`.
pub(super) const ID_LBL_APP_THEME: i32 = 1235;
pub(super) const ID_APP_THEME: i32 = 1236;
// Phase 3: preview text/code + render markdown.
pub(super) const ID_PREVIEW_TEXT: i32 = 1207;
pub(super) const ID_PREVIEW_MARKDOWN: i32 = 1208;
#[cfg(feature = "html-preview")]
pub(super) const ID_PREVIEW_HTML: i32 = 1209;
#[cfg(feature = "html-preview")]
pub(super) const ID_PREVIEW_URL_LIVE: i32 = 1210;
// "Add 'Build thumbnails here' to the folder right-click menu" — a folder-level verb
// (see `crate::foldermenu`), not a thumbnail-appearance switch, so it lives with the
// other right-click-menu checkboxes. 1211 was the one free id between the html-preview
// block above and ID_C_ARCHIVE_SHEET below.
pub(super) const ID_FOLDER_PREBUILD: i32 = 1211;
// Generic-archive (.zip/.rar/.7z) contact-sheet thumbnails (Ebook/comic tab).
pub(super) const ID_C_ARCHIVE_SHEET: i32 = 1212;
/// "In the corner of a thumbnail:" — the one three-way choice that replaced the old
/// `FormatBadge` + `HideTypeOverlay` checkbox pair. Both of those addressed the SAME corner of
/// the tile and the combination people naturally reached for (badge on, overlay not hidden)
/// let Explorer paint its icon straight over our mark. See `settings::CornerMark`.
pub(super) const ID_LBL_CORNER_MARK: i32 = 1244;
pub(super) const ID_CORNER_MARK: i32 = 1245;
/// Badge STYLE: ticked = the category-coloured icon, unticked = the older plain text chip.
/// Only meaningful while `ID_CORNER_MARK` is on "our own mark", and greyed out when it isn't.
pub(super) const ID_BADGE_ICON: i32 = 1216;
/// Burn the transparency checkerboard into Explorer thumbnails. Separate from
/// `ID_MENU_CHECKER`, which is the same idea for the two PREVIEW surfaces — different
/// surfaces, different mechanisms, so one switch could not honestly drive both.
pub(super) const ID_THUMB_CHECKER: i32 = 1217;
/// Show a video's embedded poster instead of a frame from the film. Cover art is used as a
/// FALLBACK regardless of this switch (a file whose codec Windows lacks has no frame at all);
/// this makes it the PREFERENCE, which is what a ripped-film library wants.
pub(super) const ID_VIDEO_COVER_ART: i32 = 1219;
/// How far into a video the thumbnail frame is taken from, as a percentage of its length
/// (`VideoOffset`, default 30). Lives beside the cover-art switch because it answers the same
/// question — "what picture stands for this film" — and because a black tile is the reason
/// people reach for either. See `settings::video_offset_pct` for why 30 % is not always right.
pub(super) const ID_LBL_VIDEO_OFFSET: i32 = 1232;
pub(super) const ID_VIDEO_OFFSET: i32 = 1233;
/// Portable build only: turn the per-user Explorer thumbnail registration on/off, plus the
/// status line that says which it currently is. Hidden entirely on an installed build, where
/// the machine-wide registration already covers it and a second switch would only confuse.
pub(super) const ID_PORTABLE_REG: i32 = 1214;
pub(super) const ID_PORTABLE_REG_STATUS: i32 = 1215;
/// "Thumbnail appearance" header above File types' two exiled what-the-tile-looks-like rows.
pub(super) const ID_LBL_TILE_LOOK: i32 = 1220;
/// "Choose the formats" header between those rows and the Select all/format list below.
pub(super) const ID_LBL_FORMATS_PICK: i32 = 1221;
/// "Converting & resizing" header splitting menu-appearance switches from verb behavior.
pub(super) const ID_LBL_CONVERT_VERBS: i32 = 1222;
/// "Also preview" header splitting Quick preview's behavior switches from content opt-ins.
pub(super) const ID_LBL_PREVIEW_KINDS: i32 = 1223;
/// First-section header of the Right-click menu page ("Menu appearance").
pub(super) const ID_LBL_MENU_LOOK: i32 = 1226;
/// First-section header of the Quick action page.
pub(super) const ID_LBL_QUICKACTION: i32 = 1227;
/// First-section header of the Quick preview page ("Behavior").
pub(super) const ID_LBL_PREVIEW_BEHAVIOR: i32 = 1228;
/// Opens the menu-items checklist in its own popup editor (the on-page list was what
/// kept the Right-click menu page cramped).
pub(super) const ID_MENU_ITEMS_EDIT: i32 = 1229;
/// Settings-wide search: the always-visible edit at the top-right of the pane header…
pub(super) const ID_SEARCH_GLOBAL: i32 = 1224;
/// …and the results list that drops down under it (hidden until there are matches).
pub(super) const ID_SEARCH_RESULTS: i32 = 1225;

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
    (1234, "menu_upload"), // moved off 1183 - collided with ID_RUN_DOCTOR, see the comment there
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

#[cfg(test)]
mod tests {
    /// Every control id in this file must be unique.
    ///
    /// Win32 identifies a control by its id, so a duplicate is not a compile error and not a
    /// runtime error either: the second control simply takes the first one's place, and the
    /// first quietly disappears from its page. That is exactly what happened on 2026-08-06,
    /// when `ID_RUN_DOCTOR` was given 1177 (`ID_UPDATE_AUTO`) and the "Automatically check for
    /// updates" switch vanished from Advanced. It was caught by rendering the page and noticing
    /// a control missing, which is not a thing to rely on.
    ///
    /// Parses THIS FILE rather than listing the constants, so a new id is covered the moment it
    /// is added and nobody has to remember to extend a list. Also scans `MENU_ITEM_TOGGLES`'s
    /// tuple literals: their first field is a leftover id column nothing reads by index, but a
    /// stray literal there (`(1183, "menu_upload")` collided with `ID_RUN_DOCTOR` for a while)
    /// is exactly as capable of shadowing a real control id as a `const` would be, and a plain
    /// `const ID_*` parse can't see into an array literal at all.
    #[test]
    fn control_ids_are_unique() {
        let src = include_str!("ids.rs");
        let mut seen: Vec<(String, i64)> = Vec::new();
        for line in src.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("pub(super) const ") {
                let Some((name, tail)) = rest.split_once(':') else {
                    continue;
                };
                let name = name.trim();
                if !name.starts_with("ID_") {
                    continue; // geometry constants share values legitimately
                }
                let Some((_, value)) = tail.split_once('=') else {
                    continue;
                };
                // Skip anything computed from another constant; only plain literals compare.
                let Ok(value) = value.trim().trim_end_matches(';').trim().parse::<i64>() else {
                    continue;
                };
                if let Some((other, _)) = seen.iter().find(|(_, v)| *v == value) {
                    panic!(
                        "duplicate control id {value}: {name} collides with {other}. \
                         Win32 will silently replace one control with the other; pick a free id."
                    );
                }
                seen.push((name.to_string(), value));
            } else if let Some(rest) = line.strip_prefix('(') {
                // A `MENU_ITEM_TOGGLES`-style tuple literal `(NUM, "key"),`. Distinguished from
                // `SHOT_PRESETS`'s `("label", value)` tuples by leading with a digit, not a `"`.
                if !rest.starts_with(|c: char| c.is_ascii_digit()) {
                    continue;
                }
                let Some((num, tail)) = rest.split_once(',') else {
                    continue;
                };
                let Ok(value) = num.trim().parse::<i64>() else {
                    continue;
                };
                let key = tail.trim().trim_start_matches('"');
                let key = key.split('"').next().unwrap_or(key);
                let name = format!("MENU_ITEM_TOGGLES[\"{key}\"]");
                if let Some((other, _)) = seen.iter().find(|(_, v)| *v == value) {
                    panic!(
                        "duplicate control id {value}: {name} collides with {other}. \
                         Win32 will silently replace one control with the other; pick a free id."
                    );
                }
                seen.push((name, value));
            }
        }
        assert!(
            seen.len() > 50,
            "only parsed {} ids - the parser stopped matching this file's style",
            seen.len()
        );
    }
}
