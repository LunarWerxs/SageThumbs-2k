//! One shared, fail-open pre-check in front of every `zip::ZipArchive::new` on an
//! untrusted shell input (see the routed call sites in `container/mod.rs`,
//! `container/zipfmt.rs`, `container/apk.rs`, and preview's `big_zip_entries`).
//!
//! ## Why this exists (measured, zip 8.6.0, 2026-10-07)
//!
//! `ZipArchive::new` -> `read_central_header` does
//! `Vec::with_capacity(number_of_files)` of `ZipFileData` **before reading a single
//! entry** (`zip-8.6.0/src/read/zip_archive.rs`). `ZipFileData` measures ~232 bytes
//! here (confirmed: a 256 MiB ZIP64 declaring 5.6M entries drove ~1.24 GiB of peak
//! commit over an identically-sized 1-entry control, then failed reading entry #2 —
//! the reservation is paid up front and only then freed).
//!
//! The crate already bounds `number_of_files <= file_len / 46` (its EOCD64
//! consistency check: `eocd64_offset >= number_of_files*46 + cd_offset`), so the
//! reservation is **amplified ~5x the input size, not unbounded by it** — but the
//! Quick preview's big-ZIP listing (`crates/preview/.../content.rs`) reads files off
//! disk with no 256 MiB input cap, and its `DirBudget` bounds bytes *read*, not this
//! up-front reservation. A multi-GiB archive therefore reserves multiple GiB before
//! any budgeted read can fail; a failed commit aborts the hosting process (the
//! thumbnail `dllhost`, the app, or `explorer.exe` for an in-process open).
//!
//! The crate's own `file_len/46` bound is the reason a *geometry* check ("declared
//! count vs bytes available for the directory, >=46 each") adds nothing: it is
//! already enforced, and a crafted archive sits exactly on that boundary. The only
//! lever at this layer is an **absolute ceiling** on the declared ZIP64 entry count,
//! which bounds the eager reservation regardless of how large the input is. Only the
//! ZIP64 path matters — a plain EOCD's count is a `u16` (<= 65_535 -> ~15 MiB), below
//! the ceiling by construction.
//!
//! ## Residual limit (by design, not a gap)
//!
//! The crate's `find_central_directory` scans EOCD signatures backward over the WHOLE
//! file and retries earlier candidates when one fails to parse, and its relaxed
//! "garbage after comment" rule lets a valid EOCD sit further than this pre-check's
//! tail window from EOF. A crafted archive can therefore still place its real ZIP64
//! EOCD outside the window and reach the crate's own reservation — bounded by the
//! crate to ~`file_len/46` entries (~5x the input). This guard closes the cheap
//! bypass (it checks every EOCD candidate in the tail window, not just the last) and
//! caps the reservation for every archive whose EOCD is where real writers put it; it
//! is deliberately not the O(file) full scan the crate itself performs.

use std::io::{Read, Seek, SeekFrom};

use zip::result::{ZipError, ZipResult};
use zip::ZipArchive;

/// Largest ZIP64-declared central-directory entry count we will open. Bounds the
/// crate's eager `with_capacity` reservation to ~`N * size_of::<ZipFileData>()`
/// ~= 58 MiB. We never *list* more than [`super::MAX_LIST_ENTRIES`] (50_000) entries
/// anyway, so this is ~5x generous for every archive we would actually thumbnail; a
/// genuinely larger archive gets the "entries not listed" placeholder instead of a
/// crash.
pub(crate) const MAX_ZIP_DECLARED_ENTRIES: u64 = 262_144;

const EOCD_SIG: [u8; 4] = [0x50, 0x4b, 0x05, 0x06];
const LOC64_SIG: [u8; 4] = [0x50, 0x4b, 0x06, 0x07];
const EOCD64_SIG: [u8; 4] = [0x50, 0x4b, 0x06, 0x06];
/// EOCD is 22 bytes; a trailing comment can push its start back up to 65_535 bytes.
const EOCD_MAX_BACK: u64 = 22 + 65_535;

/// Open an archive after the declared-entry-count pre-check. A drop-in for
/// `zip::ZipArchive::new`: same signature and error type, so every untrusted call
/// site routes through it by name alone.
pub fn open<R: Read + Seek>(mut reader: R) -> ZipResult<ZipArchive<R>> {
    if zip64_declares_over_ceiling(&mut reader) {
        return Err(ZipError::UnsupportedArchive(
            "declared central directory entry count exceeds SageThumbs limit",
        ));
    }
    // `ZipArchive::new` re-seeks to the end itself, so our cursor position is moot.
    ZipArchive::new(reader)
}

/// Whether ANY end-of-central-directory candidate in the tail window resolves to a
/// ZIP64 record declaring more than [`MAX_ZIP_DECLARED_ENTRIES`].
///
/// Every candidate is checked, not just the last, because the crate's own directory
/// finder scans EOCD signatures backward and retries earlier ones when a candidate
/// fails — so a junk EOCD at EOF cannot be used to slip an earlier real ZIP64 record
/// past this pre-check (the module docs note the one residual bypass this does not
/// cover). Fail-open: any candidate whose EOCD -> locator -> EOCD64 chain does not
/// cleanly resolve is skipped, so this only ever *adds* a rejection for an
/// unambiguously over-declared archive, never false-rejects a legitimate one.
fn zip64_declares_over_ceiling<R: Read + Seek>(reader: &mut R) -> bool {
    let len = match reader.seek(SeekFrom::End(0)) {
        Ok(len) if len >= 22 => len,
        _ => return false,
    };
    let Some(tail) = read_eocd_tail(reader, len) else {
        return false;
    };
    for eocd in eocd_offsets(&tail) {
        if !eocd_claims_zip64(&tail, eocd) {
            continue;
        }
        let Some(off) = locator_eocd64_offset(&tail, eocd, len) else {
            continue;
        };
        if eocd64_total_entries(reader, off).is_some_and(|n| n > MAX_ZIP_DECLARED_ENTRIES) {
            return true;
        }
    }
    false
}

/// The trailing bytes that can hold the EOCD and its preceding ZIP64 locator.
fn read_eocd_tail<R: Read + Seek>(reader: &mut R, len: u64) -> Option<Vec<u8>> {
    let back = EOCD_MAX_BACK.min(len);
    reader.seek(SeekFrom::Start(len - back)).ok()?;
    let mut tail = vec![0u8; back as usize];
    reader.read_exact(&mut tail).ok()?;
    Some(tail)
}

/// Every EOCD signature offset in `tail`, latest first (the crate scans backward).
fn eocd_offsets(tail: &[u8]) -> impl Iterator<Item = usize> + '_ {
    (0..=tail.len().saturating_sub(22))
        .rev()
        .filter(|&i| tail[i..i + 4] == EOCD_SIG)
}

/// Whether the EOCD at `eocd` carries a ZIP64 sentinel in its count or CD-offset field.
fn eocd_claims_zip64(tail: &[u8], eocd: usize) -> bool {
    let nents = u16::from_le_bytes([tail[eocd + 10], tail[eocd + 11]]);
    let cdoff = u32::from_le_bytes([
        tail[eocd + 16],
        tail[eocd + 17],
        tail[eocd + 18],
        tail[eocd + 19],
    ]);
    nents == 0xffff || cdoff == 0xffff_ffff
}

/// The EOCD64 record offset named by the ZIP64 locator in the 20 bytes before the EOCD.
fn locator_eocd64_offset(tail: &[u8], eocd: usize, len: u64) -> Option<u64> {
    let loc = eocd.checked_sub(20)?;
    if tail[loc..loc + 4] != LOC64_SIG {
        return None;
    }
    let off = u64::from_le_bytes(tail[loc + 8..loc + 16].try_into().ok()?);
    // `off` is attacker-controlled: `off + 40` would overflow (panic under
    // overflow-checks, defeating the fail-open contract) for a value near u64::MAX.
    (off <= len.saturating_sub(40)).then_some(off)
}

/// The total-entries field (record offset 32) of the EOCD64 at `off`.
fn eocd64_total_entries<R: Read + Seek>(reader: &mut R, off: u64) -> Option<u64> {
    reader.seek(SeekFrom::Start(off)).ok()?;
    let mut head = [0u8; 40];
    reader.read_exact(&mut head).ok()?;
    if head[0..4] != EOCD64_SIG {
        return None;
    }
    Some(u64::from_le_bytes(head[32..40].try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A minimal, real central-directory header for a zero-length stored entry "a".
    fn one_cdh() -> Vec<u8> {
        let mut b = vec![0x50, 0x4b, 0x01, 0x02]; // CDH signature
        b.extend_from_slice(&[0x2d, 0, 0x2d, 0]); // version made by / needed
        b.extend_from_slice(&[0, 0, 0, 0]); // flags / method (stored)
        b.extend_from_slice(&[0, 0, 0, 0]); // mod time / date
        b.extend_from_slice(&[0, 0, 0, 0]); // crc32
        b.extend_from_slice(&[0, 0, 0, 0]); // compressed size
        b.extend_from_slice(&[0, 0, 0, 0]); // uncompressed size
        b.extend_from_slice(&[1, 0]); // name len
        b.extend_from_slice(&[0, 0, 0, 0]); // extra len / comment len
        b.extend_from_slice(&[0, 0, 0, 0]); // disk start / internal attrs
        b.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]); // external attrs / local offset
        b.push(b'a'); // name
        b
    }

    fn le(buf: &mut Vec<u8>, v: u64, bytes: usize) {
        for i in 0..bytes {
            buf.push(((v >> (8 * i)) & 0xff) as u8);
        }
    }

    /// Build a ZIP64 archive of `total_len` bytes declaring `declared` entries, laid
    /// out so the declared count survives the crate's own consistency check and would
    /// reach `Vec::with_capacity`: the directory starts at offset `declared` (so
    /// `number_of_files <= directory_start`) and the EOCD64 sits at the very end.
    fn zip64_declaring(total_len: usize, declared: u64) -> Vec<u8> {
        let cdh = one_cdh();
        let cd_offset = declared; // directory_start == declared
        let tail = 56 + 20 + 22;
        let eocd64_off = (total_len - tail) as u64;

        let mut buf = vec![0u8; cd_offset as usize];
        buf.extend_from_slice(&cdh);
        buf.resize(eocd64_off as usize, 0);

        // EOCD64 record. Fields in order: record_size, version made/needed, disk,
        // disk-with-CD, entries-this-disk, total-entries, CD size, CD offset. The
        // parser reads total-entries at record offset 32 (the 7th `le` below).
        let mut e = vec![0x50, 0x4b, 0x06, 0x06];
        le(&mut e, 44, 8);
        le(&mut e, 0x2d, 2);
        le(&mut e, 0x2d, 2);
        le(&mut e, 0, 4);
        le(&mut e, 0, 4);
        le(&mut e, declared, 8);
        le(&mut e, declared, 8);
        le(&mut e, cdh.len() as u64, 8);
        le(&mut e, cd_offset, 8);
        buf.extend_from_slice(&e);

        // ZIP64 locator: signature, disk-with-EOCD64, EOCD64 offset, total disks.
        let mut l = vec![0x50, 0x4b, 0x06, 0x07];
        le(&mut l, 0, 4);
        le(&mut l, eocd64_off, 8);
        le(&mut l, 1, 4);
        buf.extend_from_slice(&l);

        // EOCD. The 0xffff / 0xffffffff sentinels are what route the crate (and our
        // pre-check) to the ZIP64 records above.
        let mut z = vec![0x50, 0x4b, 0x05, 0x06];
        le(&mut z, 0, 2);
        le(&mut z, 0, 2);
        le(&mut z, 0xffff, 2);
        le(&mut z, 0xffff, 2);
        le(&mut z, 0xffff_ffff, 4);
        le(&mut z, 0xffff_ffff, 4);
        le(&mut z, 0, 2);
        buf.extend_from_slice(&z);

        buf
    }

    /// The guard rejects an over-declared ZIP64 archive BEFORE the crate's eager
    /// `with_capacity`. Reverting the ceiling check makes `open` delegate straight to
    /// `ZipArchive::new`, which (for this layout) reserves ~`declared * 232` bytes and
    /// then fails reading entry #2 with "Invalid Central Directory header" — a
    /// different error — so this assertion is red on the pre-guard code for the
    /// intended reason.
    #[test]
    fn rejects_overdeclared_zip64_before_reservation() {
        let declared = MAX_ZIP_DECLARED_ENTRIES + 1;
        // Buffer just large enough that the crate's own `file_len/46` bound would
        // otherwise accept this count into `with_capacity`.
        let total_len = (declared as usize) * 47 + 4096;
        let bytes = zip64_declaring(total_len, declared);

        match open(Cursor::new(bytes)) {
            Err(ZipError::UnsupportedArchive(msg)) => {
                assert!(
                    msg.contains("declared central directory entry count"),
                    "wrong rejection: {msg}"
                );
            }
            other => panic!("expected the guard's rejection, got {other:?}"),
        }
    }

    /// Exactly at the ceiling, the guard does NOT fire — the archive is handed to the
    /// crate (which then fails on the crafted body, proving only that the guard let it
    /// through rather than short-circuiting at the boundary).
    #[test]
    fn allows_declared_count_at_ceiling() {
        let declared = MAX_ZIP_DECLARED_ENTRIES;
        let total_len = (declared as usize) * 47 + 4096;
        let bytes = zip64_declaring(total_len, declared);

        // Not our guard's message: past the pre-check the crate takes over (and here
        // fails on the crafted body). The point is only that the guard did not fire.
        if let Err(ZipError::UnsupportedArchive(msg)) = open(Cursor::new(bytes)) {
            assert!(!msg.contains("declared central directory entry count"));
        }
    }

    /// The scan must not stop at the LAST EOCD signature: a junk EOCD appended at EOF
    /// (the crate would skip it and retry the earlier real record) must not hide an
    /// over-declared ZIP64 EOCD sitting before it. Red on the old single-candidate
    /// `last_eocd` code, which returned the junk one and failed open.
    #[test]
    fn rejects_overdeclared_behind_trailing_junk_eocd() {
        let declared = MAX_ZIP_DECLARED_ENTRIES + 1;
        let total_len = (declared as usize) * 47 + 4096;
        let mut bytes = zip64_declaring(total_len, declared);
        // Append a minimal non-ZIP64 EOCD so the real one is no longer the last.
        let mut junk = vec![0x50, 0x4b, 0x05, 0x06];
        junk.resize(22, 0);
        bytes.extend_from_slice(&junk);

        match open(Cursor::new(bytes)) {
            Err(ZipError::UnsupportedArchive(msg)) => assert!(
                msg.contains("declared central directory entry count"),
                "wrong rejection: {msg}"
            ),
            other => panic!("expected the guard's rejection, got {other:?}"),
        }
    }

    /// A ZIP64 locator whose EOCD64 offset is near `u64::MAX` must not panic the
    /// parse (`off + 40` would overflow under overflow-checks). Fail-open: return
    /// `None`. Red on the pre-fix `off + 40 <= len` in any overflow-checked build.
    #[test]
    fn huge_locator_offset_does_not_overflow() {
        // A 20-byte ZIP64 locator carrying offset u64::MAX, then a 22-byte EOCD.
        let mut tail = vec![0x50, 0x4b, 0x06, 0x07];
        le(&mut tail, 0, 4);
        le(&mut tail, u64::MAX, 8);
        le(&mut tail, 1, 4);
        let eocd = tail.len();
        tail.extend_from_slice(&[0x50, 0x4b, 0x05, 0x06]);
        tail.resize(eocd + 22, 0);
        assert_eq!(locator_eocd64_offset(&tail, eocd, tail.len() as u64), None);
    }

    /// An ordinary small archive is unaffected: the guard returns `None` (no ZIP64
    /// sentinel) and the real archive opens.
    #[test]
    fn opens_ordinary_small_archive() {
        // A tiny empty ZIP (EOCD only, zero entries).
        let mut z = vec![0x50, 0x4b, 0x05, 0x06];
        le(&mut z, 0, 2);
        le(&mut z, 0, 2);
        le(&mut z, 0, 2);
        le(&mut z, 0, 2);
        le(&mut z, 0, 4);
        le(&mut z, 0, 4);
        le(&mut z, 0, 2);
        let zip = open(Cursor::new(z)).expect("empty zip opens");
        assert_eq!(zip.len(), 0);
    }
}
