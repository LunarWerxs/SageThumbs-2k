//! Minimal read-only OLE2 / Compound File Binary (CFB) reader — just enough to
//! pull ONE named stream out of a compound file (e.g. the thumbnail-bearing
//! `\x05SummaryInformation` stream of a 3ds Max scene or a legacy Office/Visio
//! doc). Pure Rust, NO dependency: the shell-extension DLL and the `st2k` CLI share
//! one code path (a COM/structured-storage API would need per-context init and
//! wouldn't run in the CLI). Handles both the main FAT and the mini-FAT.
//!
//! Reads from a buffer or, for a file too big to hold, straight off a seekable reader: only
//! the header, the FAT, the directory and the chains of the streams asked for are read, so
//! the file's size costs nothing but the FAT (a 300 MB document grown over many saves keeps
//! its FAT and directory at the far end, where a bounded head read never reaches them - the
//! big-file gate, 2026-09-23).
//!
//! Runs on attacker-controlled bytes inside Explorer's thumbnail host under
//! `panic = "abort"`: every read is bounds-checked (`Option`) and every chain walk
//! is iteration-capped so a hostile/looping file can't hang or OOM the host.

use std::cell::RefCell;
use std::io::{Read, Seek, SeekFrom};

use super::util::{le16, le32, le64};

const SIG: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
const ENDOFCHAIN: u32 = 0xFFFF_FFFE;
const FREESECT: u32 = 0xFFFF_FFFF;
/// Chain/FAT-length guard (4M entries ≈ a 2 GiB compound file at 512-byte sectors).
const MAX_SECTORS: usize = 1 << 22;
/// Hard cap on a returned stream (the cover cap is enforced again by the caller).
const MAX_STREAM: usize = 32 * 1024 * 1024;
/// The most directory a file may declare: 262,144 entries of 128 bytes, far past any real
/// document's. The directory chain was walked whole with only the hop cap, and a crafted file
/// (sparse, so cheap to make large) could make that 4M sectors of 4 KiB, a 16 GiB buffer on the
/// shell's thread, which aborts it under `panic = "abort"` (Dredd, 2026-09-23).
const MAX_DIRECTORY: usize = 32 * 1024 * 1024;

pub fn looks_like_ole(head: &[u8]) -> bool {
    head.starts_with(&SIG)
}

/// Where a compound file's bytes come from: a buffer, or a seekable file read a sector at a
/// time.
trait Source {
    fn len(&self) -> u64;
    /// Exactly `len` bytes at `at`, or `None` past the end.
    fn read_at(&self, at: u64, len: usize) -> Option<Vec<u8>>;
}

impl Source for [u8] {
    fn len(&self) -> u64 {
        <[u8]>::len(self) as u64
    }

    fn read_at(&self, at: u64, len: usize) -> Option<Vec<u8>> {
        let at = usize::try_from(at).ok()?;
        self.get(at..at.checked_add(len)?).map(<[u8]>::to_vec)
    }
}

/// A seekable reader as a [`Source`]: every read is a seek and an exact read.
struct Seeker<R> {
    r: RefCell<R>,
    len: u64,
}

impl<R: Read + Seek> Source for Seeker<R> {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&self, at: u64, len: usize) -> Option<Vec<u8>> {
        if at.checked_add(len as u64)? > self.len {
            return None;
        }
        let mut r = self.r.try_borrow_mut().ok()?;
        r.seek(SeekFrom::Start(at)).ok()?;
        let mut buf = vec![0u8; len];
        r.read_exact(&mut buf).ok()?;
        Some(buf)
    }
}

fn sector_off(s: u32, sector_size: usize) -> Option<u64> {
    u64::from(s).checked_add(1)?.checked_mul(sector_size as u64)
}

fn read_sector<S: Source + ?Sized>(src: &S, s: u32, sector_size: usize) -> Option<Vec<u8>> {
    src.read_at(sector_off(s, sector_size)?, sector_size)
}

/// Walk a FAT/mini-FAT chain from `start`, returning the ordered sector list.
///
/// `max_hops` is an additional, tighter ceiling on top of `MAX_SECTORS`: when the
/// caller already knows the stream's declared byte size, it can pass a hop count
/// derived from that size so a self-looping chain gets cut off in a handful of
/// hops instead of walking up to the flat 4M-entry cap. Pass `MAX_SECTORS` at
/// call sites where no size is known yet (e.g. while the FAT/directory itself is
/// still being assembled).
fn follow(start: u32, fat: &[u32], max_hops: usize) -> Option<Vec<u32>> {
    let cap = max_hops.min(MAX_SECTORS);
    let mut out = Vec::new();
    let mut s = start;
    while s != ENDOFCHAIN && s != FREESECT {
        let idx = s as usize;
        if idx >= fat.len() || out.len() > cap {
            return None;
        }
        out.push(s);
        s = fat[idx];
    }
    Some(out)
}

/// Read the named stream (`name` compared against the directory's UTF-16 name), or
/// None if absent / malformed. First match wins when several directory entries share the
/// name (they can — the scan is flat, not hierarchical).
pub fn read_stream(bytes: &[u8], name: &str) -> Option<Vec<u8>> {
    read_streams(bytes, name, 1).and_then(|mut v| v.pop())
}

/// [`read_stream`] straight off a seekable reader, reading only what the stream needs.
pub fn read_stream_from<R: Read + Seek>(mut r: R, name: &str) -> Option<Vec<u8>> {
    let len = r.seek(SeekFrom::End(0)).ok()?;
    let src = Seeker {
        r: RefCell::new(r),
        len,
    };
    read_streams_in(&src, name, 1).and_then(|mut v| v.pop())
}

/// Header fields needed before any stream can be located.
struct Header {
    sector_size: usize,
    mini_size: usize,
    first_dir: u32,
    mini_cutoff: u64,
    first_minifat: u32,
    first_difat: u32,
}

fn parse_header(bytes: &[u8]) -> Option<Header> {
    let sector_shift = le16(bytes, 0x1E)?;
    let mini_shift = le16(bytes, 0x20)?;
    if !(7..=12).contains(&sector_shift) || mini_shift != 6 {
        return None; // sane sector sizes only (128B–4KB; 64B mini)
    }
    Some(Header {
        sector_size: 1usize << sector_shift,
        mini_size: 1usize << mini_shift,
        first_dir: le32(bytes, 0x30)?,
        mini_cutoff: le32(bytes, 0x38)? as u64,
        first_minifat: le32(bytes, 0x3C)?,
        first_difat: le32(bytes, 0x44)?,
    })
}

/// Collect FAT sector indices: 109 in the header DIFAT + any DIFAT chain.
fn collect_fat_sectors<S: Source + ?Sized>(
    src: &S,
    head: &[u8],
    first_difat: u32,
    sector_size: usize,
) -> Option<Vec<u32>> {
    let mut fat_sectors: Vec<u32> = Vec::new();
    for i in 0..109 {
        let v = le32(head, 0x4C + i * 4)?;
        if v == FREESECT || v == ENDOFCHAIN {
            break;
        }
        fat_sectors.push(v);
    }
    let mut difat = first_difat;
    let mut difat_hops = 0usize;
    while difat != ENDOFCHAIN && difat != FREESECT && fat_sectors.len() <= MAX_SECTORS {
        append_difat_sector(
            src,
            &mut difat,
            &mut difat_hops,
            sector_size,
            &mut fat_sectors,
        )?;
    }
    Some(fat_sectors)
}

/// Advance one DIFAT hop: append that sector's FAT entries to `fat_sectors` and step
/// `difat`/`difat_hops` on to the next link; `None` once the hop count is a cycle.
///
/// Bound the DIFAT chain by a hop counter INDEPENDENT of whether `fat_sectors` grew, the way
/// `follow()` bounds every other chain in this file. Without it a crafted DIFAT sector whose
/// n-1 FAT entries are all FREESECT/ENDOFCHAIN (so none are pushed) and whose trailing next
/// pointer self-references loops forever — a true hang, reachable for anything the sniffer
/// classifies as OLE2 (legacy Office, .msi, .max, .msg) in the thumbnail/preview host.
fn append_difat_sector<S: Source + ?Sized>(
    src: &S,
    difat: &mut u32,
    difat_hops: &mut usize,
    sector_size: usize,
    fat_sectors: &mut Vec<u32>,
) -> Option<()> {
    let sectors = usize::try_from(src.len() / sector_size as u64).unwrap_or(usize::MAX);
    if *difat_hops >= MAX_SECTORS.min(sectors) {
        return None; // DIFAT chain longer than any real file could need: a cycle.
    }
    *difat_hops += 1;
    let sec = read_sector(src, *difat, sector_size)?;
    let n = sector_size / 4;
    for i in 0..n - 1 {
        let v = le32(&sec, i * 4)?;
        if v != FREESECT && v != ENDOFCHAIN {
            fat_sectors.push(v);
        }
    }
    *difat = le32(&sec, (n - 1) * 4)?;
    Some(())
}

/// Read a concatenated u32 table (the FAT or the mini-FAT) from a sector list.
fn read_u32_table<S: Source + ?Sized>(
    src: &S,
    sectors: impl IntoIterator<Item = u32>,
    sector_size: usize,
) -> Option<Vec<u32>> {
    let mut table: Vec<u32> = Vec::new();
    for s in sectors {
        let sec = read_sector(src, s, sector_size)?;
        for i in 0..sector_size / 4 {
            table.push(le32(&sec, i * 4)?);
        }
        if table.len() > MAX_SECTORS {
            return None;
        }
    }
    Some(table)
}

/// A directory entry's (chain start sector, declared byte size).
type StreamLoc = (u32, u64);

/// Read the directory stream and find, in directory order, up to `max` entries
/// whose name matches `name`, plus the root entry (needed for the mini-stream).
fn find_directory_entries<S: Source + ?Sized>(
    src: &S,
    fat: &[u32],
    sector_size: usize,
    first_dir: u32,
    (name, max): (&str, usize),
) -> Option<(Vec<StreamLoc>, Option<StreamLoc>)> {
    // Its size isn't known ahead of the walk, so the cap is the most directory any file needs.
    let mut dir = Vec::new();
    for s in follow(first_dir, fat, MAX_DIRECTORY / sector_size)? {
        dir.extend_from_slice(&read_sector(src, s, sector_size)?);
    }
    let mut targets: Vec<(u32, u64)> = Vec::new();
    let mut root: Option<(u32, u64)> = None;
    let (dir_chunks, _) = dir.as_chunks::<128>();
    for e in dir_chunks {
        scan_directory_entry(e, name, max, targets.len(), &mut root, &mut targets)?;
    }
    Some((targets, root))
}

/// Read one 128-byte directory slot: record it as the root entry when it is the root
/// (ETYPE 5), and push its (start, size) onto `targets` when its name matches `name` and
/// fewer than `max` targets have been found so far. Free/unused slots are ignored.
fn scan_directory_entry(
    e: &[u8],
    name: &str,
    max: usize,
    found: usize,
    root: &mut Option<StreamLoc>,
    targets: &mut Vec<StreamLoc>,
) -> Option<()> {
    let etype = e[66];
    if etype != 1 && etype != 2 && etype != 5 {
        return Some(()); // unused/free
    }
    let start = le32(e, 116)?;
    let size = le64(e, 120)?;
    if etype == 5 {
        *root = Some((start, size));
    }
    let name_len = le16(e, 64)? as usize; // bytes incl null terminator
    if (2..=64).contains(&name_len) && found < max {
        let chars = (name_len / 2).saturating_sub(1);
        let nm: String = (0..chars)
            .filter_map(|c| char::from_u32(u16::from_le_bytes([e[c * 2], e[c * 2 + 1]]) as u32))
            .collect();
        if nm == name {
            targets.push((start, size));
        }
    }
    Some(())
}

/// Read the sectors of the chain at `start` until `size` bytes are in hand or the chain ends
/// (a chain shorter than its declared size hands back what it holds, as 3.2.0's reader did; a
/// damaged stream then fails its own decode). The walk is capped at the sector count that size
/// needs (plus slack), so a self-looping chain dies in a handful of hops instead of the flat
/// MAX_SECTORS ceiling.
fn read_chain<S: Source + ?Sized>(
    src: &S,
    fat: &[u32],
    sector_size: usize,
    (start, size): (u32, usize),
) -> Option<Vec<u8>> {
    let cap = size.div_ceil(sector_size).saturating_add(2);
    let mut out = Vec::with_capacity(size);
    for s in follow(start, fat, cap)? {
        out.extend_from_slice(&read_sector(src, s, sector_size)?);
        if out.len() >= size {
            break;
        }
    }
    out.truncate(size);
    Some(out)
}

/// Read a stream whose declared size is below the mini-stream cutoff, via the per-file
/// mini-stream (the root entry's stream, capped at `MAX_STREAM`) and mini-FAT. Both are built
/// at most once per file and cached by the caller across targets.
fn read_mini_stream<S: Source + ?Sized>(
    src: &S,
    fat: &[u32],
    hdr: &Header,
    root: Option<StreamLoc>,
    (tstart, tsize): (u32, usize),
    caches: &mut (Option<Vec<u8>>, Option<Vec<u32>>),
) -> Option<Vec<u8>> {
    // `insert` hands back a borrow of the value just stored, the shell-surface
    // unwrap ban's preferred spelling of this build-once-cache-forever idiom.
    let ministream: &Vec<u8> = match &mut caches.0 {
        Some(m) => m,
        slot @ None => {
            let (rstart, rsize) = root?;
            let rsize = rsize.min(MAX_STREAM as u64) as usize;
            slot.insert(read_chain(src, fat, hdr.sector_size, (rstart, rsize))?)
        }
    };
    // The mini-FAT too is per-FILE: build it once, reuse for every small target. Its own
    // sector count isn't known ahead of the walk.
    let minifat: &Vec<u32> = match &mut caches.1 {
        Some(m) => m,
        slot @ None => {
            let sectors = follow(hdr.first_minifat, fat, MAX_SECTORS)?;
            slot.insert(read_u32_table(src, sectors, hdr.sector_size)?)
        }
    };
    read_mini_chain(ministream, minifat, hdr.mini_size, (tstart, tsize))
}

/// Walk the mini-FAT chain at `tstart`, appending `mini_size`-byte slices of `ministream`
/// until `tsize` bytes are in hand, the walk hop-bounded by `follow`.
fn read_mini_chain(
    ministream: &[u8],
    minifat: &[u32],
    mini_size: usize,
    (tstart, tsize): (u32, usize),
) -> Option<Vec<u8>> {
    // Walk via the same hop-bounded follow() the FAT/root-stream chains use
    // (not an out.len()-growth bound): when the root stream is empty and
    // minifat self-loops, growth-based termination never trips because a
    // zero-length ministream slice is appended every iteration forever.
    // follow() bounds on hop count and index range instead, so it always
    // terminates regardless of what the mini-stream itself contains, and the
    // size-derived cap kills a cycle in a handful of hops rather than 4M.
    let mini_cap = tsize.div_ceil(mini_size).saturating_add(2);
    let mut out = Vec::with_capacity(tsize);
    for idx in follow(tstart, minifat, mini_cap)? {
        let idx = idx as usize;
        let o = idx.checked_mul(mini_size)?;
        let end = o.checked_add(mini_size)?.min(ministream.len());
        out.extend_from_slice(ministream.get(o..end)?);
        if out.len() >= tsize {
            break;
        }
    }
    out.truncate(tsize);
    Some(out)
}

/// EVERY stream whose directory entry carries `name`, up to `max` of them, in directory
/// order. The scan is deliberately FLAT (no storage-tree walk), which is exactly what makes
/// this useful for Outlook `.msg` files: each attachment storage holds a stream with the
/// SAME name (`__substg1.0_3707001F`, the long filename), so "all streams named X" is "one
/// entry per attachment" without implementing red-black-tree traversal over hostile input.
/// `None` = not a compound file / malformed; an empty Vec = valid file, no such stream.
pub fn read_streams(bytes: &[u8], name: &str, max: usize) -> Option<Vec<Vec<u8>>> {
    read_streams_in(bytes, name, max)
}

fn read_streams_in<S: Source + ?Sized>(src: &S, name: &str, max: usize) -> Option<Vec<Vec<u8>>> {
    let head = src.read_at(0, 512)?;
    if !looks_like_ole(&head) {
        return None;
    }
    let hdr = parse_header(&head)?;
    let fat_sectors = collect_fat_sectors(src, &head, hdr.first_difat, hdr.sector_size)?;
    let fat = read_u32_table(src, fat_sectors.iter().copied(), hdr.sector_size)?;
    let (targets, root) =
        find_directory_entries(src, &fat, hdr.sector_size, hdr.first_dir, (name, max))?;
    // The mini-stream and mini-FAT are shared by every small stream; built at most once,
    // on first need.
    let mut caches = (None, None);
    let mut results: Vec<Vec<u8>> = Vec::with_capacity(targets.len());
    for &target in &targets {
        if let Some(out) = read_target_stream(src, &fat, &hdr, root, target, &mut caches)? {
            results.push(out);
        }
    }
    Some(results)
}

/// Read the bytes of one directory target: a zero-length target is skipped, a large one comes
/// from the main FAT, a small one from the cached mini-stream/mini-FAT. `None` = malformed,
/// `Some(None)` = zero-length target to skip.
fn read_target_stream<S: Source + ?Sized>(
    src: &S,
    fat: &[u32],
    hdr: &Header,
    root: Option<StreamLoc>,
    (tstart, tsize): StreamLoc,
    caches: &mut (Option<Vec<u8>>, Option<Vec<u32>>),
) -> Option<Option<Vec<u8>>> {
    let tsize = (tsize.min(MAX_STREAM as u64)) as usize;
    if tsize == 0 {
        return Some(None);
    }
    // Big stream → main FAT; small stream → mini-FAT inside the root stream.
    let out = if tsize as u64 >= hdr.mini_cutoff {
        read_chain(src, fat, hdr.sector_size, (tstart, tsize))?
    } else {
        read_mini_stream(src, fat, hdr, root, (tstart, tsize), caches)?
    };
    Some(Some(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds the smallest real compound file that reaches the mini-stream walk:
    /// sector 0 = FAT, sector 1 = directory (Root Entry with an EMPTY root stream +
    /// one named stream small enough to route through the mini-FAT), sector 2 =
    /// mini-FAT whose only entry self-loops (`minifat[0] = 0`). `read_stream`
    /// doesn't walk the directory tree, it just scans every 128-byte slot, so no
    /// sibling/child linkage is needed for the entries to be found.
    fn synthetic_ole_self_looping_minifat(stream_name: &str) -> Vec<u8> {
        const SECTOR: usize = 512;
        const ENDOFCHAIN: u32 = 0xFFFF_FFFE;
        const FREESECT: u32 = 0xFFFF_FFFF;

        let mut header = vec![0u8; SECTOR];
        header[0..8].copy_from_slice(&SIG);
        header[0x1E..0x20].copy_from_slice(&9u16.to_le_bytes()); // sector shift: 512
        header[0x20..0x22].copy_from_slice(&6u16.to_le_bytes()); // mini sector shift: 64
        header[0x30..0x34].copy_from_slice(&1u32.to_le_bytes()); // first directory sector
        header[0x38..0x3C].copy_from_slice(&4096u32.to_le_bytes()); // mini stream cutoff
        header[0x3C..0x40].copy_from_slice(&2u32.to_le_bytes()); // first mini-FAT sector
        header[0x44..0x48].copy_from_slice(&ENDOFCHAIN.to_le_bytes()); // no DIFAT chain
        header[0x4C..0x50].copy_from_slice(&0u32.to_le_bytes()); // DIFAT[0] = FAT sector 0
        for i in 1..109usize {
            let o = 0x4C + i * 4;
            header[o..o + 4].copy_from_slice(&FREESECT.to_le_bytes());
        }

        // Sector 0: the FAT. Each of our three sectors is its own one-sector chain.
        let mut fat = vec![0u8; SECTOR];
        for s in 0..3usize {
            fat[s * 4..s * 4 + 4].copy_from_slice(&ENDOFCHAIN.to_le_bytes());
        }
        for i in 3..(SECTOR / 4) {
            fat[i * 4..i * 4 + 4].copy_from_slice(&FREESECT.to_le_bytes());
        }

        // Sector 1: the directory. Slot 0 = Root Entry with a zero-length root
        // stream (start = ENDOFCHAIN); slot 1 = our target stream, small enough
        // (10 bytes < the 4096 mini cutoff) to route through the mini-FAT, with
        // `start = 0` so the walk begins at the self-looping mini-FAT entry.
        let mut dir = vec![0u8; SECTOR];
        let mut put_entry = |slot: usize, name: &str, kind: u8, start: u32, size: u64| {
            let base = slot * 128;
            let utf16: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
            for (i, c) in utf16.iter().enumerate() {
                dir[base + i * 2..base + i * 2 + 2].copy_from_slice(&c.to_le_bytes());
            }
            dir[base + 64..base + 66].copy_from_slice(&((utf16.len() * 2) as u16).to_le_bytes());
            dir[base + 66] = kind;
            dir[base + 116..base + 120].copy_from_slice(&start.to_le_bytes());
            dir[base + 120..base + 128].copy_from_slice(&size.to_le_bytes());
        };
        put_entry(0, "Root Entry", 5, ENDOFCHAIN, 0);
        put_entry(1, stream_name, 2, 0, 10);

        // Sector 2: the mini-FAT. Entry 0 points back at itself.
        let mut minifat = vec![0u8; SECTOR];
        minifat[0..4].copy_from_slice(&0u32.to_le_bytes());
        for i in 1..(SECTOR / 4) {
            minifat[i * 4..i * 4 + 4].copy_from_slice(&FREESECT.to_le_bytes());
        }

        [header, fat, dir, minifat].concat()
    }

    /// Regression for A023: with an empty root stream and a self-looping mini-FAT
    /// entry, the old walk was bounded only by output-buffer growth. Since the
    /// (empty) ministream always yields a zero-length slice, `out` never grew and
    /// the loop ran forever. The hop-bounded `follow()` walk must terminate (with
    /// `None`, since the chain can never actually deliver the declared bytes)
    /// instead of hanging the calling thread.
    #[test]
    fn read_stream_terminates_on_self_looping_minifat() {
        let bytes = synthetic_ole_self_looping_minifat("Target");
        assert_eq!(read_stream(&bytes, "Target"), None);
    }

    #[test]
    fn follow_caps_at_the_tighter_of_max_hops_and_max_sectors() {
        // A 3-entry chain (0 -> 1 -> 2 -> ENDOFCHAIN) with max_hops = 1 must be
        // rejected as too long, proving the tighter caller-supplied cap is what
        // actually bites, not just the flat MAX_SECTORS ceiling.
        const ENDOFCHAIN: u32 = 0xFFFF_FFFE;
        let fat = vec![1u32, 2u32, ENDOFCHAIN];
        assert_eq!(follow(0, &fat, 1), None);
        assert_eq!(follow(0, &fat, 3), Some(vec![0, 1, 2]));
    }

    /// A directory chain longer than any real file's is refused before it is buffered: it used
    /// to be read whole, up to 4M sectors of 4 KiB (Dredd, 2026-09-23). A source of empty sectors
    /// stands in for a sparse file, so the test allocates nothing large.
    #[test]
    fn a_directory_chain_past_its_cap_is_refused_unread() {
        struct Blank(std::cell::Cell<usize>);
        impl Source for Blank {
            fn len(&self) -> u64 {
                u64::MAX
            }
            fn read_at(&self, _at: u64, len: usize) -> Option<Vec<u8>> {
                self.0.set(self.0.get() + 1);
                Some(vec![0u8; len])
            }
        }
        let long = MAX_DIRECTORY / 4096 + 8;
        let mut fat: Vec<u32> = (1..=long as u32).collect();
        fat.push(ENDOFCHAIN);
        let src = Blank(std::cell::Cell::new(0));
        assert!(find_directory_entries(&src, &fat, 4096, 0, ("x", 1)).is_none());
        assert_eq!(
            src.0.get(),
            0,
            "no sector of an over-long directory is read"
        );

        let short: Vec<u32> = vec![1, 2, ENDOFCHAIN];
        assert!(find_directory_entries(&src, &short, 4096, 0, ("x", 1)).is_some());
    }
}
