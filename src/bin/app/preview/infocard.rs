//! Fallback "info card" for files the viewer can't render as an image (and for folders):
//! the shell icon + the file name + a modified-date / size line. Never an error box — a
//! calm card is the graceful degradation (plan §2).

use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    CreateSolidBrush, DeleteObject, FillRect, SelectObject, SetBkMode, SetTextColor,
    DT_END_ELLIPSIS, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, HDC, TRANSPARENT,
};

// Shared with content.rs's archive listing (and dbdoc.rs's DB view) so a file's size reads
// identically in every pane instead of drifting between separately maintained formatters.
use super::content::human_size;
use super::paint::draw_text;
use windows::Win32::UI::Shell::{SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON};
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, DrawIconEx, DI_NORMAL, HICON};
// `file_attributes()` is what lets the recursive folder-size walk below recognize a
// reparse point (junction/symlink) from the directory listing itself, with no extra
// syscall and no risk of following it first.
use std::os::windows::fs::MetadataExt;

/// The data shown on the card. Owns the shell `HICON` (destroyed on drop).
pub(super) struct InfoCard {
    name: String,
    detail: String,
    icon: Option<HICON>,
}

impl InfoCard {
    /// The card's visible text (name + detail line) — what the viewer's Ctrl+C copies.
    pub(super) fn copy_text(&self) -> String {
        format!("{}\r\n{}", self.name, self.detail)
    }
}

/// The card shown when Space was pressed on a selection with no file behind it — a Recycle Bin
/// entry, This PC, or any other virtual shell item. Before 2026-09-08 that keypress did nothing
/// at all: no window, no message, nothing in the log, so the feature looked broken rather than
/// inapplicable. This is deliberately a CARD and not a browsable panel; a Recycle Bin / This PC
/// panel was considered and rejected by the owner on 2026-08-07 (ROADMAP, "Considered and
/// rejected"). No icon: there is no file to take one from.
pub(super) fn virtual_item() -> InfoCard {
    InfoCard {
        name: crate::win::t("ic_virtual_title").to_string(),
        detail: crate::win::t("ic_virtual_detail").to_string(),
        icon: None,
    }
}

impl Drop for InfoCard {
    fn drop(&mut self) {
        if let Some(icon) = self.icon {
            unsafe {
                let _ = DestroyIcon(icon);
            }
        }
    }
}

/// Gather the card for `path`: the shell's large icon, the leaf name, and a one-line
/// detail (modified date + size for a file, item count for a folder).
pub(super) unsafe fn gather(path: &str) -> InfoCard {
    let p = std::path::Path::new(path);
    let name = p
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
        .to_string();
    let icon = shell_icon(path);
    let detail = if p.is_dir() {
        let count = std::fs::read_dir(p).map(|it| it.count()).unwrap_or(0);
        let items = crate::i18n::t("ic_items");
        // Recursive total, bounded (see `walk_folder_size`) — `ic_size_more_than` is a NEW
        // locale key (English "more than"), reported to the integrator; it is not yet in
        // en.toml, so it shows the missing-key marker until that lands.
        let walk = walk_folder_size(p);
        let size = if walk.truncated {
            format!(
                "{} {}",
                crate::i18n::t("ic_size_more_than"),
                human_size(walk.bytes)
            )
        } else {
            human_size(walk.bytes)
        };
        match modified_string(path) {
            Some(w) => format!("{count} {items}  ·  {size}  ·  {w}"),
            None => format!("{count} {items}  ·  {size}"),
        }
    } else {
        let sz = human_size(std::fs::metadata(p).map(|m| m.len()).unwrap_or(0));
        match modified_string(path) {
            Some(w) => format!("{sz}  ·  {w}"),
            None => sz,
        }
    };
    InfoCard { name, detail, icon }
}

/// Paint the card centered in `rc`: bg fill, then a left-aligned icon + name/detail text
/// block, centered vertically. Colours come from the caller's resolved dark/light palette.
pub(super) unsafe fn paint(
    hwnd: HWND,
    hdc: HDC,
    rc: &RECT,
    card: &InfoCard,
    bg: u32,
    text: u32,
    subtle: u32,
) {
    let brush = CreateSolidBrush(COLORREF(bg));
    FillRect(hdc, rc, brush);
    let _ = DeleteObject(brush.into());

    let sc = |v: i32| crate::win::dpi_scale(hwnd, v);
    let cw = rc.right - rc.left;
    let ch = rc.bottom - rc.top;
    let icon_sz = sc(48);
    let gap = sc(16);
    let text_w = sc(300);
    let block_w = icon_sz + gap + text_w;
    let x0 = rc.left + (cw - block_w).max(0) / 2;
    let icon_y = rc.top + (ch - icon_sz).max(0) / 2;

    if let Some(icon) = card.icon {
        let _ = DrawIconEx(hdc, x0, icon_y, icon, icon_sz, icon_sz, 0, None, DI_NORMAL);
    }

    SetBkMode(hdc, TRANSPARENT);
    let tx = x0 + icon_sz + gap;
    let line_h = sc(22);
    let name_top = rc.top + (ch - line_h * 2).max(0) / 2;
    let fmt = DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS;

    let name_font = crate::win::gui_font_sized(hwnd, 15, 600);
    let oldf = SelectObject(hdc, name_font.into());
    SetTextColor(hdc, COLORREF(text));
    let mut name_rc = RECT {
        left: tx,
        top: name_top,
        right: tx + text_w,
        bottom: name_top + line_h,
    };
    let mut name_w: Vec<u16> = card.name.encode_utf16().collect();
    draw_text(hdc, &mut name_w, &mut name_rc, fmt);

    let det_font = crate::win::gui_font_sized(hwnd, 12, 400);
    SelectObject(hdc, det_font.into());
    SetTextColor(hdc, COLORREF(subtle));
    let mut det_rc = RECT {
        left: tx,
        top: name_top + line_h,
        right: tx + text_w,
        bottom: name_top + line_h * 2,
    };
    let mut det_w: Vec<u16> = card.detail.encode_utf16().collect();
    draw_text(hdc, &mut det_w, &mut det_rc, fmt);

    SelectObject(hdc, oldf);
}

/// The shell's large icon for `path` (via `SHGetFileInfoW`). Caller-owned `HICON`.
unsafe fn shell_icon(path: &str) -> Option<HICON> {
    let wide = crate::win::wide(path);
    let mut sfi = SHFILEINFOW::default();
    let r = SHGetFileInfoW(
        PCWSTR(wide.as_ptr()),
        windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES(0),
        Some(&mut sfi),
        core::mem::size_of::<SHFILEINFOW>() as u32,
        SHGFI_ICON | SHGFI_LARGEICON,
    );
    if r == 0 || sfi.hIcon.is_invalid() {
        None
    } else {
        Some(sfi.hIcon)
    }
}

/// The file's last-modified time as "YYYY-MM-DD HH:MM" in local time. `None` if the file
/// can't be stat'd.
unsafe fn modified_string(path: &str) -> Option<String> {
    use windows::Win32::Foundation::SYSTEMTIME;
    use windows::Win32::Storage::FileSystem::{
        GetFileAttributesExW, GetFileExInfoStandard, WIN32_FILE_ATTRIBUTE_DATA,
    };
    use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};

    let wide = crate::win::wide(path);
    let mut data = WIN32_FILE_ATTRIBUTE_DATA::default();
    GetFileAttributesExW(
        PCWSTR(wide.as_ptr()),
        GetFileExInfoStandard,
        &mut data as *mut _ as *mut core::ffi::c_void,
    )
    .ok()?;
    let mut utc = SYSTEMTIME::default();
    FileTimeToSystemTime(&data.ftLastWriteTime, &mut utc).ok()?;
    // Convert UTC → the machine's current local time zone (None = active TZ).
    let mut st = SYSTEMTIME::default();
    if SystemTimeToTzSpecificLocalTime(None, &utc, &mut st).is_err() {
        st = utc; // fall back to UTC if the conversion fails
    }
    Some(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute
    ))
}

// ── RECURSIVE FOLDER SIZE ───────────────────────────────────────────────────────────────────
//
// `gather` runs synchronously on the UI thread (`loader::dispatch_fallback_kind` calls it
// directly from the `WM_APP_LOAD_RESOLVED` handler), so an unbounded walk here is an unbounded
// UI stall. Three bounds, each catching a case the other two don't: a flat folder of a million
// files blows the entry cap long before depth matters; a synthetic deeply-nested tree blows
// depth with almost no entries visited; a slow/dead network share can blow the wall clock on an
// otherwise perfectly ordinary-sized tree. Hitting any of them makes the reported total a FLOOR
// (`FolderSize::truncated`) — the caller must say "more than N", never present a confidently
// wrong number as if it were the whole tree.

/// `FILE_ATTRIBUTE_REPARSE_POINT` (winnt.h). Kept as a local literal rather than pulling in
/// `windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT` for the one bit test
/// below needs.
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

/// Entries visited before the walk gives up and reports a floor.
const FOLDER_WALK_MAX_ENTRIES: usize = 20_000;
/// Directory nesting the walk will still descend into (it keeps whatever it already summed).
const FOLDER_WALK_MAX_DEPTH: usize = 64;
/// Wall-clock budget for the whole walk — however many entries, however shallow the tree, a
/// dead or saturated network share must not hang the UI thread computing a number nobody is
/// waiting on that long for.
const FOLDER_WALK_BUDGET: std::time::Duration = std::time::Duration::from_millis(200);

/// The result of a (possibly bounded) folder-size walk.
struct FolderSize {
    bytes: u64,
    truncated: bool,
}

/// Whether `attrs` (a directory-listing attribute bitmask) marks a reparse point — a directory
/// junction or a symlink. Pulled out as its own pure function so "never follow a reparse point"
/// is testable without actually creating one (that needs privileges this box may not have).
fn is_reparse_point(attrs: u32) -> bool {
    attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// Sum file sizes under `root`, recursively, bounded by entry count / depth / wall clock (see
/// the constants above). Never descends into, or sizes, a reparse point: `DirEntry::metadata`
/// reports the entry's OWN attributes rather than following it (`lstat`, not `stat`), so a
/// junction or a symlink is recognizable straight from the directory listing, before anything
/// would need to follow it to find out — that is what makes a symlink loop back to an ancestor
/// directory unrepresentable here, rather than merely unlikely.
fn walk_folder_size(root: &std::path::Path) -> FolderSize {
    walk_folder_size_bounded(
        root,
        FOLDER_WALK_MAX_ENTRIES,
        FOLDER_WALK_MAX_DEPTH,
        FOLDER_WALK_BUDGET,
    )
}

/// [`walk_folder_size`] with the bounds as parameters, so a test can prove each bound's
/// truncation behaviour against a small temp tree instead of needing tens of thousands of real
/// files or a genuinely deep real directory to exercise the production constants.
fn walk_folder_size_bounded(
    root: &std::path::Path,
    max_entries: usize,
    max_depth: usize,
    budget: std::time::Duration,
) -> FolderSize {
    let start = std::time::Instant::now();
    let mut bytes: u64 = 0;
    let mut visited: usize = 0;
    let mut truncated = false;
    let mut stack: Vec<(std::path::PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    'walk: while let Some((dir, depth)) = stack.pop() {
        if start.elapsed() > budget {
            truncated = true;
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue; // unreadable subdirectory (permissions, or it vanished) — skip it
        };
        for entry in entries.flatten() {
            if visited >= max_entries {
                truncated = true;
                break 'walk;
            }
            visited += 1;
            let Ok(meta) = entry.metadata() else {
                continue; // vanished between the listing and the stat — skip it
            };
            if is_reparse_point(meta.file_attributes()) {
                continue; // a junction/symlink: never sized, never descended into
            }
            if meta.is_dir() {
                if depth + 1 > max_depth {
                    truncated = true;
                    continue;
                }
                stack.push((entry.path(), depth + 1));
            } else {
                bytes += meta.len();
            }
        }
        if start.elapsed() > budget {
            truncated = true;
            break;
        }
    }
    FolderSize { bytes, truncated }
}

#[cfg(test)]
mod folder_size_tests {
    use super::*;

    /// Every test tree lives under a `std::process::id()`-suffixed temp dir so concurrent
    /// `cargo test` runs (this repo's standing convention) can't collide on the same paths.
    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("st2k_infocard_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn is_reparse_point_reads_the_one_bit_it_cares_about() {
        const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
        assert!(is_reparse_point(FILE_ATTRIBUTE_REPARSE_POINT));
        assert!(is_reparse_point(
            FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY
        ));
        assert!(!is_reparse_point(FILE_ATTRIBUTE_DIRECTORY));
        assert!(!is_reparse_point(0));
    }

    #[test]
    fn sums_file_sizes_recursively_across_subdirectories() {
        let root = temp_dir("sum");
        std::fs::write(root.join("r.bin"), [0u8; 3]).unwrap();
        let sub = root.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("s.bin"), [0u8; 5]).unwrap();

        let got = walk_folder_size(&root);
        assert_eq!(got.bytes, 8);
        assert!(!got.truncated);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_entries_bound_reports_a_floor_not_a_wrong_total() {
        let root = temp_dir("entries");
        // Five same-sized files: with a cap below 5, the exact byte total is order-dependent,
        // but which SUBSET got counted is not what this test is about — only that hitting the
        // cap is reported truthfully (`truncated`) instead of silently answering "the total".
        for i in 0..5 {
            std::fs::write(root.join(format!("f{i}.bin")), [0u8; 10]).unwrap();
        }

        let full = walk_folder_size_bounded(&root, 100, 64, std::time::Duration::from_secs(5));
        assert_eq!(full.bytes, 50);
        assert!(!full.truncated);

        let capped = walk_folder_size_bounded(&root, 2, 64, std::time::Duration::from_secs(5));
        assert!(capped.truncated);
        assert!(
            capped.bytes < full.bytes,
            "a floor must never claim the same total as the untruncated walk"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_depth_bound_stops_descending_but_keeps_what_it_already_summed() {
        let root = temp_dir("depth");
        std::fs::write(root.join("r.bin"), [0u8; 4]).unwrap(); // depth 0
        let a = root.join("a");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::write(a.join("a.bin"), [0u8; 4]).unwrap(); // depth 1
        let b = a.join("b");
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(b.join("b.bin"), [0u8; 4]).unwrap(); // depth 2 — must be excluded below

        // max_depth = 1 admits depth-0 and depth-1 files, but refuses to push "b" (depth 2).
        let got = walk_folder_size_bounded(&root, 100, 1, std::time::Duration::from_secs(5));
        assert_eq!(got.bytes, 8, "depth-2 content must not be counted");
        assert!(got.truncated);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_wall_clock_bound_of_zero_always_truncates() {
        let root = temp_dir("clock");
        std::fs::write(root.join("r.bin"), [0u8; 4]).unwrap();

        // Any real directory read takes far more than a single nanosecond, so this bound fires
        // deterministically without depending on machine speed or load.
        let got = walk_folder_size_bounded(&root, 100, 64, std::time::Duration::from_nanos(1));
        assert!(got.truncated);

        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod human_size_tests {
    use super::*;

    /// The bug this guarded: infocard.rs used to carry its OWN `human_size`, formatting a
    /// sub-1024-byte size as "512 bytes" while content.rs's canonical version (already the one
    /// `dbdoc.rs` imports) formats the same value as "512 B" — the same file's size could read
    /// differently depending which pane showed it. Now that infocard imports the shared
    /// function instead of defining its own, this must hold.
    #[test]
    fn the_info_card_uses_the_shared_canonical_size_format() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(0), "0 B");
    }
}
