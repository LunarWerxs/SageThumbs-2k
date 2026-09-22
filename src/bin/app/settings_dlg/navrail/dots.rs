//! The dot beside a page whose settings differ from the defaults, and the memory of which dots were seen.

use super::*;

thread_local! {
    /// Bitmask of pages whose dot has already been ACKNOWLEDGED, cached from settings.
    /// `u32::MAX` is the "not loaded yet" sentinel — there are only `NCAT` (11) pages, so
    /// it can never be a real mask.
    static DOTS_SEEN: std::cell::Cell<u32> = const { std::cell::Cell::new(u32::MAX) };
}

// General: master switch, embedded pref, badge trio, checkerboard, the numbers, and the
// JPEG/PNG quality fields (also on this page — see ID_JPEG/ID_PNG in `cat_rows`'s GENERAL
// rows).
pub(super) fn general_page_has_non_defaults() -> bool {
    use sagethumbs2k_core::settings as s;
    !s::thumbnails_enabled()
        || !s::use_embedded()
        || s::max_file_size_bytes() != u64::from(s::DEFAULT_MAX_FILE_MB) * 1024 * 1024
        || s::max_thumb_size() != s::DEFAULT_THUMB_SIZE
        || s::jpeg_quality() != s::DEFAULT_JPEG as u8
        || s::png_level() != s::DEFAULT_PNG
}

// Appearance: every control on the page, all default-off except the icon style. The corner
// mark is one tri-state now, so ANY non-default value counts once — asking
// `format_badge() || hide_type_overlay()` would double-count the same choice.
pub(super) fn appearance_page_has_non_defaults() -> bool {
    use sagethumbs2k_core::settings as s;
    s::corner_mark() != s::CornerMark::default()
        || !s::format_badge_icon()
        || s::thumb_checker()
        || s::prefer_cover_art()
        || s::video_offset_pct() != s::DEFAULT_VIDEO_OFFSET_PCT
}

// File types: every extension defaults to enabled (`s::format_enabled`'s own doc comment —
// "Enabled unless an explicit 0 is stored"), so the page has changed the moment any one of
// them is unchecked.
pub(super) fn filetypes_page_has_non_defaults() -> bool {
    use sagethumbs2k_core::settings as s;
    formats::FORMATS
        .iter()
        .any(|&(ext, _)| !s::format_enabled(ext))
}

// Ebook/comic: also where ID_PDF_MARGIN actually lives (moved here from General, see
// `cat_rows`), so its non-Tight PDF page layout counts toward this page's dot.
pub(super) fn ebook_page_has_non_defaults() -> bool {
    use sagethumbs2k_core::settings as s;
    !s::container_sort()
        || !s::container_prefer_cover()
        || !s::container_skip_scanlation()
        || !s::archive_collage()
        || s::pdf_page() != sagethumbs2k_core::PdfPage::Tight
}

pub(super) fn menu_page_has_non_defaults() -> bool {
    use sagethumbs2k_core::settings as s;
    !s::menu_enabled()
        || s::menu_all_file_types()
        || s::menu_quick_verbs()
        || !s::preview_checker()
        || !s::folder_prebuild_verb()
        || s::preserve_file_date()
        || !s::keep_metadata_on_convert()
}

// Advanced: auto-update check defaults ON, and the "hide from tray" toggle defaults off.
pub(super) fn advanced_page_has_non_defaults() -> bool {
    use sagethumbs2k_core::settings as s;
    !s::update_auto_check() || s::screenshot_hide_tray()
}

/// Does this settings page hold any value the user has CHANGED from its default?
/// Drives the little dot on the nav rail — the answer to "where did I change something"
/// across nine pages, without opening each one. Reads the live settings (cheap registry /
/// portable-ini reads; runs only when a rail item repaints, not per frame).
///
/// Deliberately coarse: it names the pages that DIFFER, it does not promise the reverse
/// (a page with no dot may still have sub-state we don't track, e.g. list orderings).
pub(in super::super) fn page_has_non_defaults(ci: usize) -> bool {
    use sagethumbs2k_core::settings as s;
    match ci {
        0 => general_page_has_non_defaults(),
        1 => appearance_page_has_non_defaults(),
        2 => filetypes_page_has_non_defaults(),
        3 => ebook_page_has_non_defaults(),
        4 => menu_page_has_non_defaults(),
        // Screenshots / Quick preview: their daemon-backed master switches are OFF by
        // default (first-run offers them), so ON is the changed state.
        5 => crate::screenshot::is_enabled(),
        // Quick action: unbound (vk == 0) is the default; any bound hotkey is a change.
        6 => s::custom_action_hotkey().1 != 0,
        8 => s::preview_enabled(),
        7 => advanced_page_has_non_defaults(),
        _ => false,
    }
}

/// Acknowledged-dot mask, loaded from settings on first use (HKCU `NavDotsSeen`, or the
/// portable ini). Persisted rather than per-session: a hint that came back at every launch
/// read as an unread badge that could never be cleared.
pub(super) fn dots_seen() -> u32 {
    DOTS_SEEN.with(|c| {
        if c.get() == u32::MAX {
            c.set(sagethumbs2k_core::settings::get_dword_opt("NavDotsSeen").unwrap_or(0));
        }
        c.get()
    })
}

/// Should page `ci` show its dot? Only while it has a changed setting AND the user hasn't
/// been to the page yet. The dot answers "where did I change something" for someone opening
/// Settings cold; once they've actually opened that page it has done its job, so it stops.
pub(in super::super) fn dot_visible(ci: usize) -> bool {
    ci < 32 && page_has_non_defaults(ci) && dots_seen() & (1u32 << ci) == 0
}

/// Acknowledge page `ci`'s dot. Called on every category switch — including the initial
/// switch to page 0, so the page you land on doesn't keep a dot you're already looking at.
pub(super) fn mark_dot_seen(ci: usize) {
    if ci >= 32 {
        return;
    }
    let (cur, bit) = (dots_seen(), 1u32 << ci);
    if cur & bit != 0 {
        return;
    }
    DOTS_SEEN.with(|c| c.set(cur | bit));
    // Best-effort: a failed write costs a dot that reappears next launch, nothing more.
    let _ = sagethumbs2k_core::settings::set_dword("NavDotsSeen", cur | bit);
}
