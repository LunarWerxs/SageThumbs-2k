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
fn follow(start: u32, fat: &[u32]) -> Option<Vec<u32>> {
    let mut out = Vec::new();
    let mut s = start;
    while s != ENDOFCHAIN && s != FREESECT {
        let idx = s as usize;
        if idx >= fat.len() || out.len() > MAX_SECTORS {
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
    let mut dir = Vec::new();
    for s in follow(first_dir, &fat)? {
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
                .filter_map(|c| {
                    char::from_u32(u16::from_le_bytes([e[c * 2], e[c * 2 + 1]]) as u32)
                })
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
        let mut out = Vec::with_capacity(tsize);
        for s in follow(tstart, &fat)? {
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
        let mut ministream = Vec::with_capacity(rsize);
        for s in follow(rstart, &fat)? {
            ministream.extend_from_slice(read_sector(bytes, s, sector_size)?);
            if ministream.len() >= rsize {
                break;
            }
        }
        ministream.truncate(rsize);
        let mut minifat: Vec<u32> = Vec::new();
        for s in follow(first_minifat, &fat)? {
            let sec = read_sector(bytes, s, sector_size)?;
            for i in 0..sector_size / 4 {
                minifat.push(le32(sec, i * 4)?);
            }
            if minifat.len() > MAX_SECTORS {
                return None;
            }
        }
        let mut out = Vec::with_capacity(tsize);
        let mut s = tstart;
        while s != ENDOFCHAIN && s != FREESECT {
            let idx = s as usize;
            if idx >= minifat.len() || out.len() > tsize.saturating_add(mini_size) {
                return None;
            }
            let o = idx.checked_mul(mini_size)?;
            let end = o.checked_add(mini_size)?.min(ministream.len());
            out.extend_from_slice(ministream.get(o..end)?);
            if out.len() >= tsize {
                break;
            }
            s = minifat[idx];
        }
        out.truncate(tsize);
        Some(out)
    }
}
