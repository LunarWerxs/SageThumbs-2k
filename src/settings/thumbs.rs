//! Thumbnail-generation, menu and container settings the DLL reads on the
//! `GetThumbnail`/context-menu hot paths — the shell-facing half of the settings module.
//! The EXE-only viewer/app preferences live in `super::app_prefs`; the storage backend
//! (registry vs portable ini) lives in `super::store`.

use super::*;

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
// "cover"-named image ON, skip scanlation filler (credits/logos) OFF.

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
    get_dword("ContainerSkipScanlation", 0) != 0
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
    /// `Width`/`Height` reduced + clamped to the [32, 1024] edge.
    pub max_thumb: u32,
    /// `UseEmbedded` — prefer the embedded thumbnail for small requests.
    pub use_embedded: bool,
    /// `FormatBadge` — stamp the file's format in the thumbnail's corner. OFF by default:
    /// it alters the picture the user asked to see, so it is opt-in decoration.
    pub format_badge: bool,
    /// `FormatBadgeStyle` — how that badge is drawn once it IS on. Only read when
    /// `format_badge` is true.
    pub badge_style: crate::badge::BadgeStyle,
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

/// Read the per-`GetThumbnail` settings in one HKCU key open. Missing values fall
/// back to the same defaults the individual getters use, so the result is identical
/// to calling them one by one — just without the repeated opens.
pub fn thumb_settings() -> ThumbSettings {
    // ONE snapshot of whichever backing store is live, then every value is read out of it.
    // Collapsing N opens into one is the entire point of this function, so the portable
    // path takes the same shape: one section snapshot, not one file read per value.
    let ini: Option<std::collections::HashMap<String, String>> =
        store::portable().then(|| store::section_values(None).into_iter().collect());
    let key = match ini {
        Some(_) => None,
        None => CURRENT_USER.open(hkcu_root()).ok(),
    };
    // `gopt` is the primitive; `g` is it with a default applied. Both are needed because
    // `CornerMark` has to tell ABSENT from 0 to know whether to fall back to the legacy pair,
    // and doing that with a sentinel default would make 0 (the real "system icon" value)
    // indistinguishable from "never set".
    let gopt = |name: &str| -> Option<u32> {
        if let Some(ini) = ini.as_ref() {
            return ini.get(name).and_then(|v| v.parse().ok());
        }
        key.as_ref().and_then(|k| k.get_u32(name).ok())
    };
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
        badge_style: crate::badge::BadgeStyle::from_dword(g(
            "FormatBadgeStyle",
            DEFAULT_BADGE_STYLE,
        )),
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
        container_skip_scanlation: g("ContainerSkipScanlation", 0) != 0,
    }
}

/// `FormatBadgeStyle` default: the category-coloured icon. The badge itself is opt-in, so
/// anyone who turns it on has asked to be able to tell formats apart at a glance — and a
/// colour does that faster than three letters. `0` selects the older plain text chip.
const DEFAULT_BADGE_STYLE: u32 = 1;

/// `BadgeSize` default: [`BadgeSize::Small`], which is byte-for-byte the badge every build
/// before this setting drew. Growing the mark for everyone would change a picture people
/// already chose to have stamped, so the bigger steps are opt-in.
const DEFAULT_BADGE_SIZE: u32 = 0;

/// How big the format mark is drawn, as a share of the tile.
///
/// The badge scales off the tile's SHORT EDGE divided by a constant, so a step here is a
/// constant fraction of the picture at every thumbnail size rather than a pixel count that
/// would be invisible on a 512 px tile and cover a 96 px one. Reported 2026-09-10: at the
/// original ~18% the three letters are too small to read at a glance on a normal-DPI
/// Explorer window, and there was no way to ask for a bigger one.
///
/// Which mark you get is [`CornerMark`]; this is only its size, and it is read only when
/// that says [`CornerMark::Badge`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum BadgeSize {
    /// The original mark: about 18% of the tile's width for a 3-character label.
    #[default]
    Small,
    /// Roughly a third larger.
    Medium,
    /// Roughly twice the original, for reading the format across a room or at a glance.
    Large,
}

impl BadgeSize {
    /// `BadgeSize`: 0 = small, 1 = medium, 2 = large. An unknown value falls to the default
    /// rather than to the largest - a value we cannot read is not a request for a bigger mark.
    pub const fn from_dword(v: u32) -> Self {
        match v {
            1 => Self::Medium,
            2 => Self::Large,
            _ => Self::Small,
        }
    }

    /// `const` so a table can name a variant's stored value directly - see the settings
    /// dialog's combo, where the enum IS the option order (same contract as
    /// [`CornerMark::as_dword`]).
    pub const fn as_dword(self) -> u32 {
        match self {
            Self::Small => 0,
            Self::Medium => 1,
            Self::Large => 2,
        }
    }

    /// The divisor `crate::badge` scales the glyph cells by: `short_edge / divisor`, clamped.
    /// SMALLER divides less often, so a smaller number is a BIGGER badge. 110 is the shipped
    /// value and must not move - see `badge::badge_geometry` for why 48 was wrong.
    pub const fn divisor(self) -> u32 {
        match self {
            Self::Small => 110,
            Self::Medium => 80,
            Self::Large => 55,
        }
    }
}

/// `BadgeSize` - see [`BadgeSize`]. Only meaningful while [`corner_mark`] is
/// [`CornerMark::Badge`]; stored regardless, so switching the corner back to our mark
/// restores the size the user picked.
pub fn badge_size() -> BadgeSize {
    BadgeSize::from_dword(get_dword("BadgeSize", DEFAULT_BADGE_SIZE))
}

pub fn set_badge_size(s: BadgeSize) -> windows_registry::Result<()> {
    set_dword("BadgeSize", s.as_dword())
}

/// What ends up in the BOTTOM-RIGHT CORNER of a thumbnail we produced — the one place where
/// two different things want to draw, and only one of them can win.
///
/// # Why this is one setting and not two checkboxes
///
/// It used to be two independent booleans, `FormatBadge` (draw our mark) and `HideTypeOverlay`
/// (stop Explorer drawing its own file-type icon), and they address the SAME 20 px of tile.
/// Ticking only the first produced the combination nobody wants: Explorer stamps the associated
/// program's icon straight on top of our badge, in that exact corner (see [`crate::badge`] and
/// [`crate::typeoverlay`], whose doc comments each name the other). The user had to find a
/// second, differently-worded checkbox on the same page to get a clean result, and the pairing
/// was never stated anywhere. One three-way choice cannot express the broken combination at all.
///
/// Which mark you get, not whether a decoration is "on": every value here is a real answer.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum CornerMark {
    /// Leave the corner to Windows: Explorer draws the file's own type icon there, exactly as
    /// it does for a file we never touched. The default, and byte-for-byte what an install did
    /// before this setting existed.
    #[default]
    SystemIcon,
    /// Our own format mark — [`crate::badge::BadgeStyle`] (`FormatBadgeStyle`) then picks the
    /// plain text chip or the category-coloured page. Explorer's overlay is suppressed so it
    /// cannot paint over it.
    Badge,
    /// Nothing in the corner at all: no mark of ours, and Explorer's own icon suppressed too.
    /// The bare picture.
    None,
}

impl CornerMark {
    /// `CornerMark`: 0 = system icon, 1 = our badge, 2 = nothing. An unknown value falls to the
    /// default rather than to a blank corner — a value we cannot read is not consent to hide
    /// what Windows would otherwise show.
    pub const fn from_dword(v: u32) -> Self {
        match v {
            1 => Self::Badge,
            2 => Self::None,
            _ => Self::SystemIcon,
        }
    }

    /// `const` so a table can name a variant's stored value directly - see the settings
    /// dialog's `DEPENDENT_ON_COMBO`, where the enum IS the combo's option order.
    pub const fn as_dword(self) -> u32 {
        match self {
            Self::SystemIcon => 0,
            Self::Badge => 1,
            Self::None => 2,
        }
    }

    /// The value an install that predates `CornerMark` should read as, from the two booleans it
    /// does have. Used ONLY when `CornerMark` is absent, so an upgrade keeps whatever the user
    /// had rather than silently reverting to the default.
    ///
    /// `badge` wins over `overlay_hidden` because a user who asked for our mark asked for a
    /// mark; the fact that the old two-checkbox UI let Explorer scribble on it was the bug, not
    /// the request.
    pub fn from_legacy(badge: bool, overlay_hidden: bool) -> Self {
        match (badge, overlay_hidden) {
            (true, _) => Self::Badge,
            (false, true) => Self::None,
            (false, false) => Self::SystemIcon,
        }
    }
}

/// The install wizard's `CornerMark` choice, written by the elevated installer under
/// `HKLM\Software\SageThumbs2K\CornerMark` — the same key path as [`ROOT`], but the machine
/// hive, the same way `bin/app/license.rs`'s `LicenseMode` is written. `None` on a portable
/// build (no installer, no HKLM write) or when the value has never been written.
fn hklm_corner_mark() -> Option<u32> {
    windows_registry::LOCAL_MACHINE
        .open(ROOT)
        .and_then(|k| k.get_u32("CornerMark"))
        .ok()
}

/// `CornerMark` — see [`CornerMark`]. Resolution order: the user's own HKCU choice; failing
/// that, the installer's wizard choice recorded in HKLM (item C3); failing that, the pre-2.5
/// legacy pair, so an upgrading install keeps the corner it already had.
pub fn corner_mark() -> CornerMark {
    match get_dword_opt("CornerMark") {
        Some(v) => CornerMark::from_dword(v),
        None => match hklm_corner_mark() {
            Some(v) => CornerMark::from_dword(v),
            // The legacy pair is READ here and never written again. Leaving the old values in
            // place rather than deleting them costs nothing (this branch stops being reached
            // the moment `CornerMark` exists somewhere) and keeps a downgrade working.
            None => CornerMark::from_legacy(
                get_dword("FormatBadge", 0) != 0,
                get_dword("HideTypeOverlay", 0) != 0,
            ),
        },
    }
}

pub fn set_corner_mark(m: CornerMark) -> windows_registry::Result<()> {
    set_dword("CornerMark", m.as_dword())
}

/// Whether to stamp our own format badge — true for exactly one [`CornerMark`] value.
pub fn format_badge() -> bool {
    corner_mark() == CornerMark::Badge
}

/// `FormatBadgeStyle` — icon (default) or plain text for that badge.
pub fn format_badge_icon() -> bool {
    get_dword("FormatBadgeStyle", DEFAULT_BADGE_STYLE) != 0
}

pub fn set_format_badge_icon(on: bool) -> windows_registry::Result<()> {
    set_dword("FormatBadgeStyle", u32::from(on))
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

/// "Convert to JPG" quality, 0–100.
pub fn jpeg_quality() -> u8 {
    clamp_quality(get_dword("JPEG", DEFAULT_JPEG))
}

/// "Convert to PNG" compression level, 0–9 (legacy zlib scale).
pub fn png_level() -> u32 {
    clamp_png(get_dword("PNG", DEFAULT_PNG))
}

// ---- Menu setting -------------------------------------------------------

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
    /// `EnableMenu` — master on/off for the right-click menu.
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
    let ini: Option<std::collections::HashMap<String, String>> =
        store::portable().then(|| store::section_values(None).into_iter().collect());
    let key = match ini {
        Some(_) => None,
        None => CURRENT_USER.open(hkcu_root()).ok(),
    };
    let gopt = |name: &str| -> Option<u32> {
        if let Some(ini) = ini.as_ref() {
            return ini.get(name).and_then(|v| v.parse().ok());
        }
        key.as_ref().and_then(|k| k.get_u32(name).ok())
    };
    let g = |name: &str, default: u32| gopt(name).unwrap_or(default);
    MenuGate {
        enabled: g("EnableMenu", 1) != 0,
        all_file_types: g("MenuAllFileTypes", 0) != 0,
        quick_verbs: g("MenuQuickVerbs", 0) != 0,
    }
}

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
pub struct MenuVisibility(MenuVisibilitySource);

/// Which backing store the snapshot came from. The portable arm holds the parsed section
/// outright — same "read once per menu build" contract, no file touched per item.
enum MenuVisibilitySource {
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

#[cfg(test)]
mod tests {
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
}
