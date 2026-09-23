//! The format badge and the tile's corner mark.

use super::*;

/// `FormatBadgeStyle` default: the category-coloured icon. The badge itself is opt-in, so
/// anyone who turns it on has asked to be able to tell formats apart at a glance — and a
/// colour does that faster than three letters. `0` selects the older plain text chip.
pub(super) const DEFAULT_BADGE_STYLE: u32 = 1;

/// `BadgeSize` default: [`BadgeSize::Small`], which is byte-for-byte the badge every build
/// before this setting drew. Growing the mark for everyone would change a picture people
/// already chose to have stamped, so the bigger steps are opt-in.
pub(super) const DEFAULT_BADGE_SIZE: u32 = 0;

/// How the corner badge is drawn.
///
/// `Text` is the original: a near-black chip with light letters, deliberately colourless so
/// it never competes with the picture. `Icon` is the one people actually asked for — a
/// dog-eared page tinted by the format's CATEGORY, so a folder of mixed files is scannable
/// by colour at a glance and only needs reading when two categories sit side by side.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum BadgeStyle {
    /// Neutral dark chip, light text.
    Text,
    /// Category-coloured page mark with the label inside.
    #[default]
    Icon,
}

impl BadgeStyle {
    /// `FormatBadgeStyle`: 0 = text, anything else = icon. Unknown values fall to the
    /// default rather than to "no badge" — a badge was still asked for.
    pub fn from_dword(v: u32) -> Self {
        if v == 0 {
            Self::Text
        } else {
            Self::Icon
        }
    }
}

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
    /// Our own format mark — [`BadgeStyle`] (`FormatBadgeStyle`) then picks the
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
pub(super) fn hklm_corner_mark() -> Option<u32> {
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
