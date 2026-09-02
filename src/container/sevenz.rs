//! 7-Zip (CB7 + generic `.7z`) cover extraction via sevenz-rust2 (pure Rust;
//! bzip2/zstd/brotli/lz4 features are OFF so it stays C-free — LZMA/LZMA2/Delta/BCJ2
//! cover every real CB7). We read the archive's metadata to pick the cover, then
//! decode just that entry (a solid block may decode a few neighbors — fine for a
//! thumbnail). The header's declared entry count is bounded inside the crate as of
//! 0.21 (`bounded_count`), so a crafted header can no longer abort us on parse;
//! per-entry and aggregate output allocations are checked against small budgets
//! before any selected file is decoded.

use std::io::{BufReader, Cursor, Read, Seek};

use sevenz_rust2::{Archive, ArchiveReader, BlockDecoder, Password};

use super::select::{cover_candidates, dedupe_by_name, pick_covers, CoverPrefs, Entry};

/// Coalesce sevenz-rust2's many small reads before they reach a shell `IStream`.
///
/// The crate's encoded-header decoder can request one byte at a time. That is
/// cheap against a memory cursor or the local filesystem cache, but each request
/// against an SMB/cloud shell stream can become a separate remote round trip. A
/// real 909 MB project archive with a 235 KB encoded header issued thousands of
/// tiny reads and was still blocked after two minutes. One modest sequential
/// buffer turns that into a handful of bulk reads while preserving random seeks.
const SOURCE_BUFFER_BYTES: usize = 256 * 1024;

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

/// Non-solid contact sheets share the same small aggregate decode ceiling as a
/// solid scan. Previously four 32 MiB picks could consume 128 MiB synchronously
/// for one shell thumbnail.
const NON_SOLID_COVERS_BUDGET: u64 = SOLID_SCAN_BUDGET;

#[inline]
fn complete_item_fits(prefix: u64, item: u64, budget: u64) -> bool {
    prefix.saturating_add(item) <= budget
}

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

/// Called only from the in-memory `extract_cover` dispatch, which has no per-request
/// settings snapshot to thread through, so it reads the preferences itself here
/// rather than carrying a `prefs` parameter its caller can't supply.
pub fn extract(bytes: &[u8]) -> Option<Vec<u8>> {
    extract_seek(Cursor::new(bytes), &CoverPrefs::from_settings())
}

/// Like [`extract`], but over any seekable reader — used to stream an oversized CB7
/// cover off the shell's IStream (sevenz-rust2 reads metadata + the one entry without
/// buffering the whole archive).
pub fn extract_seek<R: Read + Seek>(source: R, prefs: &CoverPrefs) -> Option<Vec<u8>> {
    extract_seek_n(source, 1, prefs).and_then(|mut v| (!v.is_empty()).then(|| v.swap_remove(0)))
}

/// Up to `want` cover images over any seekable reader — the multi-image
/// generalization of [`extract_seek`] feeding the generic-archive contact sheet.
/// Non-solid archives decode ONLY the chosen entries (each seeks to its own pack
/// stream); a solid archive is drained in ONE sequential pass that captures the
/// targets as they stream by and stops after the last one (repeated `read_file`
/// calls would re-decode the block once per image).
pub fn extract_seek_n<R: Read + Seek>(
    source: R,
    want: usize,
    prefs: &CoverPrefs,
) -> Option<Vec<Vec<u8>>> {
    if want == 0 {
        return None;
    }

    // Parse the archive before choosing a reader shape. Solid extraction is
    // sequential and does not need ArchiveReader's cloned-name HashMap (18k long
    // project paths made that measurable); non-solid extraction drives each
    // selected one-file block by exact index so duplicate names cannot redirect
    // a budgeted read. Keeping ownership of the source also lets the solid loop
    // use BlockDecoder and actually honor an early stop — ArchiveReader 0.21.3
    // otherwise continues constructing every later block after Ok(false).
    let mut source = BufReader::with_capacity(SOURCE_BUFFER_BYTES, source);
    let password = Password::empty();
    let archive = Archive::read(&mut source, &password).ok()?;

    // Cheap conservative early-out: a SOLID archive whose metadata declares any
    // oversized entry is refused outright. Real covers (comic page / photo) are
    // well under 32 MiB, and this keeps a crafted header from steering the scan
    // toward a 100 GiB entry. The solid scan below is otherwise self-bounding
    // (per-entry reads capped at MAX_COVER, total decode capped by the budget),
    // so this is a fast pre-filter, not the safety mechanism it once was.
    let (is_solid, solid_bomb) = {
        (
            archive.is_solid,
            archive.is_solid && archive.files.iter().any(|f| f.size() > super::MAX_COVER),
        )
    };
    if solid_bomb {
        return None;
    }

    let entries: Vec<Entry> = archive
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
        solid_covers(
            &mut source,
            &archive,
            &password,
            want,
            &entries,
            SOLID_MAX_BLOCKS,
            prefs,
        )
    } else {
        // Non-solid: every entry seeks to its own pack stream, so decoding a chosen
        // cover never touches its neighbors. Pick by name (page order) and read only
        // the picks, under one aggregate cover-byte budget.
        let picks = dedupe_by_name(pick_covers(&entries, want, prefs), &entries);
        if picks.is_empty() {
            return None;
        }
        non_solid_covers(&mut source, &archive, &password, &picks, &entries)
    };
    (!out.is_empty()).then_some(out)
}

/// Decode selected entries from a non-solid archive by their exact file index.
///
/// `ArchiveReader::read_file(name)` indexes duplicate member names last-wins.
/// Budgeting the first `cover.png` and then decoding a much larger later
/// `cover.png` would therefore bypass the pre-decode aggregate cap. A non-solid
/// entry has its own one-file block, so drive that block directly and verify its
/// sole entry is the exact metadata object we budgeted.
fn non_solid_covers<R: Read + Seek>(
    source: &mut R,
    archive: &Archive,
    password: &Password,
    picks: &[usize],
    entries: &[Entry],
) -> Vec<Vec<u8>> {
    let mut remaining = NON_SOLID_COVERS_BUDGET;
    let mut found = Vec::with_capacity(picks.len());

    for &i in picks {
        let (Some(file), Some(entry)) = (archive.files.get(i), entries.get(i)) else {
            continue;
        };
        // The selection metadata is built directly from archive.files in the
        // same order. Check each pick against the CURRENT remaining budget.
        // Every byte actually emitted is charged even when validation fails;
        // a zero-byte failure remains free so a later valid pick can be tried.
        if file.size() != entry.size || file.size() > remaining {
            continue;
        }
        let Some(block_index) = archive
            .stream_map
            .file_block_index
            .get(i)
            .copied()
            .flatten()
        else {
            continue;
        };

        let target = file as *const sevenz_rust2::ArchiveEntry;
        let decoder = BlockDecoder::new(1, block_index, archive, password, source);
        // `archive.is_solid == false` promises one substream per block. Refuse
        // an inconsistent/crafted map rather than draining an unbudgeted neighbor.
        if decoder.entries().len() != 1 || !std::ptr::eq(&decoder.entries()[0], file) {
            continue;
        }

        let mut captured = None;
        let mut spent = 0u64;
        let decoded = decoder.for_each_entries(&mut |actual, rd| {
            if !std::ptr::eq(actual, target) || actual.size() > remaining {
                return Ok(false);
            }
            let mut data = Vec::with_capacity(actual.size() as usize);
            let ok = rd.take(remaining).read_to_end(&mut data).is_ok();
            // Charge every byte the codec emitted, even if CRC/length validation
            // later rejects the entry. Otherwise four corrupt picks could each
            // consume the full 8 MiB allowance while none reduced `remaining`.
            spent = data.len() as u64;
            if ok && !data.is_empty() && data.len() as u64 == actual.size() {
                captured = Some(data);
            }
            Ok(false)
        });
        remaining = remaining.saturating_sub(spent);
        if decoded.is_ok() {
            if let Some(data) = captured {
                found.push(data);
            }
        }
    }
    found
}

/// Does this entry's filename (lowercased, last path component) look like an
/// explicit cover name? Mirrors `select::pick_covers`'s "cover"-named preference
/// grouping — that helper's own `filename()` is private to its module, so this
/// repeats the same lowercase-final-component check rather than widening its
/// visibility for one caller.
fn is_cover_named(name: &str) -> bool {
    name.rsplit(['/', '\\'])
        .next()
        .unwrap_or(name)
        .to_ascii_lowercase()
        .contains("cover")
}

/// The `want`-sized target list for a solid cover scan, in the order the scan
/// should try to capture them: cover-eligible entries (the junk / scanlation /
/// exotic-vs-native rules `pick_covers` applies), cover-named ones first when
/// `prefs.prefer_cover` is set, archive/physical order preserved WITHIN each group —
/// mirrors `pick_covers`'s grouping without natural-sorting either group, since a
/// solid block's decode cost depends on physical order, not name order. Pure and
/// archive-decode-free so it can be pinned directly against synthetic entries.
fn solid_targets(entries: &[Entry], want: usize, prefs: &CoverPrefs) -> Vec<usize> {
    let eligible_idx = cover_candidates(entries, prefs);
    let ordered: Vec<usize> = if prefs.prefer_cover {
        let (mut covers, rest): (Vec<usize>, Vec<usize>) = eligible_idx
            .into_iter()
            .partition(|&i| is_cover_named(&entries[i].name));
        covers.extend(rest);
        covers
    } else {
        eligible_idx
    };
    ordered.into_iter().take(want).collect()
}

/// Cover images from a SOLID archive, cost-bounded. A solid block decodes only
/// front-to-back, so covers are picked by PHYSICAL (archive) order among the
/// eligible entries — the earliest images are the cheapest to reach — except that
/// an explicit "cover"-named entry (the same preference [`pick_covers`] applies
/// non-solid, when the caller's `CoverPrefs::prefer_cover` is on) still leads
/// the pick even when a plainer page sits physically ahead of it: reaching it may
/// cost more to decode, but showing a random early page instead of the comic's own
/// declared cover is the wrong trade for a thumbnail. The scan never decodes past
/// [`SOLID_SCAN_BUDGET`] decompressed bytes either way.
///
/// The reach cost of the chosen first target (the decompressed bytes stored ahead
/// of it, cover-named or not) is predicted from the entry sizes BEFORE any decode:
/// prior solid folders decode in full and its own folder decodes up to it, which is
/// exactly the sum of the preceding entries' sizes. If even that first target sits
/// past the budget we bail with ZERO decode (the stock icon, cheaply) — this is
/// what keeps clicking a huge project `.7z` from spiking the CPU/disk. Otherwise
/// one sequential pass captures the chosen `want` targets as the block streams by,
/// draining (not capturing) every entry in between, cover-eligible or not.
///
/// `max_blocks` bounds how many compression blocks the underlying walk may engage
/// with (see [`SOLID_MAX_BLOCKS`]) — an archive declaring more is refused from
/// metadata, before any decode, since `for_each_entries` walks every block even
/// after our closure stops.
fn solid_covers<R: Read + Seek>(
    source: &mut R,
    archive: &Archive,
    password: &Password,
    want: usize,
    entries: &[Entry],
    max_blocks: usize,
    prefs: &CoverPrefs,
) -> Vec<Vec<u8>> {
    use std::collections::HashSet;

    // Pathological-shape gate, from metadata only (no decode): a solid archive that
    // declares far more blocks than any real cover archive needs would make the walk
    // below build a decode stack and seek once per block regardless of our early stop.
    // Decline to the stock icon instead of paying for a crafted many-block header.
    if archive.blocks.len() > max_blocks {
        return Vec::new();
    }

    let targets = solid_targets(entries, want, prefs);
    if targets.is_empty() {
        return Vec::new();
    }
    let target_names: HashSet<&str> = targets.iter().map(|&i| entries[i].name.as_str()).collect();
    // The walk is sequential, so the cost to reach ANY chosen target is the sum of
    // every entry (eligible or not) before it — the smallest physical index among
    // the chosen targets is therefore the one the budget precheck must cover first.
    // `targets` was already checked non-empty above, but `min()` is matched rather
    // than `.expect()`-ed: no panicking accessor in shell-crate non-test code.
    let Some(&first) = targets.iter().min() else {
        return Vec::new();
    };
    // Predicted reach cost of that first target. Saturating in case a crafted
    // header declares absurd sizes (the sum can't then panic on overflow).
    let reach = entries[..first]
        .iter()
        .fold(0u64, |acc, e| acc.saturating_add(e.size));
    // Include the complete first target itself. The old check bounded only the
    // bytes BEFORE it, then allowed a 32 MiB cover read after almost exhausting
    // the 8 MiB budget. If the first useful result cannot fit in full, decline
    // without decoding anything.
    if !complete_item_fits(reach, entries[first].size, SOLID_SCAN_BUDGET) {
        return Vec::new();
    }

    let mut found: Vec<Vec<u8>> = Vec::with_capacity(want);
    let mut captured: HashSet<String> = HashSet::new();
    let mut drained: u64 = 0;
    let mut each = |entry: &sevenz_rust2::ArchiveEntry,
                    rd: &mut dyn Read|
     -> Result<bool, sevenz_rust2::Error> {
        // Done — enough images, or the peek budget is spent. Bail at the TOP,
        // BEFORE reading `rd`.
        if found.len() >= want || drained >= SOLID_SCAN_BUDGET {
            return Ok(false);
        }
        let name = entry.name();
        if target_names.contains(name) && !captured.contains(name) {
            let room = SOLID_SCAN_BUDGET.saturating_sub(drained);
            if entry.size() > room {
                // A partial image is useless and would violate the advertised hard
                // total budget. Stop before asking the decoder for any of it.
                return Ok(false);
            }
            // Capture on first sighting of the name (7z legally allows two entries
            // with the same name — take one, drain any later twin).
            let mut buf = Vec::with_capacity(entry.size() as usize);
            let ok = rd.take(room).read_to_end(&mut buf).is_ok();
            drained = drained.saturating_add(buf.len() as u64);
            if !ok || buf.len() as u64 != entry.size() {
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
    };

    // Drive blocks ourselves so Ok(false) really stops the outer loop. The pinned
    // ArchiveReader::for_each_entries ignores that Boolean between blocks, causing
    // pointless seek/decode-stack work after the result or budget is complete.
    for block_index in 0..archive.blocks.len() {
        let decoder = BlockDecoder::new(1, block_index, archive, password, source);
        match decoder.for_each_entries(&mut each) {
            Ok(true) => {}
            Ok(false) | Err(_) => break,
        }
    }
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
    use sevenz_rust2::{ArchiveEntry, ArchiveWriter, EncoderConfiguration, EncoderMethod};
    use std::cell::Cell;
    use std::rc::Rc;

    // Paths relative to THIS file (src/container/sevenz.rs) -> repo tests/. Both are
    // tiny SOLID .7z archives (one folder, >1 substream). Regenerate with
    // tests/fixtures/sevenz/make_fixtures.py if the format assumptions ever change.
    const SOLID_ORDER: &[u8] = include_bytes!("../../tests/fixtures/sevenz/solid_order.7z");
    const SOLID_BURIED: &[u8] = include_bytes!("../../tests/fixtures/sevenz/solid_buried.7z");

    /// Registry-default cover prefs, for tests that don't care about the values.
    fn default_prefs() -> CoverPrefs {
        CoverPrefs {
            prefer_cover: true,
            sort: true,
            skip_scanlation: false,
        }
    }

    /// [`default_prefs`] with `prefer_cover` overridden, for the tests that toggle it.
    fn prefs_with_cover(prefer_cover: bool) -> CoverPrefs {
        CoverPrefs {
            prefer_cover,
            ..default_prefs()
        }
    }

    /// A solid block decodes front-to-back, so the cover is chosen by PHYSICAL
    /// (archive) order, not by name. `solid_order.7z` stores [m.png, a.png]; "a.png"
    /// sorts first by name (the old pick), but m.png is physically first and cheapest
    /// to reach, so it must win now.
    #[test]
    fn solid_cover_is_physically_first_not_name_sorted() {
        let covers =
            extract_seek_n(Cursor::new(SOLID_ORDER), 1, &default_prefs()).expect("a cover");
        assert_eq!(covers, vec![b"PHYSICALLY-FIRST-IMAGE".to_vec()]);
    }

    /// The contact sheet (want > 1) captures the eligible images in ARCHIVE order.
    #[test]
    fn solid_contact_sheet_is_in_archive_order() {
        let covers = extract_seek_n(Cursor::new(SOLID_ORDER), 4, &default_prefs()).expect("covers");
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
        assert!(extract_seek_n(Cursor::new(SOLID_BURIED), 4, &default_prefs()).is_none());
    }

    /// Rebuild the `Entry` list `extract_seek_n` feeds `solid_covers`, so the block-cap
    /// tests below can drive `solid_covers` directly with a chosen cap (a genuine
    /// thousands-of-blocks solid fixture can't be produced with py7zr, which packs solid
    /// archives into one block — so we exercise the guard by lowering the cap instead).
    fn archive_and_entries(
        bytes: &[u8],
    ) -> (BufReader<Cursor<&[u8]>>, Archive, Password, Vec<Entry>) {
        let mut source = BufReader::with_capacity(SOURCE_BUFFER_BYTES, Cursor::new(bytes));
        let password = Password::empty();
        let archive = Archive::read(&mut source, &password).expect("archive");
        let entries = archive
            .files
            .iter()
            .take(super::super::MAX_LIST_ENTRIES)
            .map(|f| Entry {
                name: f.name().to_string(),
                is_dir: f.is_directory(),
                size: f.size(),
            })
            .collect();
        (source, archive, password, entries)
    }

    /// Real solid cover archives declare only a handful of blocks, so the block-count
    /// guard must never reject them: the same fixture that yields a cover at the real
    /// cap keeps yielding it. (Guards the false-positive direction.)
    #[test]
    fn solid_block_guard_admits_normal_archive_at_real_cap() {
        let (mut source, archive, password, entries) = archive_and_entries(SOLID_ORDER);
        assert!(
            archive.blocks.len() <= SOLID_MAX_BLOCKS,
            "a normal solid fixture must sit under the block cap"
        );
        let covers = solid_covers(
            &mut source,
            &archive,
            &password,
            1,
            &entries,
            SOLID_MAX_BLOCKS,
            &default_prefs(),
        );
        assert_eq!(covers, vec![b"PHYSICALLY-FIRST-IMAGE".to_vec()]);
    }

    /// A solid archive with more blocks than the cap is refused from metadata alone,
    /// WITHOUT decoding — the defense against a crafted many-block header that would
    /// otherwise make `for_each_entries` seek-and-build once per junk block. A cap of 0
    /// forces the guard on the tiny real fixture, standing in for the (impractical to
    /// generate) thousands-of-blocks archive. (Guards the reject direction.)
    #[test]
    fn solid_block_guard_declines_when_over_cap() {
        let (mut source, archive, password, entries) = archive_and_entries(SOLID_ORDER);
        assert!(
            !archive.blocks.is_empty(),
            "fixture must have at least one block for a cap of 0 to trip the guard"
        );
        let covers = solid_covers(
            &mut source,
            &archive,
            &password,
            4,
            &entries,
            0,
            &default_prefs(),
        );
        assert!(
            covers.is_empty(),
            "over-cap block count must decline to no cover"
        );
    }

    struct CountingReader<R> {
        inner: R,
        reads: Rc<Cell<usize>>,
    }

    impl<R: Read> Read for CountingReader<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.reads.set(self.reads.get() + 1);
            self.inner.read(buf)
        }
    }

    impl<R: Seek> Seek for CountingReader<R> {
        fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(pos)
        }
    }

    /// The seekable source is buffered before sevenz-rust2 sees it. This locks the
    /// SMB/cloud regression: its byte-at-a-time header requests must collapse into
    /// a handful of underlying reads instead of one remote round trip per byte.
    #[test]
    fn seek_extraction_coalesces_underlying_reads() {
        let reads = Rc::new(Cell::new(0));
        let source = CountingReader {
            inner: Cursor::new(SOLID_ORDER),
            reads: Rc::clone(&reads),
        };
        let covers = extract_seek_n(source, 1, &default_prefs()).expect("cover");
        assert_eq!(covers, vec![b"PHYSICALLY-FIRST-IMAGE".to_vec()]);
        assert!(
            reads.get() <= 8,
            "buffered archive parse/extract made {} underlying reads",
            reads.get()
        );
    }

    /// Prefix + cover bytes share one hard 8 MiB budget. A cover that starts
    /// inside the budget but ends outside it must be rejected up front; the old
    /// code checked only `reach` and could decode roughly 40 MiB.
    #[test]
    fn solid_budget_includes_the_complete_first_cover() {
        let entries = [
            Entry {
                name: "prefix.bin".into(),
                is_dir: false,
                size: SOLID_SCAN_BUDGET - 1024,
            },
            Entry {
                name: "cover.png".into(),
                is_dir: false,
                size: 2048,
            },
        ];
        let first = 1;
        let reach = entries[..first]
            .iter()
            .fold(0u64, |acc, e| acc.saturating_add(e.size));
        assert!(
            reach < SOLID_SCAN_BUDGET,
            "fixture must expose the old check's hole"
        );
        assert!(
            !complete_item_fits(reach, entries[first].size, SOLID_SCAN_BUDGET),
            "production budget helper must reject the incomplete fit"
        );
    }

    /// A solid comic with an explicit `cover.jpg` must not show a random
    /// earlier page. `page01.png` is physically first — the OLD pick — but
    /// `cover.jpg` is cover-named and must win when the preference is on.
    #[test]
    fn solid_targets_prefers_a_cover_named_entry_over_an_earlier_plain_page() {
        let entries = [
            Entry {
                name: "page01.png".into(),
                is_dir: false,
                size: 100,
            },
            Entry {
                name: "cover.jpg".into(),
                is_dir: false,
                size: 100,
            },
            Entry {
                name: "page02.png".into(),
                is_dir: false,
                size: 100,
            },
        ];
        assert_eq!(
            solid_targets(&entries, 1, &prefs_with_cover(true)),
            vec![1],
            "cover.jpg (index 1) must be the sole target when the preference is on"
        );
        // With the preference off, physical order alone decides (the pre-G66 rule).
        assert_eq!(
            solid_targets(&entries, 1, &prefs_with_cover(false)),
            vec![0],
            "page01.png (index 0, physically first) must win with the preference off"
        );
    }

    /// A contact sheet still fills out with the remaining pages, in archive order,
    /// after the cover-named entry leads.
    #[test]
    fn solid_targets_contact_sheet_leads_with_cover_then_archive_order() {
        let entries = [
            Entry {
                name: "page01.png".into(),
                is_dir: false,
                size: 100,
            },
            Entry {
                name: "cover.jpg".into(),
                is_dir: false,
                size: 100,
            },
            Entry {
                name: "page02.png".into(),
                is_dir: false,
                size: 100,
            },
        ];
        assert_eq!(
            solid_targets(&entries, 3, &prefs_with_cover(true)),
            vec![1, 0, 2]
        );
    }

    #[test]
    fn cover_named_detection_is_case_insensitive_and_path_aware() {
        assert!(is_cover_named("COVER.jpg"));
        assert!(is_cover_named("scans/Cover.png"));
        assert!(is_cover_named("front-cover.png"));
        assert!(!is_cover_named("page01.png"));
    }

    /// A four-cell contact sheet used to admit four MAX_COVER entries (128 MiB
    /// total). No one such item fits the new aggregate budget, and successful
    /// items decrement the same remaining-byte counter before the next decode.
    #[test]
    fn non_solid_contact_sheet_has_an_aggregate_budget() {
        assert!(!complete_item_fits(
            0,
            super::super::MAX_COVER,
            NON_SOLID_COVERS_BUDGET
        ));
        let mut remaining = NON_SOLID_COVERS_BUDGET;
        for size in [3 * 1024 * 1024, 5 * 1024 * 1024] {
            assert!(size <= remaining);
            remaining -= size;
        }
        assert_eq!(remaining, 0);
        assert!(1 > remaining, "the next successful byte must be refused");
    }

    /// `ArchiveReader::read_file(name)` is last-wins for duplicate names. Build a
    /// non-solid archive whose small first cover is followed by a same-named item
    /// larger than the whole aggregate budget: exact-index extraction must return
    /// the first bytes instead of decoding/rejecting the later duplicate.
    #[test]
    fn non_solid_duplicate_name_decodes_the_budgeted_exact_entry() {
        let mut bytes = Vec::new();
        {
            let mut writer = ArchiveWriter::new(Cursor::new(&mut bytes)).expect("writer");
            writer.set_encrypt_header(false);
            writer.set_content_methods(vec![EncoderConfiguration::new(EncoderMethod::COPY)]);
            writer
                .push_archive_entry(ArchiveEntry::new_file("cover.png"), Some(b"FIRST" as &[u8]))
                .expect("first entry");
            let later = vec![0xCC; NON_SOLID_COVERS_BUDGET as usize + 1];
            writer
                .push_archive_entry(ArchiveEntry::new_file("cover.png"), Some(later.as_slice()))
                .expect("duplicate entry");
            writer.finish().expect("finish");
        }

        let parsed = ArchiveReader::new(Cursor::new(bytes.as_slice()), Password::empty())
            .expect("read generated archive");
        assert!(!parsed.archive().is_solid, "fixture must be non-solid");
        drop(parsed);

        let covers =
            extract_seek_n(Cursor::new(bytes), 1, &default_prefs()).expect("first exact cover");
        assert_eq!(covers, vec![b"FIRST".to_vec()]);
    }
}
