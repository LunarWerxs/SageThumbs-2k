//! EXE-only viewer/app preference accessors: the Convert dialog's per-format export
//! settings, the screenshot tool's default annotation tool, per-verb output preferences,
//! the capture/custom-action hotkeys, the eyedropper's format + pick history, screenshot
//! save-destination + diagnostics + update-check toggles, and the whole Quick preview
//! viewer preference set. The DLL never reads any of these. The shared storage backend
//! (registry vs portable ini) lives in `super::store`; the thumbnail/menu settings the
//! DLL needs live in `super::thumbs`.

use super::*;

// ---- Convert… dialog per-format export settings (persisted) --------------
// The Convert dialog's per-format Settings popup (JPEG quality / PNG level /
// WebP quality+lossless) used to live in process-only statics that reset every
// launch. These persist them under their own HKCU keys (separate from the global
// thumbnail JPEG/PNG above) so a user's chosen export quality survives restarts.
// Defaults match the dialog's historical static defaults (JPEG 90 / WebP 80,
// lossy / PNG 6).

/// Convert dialog: JPEG export quality, 1–100.
pub fn cv_jpeg_quality() -> u32 {
    get_dword("CvJpegQuality", 90).clamp(1, 100)
}
/// Convert dialog: lossy-WebP export quality, 1–100.
pub fn cv_webp_quality() -> u32 {
    get_dword("CvWebpQuality", 80).clamp(1, 100)
}
/// Convert dialog: encode WebP losslessly (else lossy at [`cv_webp_quality`]).
pub fn cv_webp_lossless() -> bool {
    get_dword("CvWebpLossless", 0) != 0
}
/// Convert dialog: PNG compression level, 0–9.
pub fn cv_png_level() -> u32 {
    get_dword("CvPngLevel", 6).clamp(0, 9)
}
/// Convert dialog: lossy-magick (AVIF / JPEG XL) export quality, 1–100. Drives the
/// `-quality` flag passed to ImageMagick for those targets. Default 50 — a good
/// size/quality balance for AVIF (and reasonable for JXL).
pub fn cv_magick_quality() -> u32 {
    get_dword("CvMagickQuality", 50).clamp(1, 100)
}
/// Persist the Convert dialog's AVIF/JXL quality (clamped 1–100).
pub fn set_cv_magick_quality(q: u32) {
    let _ = set_dword("CvMagickQuality", q.clamp(1, 100));
}

/// Persist the Convert dialog's per-format settings (best-effort; clamped).
pub fn set_cv_settings(jpeg_quality: u32, webp_quality: u32, webp_lossless: bool, png_level: u32) {
    let _ = set_dword("CvJpegQuality", jpeg_quality.clamp(1, 100));
    let _ = set_dword("CvWebpQuality", webp_quality.clamp(1, 100));
    let _ = set_dword("CvWebpLossless", webp_lossless as u32);
    let _ = set_dword("CvPngLevel", png_level.clamp(0, 9));
}

/// Which annotation tool the capture editor opens with, as an INDEX into
/// `screenshot::tools::Tool::DEFAULTABLE`. That array owns the ordering and the fallback;
/// this is only the stored number, so the two cannot disagree about what index 0 means.
pub const DEFAULT_SHOT_TOOL: u32 = 0; // Arrow

/// How many tools the Settings dropdown offers, i.e. the length of
/// `screenshot::tools::Tool::DEFAULTABLE`. It lives HERE because the Settings dialog cannot
/// see that array (it is `pub(super)` inside the screenshot module) and still has to clamp
/// `CB_SETCURSEL`: an out-of-range index selects nothing and the combo renders BLANK, which
/// is what a hand-edited registry value used to produce. `tools.rs` holds a compile-time
/// assertion that the two agree, so adding a tool without updating this fails the build
/// rather than silently truncating the list.
pub const SHOT_TOOL_COUNT: u32 = 10;

/// The starting tool for the capture editor (index into `Tool::DEFAULTABLE`).
///
/// Defaults to ARROW rather than the rectangle it used to hardcode: pointing at the thing
/// you just captured is the common case, and a rectangle is the one people undo. Unclamped
/// on purpose — `Tool::from_default_index` owns the range check, so a hand-edited registry
/// value degrades in exactly one place.
pub fn screenshot_default_tool() -> u32 {
    get_dword("ShotDefaultTool", DEFAULT_SHOT_TOOL)
}

/// Persist the capture editor's starting tool. See [`screenshot_default_tool`].
pub fn set_screenshot_default_tool(index: u32) -> windows_registry::Result<()> {
    set_dword("ShotDefaultTool", index)
}

/// Preserve the source file's date/time on saved outputs (Convert / Resize /
/// Rotate). Off by default — saved files get the current time, like most tools.
pub fn preserve_file_date() -> bool {
    get_dword("PreserveFileDate", 0) != 0
}

/// Page layout for Combine-into-PDF.
///
/// `PdfLayout`: 0 = tight (default), 1 = margin, 2 = A4 sheet, 3 = Letter sheet.
/// `PdfMarginPt` is the margin in points for modes 1-3 (default 36 = half an inch).
/// Settings exposes only the margin on/off, which is the option PDF24 actually
/// added; the two sheet modes are engine features reachable by setting
/// `PdfLayout` directly, and are documented rather than given a four-way combo
/// nobody asked for.
pub fn pdf_page() -> crate::topdf::PdfPage {
    use crate::topdf::{PdfPage, A4_PT, LETTER_PT};
    let margin = f64::from(get_dword("PdfMarginPt", 36));
    match get_dword("PdfLayout", 0) {
        1 => PdfPage::Margin(margin),
        2 => PdfPage::Sheet {
            w: A4_PT.0,
            h: A4_PT.1,
            margin,
        },
        3 => PdfPage::Sheet {
            w: LETTER_PT.0,
            h: LETTER_PT.1,
            margin,
        },
        _ => PdfPage::Tight,
    }
}

/// Carry EXIF / XMP / IPTC from the source into a converted or resized output.
///
/// **On** by default: our pipeline decodes to pixels and re-encodes, so without
/// this a Convert silently throws away the camera, lens, date and GPS — which is
/// not what someone converting their own photos expects (XnView bundles ExifTool
/// precisely to avoid it). Someone who wants the metadata GONE has an explicit
/// Strip metadata verb; losing it by accident is the worse default.
///
/// Deliberately NOT consulted by Shrink for email, which always drops metadata:
/// that path exists to hand a file to someone else, and mailing your home GPS
/// coordinates is a bigger harm than losing a camera model.
pub fn keep_metadata_on_convert() -> bool {
    get_dword("KeepMetadata", 1) != 0
}

// ---- Screenshot capture hotkey ------------------------------------------
// The opt-in screenshot daemon's global hotkey, stored in the native Win32
// "hotkey control" packing: high byte = HOTKEYF_* modifiers (SHIFT 0x01,
// CONTROL 0x02, ALT 0x04), low byte = virtual-key code. The daemon converts
// these to RegisterHotKey's MOD_* flags. Default: Ctrl + PrtScn — matching the
// behavior before the hotkey became configurable.

/// Default capture hotkey: Ctrl + PrtScn, in packed HOTKEYF/VK form.
pub const DEFAULT_SHOT_HOTKEY: u32 = (0x02 << 8) | 0x2C; // HOTKEYF_CONTROL | VK_SNAPSHOT

/// The screenshot capture hotkey as `(hotkeyf_mods, vk)`.
pub fn screenshot_hotkey() -> (u32, u32) {
    let v = get_dword("ScreenshotHotkey", DEFAULT_SHOT_HOTKEY);
    ((v >> 8) & 0xFF, v & 0xFF)
}

/// Persist the capture hotkey (packed HOTKEYF/VK; only the low 16 bits are kept).
pub fn set_screenshot_hotkey(packed: u32) -> windows_registry::Result<()> {
    set_dword("ScreenshotHotkey", packed & 0xFFFF)
}

/// The OPTIONAL "quick-save" capture hotkey as `(hotkeyf_mods, vk)` — a second,
/// editor-less hotkey that grabs the whole screen straight to the clipboard + a
/// PNG. Default `0` (vk == 0) means **disabled** (no second hotkey registered);
/// the daemon skips registration when vk is 0, so it stays off until the user
/// picks a chord in Settings.
pub fn screenshot_quick_hotkey() -> (u32, u32) {
    let v = get_dword("ScreenshotQuickHotkey", 0);
    ((v >> 8) & 0xFF, v & 0xFF)
}

/// Persist the quick-save hotkey (packed HOTKEYF/VK; `0` = disabled).
pub fn set_screenshot_quick_hotkey(packed: u32) -> windows_registry::Result<()> {
    set_dword("ScreenshotQuickHotkey", packed & 0xFFFF)
}

// ---- Custom action hotkey (the user-assignable "action -> hotkey" binding) ----
// A single global hotkey bound to one of a curated set of actions (color picker,
// screenshot, convert…, rotate, files-to-folder, strip metadata, open settings). The
// action is a small opaque id (the app's `hotkey::ACTIONS` table owns the id→behavior
// map); the chord is packed exactly like the screenshot hotkeys (high byte HOTKEYF_*,
// low byte VK; `0` vk = unbound). It rides the SAME opt-in screenshot daemon, which
// registers + dispatches it — so a bound custom hotkey keeps that daemon resident even
// when the screenshot feature itself is off.

/// Default custom action id when none is stored: `1` = the colour picker (the headline ask).
pub const DEFAULT_CUSTOM_ACTION: u32 = 1;

/// The chosen custom-action id (see the app `hotkey::ACTIONS` table). Defaults to the
/// colour picker; the binding is only live once a hotkey is also assigned.
pub fn custom_action() -> u32 {
    get_dword("CustomAction", DEFAULT_CUSTOM_ACTION)
}

/// Persist the chosen custom-action id.
pub fn set_custom_action(id: u32) -> windows_registry::Result<()> {
    set_dword("CustomAction", id)
}

/// The custom action's hotkey as `(hotkeyf_mods, vk)`; `0` vk = unbound (no hotkey
/// registered). The daemon skips registration when vk is 0, so the binding stays off
/// until the user picks a chord in Settings.
pub fn custom_action_hotkey() -> (u32, u32) {
    let v = get_dword("CustomActionHotkey", 0);
    ((v >> 8) & 0xFF, v & 0xFF)
}

/// Persist the custom action hotkey (packed HOTKEYF/VK; `0` = unbound).
pub fn set_custom_action_hotkey(packed: u32) -> windows_registry::Result<()> {
    set_dword("CustomActionHotkey", packed & 0xFFFF)
}

/// Hide the screenshot daemon's notification-area (tray) icon. Off by default —
/// the icon makes the feature discoverable and offers Capture / Settings / Quit.
/// When hidden the hotkey still fires; manage the service from the Settings app.
pub fn screenshot_hide_tray() -> bool {
    get_dword("ScreenshotHideTray", 0) != 0
}

// ---- Screenshot save destination (Ctrl+S in the capture overlay) --------

/// The eyedropper's clipboard format: 0 hex `#RRGGBB` (the default and the historical
/// behaviour), 1 `rgb(r, g, b)`, 2 `hsl(...)`, 3 `hsv(...)`. Cycled with Tab inside the
/// overlay and remembered here, so a designer who lives in HSL sets it once. No Settings
/// row on purpose — the choice belongs in the tool, at the moment you can see the value.
pub fn eyedropper_format() -> u32 {
    get_dword("EyeFormat", 0).min(3)
}
/// Persist the eyedropper's clipboard format.
pub fn set_eyedropper_format(f: u32) -> windows_registry::Result<()> {
    set_dword("EyeFormat", f.min(3))
}

/// The eyedropper's pick history: up to 10 colours as comma-joined `RRGGBB` hex, most
/// recent first. Shown as a swatch row in the loupe on the NEXT session and recallable
/// with the 1–9 keys — re-grabbing yesterday's brand colour without hunting for a pixel
/// that still shows it.
pub fn eyedropper_history() -> Vec<(u8, u8, u8)> {
    let Some(s) = get_string_opt("EyeHistory") else {
        return Vec::new();
    };
    s.split(',')
        .filter_map(|t| {
            let t = t.trim();
            if t.len() != 6 {
                return None;
            }
            let v = u32::from_str_radix(t, 16).ok()?;
            Some(((v >> 16) as u8, (v >> 8) as u8, v as u8))
        })
        .take(10)
        .collect()
}
/// Persist the eyedropper pick history (most recent first; the cap is applied here so no
/// caller can grow the value without bound).
pub fn set_eyedropper_history(h: &[(u8, u8, u8)]) -> windows_registry::Result<()> {
    let joined: Vec<String> = h
        .iter()
        .take(10)
        .map(|&(r, g, b)| format!("{r:02X}{g:02X}{b:02X}"))
        .collect();
    set_string("EyeHistory", &joined.join(","))
}

/// Seconds to wait before a capture freezes the screen (0 = immediately, the default and
/// the historical behaviour). The wait is what lets a hover-only menu, a tooltip, or a
/// dropdown be summoned INTO the capture — the moment the overlay appears, focus moves and
/// those dismiss themselves, so without a delay they are uncapturable.
pub fn screenshot_delay_sec() -> u32 {
    get_dword("ShotDelaySec", 0).min(10)
}
/// Persist the capture delay. See [`screenshot_delay_sec`].
pub fn set_screenshot_delay_sec(s: u32) -> windows_registry::Result<()> {
    set_dword("ShotDelaySec", s.min(10))
}
/// The Settings combo's wire format: option index -> stored seconds. The array IS the
/// mapping (same discipline as `Tool::DEFAULTABLE`), so the dropdown and the stored value
/// cannot drift apart.
pub const SHOT_DELAY_STEPS: [u32; 5] = [0, 1, 2, 3, 5];

/// When ON, Ctrl+S (and the Save button) in the capture overlay auto-saves the PNG to
/// [`screenshot_save_dir`] (default: the Desktop). When OFF, Ctrl+S prompts for a
/// location each time. OFF by default — the capture asks where to save unless the user
/// opts into a fixed folder.
pub fn screenshot_use_save_dir() -> bool {
    get_dword("ShotUseSaveDir", 0) != 0
}

/// Persist the "use a fixed save folder" toggle.
pub fn set_screenshot_use_save_dir(on: bool) -> windows_registry::Result<()> {
    set_dword("ShotUseSaveDir", on as u32)
}

/// The folder Ctrl+S auto-saves to when [`screenshot_use_save_dir`] is on. An empty
/// string means "unset" — the app resolves that to the Desktop known folder at use
/// time (so we never bake an absolute path here, and it follows the user's real
/// Desktop). See `crate`'s app `screenshot::effective_save_dir`.
pub fn screenshot_save_dir() -> String {
    if store::portable() {
        return store::get_string(None, "ShotSaveDir").unwrap_or_default();
    }
    CURRENT_USER
        .open(hkcu_root())
        .and_then(|k| k.get_string("ShotSaveDir"))
        .unwrap_or_default()
}

/// Persist the chosen save folder (absolute path). Empty restores the Desktop default.
pub fn set_screenshot_save_dir(dir: &str) -> windows_registry::Result<()> {
    if store::portable() {
        return io_result(store::set_string(None, "ShotSaveDir", dir));
    }
    CURRENT_USER
        .create(hkcu_root())?
        .set_string("ShotSaveDir", dir)
}

// ---- Diagnostics --------------------------------------------------------

/// Verbose ("Debug") logging — when on, `safety::log_debug` traces are written to the
/// diagnostics log alongside the always-on errors/crashes. Off by default; the same
/// `Debug` DWORD `dev-register.ps1 -Debug` sets, now also toggleable in the Options
/// dialog so a user can capture detail for a bug report and turn it back off.
pub fn verbose_logging() -> bool {
    get_dword("Debug", 0) != 0
}

// ---- Updates ------------------------------------------------------------

/// Whether the app periodically checks for a newer release (throttled to once/day) and pops
/// a tray toast when one exists. ON by default. Three things honor it, none of them a
/// resident service: the per-user `SageThumbs2K_UpdateCheck` Scheduled Task (registered at
/// install; runs `--update-check` and exits), the same one-shot spawned opportunistically by
/// any ordinary app launch, and the opt-in screenshot helper's 6 h timer when it happens to
/// be running. Turning this off in Settings also removes the Scheduled Task.
pub fn update_auto_check() -> bool {
    get_dword("UpdateAutoCheck", 1) != 0
}

/// Persist the auto-update-check toggle.
pub fn set_update_auto_check(on: bool) -> windows_registry::Result<()> {
    set_dword("UpdateAutoCheck", on as u32)
}

// ---- Quick preview (QuickLook-style "press Space, see the file") --------
// The opt-in Space-to-preview popup. All EXE-side; the DLL never reads these.
// `PreviewEnabled` is the master switch and ALSO drives the resident daemon's
// residency (the app's `screenshot::enable::daemon_wanted` consults it), so a
// bound Quick preview keeps that shared tray daemon alive exactly like a bound
// custom hotkey does. The rest are viewer behavior prefs read by the viewer
// window. DWORD 0/1; getters default to the plan's §6 defaults.

/// Master switch for Quick preview. OFF by default (nothing hooks the keyboard
/// until the user opts in); also drives daemon residency.
pub fn preview_enabled() -> bool {
    get_dword("PreviewEnabled", 0) != 0
}
/// Persist the Quick preview master toggle.
pub fn set_preview_enabled(on: bool) -> windows_registry::Result<()> {
    set_dword("PreviewEnabled", on as u32)
}

/// Hold Space >= 750 ms then release closes the preview ("peek"). ON by default.
pub fn preview_hold_peek() -> bool {
    get_dword("PreviewHoldPeek", 1) != 0
}
/// Persist the hold-to-peek toggle.
pub fn set_preview_hold_peek(on: bool) -> windows_registry::Result<()> {
    set_dword("PreviewHoldPeek", on as u32)
}

/// Close the viewer when it loses focus (and isn't pinned). OFF by default.
pub fn preview_close_on_focus_loss() -> bool {
    get_dword("PreviewCloseOnFocusLoss", 0) != 0
}
/// Persist the close-on-focus-loss toggle.
pub fn set_preview_close_on_focus_loss(on: bool) -> windows_registry::Result<()> {
    set_dword("PreviewCloseOnFocusLoss", on as u32)
}

/// Bring the viewer to the front (foreground) when it opens — it still shows without
/// stealing focus and can be covered the moment you click another window. This is NOT
/// always-on-top; the toolbar pin button handles that. **ON by default.**
pub fn preview_open_front() -> bool {
    get_dword("PreviewOpenFront", 1) != 0
}
/// Persist the open-in-front toggle.
pub fn set_preview_open_front(on: bool) -> windows_registry::Result<()> {
    set_dword("PreviewOpenFront", on as u32)
}

/// Keep the Markdown outline (table-of-contents) sidebar OPEN. ON by default; the viewer's outline
/// toggle button persists the user's choice here (so it stays pinned open/closed across previews).
pub fn preview_toc_open() -> bool {
    get_dword("PreviewTocOpen", 1) != 0
}
/// Persist the Markdown outline-sidebar open/closed state.
pub fn set_preview_toc_open(on: bool) -> windows_registry::Result<()> {
    set_dword("PreviewTocOpen", on as u32)
}

// The three below are remembered VIEWER STATE, not configuration: the viewer writes them as you
// use it (exactly like the outline sidebar above), so a level or a size you set on one file is
// still there on the next one and on the next preview. They deliberately have no Settings
// control — "Reset all settings" clears them, and the caption double-click clears the size.

/// Quick preview playback volume, 0..=100 (default 100). The transport strip's slider writes here
/// when you let go of it, so the next clip starts at the level you chose instead of full blast.
pub fn preview_volume() -> u32 {
    get_dword("PreviewVolume", 100).min(100)
}

/// Persist the Quick preview playback volume (clamped to 0..=100).
pub fn set_preview_volume(v: u32) -> windows_registry::Result<()> {
    set_dword("PreviewVolume", v.min(100))
}

/// Whether Quick preview playback starts muted (default false) — the transport's speaker toggle.
pub fn preview_muted() -> bool {
    get_dword("PreviewMuted", 0) != 0
}

/// Persist the Quick preview mute state.
pub fn set_preview_muted(on: bool) -> windows_registry::Result<()> {
    set_dword("PreviewMuted", on as u32)
}

/// Whether Quick preview media repeats when it reaches the end (default true, which is what the
/// viewer did unconditionally before the transport gained a loop button). Off means the clip stops
/// on its last frame, which is what you want when you are checking whether a render finished.
pub fn preview_loop() -> bool {
    get_dword("PreviewLoop", 1) != 0
}

/// Persist the Quick preview loop state.
pub fn set_preview_loop(on: bool) -> windows_registry::Result<()> {
    set_dword("PreviewLoop", on as u32)
}

/// What ←/→ do while a video or track is playing in the Quick preview. Default false = SEEK, which
/// is what those keys mean in every media player. True = move to the previous/next file in the
/// folder, for someone flipping through a folder of clips rather than watching one.
///
/// Either way the transport's own ⏮/⏭ buttons always switch files, and PgUp/PgDn always do too, so
/// neither behaviour is ever unreachable.
pub fn preview_arrow_nav() -> bool {
    get_dword("PreviewArrowNav", 0) != 0
}

/// Show the page-thumbnail strip beside a multi-page PDF in the Quick preview. Default ON.
///
/// A switch rather than always-on because the strip costs a sixth of the window, and someone
/// reading a two-page letter wants the page, not a contact sheet of it. It hides itself on a
/// narrow window regardless (see `pdfview::strip_width`).
pub fn preview_pdf_strip() -> bool {
    get_dword("PreviewPdfStrip", 1) != 0
}

/// Which skin SageThumbs' OWN windows use, independent of the Windows app-colour setting.
///
/// `0` follow Windows (the default and the behaviour every version before this had), `1` light,
/// `2` dark. Requested by a user who wanted the Quick preview dark while the rest of Windows
/// stayed light, which previously meant flipping the whole OS.
///
/// This governs the app's windows only: Quick preview, Settings, Convert, the screenshot
/// editor. The Explorer context menu and the preview PANE live inside Explorer's process and
/// keep following Windows, because they are drawn into someone else's UI and disagreeing with
/// the surrounding shell would look broken rather than themed.
pub fn app_theme() -> u32 {
    get_dword("AppTheme", 0).min(2)
}

/// Persist the app skin. Out-of-range values are refused rather than clamped: the only writer
/// is a three-item combo, so anything else means a caller bug or a hand-edited registry, and
/// silently storing 7 as "dark" would hide it.
pub fn set_app_theme(mode: u32) -> windows_registry::Result<()> {
    if mode > 2 {
        return Ok(());
    }
    set_dword_tracking_default("AppTheme", mode, 0)
}

/// Persist the ←/→ meaning for video playback.
pub fn set_preview_arrow_nav(on: bool) -> windows_registry::Result<()> {
    set_dword("PreviewArrowNav", on as u32)
}

/// Quick preview playback speed in PERCENT (25..=400, default 100). Percent rather than a float
/// because the settings store is DWORD-only, and the transport only ever offers fixed steps.
pub fn preview_speed() -> u32 {
    get_dword("PreviewSpeed", 100).clamp(25, 400)
}

/// Persist the Quick preview playback speed (percent, clamped to the offered range).
pub fn set_preview_speed(pct: u32) -> windows_registry::Result<()> {
    set_dword("PreviewSpeed", pct.clamp(25, 400))
}

/// The viewer size the user last dragged the window out to, as a CLIENT size in **96-dpi logical
/// px** — `None` until they resize one, which is when the viewer goes back to sizing every file to
/// its own content. Logical rather than device px so a size chosen on a 150% display reopens the
/// same apparent size on a 100% one. Two DWORDs; either being absent or 0 means "not remembered".
pub fn preview_window_size() -> Option<(i32, i32)> {
    let w = get_dword("PreviewWinW", 0).min(i32::MAX as u32) as i32;
    let h = get_dword("PreviewWinH", 0).min(i32::MAX as u32) as i32;
    (w > 0 && h > 0).then_some((w, h))
}

/// Persist (or, with `None`, forget) the remembered viewer window size. Forgetting restores the
/// per-content sizing — that is what a double-click on the viewer's caption does.
pub fn set_preview_window_size(size: Option<(i32, i32)>) -> windows_registry::Result<()> {
    let (w, h) = size.unwrap_or((0, 0));
    set_dword("PreviewWinW", w.max(0) as u32)?;
    set_dword("PreviewWinH", h.max(0) as u32)
}

/// Download web-hosted images referenced by a previewed Markdown file (badges, hotlinked art).
/// **OFF by default** — fetching an image URL from a previewed document is an outbound request
/// an attacker-authored README fully controls (classic tracking-pixel shape), so it is strictly
/// opt-in. When off, remote images render as labeled alt-text chips. HTTPS-only when on.
pub fn preview_md_remote_img() -> bool {
    get_dword("PreviewMdRemoteImg", 0) != 0
}
/// Persist the remote-markdown-images toggle.
pub fn set_preview_md_remote_img(on: bool) -> windows_registry::Result<()> {
    set_dword("PreviewMdRemoteImg", on as u32)
}

/// Render local `.html`/`.htm` files as live web pages (WebView2), instead of showing their
/// source as text. **ON by default** — the viewer locks the page down (scripts off + non-`file://`
/// requests blocked), so a rendered local page can neither run scripts nor reach the network.
pub fn preview_html() -> bool {
    get_dword("PreviewHtml", 1) != 0
}
/// Persist the local-HTML-render toggle.
pub fn set_preview_html(on: bool) -> windows_registry::Result<()> {
    set_dword("PreviewHtml", on as u32)
}

/// LIVE-load the target of a `.url`/`.webloc` shortcut in an ephemeral WebView2 (no cookie/session
/// reuse) instead of showing the parsed target URL as text. **OFF by default** — pressing Space on
/// a `.url` would otherwise fire a silent outbound request to an attacker-controllable domain
/// (`.url` is a known phishing vector), so live loading is strictly opt-in.
pub fn preview_url_live() -> bool {
    get_dword("PreviewUrlLive", 0) != 0
}
/// Persist the live-`.url` toggle.
pub fn set_preview_url_live(on: bool) -> windows_registry::Result<()> {
    set_dword("PreviewUrlLive", on as u32)
}

/// Preview text/code files (Phase 3 — syntax-highlighted via the viewer's WebView2
/// host). ON by default; only consulted once Phase 3 ships.
pub fn preview_text() -> bool {
    get_dword("PreviewText", 1) != 0
}
/// Persist the text/code preview toggle.
pub fn set_preview_text(on: bool) -> windows_registry::Result<()> {
    set_dword("PreviewText", on as u32)
}

/// Render Markdown like GitHub (Phase 3 — via WebView2). ON by default; only
/// consulted once Phase 3 ships.
pub fn preview_markdown() -> bool {
    get_dword("PreviewMarkdown", 1) != 0
}
/// Persist the Markdown preview toggle.
pub fn set_preview_markdown(on: bool) -> windows_registry::Result<()> {
    set_dword("PreviewMarkdown", on as u32)
}

/// Quick preview's per-extension blocklist, exactly as the user typed it into the Settings
/// box (comma/semicolon separated, e.g. `insv, .mov ; HEIC`). Stored and returned UNPARSED —
/// parsing happens only on READ ([`preview_blocked`]) — so the edit box round-trips whatever
/// the user last typed instead of silently rewriting it under them.
///
/// Deliberately a SEPARATE list from the File-types page's `format_enabled(ext)`: that switch
/// answers "does this format get a THUMBNAIL", and reusing it here would silently tie "no
/// thumbnails for X" to "no Quick preview for X" too, which is a different promise to the
/// user (owner decision, 2026-09-08 QuickLook-parity review — do not re-litigate). Empty by
/// default: unlike QuickLook's shipped `.insv` block (added after a crash report), we have no
/// crash to justify a default entry, and the Quick preview already runs out of process, so a
/// crashing format takes down only its own window.
pub fn preview_blocked_exts_raw() -> String {
    get_string_opt("PreviewBlockedExts").unwrap_or_default()
}
/// Persist the Quick preview extension blocklist verbatim (as typed).
pub fn set_preview_blocked_exts(raw: &str) -> windows_registry::Result<()> {
    set_string("PreviewBlockedExts", raw)
}

/// Parse a comma/semicolon-separated extension list: case-insensitive, tolerant of a leading
/// dot and of surrounding spaces, empty entries dropped. Pulled out as a pure function — this
/// is the actually-testable part of the feature, with no registry/ini involved.
fn parse_blocked_exts(raw: &str) -> Vec<String> {
    raw.split([',', ';'])
        .map(|s| s.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Whether the Quick preview should refuse `ext` outright, before any decoder even sees the
/// file — the per-extension blocklist gate ([`preview_blocked_exts_raw`]). `ext` may carry a
/// leading dot or mixed case; both are normalized before comparing.
pub fn preview_blocked(ext: &str) -> bool {
    let ext = ext.trim_start_matches('.').to_ascii_lowercase();
    parse_blocked_exts(&preview_blocked_exts_raw()).contains(&ext)
}

#[cfg(test)]
mod blocked_ext_tests {
    use super::parse_blocked_exts;

    #[test]
    fn parses_case_insensitively_and_trims_dots_and_spaces() {
        assert_eq!(
            parse_blocked_exts(" .INSV, mov ;.Heic"),
            vec!["insv", "mov", "heic"]
        );
    }

    #[test]
    fn drops_empty_entries_from_either_separator_and_mixed_use() {
        assert_eq!(parse_blocked_exts(",, ; ,mp4,,"), vec!["mp4"]);
        assert_eq!(
            parse_blocked_exts("insv;mov,heic"),
            vec!["insv", "mov", "heic"]
        );
        assert!(parse_blocked_exts("").is_empty());
        assert!(parse_blocked_exts("   ").is_empty());
    }
}
