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

/// The most a 7z's end header may declare before the archive is left alone (the listing is
/// parsed from it; a real 18,037-entry archive's is 235 KB).
pub(crate) const MAX_HEADER_BYTES: u64 = 4 << 20;

/// The most an ENCODED end header may unpack to. 7-Zip compresses its header by default, so
/// the small header on disk only points at a packed stream, and `sevenz_rust2` decodes that
/// stream until the unpack size it declares: a crafted one declaring terabytes of LZMA-packed
/// zeros grew the listing buffer until the allocation failed, which aborts the shell under
/// `panic = "abort"` (Dredd, 2026-09-23).
const MAX_UNPACKED_HEADER_BYTES: u64 = 64 << 20;

/// Is this 7z's header safe to hand to `sevenz_rust2`? Its start header must carry its own
/// CRC (a zeroed one sends the crate hunting for an end header a byte at a time through the
/// last megabyte), declare a non-empty end header no larger than [`MAX_HEADER_BYTES`], and an
/// encoded end header must declare no stream larger than [`MAX_UNPACKED_HEADER_BYTES`]. The
/// parse mirrors the crate's own (`read_pack_info` / `read_unpack_info` / `read_block`), so an
/// archive the crate reads is never refused here. Leaves the reader at an unspecified position.
pub(crate) fn header_is_safe<R: Read + Seek>(r: &mut R) -> bool {
    header_check(r).unwrap_or(false)
}

const K_END: u8 = 0x00;
const K_HEADER: u8 = 0x01;
const K_PACK_INFO: u8 = 0x06;
const K_UNPACK_INFO: u8 = 0x07;
const K_SIZE: u8 = 0x09;
const K_CRC: u8 = 0x0A;
const K_FOLDER: u8 = 0x0B;
const K_CODERS_UNPACK_SIZE: u8 = 0x0C;
const K_ENCODED_HEADER: u8 = 0x17;

/// Caps on the counts an encoded header's stream description may declare: a real one has one
/// folder of one to four coders.
const MAX_HEADER_COUNT: u64 = 64;

fn header_check<R: Read + Seek>(r: &mut R) -> Option<bool> {
    let mut start = [0u8; 32];
    r.seek(std::io::SeekFrom::Start(0)).ok()?;
    r.read_exact(&mut start).ok()?;
    let le32 = |at: usize| u32::from_le_bytes(start[at..at + 4].try_into().unwrap_or_default());
    let le64 = |at: usize| u64::from_le_bytes(start[at..at + 8].try_into().unwrap_or_default());
    if !super::is_7z(&start) || crc32(&start[12..32]) != le32(8) {
        return Some(false);
    }
    let (offset, size) = (le64(12), le64(20));
    if size == 0 || size > MAX_HEADER_BYTES {
        return Some(false);
    }
    r.seek(std::io::SeekFrom::Start(32u64.checked_add(offset)?))
        .ok()?;
    let mut next = vec![0u8; usize::try_from(size).ok()?];
    r.read_exact(&mut next).ok()?;
    match next.first() {
        Some(&K_HEADER) => Some(true),
        Some(&K_ENCODED_HEADER) => {
            let most = encoded_header_unpack_max(&mut HeaderCursor(&next[1..]))?;
            Some(most <= MAX_UNPACKED_HEADER_BYTES)
        }
        _ => Some(false),
    }
}

/// CRC-32 (IEEE), bitwise: only ever run over the start header's 20 bytes.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & (crc & 1).wrapping_neg());
        }
    }
    !crc
}

/// A byte reader over an end header, every read checked.
struct HeaderCursor<'a>(&'a [u8]);

impl HeaderCursor<'_> {
    fn u8(&mut self) -> Option<u8> {
        let (&b, rest) = self.0.split_first()?;
        self.0 = rest;
        Some(b)
    }

    fn skip(&mut self, n: u64) -> Option<()> {
        self.0 = self.0.get(usize::try_from(n).ok()?..)?;
        Some(())
    }

    /// 7-Zip's variable-length number: the first byte's leading one bits count the bytes that
    /// follow (little-endian), its remaining bits are the top of the value.
    fn number(&mut self) -> Option<u64> {
        let first = self.u8()?;
        let mut value = 0u64;
        for i in 0..8u32 {
            let mask = 0x80u8 >> i;
            if first & mask == 0 {
                let high = u64::from(first & mask.wrapping_sub(1));
                return Some(value | (high << (8 * i)));
            }
            value |= u64::from(self.u8()?) << (8 * i);
        }
        Some(value)
    }

    /// Step over the CRC-32s of those of `n` items the defined-vector says carry one.
    fn skip_digests(&mut self, n: u64) -> Option<()> {
        let defined = self.defined(n)?;
        self.skip(defined * 4)
    }

    /// Step over `n` numbers.
    fn skip_numbers(&mut self, n: u64) -> Option<()> {
        (0..n).try_for_each(|_| self.number().map(drop))
    }

    /// A count, refused past [`MAX_HEADER_COUNT`].
    fn count(&mut self) -> Option<u64> {
        self.number().filter(|&n| n <= MAX_HEADER_COUNT)
    }

    /// An "all defined, or a bit per item" vector: how many items it defines.
    fn defined(&mut self, n: u64) -> Option<u64> {
        if self.u8()? != 0 {
            return Some(n);
        }
        let bytes = self.0.get(..usize::try_from(n.div_ceil(8)).ok()?)?;
        let set = bytes.iter().map(|b| u64::from(b.count_ones())).sum();
        self.skip(n.div_ceil(8))?;
        Some(set)
    }
}

/// The largest stream an encoded header's description declares it unpacks to: its pack info is
/// stepped over, its folders read for how many streams each outputs, then their sizes.
fn encoded_header_unpack_max(c: &mut HeaderCursor) -> Option<u64> {
    let mut nid = c.u8()?;
    if nid == K_PACK_INFO {
        skip_pack_info(c)?;
        nid = c.u8()?;
    }
    if nid != K_UNPACK_INFO {
        return None;
    }
    let outputs = folders_outputs(c)?;
    if c.u8()? != K_CODERS_UNPACK_SIZE {
        return None;
    }
    (0..outputs).try_fold(0u64, |most, _| c.number().map(|n| most.max(n)))
}

fn skip_pack_info(c: &mut HeaderCursor) -> Option<()> {
    c.number()?; // pack position
    let streams = c.count()?;
    let nid = c.u8()?;
    let nid = optional_section(c, nid, K_SIZE, |c| c.skip_numbers(streams))?;
    let nid = optional_section(c, nid, K_CRC, |c| c.skip_digests(streams))?;
    (nid == K_END).then_some(())
}

/// When `nid` opens the optional section `kind`, read it with `body`; the id after it either way.
fn optional_section(
    c: &mut HeaderCursor,
    nid: u8,
    kind: u8,
    body: impl FnOnce(&mut HeaderCursor) -> Option<()>,
) -> Option<u8> {
    if nid != kind {
        return Some(nid);
    }
    body(c)?;
    c.u8()
}

/// The folder list: how many streams its folders output between them.
fn folders_outputs(c: &mut HeaderCursor) -> Option<u64> {
    if c.u8()? != K_FOLDER {
        return None;
    }
    let folders = c.count()?;
    if c.u8()? != 0 {
        return None; // external folder data: the crate refuses it too
    }
    (0..folders).try_fold(0u64, |n, _| folder_outputs(c).map(|o| n + o))
}

/// One folder's coders, bind pairs and packed-stream indices; how many streams it outputs.
fn folder_outputs(c: &mut HeaderCursor) -> Option<u64> {
    let coders = c.count()?;
    let (inputs, outputs) = (0..coders).try_fold((0u64, 0u64), |(i, o), _| {
        coder_streams(c).map(|(ci, co)| (i + ci, o + co))
    })?;
    let pairs = outputs.checked_sub(1)?;
    c.skip_numbers(pairs * 2)?;
    let packed = inputs.checked_sub(pairs)?;
    if packed > 1 {
        c.skip_numbers(packed)?;
    }
    Some(outputs)
}

/// One coder's description: how many streams it takes in and puts out.
fn coder_streams(c: &mut HeaderCursor) -> Option<(u64, u64)> {
    let bits = c.u8()?;
    if bits & 0x80 != 0 {
        return None; // alternative methods: the crate refuses them too
    }
    c.skip(u64::from(bits & 0x0F))?;
    let streams = if bits & 0x10 == 0 {
        (1, 1)
    } else {
        (c.count()?, c.count()?)
    };
    if bits & 0x20 != 0 {
        let props = c.number()?;
        c.skip(props)?;
    }
    Some(streams)
}

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
    if !header_is_safe(&mut source) {
        return None;
    }
    source.seek(std::io::SeekFrom::Start(0)).ok()?;
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
        if let Some((spent, data)) =
            non_solid_pick(source, archive, password, i, entries, remaining)
        {
            remaining = remaining.saturating_sub(spent);
            if let Some(data) = data {
                found.push(data);
            }
        }
    }
    found
}

/// Decode one budgeted non-solid pick: the entry at index `i` in its own one-file
/// block, charging every byte the codec emitted (whether or not it validated).
/// `None` means "skip this pick" (metadata mismatch, over budget, or a block map
/// inconsistent with the one-substream-per-block promise).
fn non_solid_pick<R: Read + Seek>(
    source: &mut R,
    archive: &Archive,
    password: &Password,
    i: usize,
    entries: &[Entry],
    remaining: u64,
) -> Option<(u64, Option<Vec<u8>>)> {
    let (Some(file), Some(entry)) = (archive.files.get(i), entries.get(i)) else {
        return None;
    };
    // The selection metadata is built directly from archive.files in the
    // same order. Check each pick against the CURRENT remaining budget.
    // Every byte actually emitted is charged even when validation fails;
    // a zero-byte failure remains free so a later valid pick can be tried.
    if file.size() != entry.size || file.size() > remaining {
        return None;
    }
    let block_index = archive
        .stream_map
        .file_block_index
        .get(i)
        .copied()
        .flatten()?;

    let target = file as *const sevenz_rust2::ArchiveEntry;
    let decoder = BlockDecoder::new(1, block_index, archive, password, source);
    // `archive.is_solid == false` promises one substream per block. Refuse
    // an inconsistent/crafted map rather than draining an unbudgeted neighbor.
    if decoder.entries().len() != 1 || !std::ptr::eq(&decoder.entries()[0], file) {
        return None;
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
    Some((spent, captured.filter(|_| decoded.is_ok())))
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
    use std::collections::{HashMap, HashSet};

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
    // Target name -> its position in `targets`, so the physical-order walk can stamp
    // every capture with the rank `solid_targets` gave it (cover-named first).
    let mut target_ranks: HashMap<&str, usize> = HashMap::new();
    for (rank, &i) in targets.iter().enumerate() {
        target_ranks.entry(entries[i].name.as_str()).or_insert(rank);
    }
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

    let mut found: Vec<(usize, Vec<u8>)> = Vec::with_capacity(want);
    let mut captured: HashSet<String> = HashSet::new();
    let mut drained: u64 = 0;
    let mut each = |entry: &sevenz_rust2::ArchiveEntry,
                    rd: &mut dyn Read|
     -> Result<bool, sevenz_rust2::Error> {
        solid_step(
            entry,
            rd,
            &mut found,
            &mut captured,
            &mut drained,
            want,
            &target_ranks,
        )
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
    // The walk captured in PHYSICAL order; return the images in `targets` order so a
    // cover-named entry leads the result exactly as `solid_targets` (and the doc above)
    // promises. Rank is unique per name, so this sort is a stable reordering.
    found.sort_by_key(|&(rank, _)| rank);
    found.into_iter().map(|(_, buf)| buf).collect()
}

/// One step of the solid-cover walk: capture the entry `rd` is streaming when it
/// is an unclaimed target that fits the remaining budget, otherwise drain it to
/// advance the solid stream. `Ok(false)` stops the block walk (done or spent).
fn solid_step(
    entry: &sevenz_rust2::ArchiveEntry,
    rd: &mut dyn Read,
    found: &mut Vec<(usize, Vec<u8>)>,
    captured: &mut std::collections::HashSet<String>,
    drained: &mut u64,
    want: usize,
    target_ranks: &std::collections::HashMap<&str, usize>,
) -> Result<bool, sevenz_rust2::Error> {
    // Done — enough images, or the peek budget is spent. Bail at the TOP,
    // BEFORE reading `rd`.
    if found.len() >= want || *drained >= SOLID_SCAN_BUDGET {
        return Ok(false);
    }
    let name = entry.name();
    // An unclaimed target: the rank is its position in `solid_targets` order, not the
    // physical order this walk reaches it in.
    let unclaimed_rank = target_ranks
        .get(name)
        .copied()
        .filter(|_| !captured.contains(name));
    if let Some(rank) = unclaimed_rank {
        let room = SOLID_SCAN_BUDGET.saturating_sub(*drained);
        if entry.size() > room {
            // A partial image is useless and would violate the advertised hard
            // total budget. Stop before asking the decoder for any of it.
            return Ok(false);
        }
        // Capture on first sighting of the name (7z legally allows two entries
        // with the same name — take one, drain any later twin).
        let mut buf = Vec::with_capacity(entry.size() as usize);
        let ok = rd.take(room).read_to_end(&mut buf).is_ok();
        *drained = drained.saturating_add(buf.len() as u64);
        if !ok || buf.len() as u64 != entry.size() {
            // A failed mid-entry read leaves the SHARED solid stream desynced —
            // the crate aborts the walk on any error, so stop with what we have.
            return Ok(false);
        }
        if !buf.is_empty() {
            captured.insert(name.to_string());
            found.push((rank, buf));
        }
    } else {
        // A non-target neighbor must be decoded to advance the solid stream to
        // the next entry — drain it to nowhere, capped at the remaining budget
        // so one large neighbor can't overshoot (a partial drain only ever
        // precedes the top-of-callback bail, so it never desyncs a later read).
        let room = SOLID_SCAN_BUDGET.saturating_sub(*drained);
        *drained = drained.saturating_add(
            std::io::copy(&mut rd.take(room), &mut std::io::sink()).unwrap_or(u64::MAX),
        );
    }
    Ok(found.len() < want && *drained < SOLID_SCAN_BUDGET)
}

/// List up to `max` of a 7-Zip archive's entries from metadata only (no block decode, no bomb risk).
pub fn list(bytes: &[u8], max: usize) -> Option<Vec<Entry>> {
    if !header_is_safe(&mut Cursor::new(bytes)) {
        return None;
    }
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
    const SOLID_ORDER: &[u8] = include_bytes!("../../../../tests/fixtures/sevenz/solid_order.7z");
    const SOLID_BURIED: &[u8] = include_bytes!("../../../../tests/fixtures/sevenz/solid_buried.7z");

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

    /// A start header followed by `next` as the end header, with a correct start-header CRC.
    fn with_end_header(next: &[u8]) -> Vec<u8> {
        let mut out = vec![b'7', b'z', 0xBC, 0xAF, 0x27, 0x1C, 0, 4];
        let mut tail = Vec::new();
        tail.extend_from_slice(&0u64.to_le_bytes()); // end header right after the start header
        tail.extend_from_slice(&(next.len() as u64).to_le_bytes());
        tail.extend_from_slice(&0u32.to_le_bytes()); // end-header CRC: not checked here
        out.extend_from_slice(&crc32(&tail).to_le_bytes());
        out.extend_from_slice(&tail);
        out.extend_from_slice(next);
        out
    }

    /// An encoded end header over one LZMA folder that declares `unpack` bytes.
    fn encoded_header(unpack: u64) -> Vec<u8> {
        let mut h = vec![K_ENCODED_HEADER, K_PACK_INFO, 0, 1, K_SIZE, 0x10, K_END];
        h.extend_from_slice(&[K_UNPACK_INFO, K_FOLDER, 1, 0, 1, 0x03, 0x03, 0x01, 0x01]);
        h.push(K_CODERS_UNPACK_SIZE);
        h.push(0xFF);
        h.extend_from_slice(&unpack.to_le_bytes());
        h.push(K_END);
        h
    }

    #[test]
    fn crc32_is_the_ieee_one() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn a_seven_zip_number_reads_its_length_from_the_first_byte() {
        assert_eq!(HeaderCursor(&[0x7F]).number(), Some(127));
        assert_eq!(HeaderCursor(&[0x80, 0x80]).number(), Some(128));
        assert_eq!(HeaderCursor(&[0xC1, 0x02, 0x03]).number(), Some(0x01_0302));
        let mut nine = vec![0xFF];
        nine.extend_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(HeaderCursor(&nine).number(), Some(u64::MAX));
        assert_eq!(HeaderCursor(&[0x80]).number(), None, "a truncated number");
    }

    /// A zeroed start header sent sevenz_rust2 hunting for an end header a byte at a time over
    /// the file's last megabyte, each step a fresh 256 KiB stream read (Dredd, 2026-09-23).
    #[test]
    fn a_zeroed_start_header_is_refused() {
        let mut zeroed = vec![b'7', b'z', 0xBC, 0xAF, 0x27, 0x1C, 0, 4];
        zeroed.resize(32 + 4096, 0);
        assert!(!header_is_safe(&mut Cursor::new(&zeroed)));
        assert!(list(&zeroed, 10).is_none());
    }

    /// An encoded end header declaring a terabyte of listing is refused before the crate
    /// decodes it; one declaring a real listing's size passes the same parse.
    #[test]
    fn an_encoded_header_is_refused_past_its_unpack_cap() {
        assert!(!header_is_safe(&mut Cursor::new(with_end_header(
            &encoded_header(1 << 40)
        ))));
        assert!(header_is_safe(&mut Cursor::new(with_end_header(
            &encoded_header(4096)
        ))));
        assert!(header_is_safe(&mut Cursor::new(with_end_header(&[
            K_HEADER, K_END
        ]))));
    }

    /// Every real archive passes: the two fixtures written by 7-Zip-compatible tooling, and
    /// what sevenz_rust2 itself writes.
    #[test]
    fn real_archives_pass_the_header_check() {
        for (name, bytes) in [("solid_order", SOLID_ORDER), ("solid_buried", SOLID_BURIED)] {
            assert!(header_is_safe(&mut Cursor::new(bytes)), "{name}");
        }
        let mut written = Cursor::new(Vec::new());
        let mut w = ArchiveWriter::new(&mut written).expect("writer");
        w.push_archive_entry(
            ArchiveEntry::new_file("a.png"),
            Some(Cursor::new(vec![7u8; 300])),
        )
        .expect("entry");
        w.finish().expect("finish");
        assert!(header_is_safe(&mut Cursor::new(written.into_inner())));
    }
}
