//! Hex dump for a file the viewer can't otherwise render: an unknown binary used to fall
//! straight to the InfoCard (icon + name + size, nothing about what's actually inside it).
//! QuickLook ships one; we didn't.
//!
//! Rendered as MARKDOWN, not a dedicated `ContentKind` — deliberately. A new content kind would
//! need `window.rs`'s `ContentKind` enum and `paint.rs`'s per-kind paint dispatch, both outside
//! this task's file list, and it would have to reinvent scrolling/selection/Ctrl+F from scratch.
//! A fenced ```` ``` ```` code block already IS a fixed-width monospace panel with all three
//! working (`markdown.rs::paint_code`: "Code isn't wrapped (line-per-line)"), which is exactly
//! what a hex dump needs and nothing a dedicated kind would add on top.
//!
//! Hooked like `dbdoc`/`mailmsg` in SHAPE (`to_markdown` returning `None` is the "not for me"
//! fall-through the caller already knows how to handle) but with the OPPOSITE trigger. Those two
//! fire before `classify` runs, because each recognizes a specific format by extension. There is
//! no extension for "is an unknown binary" — the only thing that means that is `classify` ITSELF
//! having already tried everything else and landed on `InfoCard`. So this runs AFTER `classify`,
//! as the one case that verdict is allowed to be reconsidered from
//! (`loader::resolve_hex_or_card` / its `load_static` twin) — `classify` stays untouched, per
//! this repo's standing rule for the DB/mail hooks.

use super::content::{human_size, read_capped};
use super::docconv::{fence_for, md_cell};

/// Bytes read from disk — the I/O bound. A 4 GB file must never be pulled toward memory just to
/// show its first few hundred lines; `read_capped` already reads at most this many (+1, to
/// detect truncation) and nothing here ever asks it for more.
const HEX_READ_BYTES: usize = 64 * 1024;

/// Bytes actually turned into dump lines — the RENDER bound, independent of the read bound
/// above on purpose. Even with the bytes already safely in memory, laying out an unbounded
/// fenced code block is its own cost the read cap does nothing to stop: `markdown.rs::
/// paint_code` doesn't wrap or virtualize, so every line is its own GDI draw call. Smaller than
/// the read cap so truncation can always be decided without changing what's actually shown.
const HEX_RENDER_BYTES: usize = 16 * 1024;

/// Bytes shown per row — the width a classic hex dump is built on.
const ROW: usize = 16;

/// Format `bytes` as a classic hex dump: an offset (relative to `base_offset`, at least 8 hex
/// digits), 16 bytes of hex per row with the usual mid-row gap, and an ASCII gutter (`.` for
/// anything outside the printable range `0x20..=0x7e`). A pure function of the bytes, so it's
/// tested without touching a file.
///
/// Every row iterates the full `0..ROW` regardless of how many bytes it actually has, writing
/// three padding spaces for a missing byte instead of skipping it — that's what keeps the `|`
/// gutter starting at the same column on a short final row as on every full row above it.
pub(super) fn format_hex_dump(bytes: &[u8], base_offset: u64) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity((bytes.len() / ROW + 1) * 78);
    for (row_idx, chunk) in bytes.chunks(ROW).enumerate() {
        let offset = base_offset + (row_idx * ROW) as u64;
        let _ = write!(out, "{offset:08x}  ");
        for i in 0..ROW {
            match chunk.get(i) {
                Some(b) => {
                    let _ = write!(out, "{b:02x} ");
                }
                None => out.push_str("   "), // pad a missing byte's 3 columns
            }
            if i == 7 {
                out.push(' '); // the classic extra gap halfway through the row
            }
        }
        out.push('|');
        for &b in chunk {
            out.push(if (0x20..=0x7e).contains(&b) {
                b as char
            } else {
                '.'
            });
        }
        out.push_str("|\n");
    }
    out
}

/// The hex-dump view for `path` as markdown, or `None` when there's nothing to show: a
/// directory, an unreadable path, or an empty file. The caller (`loader::resolve_hex_or_card`
/// and its `load_static` twin) falls back to the ordinary info card on `None`, exactly like
/// `dbdoc`/`mailmsg`'s own `to_markdown`.
pub(super) fn to_markdown(path: &str) -> Option<String> {
    let p = std::path::Path::new(path);
    if p.is_dir() {
        return None; // never a byte-for-byte dump of "here are some directory entries"
    }
    let (bytes, read_capped_flag) = read_capped(path, HEX_READ_BYTES)?;
    if bytes.is_empty() {
        return None;
    }
    let total_len = std::fs::metadata(path)
        .map(|m| m.len())
        .unwrap_or(bytes.len() as u64);
    let shown_len = bytes.len().min(HEX_RENDER_BYTES);
    let truncated = read_capped_flag || bytes.len() > HEX_RENDER_BYTES;
    let dump = format_hex_dump(&bytes[..shown_len], 0);

    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());
    let mut md = format!("# {}\n\n", md_cell(&name));
    if truncated {
        // Matches `dbdoc::render`'s own truncation note in SHAPE (a hardcoded English sentence,
        // not a locale key) — that module already made this exact call for the same kind of
        // aside (a truncation note carrying two numbers), and a two-number sentence doesn't
        // decompose into the word-for-word fragments this repo's `t()` keys otherwise cover
        // (see `ic_items`: a single localized noun after a Rust-formatted count).
        md.push_str(&format!(
            "*Showing the first {} of {}.*\n\n",
            human_size(shown_len as u64),
            human_size(total_len)
        ));
    } else {
        md.push_str(&format!("*{}*\n\n", human_size(total_len)));
    }
    // Every byte in `dump` came from an untrusted file, including its ASCII gutter — a run of
    // sixteen 0x60 bytes writes sixteen literal backticks into one line. They can never stand
    // ALONE on a line here (CommonMark only closes a fence at a line of nothing but the fence
    // character, and every row is offset+hex+pipes first), but `fence_for` is free and is
    // exactly the defence `dbdoc`'s own DDL fence already relies on for the identical class of
    // untrusted text, so there's no reason to reason case-by-case about whether THIS content
    // needs it too.
    let fence = fence_for(&dump);
    md.push_str(&fence);
    md.push('\n');
    md.push_str(&dump);
    md.push_str(&fence);
    md.push('\n');
    Some(md)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_row_formats_offset_hex_and_ascii_gutter() {
        let bytes: Vec<u8> = (0..16).collect();
        let dump = format_hex_dump(&bytes, 0);
        assert_eq!(
            dump,
            "00000000  00 01 02 03 04 05 06 07  08 09 0a 0b 0c 0d 0e 0f |................|\n"
        );
    }

    #[test]
    fn a_short_final_row_pads_the_hex_column_so_the_ascii_gutter_still_aligns() {
        let dump = format_hex_dump(b"Hi!", 0);
        // 5 missing bytes before the mid-row gap (3 cols each), then the gap itself plus 8 more
        // missing bytes after it — computed, not hand-counted, so a layout change here can't
        // silently drift the test out of sync with the code it's checking.
        let expected = format!(
            "00000000  48 69 21 {}{}|Hi!|\n",
            " ".repeat(3 * 5),
            " ".repeat(1 + 3 * 8),
        );
        assert_eq!(dump, expected);
    }

    #[test]
    fn non_printable_bytes_render_as_dots_in_the_ascii_gutter() {
        // 0x20 (space) and 0x7e ('~') are the printable-ASCII boundary; everything else here
        // (0x00, 0x1f, 0x7f, 0xff) must not survive into the gutter as itself.
        let dump = format_hex_dump(&[0x00, 0x1f, 0x20, 0x7e, 0x7f, 0xff], 0);
        assert!(dump.ends_with("|.. ~..|\n"), "got: {dump:?}");
    }

    #[test]
    fn the_offset_advances_by_16_bytes_each_row() {
        let dump = format_hex_dump(&[0u8; 20], 0);
        let lines: Vec<&str> = dump.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("00000000  "));
        assert!(lines[1].starts_with("00000010  "));
    }

    #[test]
    fn a_nonzero_base_offset_is_reflected_in_the_first_rows_label() {
        let dump = format_hex_dump(&[0u8; 4], 0x1000);
        assert!(dump.starts_with("00001000  "));
    }

    /// Every test file lives under a `std::process::id()`-suffixed temp dir so concurrent
    /// `cargo test` runs (this repo's standing convention) can't collide on the same paths.
    fn temp_file(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("st2k_hexview_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn a_small_file_renders_untruncated_with_its_full_size_noted() {
        let path = temp_file("small.bin", &[0x89, b'P', b'N', b'G']);
        let md = to_markdown(path.to_str().unwrap()).expect("small binary must render");
        assert!(md.contains("small.bin"));
        assert!(md.contains("89 50 4e 47"));
        assert!(!md.contains("Showing the first"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_file_past_the_render_cap_is_truncated_and_says_so() {
        let bytes = vec![b'A'; HEX_RENDER_BYTES + ROW];
        let path = temp_file("big.bin", &bytes);
        let md = to_markdown(path.to_str().unwrap()).expect("oversized binary must still render");
        assert!(md.contains("Showing the first"));
        // Exactly HEX_RENDER_BYTES / ROW full rows of "41 41 ... 41" were rendered — never a
        // partial extra row reaching past the render cap.
        let full_row = "41 41 41 41 41 41 41 41  41 41 41 41 41 41 41 41";
        assert_eq!(md.matches(full_row).count(), HEX_RENDER_BYTES / ROW);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_empty_file_declines_rather_than_render_nothing() {
        let path = temp_file("empty.bin", &[]);
        assert!(to_markdown(path.to_str().unwrap()).is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_directory_declines() {
        let dir = std::env::temp_dir().join(format!("st2k_hexview_dir_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(to_markdown(dir.to_str().unwrap()).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_fence_longer_than_any_backtick_run_in_the_dump_is_always_chosen() {
        // Sixteen 0x60 bytes in a row: the ASCII gutter for that row is sixteen literal
        // backticks, all on one line — exactly the case `fence_for` exists to outrun.
        let dump = format_hex_dump(&[0x60; 16], 0);
        let fence = fence_for(&dump);
        assert!(
            fence.len() > 16,
            "fence must out-run the longest run in the content"
        );
    }
}
