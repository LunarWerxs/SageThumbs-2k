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
    if let Some(declared) = declared_zip64_entry_count(&mut reader) {
        if declared > MAX_ZIP_DECLARED_ENTRIES {
            return Err(ZipError::UnsupportedArchive(
                "declared central directory entry count exceeds SageThumbs limit",
            ));
        }
    }
    // `ZipArchive::new` re-seeks to the end itself, so our cursor position is moot.
    ZipArchive::new(reader)
}

/// The total entry count a ZIP64 end-of-central-directory record declares, or `None`.
///
/// Fail-open by design: any parse that does not cleanly resolve the
/// EOCD -> locator -> EOCD64 chain returns `None`, leaving the archive to the crate's
/// own (already file-size-bounded) handling. It can therefore only ever *add* a
/// rejection for an unambiguously over-declared ZIP64 archive, never false-reject a
/// legitimate one. A plain (non-ZIP64) archive also returns `None` — its `u16` count
/// cannot exceed the ceiling.
fn declared_zip64_entry_count<R: Read + Seek>(reader: &mut R) -> Option<u64> {
    let len = reader.seek(SeekFrom::End(0)).ok()?;
    if len < 22 {
        return None;
    }
    let tail = read_eocd_tail(reader, len)?;
    let eocd = last_eocd(&tail)?;
    if !eocd_claims_zip64(&tail, eocd) {
        return None;
    }
    let eocd64_off = locator_eocd64_offset(&tail, eocd, len)?;
    eocd64_total_entries(reader, eocd64_off)
}

/// The trailing bytes that can hold the EOCD and its preceding ZIP64 locator.
fn read_eocd_tail<R: Read + Seek>(reader: &mut R, len: u64) -> Option<Vec<u8>> {
    let back = EOCD_MAX_BACK.min(len);
    reader.seek(SeekFrom::Start(len - back)).ok()?;
    let mut tail = vec![0u8; back as usize];
    reader.read_exact(&mut tail).ok()?;
    Some(tail)
}

/// Offset of the last EOCD signature in `tail` (the crate scans backward likewise).
fn last_eocd(tail: &[u8]) -> Option<usize> {
    (0..=tail.len().saturating_sub(22))
        .rev()
        .find(|&i| tail[i..i + 4] == EOCD_SIG)
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
    (off + 40 <= len).then_some(off)
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
