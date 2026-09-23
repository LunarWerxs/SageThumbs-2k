//! Thumbnail-generation, menu and container settings the DLL reads on the
//! `GetThumbnail`/context-menu hot paths — the shell-facing half of the settings module.
//! The EXE-only viewer/app preferences live in `super::app_prefs`; the storage backend
//! (registry vs portable ini) lives in `super::store`.

use super::*;
mod badges;
use badges::*;
mod menu;
pub use badges::{
    badge_size, corner_mark, format_badge, format_badge_icon, set_badge_size, set_corner_mark,
    set_format_badge_icon, BadgeSize, BadgeStyle, CornerMark,
};
#[cfg(test)]
use menu::*;
pub use menu::{
    menu_all_file_types, menu_enabled, menu_gate, menu_item_shown, menu_order, menu_preview,
    menu_quick_verbs, menu_visibility, set_menu_item_shown, set_menu_order, MenuGate,
    MenuVisibility,
};

// Defaults + bounds, matching the legacy SageThumbs.h constants.
/// Skip files bigger than this (MB). Legacy 100 -> 256 (2026-08-11) -> 4096 (2026-08-15), and
/// both raises fixed the same class of fault: the user-facing PREFERENCE, not the safety
/// limit, was what refused a file nothing else objected to. At 100, `effective_input_cap`
/// takes `min(MaxSize, 256 MiB)`, so a 150 MB TIFF was refused by the knob — and since we own
/// that extension's thumbnail slot, it then got no thumbnail from anyone.
///
/// **256 was still wrong, more subtly: it EQUALLED the hard ceiling, which silently closed a
/// rescue window.** `streamsrc::stream_source` refuses a file when `size > min(MaxSize, hard
/// cap)` and then offers the oversized WIC rescue when `size <= MaxSize` — the two conditions
/// describing "our buffering ceiling refused it" and "the user did not ask us to skip it".
/// Make MaxSize equal the hard cap and that pair reads `size > 256 MiB && size <= 256 MiB`,
/// which is false for every file that has ever existed. The rescue was unreachable at the
/// default setting, and its own tests missed it by driving the cascade with MaxSize unlimited.
/// Sitting the default clear of the ceiling is what re-opens it; the gate's logic was right.
///
/// This LOOSENS no buffering: 256 MiB remains the real wall for anything we read into memory
/// (`effective_input_cap` still clamps). What it opens is the path that never buffers — WIC
/// reading the file itself and scaling during decode, measured at 2.1 s and no measurable
/// memory for a 340 MP PNG. 4096 keeps a meaningful "don't even try" for the genuinely absurd
/// while covering every real image file; the honest cost ceiling is pixels, not bytes, and
/// that one is `decode::limits::MAX_SCALED_SOURCE_PIXELS`.
///
/// It also matters far less than it looks: the cap gates only the "buffer the whole file"
/// tail of `streamsrc::stream_source`. Video, audio cover art, OpenEXR, archives, RAW and the
/// baked-preview containers (PSD/PSB/.blend/DWG) are all served by targeted or streaming
/// reads that never consult it — a 4 GB movie thumbnails regardless of this number.
pub const DEFAULT_MAX_FILE_MB: u32 = 4096; // FILE_MAX_SIZE
                                           // Raised from the legacy 256/512 (2026-06-22): on Hi-DPI / 4K / large ("jumbo")
                                           // icon views the shell requests thumbnails well past 512px. Capping below the
                                           // requested size handed back an undersized bitmap the shell could neither display
                                           // crisply NOR durably cache — so it re-extracted on every refresh (an expensive
                                           // 4K video-frame decode each time). We honor the request up to 1024 now; small
                                           // views are unaffected (the provider still does `cx.min(max_thumb)`).
pub const DEFAULT_THUMB_SIZE: u32 = 1024; // THUMB_STORE_SIZE (was 256)
pub const THUMB_MIN: u32 = 32; // THUMB_MIN_SIZE
/// Ceiling the user may raise the thumbnail edge to. Raised 1024 -> 2560 (2026-08-14, issue
/// #26.5). The old 1024 was historical, not technical: Windows itself keeps thumbnail cache
/// buckets above it (`thumbcache_1280/1920/2560.db`), and the decoders' own guard is
/// `decode::limits::MAX_DIM` at 16384, so nothing in the pipeline needed 1024 specifically.
/// The request came from cover-art libraries viewed on a 4K/85" screen, where a 1024 px tile
/// cannot resolve the text printed on a movie poster.
///
/// The DEFAULT deliberately stays 1024. Above it, every raised edge costs memory, decode time,
/// cache-file growth and Explorer responsiveness on exactly the large collections that want it
/// — so this is a ceiling a user opts into, not one everybody pays for. `max_thumb_size()` is
/// also only ever an upper bound on what the shell asks for (`cx.min(max_thumb)`), so raising
/// it changes nothing until Explorer genuinely requests a bigger tile.
pub const THUMB_MAX: u32 = 2560; // THUMB_MAX_SIZE (was 512, then 1024)
pub const EMBEDDED_MAX_REQUEST: u32 = 96; // THUMB_EMBEDDED_MIN_SIZE
pub const DEFAULT_JPEG: u32 = 90; // JPEG_DEFAULT
pub const DEFAULT_PNG: u32 = 9; // PNG_DEFAULT
/// Default classic-menu preview placement: `1` = at the top of the SageThumbs
/// submenu (how the original SageThumbs showed its preview). The SINGLE source of
/// truth for both the first-run getter default ([`menu_preview`]) and the Options
/// dialog's "Defaults" button, so the two can't disagree (they used to: the getter
/// defaulted to 1 while "Defaults" selected 2).
pub const DEFAULT_MENU_PREVIEW: u32 = 1;

// ---- Ebook/comic archive cover-selection (CBZ/CB7/CBR) -------------------
// Ports DarkThumbs' CBXManager toggles. Defaults: natural-sort ON, prefer a
// "cover"-named image ON, skip scanlation filler (credits/logos) ON since 2026-09-15
// (Michael: it is what anyone with such an archive wants, and it touches nothing else;
// it left the welcome window's page 2 the same day and lives only in Settings).

/// Pick archive pages in natural sort order (else first in archive order).
pub fn container_sort() -> bool {
    get_dword("ContainerSort", 1) != 0
}
/// Prefer an image whose name contains "cover".
pub fn container_prefer_cover() -> bool {
    get_dword("ContainerPreferCover", 1) != 0
}
/// Skip scanlation filler pages (credits/logo/recruit/invite).
pub fn container_skip_scanlation() -> bool {
    get_dword("ContainerSkipScanlation", 1) != 0
}

/// Contact-sheet thumbnails for GENERIC archives (.zip/.rar/.7z): compose up to 4
/// images into one tile (also what tells a zip of photos apart from a lone photo
/// in a grid view). Off = classic single first-image cover, CBXShell-style.
/// Comics/ebooks (cbz/cb7/cbr/epub/…) always keep their single cover regardless.
pub fn archive_collage() -> bool {
    get_dword("ArchiveCollage", 1) != 0
}

// ---- Thumbnail-generation settings (read by the provider/decoder) -------

/// Master switch for the thumbnail provider.
pub fn thumbnails_enabled() -> bool {
    get_dword("EnableThumbs", 1) != 0
}

/// Files larger than this are not thumbnailed. `0` removes the user limit, but
/// the provider still caps the in-memory read at a hard ceiling
/// (`decode::limits::MAX_INPUT_BYTES`, currently 256 MiB), so "unlimited"
/// effectively means "up to that ceiling".
pub fn max_file_size_bytes() -> u64 {
    let mb = get_dword("MaxSize", DEFAULT_MAX_FILE_MB) as u64;
    if mb == 0 {
        u64::MAX
    } else {
        mb * 1024 * 1024
    }
}

/// Reduce a stored Width/Height pair to a single thumbnail edge in the legacy
/// [THUMB_MIN, THUMB_MAX] range: take the larger of the two so either knob
/// raises the ceiling, then clamp. Pure so it can be tested without HKCU.
pub(crate) fn clamp_thumb_size(w: u32, h: u32) -> u32 {
    w.max(h).clamp(THUMB_MIN, THUMB_MAX)
}

/// The max thumbnail edge to generate, clamped to the [`THUMB_MIN`, `THUMB_MAX`] range.
/// The original stored Width/Height separately; we cap the square request box
/// at the larger of the two so either knob raises the ceiling.
pub fn max_thumb_size() -> u32 {
    let w = get_dword("Width", DEFAULT_THUMB_SIZE);
    let h = get_dword("Height", DEFAULT_THUMB_SIZE);
    clamp_thumb_size(w, h)
}

/// Prefer the image's embedded (EXIF) thumbnail when the request is small (<= 96px).
/// ON by default: for a small tile of a 12-50 MP photo, grabbing the camera-baked ~160px
/// thumbnail is sub-millisecond vs a full multi-megapixel decode + downscale, and at that
/// size it's visually identical. Falls back to a full decode when no embedded thumb exists.
/// Users who want byte-exact small tiles can turn it off in Settings.
pub fn use_embedded() -> bool {
    get_dword("UseEmbedded", 1) != 0
}

/// A snapshot of the four settings every `GetThumbnail` consults, read with a
/// SINGLE HKCU key open instead of one open per getter. The provider used to call
/// [`thumbnails_enabled`], [`max_file_size_bytes`], [`max_thumb_size`] (which opens
/// twice) and [`use_embedded`] separately — ~5 `RegOpenKeyEx`es on the hot path,
/// per file, in a folder of thousands of thumbnails. Pulling them all from one open
/// key collapses that to one open. Semantics are UNCHANGED: it's still a fresh read
/// per `GetThumbnail` (a fresh provider instance per request — see the module docs),
/// so a Settings change still takes effect immediately for the next thumbnail; we
/// only stop re-opening the same key five times within a single request.
pub struct ThumbSettings {
    /// `EnableThumbs` — master on/off for the provider.
    pub enabled: bool,
    /// `MaxSize` resolved to bytes (`u64::MAX` when the user limit is 0/unlimited).
    pub max_file_bytes: u64,
    /// `Width`/`Height` reduced + clamped to the [32, 2560] edge.
    pub max_thumb: u32,
    /// `UseEmbedded` — prefer the embedded thumbnail for small requests.
    pub use_embedded: bool,
    /// `FormatBadge` — stamp the file's format in the thumbnail's corner. OFF by default:
    /// it alters the picture the user asked to see, so it is opt-in decoration.
    pub format_badge: bool,
    /// `FormatBadgeStyle` — how that badge is drawn once it IS on. Only read when
    /// `format_badge` is true.
    pub badge_style: BadgeStyle,
    /// `BadgeSize` - how big that badge is drawn. Only read when `format_badge` is true.
    pub badge_size: crate::settings::BadgeSize,
    /// `ThumbChecker` — burn a transparency checkerboard into the thumbnail behind
    /// see-through pixels. OFF by default; correct alpha is the better default, this is for
    /// people who want the original SageThumbs' look back.
    pub thumb_checker: bool,
    /// `VideoCoverArt` — for a video that carries embedded poster art, show the poster
    /// instead of a frame from the film. OFF by default: a real frame is the more useful
    /// tile for the videos most people have (phone clips, screen recordings, camera
    /// footage), and a poster there is often a generic stand-in. For a ripped-film library
    /// the reverse is true, which is what this is for. Cover art is used as a FALLBACK
    /// whatever this says, since a file whose codec Windows lacks has no frame to show.
    pub prefer_cover_art: bool,
    /// `VideoOffset` resolved to the [0.0, 0.95] fraction every seek site wants — see
    /// [`video_offset_frac`].
    pub video_offset_frac: f64,
    /// `ArchiveCollage` — see [`archive_collage`]. The raw stored DWORD rather than a bool: the
    /// consuming container code (P06b) reads it as a count/strength knob, not a pure toggle.
    pub archive_collage: u32,
    /// `ContainerPreferCover` — see [`container_prefer_cover`].
    pub container_prefer_cover: bool,
    /// `ContainerSort` — see [`container_sort`].
    pub container_sort: bool,
    /// `ContainerSkipScanlation` — see [`container_skip_scanlation`].
    pub container_skip_scanlation: bool,
}

/// A getter over ONE snapshot of whichever backing store is live: the portable ini root
/// section read once into a map, or a single read-only HKCU key open. Collapsing N opens
/// into one is the whole point of [`thumb_settings`] and [`menu_gate`], which each read a
/// dozen values out of it. The getter answers `None` for an absent value, so a caller that
/// has to tell ABSENT from 0 (e.g. `CornerMark`) can, while `unwrap_or` gives the rest their
/// defaults.
fn snapshot_u32_getter() -> impl Fn(&str) -> Option<u32> {
    let ini: Option<std::collections::HashMap<String, String>> =
        store::portable().then(|| store::section_values(None).into_iter().collect());
    let key = match ini {
        Some(_) => None,
        None => CURRENT_USER.open(hkcu_root()).ok(),
    };
    move |name: &str| {
        if let Some(ini) = ini.as_ref() {
            return ini.get(name).and_then(|v| v.parse().ok());
        }
        key.as_ref().and_then(|k| k.get_u32(name).ok())
    }
}

/// Read the per-`GetThumbnail` settings in one HKCU key open. Missing values fall
/// back to the same defaults the individual getters use, so the result is identical
/// to calling them one by one — just without the repeated opens.
pub fn thumb_settings() -> ThumbSettings {
    // ONE snapshot of whichever backing store is live, then every value is read out of it.
    // Collapsing N opens into one is the entire point of this function, so the portable
    // path takes the same shape: one section snapshot, not one file read per value.
    let gopt = snapshot_u32_getter();
    // `gopt` is the primitive; `g` is it with a default applied. Both are needed because
    // `CornerMark` has to tell ABSENT from 0 to know whether to fall back to the legacy pair,
    // and doing that with a sentinel default would make 0 (the real "system icon" value)
    // indistinguishable from "never set".
    let g = |name: &str, default: u32| gopt(name).unwrap_or(default);
    // Same derivation as `corner_mark()`, off this one snapshot rather than re-opening the key.
    // It has to agree with that function exactly, which is what
    // `thumb_settings_agrees_with_the_individual_accessors` asserts.
    let mark = match gopt("CornerMark") {
        Some(v) => crate::settings::CornerMark::from_dword(v),
        None => crate::settings::CornerMark::from_legacy(
            g("FormatBadge", 0) != 0,
            g("HideTypeOverlay", 0) != 0,
        ),
    };
    let mb = g("MaxSize", DEFAULT_MAX_FILE_MB) as u64;
    ThumbSettings {
        enabled: g("EnableThumbs", 1) != 0,
        max_file_bytes: if mb == 0 { u64::MAX } else { mb * 1024 * 1024 },
        max_thumb: clamp_thumb_size(
            g("Width", DEFAULT_THUMB_SIZE),
            g("Height", DEFAULT_THUMB_SIZE),
        ),
        use_embedded: g("UseEmbedded", 1) != 0,
        format_badge: mark == crate::settings::CornerMark::Badge,
        badge_style: BadgeStyle::from_dword(g("FormatBadgeStyle", DEFAULT_BADGE_STYLE)),
        badge_size: crate::settings::BadgeSize::from_dword(g("BadgeSize", DEFAULT_BADGE_SIZE)),
        thumb_checker: g("ThumbChecker", 0) != 0,
        prefer_cover_art: g("VideoCoverArt", 0) != 0,
        video_offset_frac: f64::from(clamp_video_offset_pct(g(
            "VideoOffset",
            DEFAULT_VIDEO_OFFSET_PCT,
        ))) / 100.0,
        archive_collage: g("ArchiveCollage", 1),
        container_prefer_cover: g("ContainerPreferCover", 1) != 0,
        container_sort: g("ContainerSort", 1) != 0,
        container_skip_scanlation: g("ContainerSkipScanlation", 1) != 0,
    }
}

/// `ThumbChecker` — burn the transparency checkerboard into Explorer thumbnails. Default OFF
/// (the shell composites real alpha over the folder background, which is normally what you
/// want); see [`crate::checkerpx`] for why this is a separate switch from `PreviewChecker`.
pub fn thumb_checker() -> bool {
    get_dword("ThumbChecker", 0) != 0
}

pub fn set_thumb_checker(on: bool) -> windows_registry::Result<()> {
    set_dword("ThumbChecker", u32::from(on))
}

/// `VideoCoverArt` — prefer a video's embedded poster over a frame from the film itself.
/// Default OFF: see [`ThumbSettings::prefer_cover_art`] for why a frame is the better
/// default and a poster the better option.
pub fn prefer_cover_art() -> bool {
    get_dword("VideoCoverArt", 0) != 0
}

pub fn set_prefer_cover_art(on: bool) -> windows_registry::Result<()> {
    set_dword("VideoCoverArt", u32::from(on))
}

/// `VideoOffset` — how far INTO a video the thumbnail frame is taken from, as a percentage of
/// its running time. 30 % has always been the hard-coded mark; this makes it a setting.
///
/// The default is unchanged, because 30 % is a good answer for the videos most people have.
/// It is a bad answer for a specific and common library: films and TV rips that open on a
/// black distributor card, a fade-in, or a title sequence over black. Those thumbnail as a
/// black rectangle, which is indistinguishable from "SageThumbs failed" (issue #26.4).
pub const DEFAULT_VIDEO_OFFSET_PCT: u32 = 30;
/// Upper bound. Not 100: seeking to the very end lands on credits, a fade-out, or past the
/// last keyframe, so the tile would be black for the opposite reason.
pub const VIDEO_OFFSET_PCT_MAX: u32 = 95;

/// Clamp a stored percentage into the usable range. Pure, so the range is testable without
/// touching HKCU. 0 is allowed and means "the first frame".
pub(crate) fn clamp_video_offset_pct(pct: u32) -> u32 {
    pct.min(VIDEO_OFFSET_PCT_MAX)
}

pub fn video_offset_pct() -> u32 {
    clamp_video_offset_pct(get_dword("VideoOffset", DEFAULT_VIDEO_OFFSET_PCT))
}

pub fn set_video_offset_pct(pct: u32) -> windows_registry::Result<()> {
    set_dword_tracking_default(
        "VideoOffset",
        clamp_video_offset_pct(pct),
        DEFAULT_VIDEO_OFFSET_PCT,
    )
}

/// The same value as the fraction every seek site actually wants.
///
/// One conversion in one place: the seek fraction is threaded through four separate call
/// paths (`video::frame_from_path`, `frame_from_bytes_repr`, `mp4::keyframe_mini_mp4`,
/// `mkv::keyframe_mini_mkv` and `video::frame_from_block_stream`), and they must agree or the
/// same file thumbnails differently in Explorer, the preview pane and the CLI.
pub fn video_offset_frac() -> f64 {
    f64::from(video_offset_pct()) / 100.0
}

/// Whether to suppress Explorer's own file-type icon on the thumbnails of the formats we hook
/// — derived from [`CornerMark`], because it IS half of that one decision.
///
/// True for both non-default values: our badge needs the corner to itself, and "nothing" means
/// nothing. It stays FALSE by default, which matters beyond tidiness: applying it writes into
/// other programs' ProgID keys (see [`crate::typeoverlay`]), and that should never happen
/// without being asked for.
pub fn hide_type_overlay() -> bool {
    corner_mark() != CornerMark::SystemIcon
}

/// `FolderPrebuildVerb` — the folder right-click entry that pre-builds thumbnails. Default ON,
/// unlike [`hide_type_overlay`]: it only creates keys of our own under `HKCU`, changes nothing
/// that already exists, and adds no code to Explorer's process (see [`crate::foldermenu`]).
/// The product already puts a right-click menu on files, so a folder entry is in character.
pub fn folder_prebuild_verb() -> bool {
    get_dword("FolderPrebuildVerb", 1) != 0
}

pub fn set_folder_prebuild_verb(on: bool) -> windows_registry::Result<()> {
    set_dword("FolderPrebuildVerb", u32::from(on))
}

// ---- Convert-verb quality settings --------------------------------------

/// Clamp a stored JPEG quality DWORD into the 1..=100 byte range. Pure so it
/// can be tested without HKCU. `0` is refused rather than passed through — a saved quality of
/// 0 is not "as small as possible", it silently produces a degenerate/near-blank JPEG (item
/// 104), so the floor matches the lower bound every other quality knob in this module already
/// uses (`cv_jpeg_quality`, `cv_webp_quality`, `cv_magick_quality` are all `.clamp(1, 100)`).
pub(crate) fn clamp_quality(q: u32) -> u8 {
    q.clamp(1, 100) as u8
}

/// Clamp a stored PNG compression DWORD into the legacy 0..=9 zlib range. Pure
/// so it can be tested without HKCU.
pub(crate) fn clamp_png(l: u32) -> u32 {
    l.min(9)
}

/// "Convert to JPG" quality, 1–100.
pub fn jpeg_quality() -> u8 {
    clamp_quality(get_dword("JPEG", DEFAULT_JPEG))
}

/// "Convert to PNG" compression level, 0–9 (legacy zlib scale).
pub fn png_level() -> u32 {
    clamp_png(get_dword("PNG", DEFAULT_PNG))
}

// ---- Menu setting -------------------------------------------------------

// NOTE: the old `modern_menu_active()` (HKLM `ModernMenuActive`) was REMOVED 2026-07-21.
// It gated whether the classic menu emitted its quick-verb copies, on the false premise
// that Windows bridges the packaged (modern-compact-menu) verbs into the legacy "Show
// more options" menu. It doesn't — packaged verbs live only in the compact flyout — so the
// gate just hid the quick verbs on every classic-menu-default machine (see contextmenu.rs).
// The installer still writes the now-inert `ModernMenuActive` key; nothing reads it.

/// Draw a subtle checkerboard behind the menu preview's transparent areas, so a
/// transparent (or white-on-transparent) image doesn't vanish into the flat menu
/// background. On by default.
pub fn preview_checker() -> bool {
    get_dword("PreviewChecker", 1) != 0
}

// ---- Per-extension enable (read by registration) ------------------------

/// Whether a given extension (no dot, lowercase) is hooked. Enabled unless an
/// explicit `0` is stored under `…\SageThumbs2K\<ext>\Enabled`.
///
/// SEMANTICS NOTE: although this flag lives in HKCU, it is read at (elevated)
/// (re-)registration time to drive MACHINE-WIDE HKCR registration, so toggling
/// a format here enables/disables that format's thumbnails for ALL users — it
/// is an "all users" switch, not a per-user one (there is no per-user gate).
pub fn format_enabled(ext: &str) -> bool {
    if store::portable() {
        return store::get_u32(Some(ext), "Enabled")
            .map(|v| v != 0)
            .unwrap_or(true);
    }
    CURRENT_USER
        .open(format!(r"{}\{ext}", hkcu_root()))
        .and_then(|k| k.get_u32("Enabled"))
        .map(|v| v != 0)
        .unwrap_or(true)
}

/// A one-shot snapshot of every per-extension `Enabled` flag, for a caller that's about to
/// call the equivalent of [`format_enabled`] once per format in a sweep over `FORMATS`
/// (`register.rs`'s HKCR (re)registration, `typeoverlay.rs`, `doctor.rs`'s per-format audit —
/// each on the order of ~330 lookups). In portable mode, [`format_enabled`] goes through
/// `store::get_u32`, which — per `store::load`'s own doc comment — re-reads and re-parses the
/// WHOLE ini file from disk on every single call; unlike [`menu_visibility`]'s "read the tree
/// once per menu build" snapshot, nothing collapsed that for the per-extension flags. This
/// does: one [`store::full_doc`] parse up front, then every [`FormatEnabledSnapshot::enabled`]
/// lookup is an in-memory map hit. The registry arm doesn't get the same win (each extension is
/// its own HKCU subkey, so there's no single tree to snapshot) but stays correct by falling
/// back to [`format_enabled`] per lookup — the whole benefit here is portable-only.
pub struct FormatEnabledSnapshot(FormatEnabledSource);

enum FormatEnabledSource {
    Registry,
    Portable(std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>),
}

/// Take the snapshot. Call this once before a sweep and reuse it for every extension, instead
/// of calling [`format_enabled`] per extension.
pub fn format_enabled_snapshot() -> FormatEnabledSnapshot {
    FormatEnabledSnapshot(if store::portable() {
        FormatEnabledSource::Portable(store::full_doc())
    } else {
        FormatEnabledSource::Registry
    })
}

impl FormatEnabledSnapshot {
    /// Same semantics as [`format_enabled`] (default true unless an explicit `0` is stored),
    /// reusing the one-shot parse in portable mode.
    pub fn enabled(&self, ext: &str) -> bool {
        match &self.0 {
            FormatEnabledSource::Registry => format_enabled(ext),
            FormatEnabledSource::Portable(doc) => doc
                .get(ext)
                .and_then(|v| v.get("Enabled"))
                .and_then(|v| v.parse::<u32>().ok())
                .map(|v| v != 0)
                .unwrap_or(true),
        }
    }
}

/// Persist a per-extension enable flag (used by the Options dialog).
pub fn set_format_enabled(ext: &str, enabled: bool) -> windows_registry::Result<()> {
    if store::portable() {
        return io_result(store::set_u32(Some(ext), "Enabled", enabled as u32));
    }
    CURRENT_USER
        .create(format!(r"{}\{ext}", hkcu_root()))?
        .set_u32("Enabled", enabled as u32)
}

// ---- Per-menu-item visibility (the "Displayed menu items" checklist) -----

#[cfg(test)]
mod tests;
