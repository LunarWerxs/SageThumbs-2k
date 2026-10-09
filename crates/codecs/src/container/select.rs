//! The shared "which entry is the cover" algorithm for archive containers
//! (CBZ / CB7 / CBR). Ported from CBXShell: skip non-images / junk, prefer an
//! entry named as the cover, else take the natural-sorted first page (so page2 sorts
//! before page10, matching Explorer, via Win32 `StrCmpLogicalW`). A back, variant
//! or gallery cover goes to the END rather than the front: see [`cover_rank`].

use windows::core::PCWSTR;
use windows::Win32::UI::Shell::StrCmpLogicalW;

/// One archive entry's metadata (the bits cover-selection needs).
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

/// The three cover-selection preferences, read once and passed in, so a caller that
/// already holds a settings snapshot does not cost one registry open per preference per
/// archive.
#[derive(Clone, Copy, Debug)]
pub struct CoverPrefs {
    /// `ContainerPreferCover`: an image named as the front cover leads, and one named as
    /// any other cover trails (see [`cover_rank`]).
    pub prefer_cover: bool,
    /// `ContainerSort`: natural-sort the pages (else archive order).
    pub sort: bool,
    /// `ContainerSkipScanlation`: drop scanlation filler pages.
    pub skip_scanlation: bool,
}

impl CoverPrefs {
    /// The preferences as the individual accessors read them (three registry opens).
    pub fn from_settings() -> Self {
        Self {
            prefer_cover: st2k_base::settings::container_prefer_cover(),
            sort: st2k_base::settings::container_sort(),
            skip_scanlation: st2k_base::settings::container_skip_scanlation(),
        }
    }

    /// The preferences out of an existing per-request settings snapshot (no registry
    /// access).
    pub fn from_thumb_settings(cfg: &st2k_base::settings::ThumbSettings) -> Self {
        Self {
            prefer_cover: cfg.container_prefer_cover,
            sort: cfg.container_sort,
            skip_scanlation: cfg.container_skip_scanlation,
        }
    }
}

/// Index of the chosen cover entry among `entries`, or None if none qualify.
pub fn pick_cover(entries: &[Entry], prefs: &CoverPrefs) -> Option<usize> {
    pick_covers(entries, 1, prefs).into_iter().next()
}

/// Up to `want` cover entries, best-first — the same filter pipeline as the single
/// cover: with the preference on, the [`cover_rank`] groups in order (front cover,
/// pages, other covers), each group in natural-sort order (when sorting is on, else
/// archive order). `want = 1` reproduces [`pick_cover`] exactly; the contact-sheet
/// thumbnail asks for 4. Empty when nothing qualifies. The preferences come from the
/// caller — read once per request and passed down, never read from the registry here.
pub fn pick_covers(entries: &[Entry], want: usize, prefs: &CoverPrefs) -> Vec<usize> {
    let mut picks = cover_candidates(entries, prefs);
    // Natural sort (default on); else keep archive order.
    if prefs.sort {
        natural_sort(&mut picks, entries);
    }
    // Then the rank groups, by a STABLE sort, so each group keeps the order above.
    if prefs.prefer_cover {
        picks.sort_by_cached_key(|&i| cover_rank(&entries[i].name));
    }
    picks.truncate(want);
    picks
}

/// Where an entry's NAME puts it in the pick when `ContainerPreferCover` is on, best first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CoverRank {
    /// Named as THE cover: `cover.jpg`, `Front Cover.png`, `00_cover.jpg`, `FrontCover.jpg`.
    Front,
    /// An ordinary page, including a name that only contains the letters (`Discovery.jpg`).
    Page,
    /// Named as some OTHER cover: back, inside or variant, or a covers gallery. Scans put
    /// these at the end of the book. Until 2026-10-09 any name containing "cover" led the
    /// pick, so a `Back Cover.jpg` or `Variant Cover B.jpg` beat the real first page, and
    /// turning the preference off to escape that lost the books whose cover IS `cover.jpg`.
    Other,
}

/// Words that, beside "cover", name a cover other than the front one. Matched as whole
/// words NEXT TO "cover", looking past only a variant letter or "front" (see
/// [`qualifies_cover`]), so a series title ("Back 2 School 001 - Cover") does not demote
/// its own cover.
const NOT_FRONT: &[&str] = &[
    "back",
    "rear",
    "inside",
    "inner",
    "interior",
    "variant",
    "variants",
    "var",
    "alt",
    "alternate",
    "alternative",
    "textless",
    "virgin",
    "incentive",
    "sketch",
    "exclusive",
    "gallery",
];

/// Rank an archive entry by its file name: see [`CoverRank`]. Case-insensitive, and only
/// the final path component counts.
pub fn cover_rank(name: &str) -> CoverRank {
    let file = filename(name);
    let stem = file.rsplit_once('.').map_or(file.as_str(), |(s, _)| s);
    let words: Vec<&str> = stem
        .split(|c: char| !c.is_ascii_alphabetic())
        .filter(|w| !w.is_empty())
        .collect();
    let mut rank = CoverRank::Page;
    for (i, word) in words.iter().enumerate() {
        match cover_word(word) {
            Some(CoverRank::Front) if !qualifies_cover(&words[..i], &words[i + 1..]) => {
                rank = rank.min(CoverRank::Front);
            }
            Some(_) => return CoverRank::Other,
            None => {}
        }
    }
    rank
}

/// True when the nearest real word on either side of a cover word is a [`NOT_FRONT`] one.
/// A variant letter and "front" are looked past, so "Inside Front Cover" and "Cover B
/// variant" still read as other covers.
fn qualifies_cover(before: &[&str], after: &[&str]) -> bool {
    let real = |w: &&&str| w.len() > 1 && **w != "front";
    let other = |w: Option<&&str>| w.is_some_and(|w| NOT_FRONT.contains(w));
    other(before.iter().rfind(real)) || other(after.iter().find(real))
}

/// One word's cover meaning on its own: `cover` (and `frontcover`, `coverart`, `coverb`) is
/// [`CoverRank::Front`], `covers` (a gallery) and `backcover`-style compounds are
/// [`CoverRank::Other`], and every other word, `discover` included, is `None`.
fn cover_word(word: &str) -> Option<CoverRank> {
    let qualifier = match word {
        "cover" => return Some(CoverRank::Front),
        "covers" => return Some(CoverRank::Other),
        w => w
            .strip_suffix("cover")
            .or_else(|| w.strip_prefix("cover"))?,
    };
    if NOT_FRONT.contains(&qualifier) {
        Some(CoverRank::Other)
    } else if qualifier == "front" || qualifier == "art" || qualifier.len() == 1 {
        Some(CoverRank::Front)
    } else {
        None
    }
}

/// Cover-eligible entry indices in PHYSICAL/archive order, after the same junk,
/// scanlation and native-format filtering as [`pick_covers`] but before its
/// preference groups and natural sort.
///
/// Solid 7z scanning needs membership plus physical order: sorting tens of
/// thousands of project-archive paths only to throw that order away made header-only
/// misses needlessly CPU-heavy. Keeping the filtering in one helper prevents the
/// cheap solid path from drifting from the normal name-ranked picker. The
/// preferences come from the caller — read once per request and passed down,
/// never read from the registry here.
pub fn cover_candidates(entries: &[Entry], prefs: &CoverPrefs) -> Vec<usize> {
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
    let candidates = if prefs.skip_scanlation {
        let clean: Vec<usize> = candidates
            .iter()
            .copied()
            .filter(|&i| !is_scanlation_junk(&entries[i].name))
            .collect();
        if clean.is_empty() {
            candidates
        } else {
            clean
        }
    } else {
        candidates
    };

    // Prefer image types the COMPACT (no-ImageMagick) install can actually decode: a
    // JPEG-2000 cover renders only on the full build, so fall back to it only when no
    // natively-decodable image exists — never let a .jp2 shadow a sibling .jpg (#94).
    let candidates = {
        let native: Vec<usize> = candidates
            .iter()
            .copied()
            .filter(|&i| !is_exotic_cover(&entries[i].name))
            .collect();
        if native.is_empty() {
            candidates
        } else {
            native
        }
    };

    candidates
}

/// Natural (logical) compare of two pre-encoded, NUL-terminated UTF-16 sort keys — so
/// page2 sorts before page10, matching Explorer (Win32 `StrCmpLogicalW`). Shared with the
/// PDF combiner's page sorter in `crate::topdf`.
pub fn cmp_logical_keys(a: &[u16], b: &[u16]) -> std::cmp::Ordering {
    unsafe { StrCmpLogicalW(PCWSTR(a.as_ptr()), PCWSTR(b.as_ptr())) }.cmp(&0)
}

/// Natural-sort candidate indices by entry name via `StrCmpLogicalW` (page2 before
/// page10, matching Explorer). Precomputes each candidate's UTF-16 sort key ONCE
/// (demote brackets, then encode), so the O(n log n) sort doesn't re-allocate two
/// wide buffers per comparison (mirrors verbs::fileops::natural_key_cmp). Matters
/// on large comic archives with many image entries.
fn natural_sort(pool: &mut Vec<usize>, entries: &[Entry]) {
    let mut keyed: Vec<(Vec<u16>, usize)> = pool
        .iter()
        .map(|&i| (wide(&demote_brackets(&entries[i].name)), i))
        .collect();
    keyed.sort_by(|a, b| cmp_logical_keys(&a.0, &b.0));
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
    picks
        .into_iter()
        .filter(|&i| seen.insert(entries[i].name.as_str()))
        .collect()
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
    ["credit", "logo", "recruit", "invite"]
        .iter()
        .any(|w| f.contains(w))
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
    path.rsplit(['/', '\\'])
        .next()
        .unwrap_or(path)
        .to_ascii_lowercase()
}

/// Demote '[' past 'z' so bracketed "[extras]/[credits]" pages sort after real
/// pages (the CBXShell behavior). Applied when building each candidate's natural-
/// sort key in [`pick_cover`].
fn demote_brackets(s: &str) -> String {
    // '{' (0x7B) sorts just after 'z' (0x7A); '[' (0x5B) would sort before 'a'.
    s.replace('[', "{")
}

fn wide(s: &str) -> Vec<u16> {
    st2k_base::host::wide(s)
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

    fn entries(names: &[&str]) -> Vec<Entry> {
        names
            .iter()
            .map(|n| Entry {
                name: (*n).to_string(),
                is_dir: false,
                size: 100,
            })
            .collect()
    }

    fn picked(names: &[&str], prefer_cover: bool) -> String {
        let prefs = CoverPrefs {
            prefer_cover,
            sort: true,
            skip_scanlation: true,
        };
        let e = entries(names);
        pick_cover(&e, &prefs).map_or_else(String::new, |i| e[i].name.clone())
    }

    /// The comic shapes behind a 2026-10-09 report: with "Prefer a cover image" on, any
    /// name containing "cover" led, so the back cover, a variant cover, a covers gallery or
    /// a page called "Discovery" became the thumbnail. Each row's answer is the front cover.
    #[test]
    fn the_front_cover_wins_over_back_variant_and_gallery_covers() {
        let rows: [(&[&str], &str); 6] = [
            (
                &[
                    "Saga 001 (2012) - p000.jpg",
                    "Saga 001 (2012) - p001.jpg",
                    "Saga 001 (2012) - Back Cover.jpg",
                    "Saga 001 (2012) - Variant Cover B.jpg",
                ],
                "Saga 001 (2012) - p000.jpg",
            ),
            (&["001.jpg", "002.jpg", "zz_backcover.jpg"], "001.jpg"),
            (
                &["01 - Discovery.jpg", "00 - Intro.jpg", "02.jpg"],
                "00 - Intro.jpg",
            ),
            (
                &[
                    "Comic 05 - 000.jpg",
                    "Comic 05 - covers 01.jpg",
                    "Comic 05 - 001.jpg",
                ],
                "Comic 05 - 000.jpg",
            ),
            // What the preference is FOR: digits sort before letters, so without it the
            // cover named as such would lose to page 01.
            (&["01.jpg", "02.jpg", "cover.jpg"], "cover.jpg"),
            (
                &["01.jpg", "back cover.jpg", "front cover.jpg"],
                "front cover.jpg",
            ),
        ];
        for (names, want) in rows {
            assert_eq!(picked(names, true), want, "{names:?}");
        }
    }

    /// Off means the first page by name, whatever the names say.
    #[test]
    fn with_the_preference_off_names_do_not_reorder_anything() {
        assert_eq!(picked(&["01.jpg", "02.jpg", "cover.jpg"], false), "01.jpg");
        assert_eq!(
            picked(&["b.jpg", "a back cover.jpg"], false),
            "a back cover.jpg"
        );
    }

    /// A contact sheet still gets every page: other covers trail rather than vanish.
    #[test]
    fn other_covers_trail_the_pages_on_a_contact_sheet() {
        let prefs = CoverPrefs {
            prefer_cover: true,
            sort: true,
            skip_scanlation: true,
        };
        let e = entries(&["Back Cover.jpg", "02.jpg", "cover.jpg", "01.jpg"]);
        let names: Vec<&str> = pick_covers(&e, 4, &prefs)
            .into_iter()
            .map(|i| e[i].name.as_str())
            .collect();
        assert_eq!(names, ["cover.jpg", "01.jpg", "02.jpg", "Back Cover.jpg"]);
    }

    #[test]
    fn cover_rank_reads_whole_words_in_the_file_name_only() {
        for name in [
            "COVER.jpg",
            "scans/Cover.png",
            "front-cover.png",
            "FrontCover.jpg",
            "00_cover.jpg",
            "Cover A.jpg",
            "CoverB.jpg",
            "cover art.png",
            // "Back" and "Inside" belong to the title, not to "cover".
            "Back to the Future 001 - Cover.jpg",
            "Back 2 School 001 - Cover.jpg",
            "Inside Out 002 - Cover.jpg",
        ] {
            assert_eq!(cover_rank(name), CoverRank::Front, "{name}");
        }
        for name in [
            "page01.png",
            "Discovery.jpg",
            "recovered.jpg",
            "coverage.png",
            "cover/page01.png",
        ] {
            assert_eq!(cover_rank(name), CoverRank::Page, "{name}");
        }
        for name in [
            "Back Cover.jpg",
            "backcover.jpg",
            "Cover (back).jpg",
            "Inside Front Cover.jpg",
            "Variant Cover B.jpg",
            "Cover B variant.jpg",
            "covers 01.jpg",
            "cover gallery 2.png",
        ] {
            assert_eq!(cover_rank(name), CoverRank::Other, "{name}");
        }
    }
}
