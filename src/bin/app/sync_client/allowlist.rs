//! Which settings sync at all: the allow list, and the values that must never leave the machine.

#[derive(Clone, Copy)]
pub(super) enum Kind {
    Dword,
    Str,
}

/// The syncable-key allowlist — portable preferences ONLY.
///
/// Widened 2026-08-25 from 33 keys to 60. Everything the Quick preview viewer learned since this
/// list was written — every playback, layout and rendering preference it has — was silently
/// stranded on one machine, along with the PDF layout, the screenshot tool defaults, the convert
/// metadata switch and half a dozen others. None of them was excluded on purpose; they simply
/// arrived after the list did, and "not synced" is what a missing entry means.
///
/// [`NEVER_SYNCED`] now names every key that stays behind, with the reason, and the test at the
/// bottom of this file reads `settings.rs` and fails on any key that is on neither list. That
/// check is the durable half of this change: widening once only helps until the next setting.
pub(super) const ALLOW: &[(&str, Kind)] = &[
    ("EnableThumbs", Kind::Dword),
    ("MaxSize", Kind::Dword),
    ("Width", Kind::Dword),
    ("Height", Kind::Dword),
    ("UseEmbedded", Kind::Dword),
    ("JPEG", Kind::Dword),
    ("PNG", Kind::Dword),
    ("EnableMenu", Kind::Dword),
    ("MenuAllFileTypes", Kind::Dword),
    ("MenuPreview", Kind::Dword),
    ("MenuQuickVerbs", Kind::Dword),
    ("PreviewChecker", Kind::Dword),
    ("AppTheme", Kind::Dword),
    ("FormatBadge", Kind::Dword),
    ("FormatBadgeStyle", Kind::Dword),
    // How big that badge is drawn. Unlike `CornerMark` below, this touches nothing outside
    // our own bitmap - no per-ProgID registry, nothing machine-shaped - so a pulled value is
    // honoured exactly as it was set on the other machine.
    ("BadgeSize", Kind::Dword),
    ("ThumbChecker", Kind::Dword),
    // NOT synced: HideTypeOverlay. It is not just a value - flipping it rewrites
    // per-ProgID registry keys on THIS machine, and the ProgIDs differ per machine, so a
    // synced 1 would record a suppression that was never actually applied here.
    ("PreserveFileDate", Kind::Dword),
    ("ContainerSort", Kind::Dword),
    ("ContainerPreferCover", Kind::Dword),
    ("ContainerSkipScanlation", Kind::Dword),
    ("CvJpegQuality", Kind::Dword),
    ("CvWebpQuality", Kind::Dword),
    ("CvWebpLossless", Kind::Dword),
    ("CvPngLevel", Kind::Dword),
    ("CvMagickQuality", Kind::Dword),
    ("ScreenshotHotkey", Kind::Dword),
    ("ScreenshotQuickHotkey", Kind::Dword),
    ("CustomAction", Kind::Dword),
    ("CustomActionHotkey", Kind::Dword),
    ("ScreenshotHideTray", Kind::Dword),
    ("ShotUseSaveDir", Kind::Dword),
    ("UpdateAutoCheck", Kind::Dword),
    ("Lang", Kind::Str),
    ("MenuOrder", Kind::Str),
    // ── Widened 2026-08-25 ──────────────────────────────────────────────────────────────
    // The Quick preview viewer, in full. Every one of these is a statement about how you like
    // to read things, and not one of them travelled before now.
    ("PreviewEnabled", Kind::Dword),
    ("PreviewArrowNav", Kind::Dword),
    ("PreviewHoldPeek", Kind::Dword),
    ("PreviewCloseOnFocusLoss", Kind::Dword),
    ("PreviewOpenFront", Kind::Dword),
    ("PreviewText", Kind::Dword),
    ("PreviewMarkdown", Kind::Dword),
    ("PreviewTocOpen", Kind::Dword),
    ("PreviewMdRemoteImg", Kind::Dword),
    ("PreviewHtml", Kind::Dword),
    ("PreviewUrlLive", Kind::Dword),
    ("PreviewPdfStrip", Kind::Dword),
    // The Quick-preview extension blocklist (2026-09-08). A free-text preference like
    // `ShotSaveDir`, and portable in the same sense: "never Quick-preview .insv" is a
    // statement about the user, not about this machine.
    ("PreviewBlockedExts", Kind::Str),
    ("PreviewLoop", Kind::Dword),
    ("PreviewMuted", Kind::Dword),
    ("PreviewVolume", Kind::Dword),
    ("PreviewSpeed", Kind::Dword),
    // Documents and containers.
    ("PdfLayout", Kind::Dword),
    ("PdfMarginPt", Kind::Dword),
    ("ArchiveCollage", Kind::Dword),
    // Thumbnails and the convert verbs.
    ("VideoCoverArt", Kind::Dword),
    ("VideoOffset", Kind::Dword),
    ("KeepMetadata", Kind::Dword),
    ("FolderPrebuildVerb", Kind::Dword),
    // Screenshot tool defaults (its SAVE FOLDER stays behind — see NEVER_SYNCED).
    ("ShotDefaultTool", Kind::Dword),
    ("ShotDelaySec", Kind::Dword),
    ("EyeFormat", Kind::Dword),
];

/// Every setting that deliberately does NOT sync, with the reason it doesn't.
///
/// Read by `every_setting_is_classified` below, which is the only thing that consumes it at
/// runtime. Its real job is to be the written-down decision, and to fail the build's test run
/// when a new setting has no decision yet.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) const NEVER_SYNCED: &[&str] = &[
    // An absolute path on THIS PC.
    "ShotSaveDir",
    // Window geometry.
    "PreviewWinW",
    "PreviewWinH",
    // Local diagnostics and dev-machine flags.
    "Debug",
    "DevMachine",
    // Install state, not a preference.
    "InstallReported",
    // Not just a value: flipping it rewrites per-ProgID registry keys on THIS machine, and the
    // ProgIDs differ per machine — so a synced 1 would record a suppression never applied here.
    "HideTypeOverlay",
    // Its successor, and it inherits the reason. `CornerMark` now carries the overlay decision
    // as well as the badge one (see `settings::CornerMark`), and two of its three values mean
    // "suppress Explorer's overlay" — which only takes effect when `typeoverlay::sync` runs
    // against THIS machine's ProgIDs. A pulled value would be recorded and never applied, so
    // the setting would read as honoured while the corner still showed the other thing. The
    // badge half used to sync on its own; it cannot any more without lying about the other half.
    "CornerMark",
    // The eyedropper's recent-colours list is CONTENT, not a preference, and it only grows.
    // `EyeFormat` (which format you want them copied in) does sync.
    "EyeHistory",
    // The sign-in prompt's own schedule (see the app's `nudge.rs`). Half of it — how long this
    // copy has been installed, how many times it has been opened — describes one machine, so
    // syncing the blob would mix two machines' histories into one and make the gate meaningless.
    "SignInNudge",
];
