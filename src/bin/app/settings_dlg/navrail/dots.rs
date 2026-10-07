//! The dot beside a page whose settings differ from the defaults, and the memory of which dots were seen.

use super::*;

thread_local! {
    /// Bitmask of pages whose dot has already been ACKNOWLEDGED, cached from settings.
    /// `u32::MAX` is the "not loaded yet" sentinel — there are only `NCAT` (12) pages, so
    /// it can never be a real mask.
    static DOTS_SEEN: std::cell::Cell<u32> = const { std::cell::Cell::new(u32::MAX) };
}

// General: master switch, embedded pref, badge trio, checkerboard, the numbers, and the
// JPEG/PNG quality fields (also on this page — see ID_JPEG/ID_PNG in `cat_rows`'s GENERAL
// rows).
pub(super) fn general_page_has_non_defaults() -> bool {
    use st2k_base::settings as s;
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
    use st2k_base::settings as s;
    s::corner_mark() != s::CornerMark::default()
        || !s::format_badge_icon()
        || s::thumb_checker()
        || s::prefer_cover_art()
        || s::video_offset_pct() != s::DEFAULT_VIDEO_OFFSET_PCT
        || s::app_theme() != 0
}

// File types: every extension defaults to enabled (`s::format_enabled`'s own doc comment —
// "Enabled unless an explicit 0 is stored"), so the page has changed the moment any one of
// them is unchecked.
pub(super) fn filetypes_page_has_non_defaults() -> bool {
    use st2k_base::settings as s;
    formats::FORMATS
        .iter()
        .any(|&(ext, _)| !s::format_enabled(ext))
}

// Ebook/comic: also where ID_PDF_MARGIN actually lives (moved here from General, see
// `cat_rows`), so its non-Tight PDF page layout counts toward this page's dot.
pub(super) fn ebook_page_has_non_defaults() -> bool {
    use st2k_base::settings as s;
    !s::container_sort()
        || !s::container_prefer_cover()
        || !s::container_skip_scanlation()
        || !s::archive_collage()
        || s::pdf_page() != st2k_base::settings::PdfPage::Tight
}

pub(super) fn menu_page_has_non_defaults() -> bool {
    use st2k_base::settings as s;
    !s::menu_enabled()
        || s::menu_all_file_types()
        || s::menu_quick_verbs()
        || !s::preview_checker()
        || !s::folder_prebuild_verb()
        || s::preserve_file_date()
        || !s::keep_metadata_on_convert()
}

// Advanced: auto-update check defaults ON, the "hide from tray" toggle defaults off, and a
// tray double-click takes a screenshot.
pub(super) fn advanced_page_has_non_defaults() -> bool {
    use st2k_base::settings as s;
    !s::update_auto_check()
        || s::screenshot_hide_tray()
        || s::tray_double_click() != s::TrayDoubleClick::Capture
}

// Screenshot files: the save format, its JPEG quality, the name template and the Ctrl+S
// folder switch.
pub(super) fn shot_files_page_has_non_defaults() -> bool {
    use st2k_base::settings as s;
    s::shot_save_format() != s::ShotFormat::Png
        || s::shot_save_quality() != s::DEFAULT_SHOT_SAVE_QUALITY
        || s::shot_file_name() != s::DEFAULT_SHOT_FILE_NAME
        || s::screenshot_use_save_dir()
}

/// Page `ci`'s bit in the persisted seen-mask. The bits were the page indices until
/// Screenshot files went in at 6 (2026-10-07); every older page keeps its old bit and the
/// new page takes the next free one, so a dot someone already acknowledged neither comes
/// back nor moves to a neighbouring page.
fn seen_bit(ci: usize) -> u32 {
    let bit = match ci {
        0..=5 => ci,
        6 => NCAT - 1,
        _ => ci - 1,
    };
    1u32 << bit
}

/// Does this settings page hold any value the user has CHANGED from its default?
/// Drives the little dot on the nav rail — the answer to "where did I change something"
/// across nine pages, without opening each one. Reads the live settings (cheap registry /
/// portable-ini reads; runs only when a rail item repaints, not per frame).
///
/// Deliberately coarse: it names the pages that DIFFER, it does not promise the reverse
/// (a page with no dot may still have sub-state we don't track, e.g. list orderings).
pub(in super::super) fn page_has_non_defaults(ci: usize) -> bool {
    use st2k_base::settings as s;
    match ci {
        0 => general_page_has_non_defaults(),
        1 => appearance_page_has_non_defaults(),
        2 => filetypes_page_has_non_defaults(),
        3 => ebook_page_has_non_defaults(),
        4 => menu_page_has_non_defaults(),
        // Screenshots / Quick preview: their daemon-backed master switches are OFF by
        // default (first-run offers them), so ON is the changed state.
        5 => st2k_screenshot::screenshot::is_enabled(),
        6 => shot_files_page_has_non_defaults(),
        // Quick action: unbound (vk == 0) is the default; any bound hotkey is a change.
        7 => s::custom_action_hotkey().1 != 0,
        9 => s::preview_enabled(),
        8 => advanced_page_has_non_defaults(),
        _ => false,
    }
}

/// Acknowledged-dot mask, loaded from settings on first use (HKCU `NavDotsSeen`, or the
/// portable ini). Persisted rather than per-session: a hint that came back at every launch
/// read as an unread badge that could never be cleared.
pub(super) fn dots_seen() -> u32 {
    DOTS_SEEN.with(|c| {
        if c.get() == u32::MAX {
            c.set(st2k_base::settings::get_dword_opt("NavDotsSeen").unwrap_or(0));
        }
        c.get()
    })
}

/// Should page `ci` show its dot? Only while it has a changed setting AND the user hasn't
/// been to the page yet. The dot answers "where did I change something" for someone opening
/// Settings cold; once they've actually opened that page it has done its job, so it stops.
pub(in super::super) fn dot_visible(ci: usize) -> bool {
    page_has_non_defaults(ci) && dots_seen() & seen_bit(ci) == 0
}

/// Acknowledge page `ci`'s dot. Called on every category switch — including the initial
/// switch to page 0, so the page you land on doesn't keep a dot you're already looking at.
pub(super) fn mark_dot_seen(ci: usize) {
    if ci >= 32 {
        return;
    }
    let (cur, bit) = (dots_seen(), seen_bit(ci));
    if cur & bit != 0 {
        return;
    }
    DOTS_SEEN.with(|c| c.set(cur | bit));
    // Best-effort: a failed write costs a dot that reappears next launch, nothing more.
    let _ = st2k_base::settings::set_dword("NavDotsSeen", cur | bit);
}
