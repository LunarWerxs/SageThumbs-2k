//! RAR (CBR) cover extraction via the `unrar` crate (RarLab UnRAR, behind the
//! `rar` feature — decompression-only use is permitted by the UnRAR license).
//!
//! UnRAR works on file paths and can't random-seek, so we spill the stream to a
//! temp file, LIST entries to pick the cover, then RE-OPEN in processing mode
//! and skip to that entry (mirrors DarkThumbs' list-then-reopen).

use std::io::Write;

use unrar::Archive;

use super::select::{pick_cover, Entry};

pub fn extract(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut tmp = tempfile::NamedTempFile::new().ok()?;
    tmp.write_all(bytes).ok()?;
    tmp.flush().ok()?;
    let path = tmp.path().to_path_buf();

    // Pass 1: list → pick the cover entry.
    let mut entries = Vec::new();
    for e in Archive::new(&path).open_for_listing().ok()? {
        let Ok(h) = e else { continue };
        entries.push(Entry {
            name: h.filename.to_string_lossy().into_owned(),
            is_dir: h.is_directory(),
            size: h.unpacked_size as u64,
        });
    }
    let idx = pick_cover(&entries)?;
    let target = entries[idx].name.clone();

    // Pass 2: process — skip to the target, then read it.
    let mut archive = Archive::new(&path).open_for_processing().ok()?;
    loop {
        let cursor = match archive.read_header() {
            Ok(Some(c)) => c,
            _ => return None,
        };
        let is_target = cursor.entry().filename.to_string_lossy() == target;
        archive = if is_target {
            let (data, _next) = cursor.read().ok()?;
            if data.is_empty() || data.len() as u64 > super::MAX_COVER {
                return None;
            }
            return Some(data);
        } else {
            cursor.skip().ok()?
        };
    }
}
