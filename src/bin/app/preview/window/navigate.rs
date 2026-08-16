//! Sibling-file navigation: which files count, and Explorer's sort order.
//!
//! Split out of `window.rs` 2026-07-31 (pure move).

use super::*;
use std::cell::RefCell;

/// True if `ext` (lowercase, no dot) is something the viewer can render — used to filter the
/// folder listing for ←/→ navigation so arrows skip files nothing can preview. Must stay in sync
/// with what `loader::load` actually handles: decoded formats + text/markdown + archives + fonts
/// + SQLite databases.
pub(in crate::preview) fn is_previewable_ext(ext: &str) -> bool {
    use sagethumbs2k_core::formats;
    formats::is_known(ext)
        || formats::is_preview_text(ext)
        || formats::is_preview_markdown(ext)
        || formats::is_preview_doc(ext)
        || content::is_archive_ext(ext)
        || crate::preview::font::is_font_ext(ext)
        || crate::preview::dbdoc::is_db_ext(ext)
}

/// Explorer-style filename order (`image2` before `image10`). Precompute each UTF-16 key once
/// so a large-folder O(n log n) sort does not allocate inside every comparison.
pub(in crate::preview) fn sort_paths_like_explorer(
    files: Vec<std::path::PathBuf>,
) -> Vec<std::path::PathBuf> {
    let mut keyed: Vec<(Vec<u16>, std::path::PathBuf)> = files
        .into_iter()
        .map(|p| {
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy())
                .unwrap_or_default();
            (crate::win::wide(&name), p)
        })
        .collect();
    keyed.sort_by(|a, b| {
        unsafe { StrCmpLogicalW(PCWSTR(a.0.as_ptr()), PCWSTR(b.0.as_ptr())) }
            .cmp(&0)
            .then_with(|| a.1.cmp(&b.1))
    });
    keyed.into_iter().map(|(_, p)| p).collect()
}

/// Previewable files directly inside `dir` (no recursion), unsorted.
fn scan_previewable_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    rd.flatten()
        .filter(|e| {
            // Use the DirEntry's cached file_type (no extra per-entry `stat` syscall — a huge
            // folder would otherwise cost thousands of stats on the UI thread) and gate on the
            // extension BEFORE anything else.
            e.file_type().map(|t| t.is_file()).unwrap_or(false)
                && std::path::Path::new(&e.file_name())
                    .extension()
                    .and_then(|x| x.to_str())
                    .map(|x| is_previewable_ext(&x.to_ascii_lowercase()))
                    .unwrap_or(false)
        })
        .map(|e| e.path())
        .take(20_000) // sanity cap for a pathological folder
        .collect()
}

thread_local! {
    /// One-entry cache of the last folder's sorted, previewable-file listing. Arrow-key
    /// navigation only ever looks at ONE folder at a time — the current file's parent — so a
    /// single slot matches the real access pattern; without it, holding an arrow key
    /// re-enumerates AND re-sorts up to 20,000 entries on the UI thread on every single step.
    /// Keyed on the directory's own mtime (which NTFS/Explorer bump on any create, delete, or
    /// rename inside it), so a stale listing can never outlive the folder it describes.
    static SIBLING_CACHE: RefCell<Option<(std::path::PathBuf, std::time::SystemTime, Vec<std::path::PathBuf>)>> =
        const { RefCell::new(None) };
}

/// Sorted previewable siblings of `dir`, served from the cache when it's still fresh.
fn cached_sorted_siblings(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mtime = std::fs::metadata(dir).and_then(|m| m.modified()).ok();
    SIBLING_CACHE.with(|cell| {
        let mut cache = cell.borrow_mut();
        if let (Some((cdir, cmtime, files)), Some(mtime)) = (cache.as_ref(), mtime) {
            if cdir == dir && *cmtime == mtime {
                return files.clone();
            }
        }
        let sorted = sort_paths_like_explorer(scan_previewable_files(dir));
        // Only cache when the mtime actually resolved — an unreadable directory (permissions,
        // a dropped network share) should be retried next time, not remembered as empty.
        *cache = mtime.map(|m| (dir.to_path_buf(), m, sorted.clone()));
        sorted
    })
}

/// Index of `cur` within `files` by case-insensitive file-name match. Every entry in `files` is
/// a sibling inside the SAME directory, so the file name alone disambiguates them — and it must
/// be compared case-insensitively: NTFS and Explorer's own `StrCmpLogicalW` sort are both
/// case-insensitive, but a stored path can disagree in case with what `read_dir` just returned.
/// An exact-equality compare then misses the current file entirely, and the caller's
/// `unwrap_or(0)` silently snaps every arrow press back to the first file in the folder.
fn position_case_insensitive(files: &[std::path::PathBuf], cur: &std::path::Path) -> Option<usize> {
    let cur_name = cur.file_name()?.to_string_lossy();
    files.iter().position(|p| {
        p.file_name()
            .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(&cur_name))
    })
}

/// Whether `a` and `b` name the same file, ignoring case (same reasoning as
/// [`position_case_insensitive`] — used for the "did navigation actually move" guard).
fn same_file_name_ignore_case(a: &std::path::Path, b: &std::path::Path) -> bool {
    match (a.file_name(), b.file_name()) {
        (Some(x), Some(y)) => x
            .to_string_lossy()
            .eq_ignore_ascii_case(&y.to_string_lossy()),
        _ => false,
    }
}

/// Flip to the next/prev previewable file in the current file's folder (QuickLook-style folder
/// traversal, wrapping at the ends), without closing the popup. Sorted case-insensitively by
/// Explorer's logical filename order.
pub(in crate::preview) unsafe fn nav_sibling(hwnd: HWND, delta: i32) {
    let st = &*state(hwnd);
    let cur = match st.path.borrow().clone() {
        Some(p) => p,
        None => return,
    };
    let cur_path = std::path::Path::new(&cur);
    let Some(dir) = cur_path.parent() else {
        return;
    };
    let files = cached_sorted_siblings(dir);
    if files.len() < 2 {
        return;
    }
    let idx = position_case_insensitive(&files, cur_path).unwrap_or(0) as i32;
    let n = files.len() as i32;
    let ni = ((idx + delta) % n + n) % n; // wrap around at both ends
    let next_path = &files[ni as usize];
    let next = next_path.to_string_lossy().into_owned();
    if !same_file_name_ignore_case(next_path, cur_path) {
        request_load(hwnd, &next);
        // Read ahead in the direction of travel (issue #20), so a run of ←/→ steps decodes
        // ahead of the user instead of making them wait at each stop. TWO files, not one:
        // a cold decode measures ~250 ms for a 12 MP photo, so a single lookahead only
        // covers someone pausing at least that long on each frame — anyone arrowing faster
        // outruns it immediately. Two matches `MAX_PREFETCH_IN_FLIGHT`, which is the real
        // ceiling; asking for more would just queue work the user has already run past.
        for step in 1..=2 {
            let ahead = (((ni + delta * step) % n + n) % n) as usize;
            content::spawn_prefetch(files[ahead].to_string_lossy().into_owned());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position_case_insensitive_matches_differently_cased_names() {
        let files = vec![
            std::path::PathBuf::from(r"C:\photos\Image1.JPG"),
            std::path::PathBuf::from(r"C:\photos\image2.jpg"),
        ];
        let cur = std::path::Path::new(r"C:\photos\image1.jpg");
        assert_eq!(position_case_insensitive(&files, cur), Some(0));
    }

    #[test]
    fn position_case_insensitive_misses_are_none_not_zero() {
        // A real miss must come back `None` (so the caller can fall back deliberately),
        // never a value that happens to look like a valid index.
        let files = vec![std::path::PathBuf::from(r"C:\photos\a.jpg")];
        let cur = std::path::Path::new(r"C:\photos\not-there.jpg");
        assert_eq!(position_case_insensitive(&files, cur), None);
    }

    #[test]
    fn same_file_name_ignore_case_ignores_case_but_not_identity() {
        assert!(same_file_name_ignore_case(
            std::path::Path::new(r"C:\a\Pic.PNG"),
            std::path::Path::new(r"C:\b\pic.png"),
        ));
        assert!(!same_file_name_ignore_case(
            std::path::Path::new(r"C:\a\pic.png"),
            std::path::Path::new(r"C:\a\other.png"),
        ));
    }

    /// A folder gaining a file must be visible on the very next call, not served from a stale
    /// single-entry cache — the whole point of keying the cache on the directory's own mtime.
    #[test]
    fn cached_sorted_siblings_reflects_newly_added_files_after_mtime_change() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("st2k_navtest_{}_{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.jpg"), b"x").unwrap();

        let first = cached_sorted_siblings(&dir);
        assert_eq!(first.len(), 1, "{first:?}");

        // A new file changes the directory's own mtime; the cache must not keep serving the
        // stale one-file listing captured above.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(dir.join("b.jpg"), b"x").unwrap();
        let second = cached_sorted_siblings(&dir);
        assert_eq!(
            second.len(),
            2,
            "cache served a stale listing after the folder changed: {second:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
