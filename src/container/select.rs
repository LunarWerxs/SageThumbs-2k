//! The shared "which entry is the cover" algorithm for archive containers
//! (CBZ / CB7 / CBR). Ported from CBXShell: skip non-images / junk, prefer an
//! entry named "cover", else take the natural-sorted first page (so page2 sorts
//! before page10, matching Explorer, via Win32 `StrCmpLogicalW`).

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
    pick_covers(entries, 1).into_iter().next()
}

/// Up to `want` cover entries, best-first — the same filter pipeline as the single
/// cover: "cover"-named images lead (when that preference is on), the remaining
/// pages follow, each group in natural-sort order (when sorting is on, else archive
/// order). `want = 1` reproduces [`pick_cover`] exactly; the contact-sheet thumbnail
/// asks for 4. Empty when nothing qualifies.
pub fn pick_covers(entries: &[Entry], want: usize) -> Vec<usize> {
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
        return Vec::new();
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

    // Prefer image types the COMPACT (no-ImageMagick) install can actually decode: a
    // JPEG-2000 cover renders only on the full build, so fall back to it only when no
    // natively-decodable image exists — never let a .jp2 shadow a sibling .jpg (#94).
    let candidates = {
        let native: Vec<usize> =
            candidates.iter().copied().filter(|&i| !is_exotic_cover(&entries[i].name)).collect();
        if native.is_empty() { candidates } else { native }
    };

    // Split off images whose filename contains "cover" (default on) — they lead the
    // result. With `want = 1` a non-empty cover group IS the pool, so behavior
    // matches the historical single-cover pick; the rest only matter for `want > 1`.
    let (mut covers, mut rest): (Vec<usize>, Vec<usize>) = if crate::settings::container_prefer_cover()
    {
        candidates.into_iter().partition(|&i| filename(&entries[i].name).contains("cover"))
    } else {
        (Vec::new(), candidates)
    };

    // Natural sort (default on) each group; else keep archive order.
    if crate::settings::container_sort() {
        natural_sort(&mut covers, entries);
        natural_sort(&mut rest, entries);
    }
    covers.extend(rest);
    covers.truncate(want);
    covers
}

/// Natural-sort candidate indices by entry name via `StrCmpLogicalW` (page2 before
/// page10, matching Explorer). Precomputes each candidate's UTF-16 sort key ONCE
/// (demote brackets, then encode), so the O(n log n) sort doesn't re-allocate two
/// wide buffers per comparison (mirrors verbs::fileops::natural_sort_key). Matters
/// on large comic archives with many image entries.
fn natural_sort(pool: &mut Vec<usize>, entries: &[Entry]) {
    let mut keyed: Vec<(Vec<u16>, usize)> =
        pool.iter().map(|&i| (wide(&demote_brackets(&entries[i].name)), i)).collect();
    keyed.sort_by(|a, b| {
        unsafe { StrCmpLogicalW(PCWSTR(a.0.as_ptr()), PCWSTR(b.0.as_ptr())) }.cmp(&0)
    });
    *pool = keyed.into_iter().map(|(_, i)| i).collect();
}

/// Drop picks whose entry NAME duplicates an earlier pick's. Archive formats
/// allow duplicate member names, and the RAR/7z streaming scans route captured
/// bytes BY NAME — two same-named picks would collide into one buffer (appending
/// or overwriting, corrupting the sheet) while another rank stayed empty. Keeping
/// the first pick per name eliminates the collision; the sheet just shows one
/// cell fewer in that (pathological, crafted-archive) case. ZIP reads by index
/// and doesn't need this.
pub fn dedupe_by_name(picks: Vec<usize>, entries: &[Entry]) -> Vec<usize> {
    let mut seen = std::collections::HashSet::new();
    picks.into_iter().filter(|&i| seen.insert(entries[i].name.as_str())).collect()
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

/// Cover image types that decode ONLY on the full (ImageMagick/openjpeg) install
/// (JPEG-2000). [`pick_cover`] deprioritizes these so a compact install never picks an
/// undecodable cover when a natively-decodable sibling page exists.
fn is_exotic_cover(name: &str) -> bool {
    let ext = filename(name).rsplit('.').next().unwrap_or("").to_string();
    matches!(ext.as_str(), "jp2" | "j2k" | "jpf" | "jpx" | "jpm")
}

/// Lowercased final path component.
fn filename(path: &str) -> String {
    path.rsplit(['/', '\\']).next().unwrap_or(path).to_ascii_lowercase()
}

/// Demote '[' past 'z' so bracketed "[extras]/[credits]" pages sort after real
/// pages (the CBXShell behavior). Applied when building each candidate's natural-
/// sort key in [`pick_cover`].
fn demote_brackets(s: &str) -> String {
    // '{' (0x7B) sorts just after 'z' (0x7A); '[' (0x5B) would sort before 'a'.
    s.replace('[', "{")
}

fn wide(s: &str) -> Vec<u16> {
    crate::wide(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exotic_cover_detection() {
        // JPEG-2000 family = full-install-only → deprioritized.
        assert!(is_exotic_cover("Page 01.JP2"));
        assert!(is_exotic_cover("scans/cover.jpx"));
        assert!(is_exotic_cover("x.j2k"));
        // Natively / WIC-decodable types are NOT exotic.
        assert!(!is_exotic_cover("Page 01.jpg"));
        assert!(!is_exotic_cover("cover.png"));
        assert!(!is_exotic_cover("art.webp"));
    }
}
