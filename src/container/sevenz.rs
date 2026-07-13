//! 7-Zip (CB7) cover extraction via sevenz-rust2 (pure Rust; zstd/brotli/lz4
//! features are OFF so it stays C-free — LZMA/LZMA2/Delta/BCJ2 cover every real
//! CB7). We read the archive's metadata to pick the cover, then decode just that
//! entry (a solid block may decode a few neighbors — fine for a thumbnail).

use std::io::{Cursor, Read, Seek};

use sevenz_rust2::{Password, SevenZReader};

use super::select::{pick_cover, Entry};

pub fn extract(bytes: &[u8]) -> Option<Vec<u8>> {
    extract_seek(Cursor::new(bytes))
}

/// Like [`extract`], but over any seekable reader — used to stream an oversized CB7
/// cover off the shell's IStream (sevenz-rust2 reads metadata + the one entry without
/// buffering the whole archive).
pub fn extract_seek<R: Read + Seek>(source: R) -> Option<Vec<u8>> {
    let mut reader = SevenZReader::new(source, Password::empty()).ok()?;

    // SAFETY: in a SOLID archive, read_file decodes the whole block to reach any
    // entry, and sevenz-rust2 eagerly `Vec::with_capacity(entry.size)` per
    // neighbor from the attacker-declared uncompressed size. A tiny crafted
    // .cb7 whose neighbor declares 100 GiB would abort the host before our cover
    // is reached. Refuse a solid archive that contains any oversized entry (a
    // real comic page is well under 32 MiB). Non-solid archives only decode the
    // chosen entry, which pick_cover already bounds.
    let solid_bomb = {
        let a = reader.archive();
        a.is_solid && a.files.iter().any(|f| f.size() > super::MAX_COVER)
    };
    if solid_bomb {
        return None;
    }

    let entries: Vec<Entry> = reader
        .archive()
        .files
        .iter()
        .map(|f| Entry {
            name: f.name().to_string(),
            is_dir: f.is_directory(),
            size: f.size(),
        })
        .collect();

    let idx = pick_cover(&entries)?;
    let target = entries[idx].name.clone();

    let data = reader.read_file(&target).ok()?;
    if data.is_empty() || data.len() as u64 > super::MAX_COVER {
        return None;
    }
    Some(data)
}

/// List up to `max` of a 7-Zip archive's entries from metadata only (no block decode, no bomb risk).
pub fn list(bytes: &[u8], max: usize) -> Option<Vec<Entry>> {
    let reader = SevenZReader::new(Cursor::new(bytes), Password::empty()).ok()?;
    Some(
        reader
            .archive()
            .files
            .iter()
            .take(max)
            .map(|f| Entry {
                name: f.name().to_string(),
                is_dir: f.is_directory(),
                size: f.size(),
            })
            .collect(),
    )
}
