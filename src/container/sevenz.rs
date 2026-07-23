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

/// How far (in DECOMPRESSED bytes) the solid cover scan will decode before it
/// gives up. A solid block only decodes front-to-back, so reaching a cover costs
/// the full decode of every entry stored ahead of it. A big project `.7z`
/// (thousands of small files — none over `MAX_COVER`, so `solid_bomb` never trips)
/// buries its first image tens of MB in; the old 512 MiB budget let a single
/// thumbnail decompress most of a multi-hundred-MB archive, pegging Explorer's
/// host with a CPU + I/O spike. We only peek this far: covers within it thumbnail,
/// anything deeper degrades to the stock icon. The reach cost of the first cover is
/// predicted from the entry sizes up front, so a too-deep cover costs NO decode.
const SOLID_SCAN_BUDGET: u64 = 8 * 1024 * 1024;

/// Cap on how many compression blocks a solid cover scan will engage with. A solid
/// archive packs its files into a HANDFUL of large blocks — that is what "solid"
/// means — so a real cover archive is one or a few blocks. `sevenz_rust2`'s
/// `ArchiveReader::for_each_entries` builds a fresh decode stack and seeks the source
/// ONCE PER BLOCK, and its outer block loop ignores our closure's early `Ok(false)`:
/// after we've captured our covers (or spent the peek budget) it keeps walking every
/// remaining block anyway. A crafted "solid" `.7z` that declares tens of thousands of
/// tiny junk blocks therefore turns a cheap front-cover scan into a long seek-and-build
/// spin — a linear crafted-header amplification. We refuse such an archive from the
/// declared block count alone, BEFORE any decode, bounding the walk to a small
/// constant. This is also defense-in-depth for the allocation angle: the enabled
/// codecs (COPY/LZMA/LZMA2/BCJ/Delta/BCJ2 — ppmd/aes are off) allocate their decode
/// dictionaries lazily (only on a read our closure skips past budget) and fallibly
/// (`try_reserve`, so a hostile dict size degrades to a decode error, not an allocator
/// abort), so today the walk can't OOM-abort the panic=abort host; capping the block
/// count keeps that true even if a future crate bump regresses to eager per-block
/// allocation. Well above any real cover archive, finite against a crafted one.
const SOLID_MAX_BLOCKS: usize = 4096;

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

    // Cheap conservative early-out: a SOLID archive whose metadata declares any
    // oversized entry is refused outright. Real covers (comic page / photo) are
    // well under 32 MiB, and this keeps a crafted header from steering the scan
    // toward a 100 GiB entry. The solid scan below is otherwise self-bounding
    // (per-entry reads capped at MAX_COVER, total decode capped by the budget),
    // so this is a fast pre-filter, not the safety mechanism it once was.
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

    let out = if is_solid {
        // A solid block decodes front-to-back, so name-selecting a cover that sits
        // deep in the block would decompress everything before it. Pick by PHYSICAL
        // order instead (earliest images are cheapest to reach), bounded by the
        // peek budget — see `solid_covers`.
        solid_covers(&mut reader, want, &entries, SOLID_MAX_BLOCKS)
    } else {
        // Non-solid: every entry seeks to its own pack stream, so decoding a chosen
        // cover never touches its neighbors. Pick by name (page order) and read only
        // the picks.
        let picks = dedupe_by_name(pick_covers(&entries, want), &entries);
        if picks.is_empty() {
            return None;
        }
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

/// Cover images from a SOLID archive, cost-bounded. A solid block decodes only
/// front-to-back, so covers are picked by PHYSICAL (archive) order — the earliest
/// images are the cheapest to reach — and the scan never decodes past
/// [`SOLID_SCAN_BUDGET`] decompressed bytes.
///
/// The reach cost of the physically-first eligible image (the decompressed bytes
/// stored ahead of it) is predicted from the entry sizes BEFORE any decode: prior
/// solid folders decode in full and its own folder decodes up to it, which is
/// exactly the sum of the preceding entries' sizes. If even that first cover sits
/// past the budget we bail with ZERO decode (the stock icon, cheaply) — this is
/// what keeps clicking a huge project `.7z` from spiking the CPU/disk. Otherwise
/// one sequential pass captures the first `want` eligible images as the block
/// streams by, in archive order.
///
/// `max_blocks` bounds how many compression blocks the underlying walk may engage
/// with (see [`SOLID_MAX_BLOCKS`]) — an archive declaring more is refused from
/// metadata, before any decode, since `for_each_entries` walks every block even
/// after our closure stops.
fn solid_covers<R: Read + Seek>(
    reader: &mut ArchiveReader<R>,
    want: usize,
    entries: &[Entry],
    max_blocks: usize,
) -> Vec<Vec<u8>> {
    use std::collections::HashSet;

    // Pathological-shape gate, from metadata only (no decode): a solid archive that
    // declares far more blocks than any real cover archive needs would make the walk
    // below build a decode stack and seek once per block regardless of our early stop.
    // Decline to the stock icon instead of paying for a crafted many-block header.
    if reader.archive().blocks.len() > max_blocks {
        return Vec::new();
    }

    // The name-filtered candidate set (the junk / scanlation / exotic-vs-native
    // rules `pick_covers` applies), membership only — physical order is imposed
    // below, so the ordering `pick_covers` also does is discarded here.
    let eligible: HashSet<&str> =
        pick_covers(entries, usize::MAX).into_iter().map(|i| entries[i].name.as_str()).collect();
    let Some(first) = entries.iter().position(|e| eligible.contains(e.name.as_str())) else {
        return Vec::new();
    };
    // Predicted reach cost of that first cover. Saturating in case a crafted header
    // declares absurd sizes (the sum can't then panic on overflow).
    let reach = entries[..first].iter().fold(0u64, |acc, e| acc.saturating_add(e.size));
    if reach > SOLID_SCAN_BUDGET {
        return Vec::new();
    }

    let mut found: Vec<Vec<u8>> = Vec::with_capacity(want);
    let mut captured: HashSet<String> = HashSet::new();
    let mut drained: u64 = 0;
    let _ = reader.for_each_entries(&mut |entry: &sevenz_rust2::ArchiveEntry,
                                          rd: &mut dyn Read| {
        // Done — enough images, or the peek budget is spent. Bail at the TOP,
        // BEFORE reading `rd`: the crate decodes lazily and its outer per-block loop
        // keeps walking blocks after an inner stop, so only a no-read return here
        // avoids decoding the entries of later blocks.
        if found.len() >= want || drained >= SOLID_SCAN_BUDGET {
            return Ok(false);
        }
        let name = entry.name();
        if eligible.contains(name) && !captured.contains(name) {
            // Capture on first sighting of the name (7z legally allows two entries
            // with the same name — take one, drain any later twin).
            let mut buf = Vec::with_capacity(entry.size().min(super::MAX_COVER) as usize);
            let ok = rd.take(super::MAX_COVER).read_to_end(&mut buf).is_ok();
            drained = drained.saturating_add(buf.len() as u64);
            if !ok {
                // A failed mid-entry read leaves the SHARED solid stream desynced —
                // the crate aborts the walk on any error, so stop with what we have.
                return Ok(false);
            }
            if !buf.is_empty() {
                captured.insert(name.to_string());
                found.push(buf);
            }
        } else {
            // A non-target neighbor must be decoded to advance the solid stream to
            // the next entry — drain it to nowhere, capped at the remaining budget
            // so one large neighbor can't overshoot (a partial drain only ever
            // precedes the top-of-callback bail, so it never desyncs a later read).
            let room = SOLID_SCAN_BUDGET.saturating_sub(drained);
            drained = drained.saturating_add(
                std::io::copy(&mut rd.take(room), &mut std::io::sink()).unwrap_or(u64::MAX),
            );
        }
        Ok(found.len() < want && drained < SOLID_SCAN_BUDGET)
    });
    found
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

#[cfg(test)]
mod tests {
    use super::*;

    // Paths relative to THIS file (src/container/sevenz.rs) -> repo tests/. Both are
    // tiny SOLID .7z archives (one folder, >1 substream). Regenerate with
    // tests/fixtures/sevenz/make_fixtures.py if the format assumptions ever change.
    const SOLID_ORDER: &[u8] = include_bytes!("../../tests/fixtures/sevenz/solid_order.7z");
    const SOLID_BURIED: &[u8] = include_bytes!("../../tests/fixtures/sevenz/solid_buried.7z");

    /// A solid block decodes front-to-back, so the cover is chosen by PHYSICAL
    /// (archive) order, not by name. `solid_order.7z` stores [m.png, a.png]; "a.png"
    /// sorts first by name (the old pick), but m.png is physically first and cheapest
    /// to reach, so it must win now.
    #[test]
    fn solid_cover_is_physically_first_not_name_sorted() {
        let covers = extract_seek_n(Cursor::new(SOLID_ORDER), 1).expect("a cover");
        assert_eq!(covers, vec![b"PHYSICALLY-FIRST-IMAGE".to_vec()]);
    }

    /// The contact sheet (want > 1) captures the eligible images in ARCHIVE order.
    #[test]
    fn solid_contact_sheet_is_in_archive_order() {
        let covers = extract_seek_n(Cursor::new(SOLID_ORDER), 4).expect("covers");
        assert_eq!(
            covers,
            vec![
                b"PHYSICALLY-FIRST-IMAGE".to_vec(),
                b"name-sorts-first-but-second-physically".to_vec(),
            ]
        );
    }

    /// The peek budget: `solid_buried.7z` stores its only image behind ~12 MiB of
    /// non-image data in the solid block, past the 8 MiB budget. Reaching it would
    /// mean decompressing that whole prefix — the exact CPU/disk spike this bounds —
    /// so the scan declines to the stock icon instead. The reach cost is predicted
    /// from the header, so this decodes nothing.
    #[test]
    fn solid_cover_past_budget_declines() {
        assert!(extract_seek_n(Cursor::new(SOLID_BURIED), 4).is_none());
    }

    /// Rebuild the `Entry` list `extract_seek_n` feeds `solid_covers`, so the block-cap
    /// tests below can drive `solid_covers` directly with a chosen cap (a genuine
    /// thousands-of-blocks solid fixture can't be produced with py7zr, which packs solid
    /// archives into one block — so we exercise the guard by lowering the cap instead).
    fn reader_and_entries(archive: &[u8]) -> (ArchiveReader<Cursor<&[u8]>>, Vec<Entry>) {
        let reader = ArchiveReader::new(Cursor::new(archive), Password::empty()).expect("reader");
        let entries = reader
            .archive()
            .files
            .iter()
            .take(super::super::MAX_LIST_ENTRIES)
            .map(|f| Entry {
                name: f.name().to_string(),
                is_dir: f.is_directory(),
                size: f.size(),
            })
            .collect();
        (reader, entries)
    }

    /// Real solid cover archives declare only a handful of blocks, so the block-count
    /// guard must never reject them: the same fixture that yields a cover at the real
    /// cap keeps yielding it. (Guards the false-positive direction.)
    #[test]
    fn solid_block_guard_admits_normal_archive_at_real_cap() {
        let (mut reader, entries) = reader_and_entries(SOLID_ORDER);
        assert!(
            reader.archive().blocks.len() <= SOLID_MAX_BLOCKS,
            "a normal solid fixture must sit under the block cap"
        );
        let covers = solid_covers(&mut reader, 1, &entries, SOLID_MAX_BLOCKS);
        assert_eq!(covers, vec![b"PHYSICALLY-FIRST-IMAGE".to_vec()]);
    }

    /// A solid archive with more blocks than the cap is refused from metadata alone,
    /// WITHOUT decoding — the defense against a crafted many-block header that would
    /// otherwise make `for_each_entries` seek-and-build once per junk block. A cap of 0
    /// forces the guard on the tiny real fixture, standing in for the (impractical to
    /// generate) thousands-of-blocks archive. (Guards the reject direction.)
    #[test]
    fn solid_block_guard_declines_when_over_cap() {
        let (mut reader, entries) = reader_and_entries(SOLID_ORDER);
        assert!(
            !reader.archive().blocks.is_empty(),
            "fixture must have at least one block for a cap of 0 to trip the guard"
        );
        let covers = solid_covers(&mut reader, 4, &entries, 0);
        assert!(covers.is_empty(), "over-cap block count must decline to no cover");
    }
}
