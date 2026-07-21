//! 7-Zip (CB7 + generic `.7z`) cover extraction via sevenz-rust2 (pure Rust;
//! bzip2/zstd/brotli/lz4 features are OFF so it stays C-free — LZMA/LZMA2/Delta/BCJ2
//! cover every real CB7). We read the archive's metadata to pick the cover, then
//! decode just that entry (a solid block may decode a few neighbors — fine for a
//! thumbnail). The header's declared entry count is bounded inside the crate as of
//! 0.21 (`bounded_count`), so a crafted header can no longer abort us on parse;
//! the per-entry `read_file` allocation is bounded by the `solid_bomb` guard below.

use std::io::{Cursor, Read, Seek};

use sevenz_rust2::{ArchiveReader, Password};

use super::select::{dedupe_by_name, pick_covers, Entry};

/// Total decompressed bytes a SOLID one-pass scan may drain before giving up with
/// whatever images it has collected. Solid blocks decode front-to-back, so a huge
/// non-image neighbor stored ahead of the pictures costs its full decode; this
/// bounds that cost in Explorer's thumbnail host.
const SOLID_SCAN_BUDGET: u64 = 512 * 1024 * 1024;

pub fn extract(bytes: &[u8]) -> Option<Vec<u8>> {
    extract_seek(Cursor::new(bytes))
}

/// Like [`extract`], but over any seekable reader — used to stream an oversized CB7
/// cover off the shell's IStream (sevenz-rust2 reads metadata + the one entry without
/// buffering the whole archive).
pub fn extract_seek<R: Read + Seek>(source: R) -> Option<Vec<u8>> {
    extract_seek_n(source, 1).and_then(|mut v| (!v.is_empty()).then(|| v.swap_remove(0)))
}

/// Up to `want` cover images over any seekable reader — the multi-image
/// generalization of [`extract_seek`] feeding the generic-archive contact sheet.
/// Non-solid archives decode ONLY the chosen entries (each seeks to its own pack
/// stream); a solid archive is drained in ONE sequential pass that captures the
/// targets as they stream by and stops after the last one (repeated `read_file`
/// calls would re-decode the block once per image).
pub fn extract_seek_n<R: Read + Seek>(source: R, want: usize) -> Option<Vec<Vec<u8>>> {
    let mut reader = ArchiveReader::new(source, Password::empty()).ok()?;

    // SAFETY: in a SOLID archive, reaching any entry decodes the whole block up to
    // it, and sevenz-rust2's `read_file` eagerly `Vec::with_capacity(entry.size)`
    // per neighbor from the attacker-declared uncompressed size. A tiny crafted
    // .cb7 whose neighbor declares 100 GiB would abort the host before our cover
    // is reached. Refuse a solid archive that contains any oversized entry (a
    // real comic page / photo is well under 32 MiB). Non-solid archives only
    // decode the chosen entries, which pick_covers already bounds.
    let (is_solid, solid_bomb) = {
        let a = reader.archive();
        (a.is_solid, a.is_solid && a.files.iter().any(|f| f.size() > super::MAX_COVER))
    };
    if solid_bomb {
        return None;
    }

    let entries: Vec<Entry> = reader
        .archive()
        .files
        .iter()
        .take(super::MAX_LIST_ENTRIES)
        .map(|f| Entry {
            name: f.name().to_string(),
            is_dir: f.is_directory(),
            size: f.size(),
        })
        .collect();

    let picks = dedupe_by_name(pick_covers(&entries, want), &entries);
    if picks.is_empty() {
        return None;
    }

    let out = if is_solid && picks.len() > 1 {
        collect_solid(&mut reader, &picks, &entries)
    } else {
        // Non-solid (or a single target): read_file seeks straight to each entry's
        // own pack stream — nothing else is decompressed.
        picks
            .iter()
            .filter_map(|&i| {
                let data = reader.read_file(&entries[i].name).ok()?;
                (!data.is_empty() && data.len() as u64 <= super::MAX_COVER).then_some(data)
            })
            .collect()
    };
    (!out.is_empty()).then_some(out)
}

/// ONE sequential pass over a solid archive's blocks, capturing every picked entry
/// as it streams by. Stops as soon as all targets are captured or the drain budget
/// is spent (returning what was collected — the sheet degrades gracefully). Output
/// is re-ordered to match `picks` (cover first), not archive order.
fn collect_solid<R: Read + Seek>(
    reader: &mut ArchiveReader<R>,
    picks: &[usize],
    entries: &[Entry],
) -> Vec<Vec<u8>> {
    use std::collections::HashMap;
    // Mutable: each name is REMOVED as it's captured, so a later duplicate-named
    // physical entry (legal in 7z) drains to the sink instead of overwriting an
    // already-captured slot.
    let mut targets: HashMap<&str, usize> =
        picks.iter().enumerate().map(|(rank, &i)| (entries[i].name.as_str(), rank)).collect();
    let mut found: Vec<Option<Vec<u8>>> = vec![None; picks.len()];
    let mut remaining = picks.len();
    let mut drained: u64 = 0;
    let _ = reader.for_each_entries(&mut |entry: &sevenz_rust2::ArchiveEntry,
                                          rd: &mut dyn Read| {
        if let Some(rank) = targets.remove(entry.name()) {
            let mut buf = Vec::with_capacity(entry.size().min(super::MAX_COVER) as usize);
            let ok = rd.take(super::MAX_COVER).read_to_end(&mut buf).is_ok();
            drained = drained.saturating_add(buf.len() as u64);
            if !ok {
                // A failed mid-entry read leaves the SHARED solid stream desynced —
                // the next capture would read misaligned bytes and store garbage
                // (the crate's own solid path aborts the whole walk on any error).
                // Stop with whatever was already captured.
                return Ok(false);
            }
            if !buf.is_empty() {
                found[rank] = Some(buf);
            }
            remaining -= 1;
        } else {
            // A solid neighbor must be decoded to get past it — drain it to nowhere,
            // but never let a pathological archive burn unbounded CPU: past the
            // budget, stop with whatever images were already captured.
            drained = drained
                .saturating_add(std::io::copy(rd, &mut std::io::sink()).unwrap_or(u64::MAX));
        }
        Ok(remaining > 0 && drained < SOLID_SCAN_BUDGET)
    });
    found.into_iter().flatten().collect()
}

/// List up to `max` of a 7-Zip archive's entries from metadata only (no block decode, no bomb risk).
pub fn list(bytes: &[u8], max: usize) -> Option<Vec<Entry>> {
    let reader = ArchiveReader::new(Cursor::new(bytes), Password::empty()).ok()?;
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
