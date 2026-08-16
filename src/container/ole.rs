//! Minimal read-only OLE2 / Compound File Binary (CFB) reader — just enough to
//! pull ONE named stream out of a compound file (e.g. the thumbnail-bearing
//! `\x05SummaryInformation` stream of a 3ds Max scene or a legacy Office/Visio
//! doc). Pure Rust, NO dependency: the shell-extension DLL and the `st2k` CLI share
//! one code path (a COM/structured-storage API would need per-context init and
//! wouldn't run in the CLI). Handles both the main FAT and the mini-FAT.
//!
//! Runs on attacker-controlled bytes inside Explorer's thumbnail host under
//! `panic = "abort"`: every read is bounds-checked (`Option`) and every chain walk
//! is iteration-capped so a hostile/looping file can't hang or OOM the host.

use super::util::{le16, le32, le64};

const SIG: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
const ENDOFCHAIN: u32 = 0xFFFF_FFFE;
const FREESECT: u32 = 0xFFFF_FFFF;
/// Chain/FAT-length guard (4M entries ≈ a 2 GiB compound file at 512-byte sectors).
const MAX_SECTORS: usize = 1 << 22;
/// Hard cap on a returned stream (the cover cap is enforced again by the caller).
const MAX_STREAM: usize = 32 * 1024 * 1024;

pub fn looks_like_ole(head: &[u8]) -> bool {
    head.starts_with(&SIG)
}

fn sector_off(s: u32, sector_size: usize) -> Option<usize> {
    (s as usize).checked_add(1)?.checked_mul(sector_size)
}

fn read_sector(bytes: &[u8], s: u32, sector_size: usize) -> Option<&[u8]> {
    let o = sector_off(s, sector_size)?;
    bytes.get(o..o.checked_add(sector_size)?)
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
/// None if absent / malformed.
pub fn read_stream(bytes: &[u8], name: &str) -> Option<Vec<u8>> {
    if !looks_like_ole(bytes) {
        return None;
    }
    let sector_shift = le16(bytes, 0x1E)?;
    let mini_shift = le16(bytes, 0x20)?;
    if !(7..=12).contains(&sector_shift) || mini_shift != 6 {
        return None; // sane sector sizes only (128B–4KB; 64B mini)
    }
    let sector_size = 1usize << sector_shift;
    let mini_size = 1usize << mini_shift;
    let first_dir = le32(bytes, 0x30)?;
    let mini_cutoff = le32(bytes, 0x38)? as u64;
    let first_minifat = le32(bytes, 0x3C)?;
    let first_difat = le32(bytes, 0x44)?;

    // --- Collect FAT sector indices: 109 in the header DIFAT + any DIFAT chain.
    let mut fat_sectors: Vec<u32> = Vec::new();
    for i in 0..109 {
        let v = le32(bytes, 0x4C + i * 4)?;
        if v == FREESECT || v == ENDOFCHAIN {
            break;
        }
        fat_sectors.push(v);
    }
    let mut difat = first_difat;
    while difat != ENDOFCHAIN && difat != FREESECT && fat_sectors.len() <= MAX_SECTORS {
        let sec = read_sector(bytes, difat, sector_size)?;
        let n = sector_size / 4;
        for i in 0..n - 1 {
            let v = le32(sec, i * 4)?;
            if v != FREESECT && v != ENDOFCHAIN {
                fat_sectors.push(v);
            }
        }
        difat = le32(sec, (n - 1) * 4)?;
    }

    // --- Read the FAT itself (concatenated u32 entries).
    let mut fat: Vec<u32> = Vec::new();
    for &fs in &fat_sectors {
        let sec = read_sector(bytes, fs, sector_size)?;
        for i in 0..sector_size / 4 {
            fat.push(le32(sec, i * 4)?);
        }
        if fat.len() > MAX_SECTORS {
            return None;
        }
    }

    // --- Read the directory stream and find the target + root entries.
    // Its size isn't known ahead of the walk, so no tighter cap is available yet.
    let mut dir = Vec::new();
    for s in follow(first_dir, &fat, MAX_SECTORS)? {
        dir.extend_from_slice(read_sector(bytes, s, sector_size)?);
    }
    let mut target: Option<(u32, u64)> = None;
    let mut root: Option<(u32, u64)> = None;
    for e in dir.chunks_exact(128) {
        let etype = e[66];
        if etype != 1 && etype != 2 && etype != 5 {
            continue; // unused/free
        }
        let start = le32(e, 116)?;
        let size = le64(e, 120)?;
        if etype == 5 {
            root = Some((start, size));
        }
        let name_len = le16(e, 64)? as usize; // bytes incl null terminator
        if (2..=64).contains(&name_len) {
            let chars = (name_len / 2).saturating_sub(1);
            let nm: String = (0..chars)
                .filter_map(|c| char::from_u32(u16::from_le_bytes([e[c * 2], e[c * 2 + 1]]) as u32))
                .collect();
            if nm == name {
                target = Some((start, size));
            }
        }
    }
    let (tstart, tsize) = target?;
    let tsize = (tsize.min(MAX_STREAM as u64)) as usize;
    if tsize == 0 {
        return None;
    }

    // --- Big stream → main FAT; small stream → mini-FAT inside the root stream.
    if tsize as u64 >= mini_cutoff {
        // Cap the walk at the sector count the declared size actually needs (plus
        // slack): a self-looping chain then dies in a handful of hops instead of
        // the flat MAX_SECTORS ceiling.
        let cap = tsize.div_ceil(sector_size).saturating_add(2);
        let mut out = Vec::with_capacity(tsize);
        for s in follow(tstart, &fat, cap)? {
            out.extend_from_slice(read_sector(bytes, s, sector_size)?);
            if out.len() >= tsize {
                break;
            }
        }
        out.truncate(tsize);
        Some(out)
    } else {
        let (rstart, rsize) = root?;
        let rsize = (rsize.min(MAX_STREAM as u64)) as usize;
        let root_cap = rsize.div_ceil(sector_size).saturating_add(2);
        let mut ministream = Vec::with_capacity(rsize);
        for s in follow(rstart, &fat, root_cap)? {
            ministream.extend_from_slice(read_sector(bytes, s, sector_size)?);
            if ministream.len() >= rsize {
                break;
            }
        }
        ministream.truncate(rsize);
        let mut minifat: Vec<u32> = Vec::new();
        // The mini-FAT's own sector count isn't known ahead of the walk either.
        for s in follow(first_minifat, &fat, MAX_SECTORS)? {
            let sec = read_sector(bytes, s, sector_size)?;
            for i in 0..sector_size / 4 {
                minifat.push(le32(sec, i * 4)?);
            }
            if minifat.len() > MAX_SECTORS {
                return None;
            }
        }
        // Walk via the same hop-bounded follow() the FAT/root-stream chains use
        // (not an out.len()-growth bound): when the root stream is empty and
        // minifat self-loops, growth-based termination never trips because a
        // zero-length ministream slice is appended every iteration forever.
        // follow() bounds on hop count and index range instead, so it always
        // terminates regardless of what the mini-stream itself contains, and the
        // size-derived cap kills a cycle in a handful of hops rather than 4M.
        let mini_cap = tsize.div_ceil(mini_size).saturating_add(2);
        let mut out = Vec::with_capacity(tsize);
        for idx in follow(tstart, &minifat, mini_cap)? {
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
}
