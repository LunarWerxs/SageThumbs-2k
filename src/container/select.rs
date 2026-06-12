//! The shared "which entry is the cover" algorithm for archive containers
//! (CBZ / CB7 / CBR). Ported from CBXShell: skip non-images / junk, prefer an
//! entry named "cover", else take the natural-sorted first page (so page2 sorts
//! before page10, matching Explorer, via Win32 `StrCmpLogicalW`).

use std::cmp::Ordering;

use windows::core::PCWSTR;
use windows::Win32::UI::Shell::StrCmpLogicalW;

/// One archive entry's metadata (the bits cover-selection needs).
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

/// Index of the chosen cover entry among `entries`, or None if none qualify.
pub fn pick_cover(entries: &[Entry]) -> Option<usize> {
    let candidates: Vec<usize> = (0..entries.len())
        .filter(|&i| {
            let e = &entries[i];
            !e.is_dir
                && e.size > 0
                && e.size <= super::MAX_COVER
                && !is_junk(&e.name)
                && super::is_image_name(&e.name)
        })
        .collect();
    if candidates.is_empty() {
        return None;
    }

    // Skip scanlation junk — credits / logo / recruitment / invite pages that
    // scanlators slip in and that otherwise sort ahead of page 1 (DarkThumbs'
    // opt-in filter, conservatively worded). Only applied when real images
    // remain, so a comic whose every page name matches still yields a thumbnail.
    let candidates = if crate::settings::container_skip_scanlation() {
        let clean: Vec<usize> = candidates
            .iter()
            .copied()
            .filter(|&i| !is_scanlation_junk(&entries[i].name))
            .collect();
        if clean.is_empty() { candidates } else { clean }
    } else {
        candidates
    };

    // Prefer an image whose filename contains "cover" (default on).
    let mut pool = if crate::settings::container_prefer_cover() {
        let covers: Vec<usize> = candidates
            .iter()
            .copied()
            .filter(|&i| filename(&entries[i].name).contains("cover"))
            .collect();
        if covers.is_empty() { candidates } else { covers }
    } else {
        candidates
    };

    // Natural sort (default on) → first page; else first in archive order.
    if crate::settings::container_sort() {
        pool.sort_by(|&a, &b| natural_cmp(&entries[a].name, &entries[b].name));
    }
    pool.first().copied()
}

/// Archive cruft that is never a cover.
fn is_junk(name: &str) -> bool {
    name.contains("__MACOSX") || filename(name).eq_ignore_ascii_case("thumbs.db")
}

/// Scanlation filler pages (credits, group logo, recruitment, invites). The word
/// list is a conservative subset of DarkThumbs' (its "note" entry is dropped — it
/// false-matches "footnote"/"notes"/real titles like "Death Note").
fn is_scanlation_junk(name: &str) -> bool {
    let f = filename(name);
    ["credit", "logo", "recruit", "invite"].iter().any(|w| f.contains(w))
}

/// Lowercased final path component.
fn filename(path: &str) -> String {
    path.rsplit(['/', '\\']).next().unwrap_or(path).to_ascii_lowercase()
}

/// Natural (logical) comparison matching Explorer's ordering, with '[' demoted
/// past 'z' so bracketed "[extras]/[credits]" pages sort after real pages (the
/// CBXShell behavior).
fn natural_cmp(a: &str, b: &str) -> Ordering {
    let wa = wide(&demote_brackets(a));
    let wb = wide(&demote_brackets(b));
    let r = unsafe { StrCmpLogicalW(PCWSTR(wa.as_ptr()), PCWSTR(wb.as_ptr())) };
    r.cmp(&0)
}

fn demote_brackets(s: &str) -> String {
    // '{' (0x7B) sorts just after 'z' (0x7A); '[' (0x5B) would sort before 'a'.
    s.replace('[', "{")
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
