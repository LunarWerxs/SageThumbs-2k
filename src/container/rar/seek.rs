//! A big RAR's covers without buffering the archive: walk the block headers over a reader (each
//! header says how long its data is, so the walk seeks past every entry), pick the covers from
//! the names exactly as the buffered path does, and hand `rars` a small archive holding only
//! those entries. Past the input ceiling the buffered read is refused, and the file's head is no
//! help either: the page that sorts first can sit anywhere, so the whole list has to be read.
//!
//! Only an archive that can be cut apart safely: unencrypted, single-volume and not solid (a
//! solid entry's data depends on every entry before it). Anything else is `None`.

use std::io::{Read, Seek, SeekFrom};

use super::super::names::decode_entry_name;
use super::super::select::{dedupe_by_name, pick_covers, CoverPrefs, Entry};

const RAR4: &[u8] = b"Rar!\x1a\x07\x00";
const RAR5: &[u8] = b"Rar!\x1a\x07\x01\x00";

/// Most bytes the cut-down archive may hold (its headers and the picked entries' packed data).
const MAX_MINI_BYTES: u64 = 2 * super::super::MAX_COVER;
/// Most blocks walked: the buffered listing's entry cap, with room for service blocks.
const MAX_BLOCKS: usize = 2 * super::super::MAX_LIST_ENTRIES;
/// Longest header read. RAR 4 cannot exceed 64 KiB; RAR 5 allows 2 MiB and never comes near it.
const MAX_HEADER: u64 = 2 << 20;

/// One member: where its block (header and data) sits, and what the picker needs.
struct Member {
    at: u64,
    len: u64,
    name: Vec<u8>,
    is_dir: bool,
    size: u64,
}

/// What the walk found: the signature and main header (`lead` bytes from the start), the file
/// blocks, and the end-of-archive block.
#[derive(Default)]
struct Layout {
    lead: u64,
    members: Vec<Member>,
    end: Option<(u64, u64)>,
}

/// Up to `want` covers of the RAR `r` holds, picked and decoded as [`super::extract_n`] picks
/// and decodes them from the whole file.
pub(crate) fn covers_seek<R: Read + Seek>(
    mut r: R,
    want: usize,
    prefs: &CoverPrefs,
) -> Option<Vec<Vec<u8>>> {
    let total = r.seek(SeekFrom::End(0)).ok()?;
    let mut sig = [0u8; 8];
    read_at(&mut r, 0, &mut sig)?;
    let layout = if sig.starts_with(RAR5) {
        walk(&mut r, total, RAR5.len() as u64, block5)?
    } else if sig.starts_with(RAR4) {
        walk(&mut r, total, RAR4.len() as u64, block4)?
    } else {
        return None;
    };
    if layout.lead == 0 || layout.lead > MAX_HEADER {
        return None;
    }
    let keep = picks(&layout, want, prefs)?;
    let mini = cut_down(&mut r, &layout, &keep)?;
    super::extract_n(&mini, want, prefs)
}

/// The members the buffered read would pick, in the order they sit in the file.
fn picks(layout: &Layout, want: usize, prefs: &CoverPrefs) -> Option<Vec<usize>> {
    let entries: Vec<Entry> = layout
        .members
        .iter()
        .take(super::super::MAX_LIST_ENTRIES)
        .map(|m| Entry {
            name: decode_entry_name(&m.name, false),
            is_dir: m.is_dir,
            size: m.size,
        })
        .collect();
    let mut keep = dedupe_by_name(pick_covers(&entries, want, prefs), &entries);
    keep.sort_unstable();
    (!keep.is_empty()).then_some(keep)
}

/// The archive cut down to its signature and main header, the `keep` members and the end
/// block, read off `r`; `None` past [`MAX_MINI_BYTES`].
fn cut_down<R: Read + Seek>(r: &mut R, layout: &Layout, keep: &[usize]) -> Option<Vec<u8>> {
    let end_len = layout.end.map_or(0, |(_, len)| len);
    let bytes = keep.iter().try_fold(layout.lead + end_len, |sum, &i| {
        sum.checked_add(layout.members[i].len)
    })?;
    if bytes > MAX_MINI_BYTES {
        return None;
    }
    let mut mini = read_vec(r, 0, layout.lead)?;
    for &i in keep {
        let m = &layout.members[i];
        mini.extend(read_vec(r, m.at, m.len)?);
    }
    if let Some((at, len)) = layout.end {
        mini.extend(read_vec(r, at, len)?);
    }
    Some(mini)
}

/// What one block adds to the walk; the block reader answers `None` for an archive that cannot
/// be cut apart (a volume, solid, encrypted) or a block it cannot read.
enum Step {
    /// A block to step over, `len` bytes of header and data.
    Skip(u64),
    /// The main header, `len` bytes: the lead ends after it.
    Main(u64),
    /// A file entry.
    File(Member),
    /// The end-of-archive block, `len` bytes: the walk stops.
    End(u64),
}

/// Walk the blocks from `first` with `block`, the reader for this RAR version.
fn walk<R: Read + Seek>(
    r: &mut R,
    total: u64,
    first: u64,
    block: fn(&mut R, u64) -> Option<Step>,
) -> Option<Layout> {
    let mut out = Layout::default();
    let mut at = first;
    for _ in 0..MAX_BLOCKS {
        if at + 7 > total {
            break;
        }
        let len = match block(r, at)? {
            Step::Skip(len) => len,
            Step::Main(len) => {
                out.lead = at + len;
                len
            }
            Step::File(m) => {
                let len = m.len;
                out.members.push(m);
                len
            }
            Step::End(len) => {
                out.end = Some((at, len));
                break;
            }
        };
        at = at.checked_add(len)?;
    }
    Some(out)
}

/// A RAR 1.5-4.x block at `at`: a 7-byte base header (CRC, type, flags, header size), the data
/// size in the four bytes after it when flag 0x8000 is set.
fn block4<R: Read + Seek>(r: &mut R, at: u64) -> Option<Step> {
    let mut base = [0u8; 7];
    read_at(r, at, &mut base)?;
    let (kind, flags) = (base[2], u16::from_le_bytes([base[3], base[4]]));
    let head = u64::from(u16::from_le_bytes([base[5], base[6]]));
    if head < 7 {
        return None;
    }
    let header = read_vec(r, at, head)?;
    let data = if flags & 0x8000 != 0 {
        u64::from(le32(&header, 7)?)
    } else {
        0
    };
    let len = head.checked_add(data)?;
    match kind {
        // Main header: multi-volume, solid, encrypted headers.
        0x73 if flags & (0x0001 | 0x0008 | 0x0080) != 0 => None,
        0x73 => Some(Step::Main(len)),
        0x74 => file4(&header, flags, at, head, data).map(Step::File),
        0x7B => Some(Step::End(len)),
        _ => Some(Step::Skip(len)),
    }
}

/// A RAR 4 file header's own fields; `data` is the low 32 bits of the packed size.
fn file4(header: &[u8], flags: u16, at: u64, head: u64, mut data: u64) -> Option<Member> {
    // Split across volumes, encrypted, solid.
    if flags & (0x0001 | 0x0002 | 0x0004 | 0x0010) != 0 {
        return None;
    }
    let large = flags & 0x0100 != 0;
    let mut size = u64::from(le32(header, 11)?);
    if large {
        data += u64::from(le32(header, 32)?) << 32;
        size |= u64::from(le32(header, 36)?) << 32;
    }
    let name_len = usize::from(u16::from_le_bytes([*header.get(26)?, *header.get(27)?]));
    let from = if large { 40 } else { 32 };
    let raw = header.get(from..from + name_len)?;
    // A Unicode name is stored after the legacy one, behind a NUL.
    let name = raw.split(|&b| b == 0).next().unwrap_or(raw).to_vec();
    Some(Member {
        at,
        len: head.checked_add(data)?,
        name,
        is_dir: flags & 0x00E0 == 0x00E0,
        size,
    })
}

/// The fields every RAR 5 header opens with, and where its own fields start.
struct Head5 {
    kind: u64,
    flags: u64,
    header: Vec<u8>,
    /// Offset in `header` just past the common fields.
    q: usize,
    /// The whole block: CRC, size, header and data.
    len: u64,
}

/// Read the RAR 5 header at `at`: CRC32, the header size as a vint, then type, flags, and the
/// optional extra-area and data sizes.
fn head5<R: Read + Seek>(r: &mut R, at: u64) -> Option<Head5> {
    let mut pre = [0u8; 7];
    read_at(r, at, &mut pre)?;
    let mut p = 4;
    let head = vint(&pre, &mut p)?;
    if head == 0 || head > MAX_HEADER {
        return None;
    }
    let header = read_vec(r, at + p as u64, head)?;
    let mut q = 0;
    let kind = vint(&header, &mut q)?;
    let flags = vint(&header, &mut q)?;
    // The extra-area size (not needed), then the data size.
    optional_vint(&header, &mut q, flags & 0x0001 != 0)?;
    let data = optional_vint(&header, &mut q, flags & 0x0002 != 0)?;
    let len = (p as u64).checked_add(head)?.checked_add(data)?;
    Some(Head5 {
        kind,
        flags,
        header,
        q,
        len,
    })
}

/// A RAR 5 block at `at`.
fn block5<R: Read + Seek>(r: &mut R, at: u64) -> Option<Step> {
    let Head5 {
        kind,
        flags,
        header,
        mut q,
        len,
    } = head5(r, at)?;
    match kind {
        // Main header: a volume, or solid.
        1 if vint(&header, &mut q)? & (0x0001 | 0x0004) != 0 => None,
        1 => Some(Step::Main(len)),
        // Archive encryption: every header after it is ciphertext.
        4 => None,
        // Data continued from or into another volume.
        2 if flags & (0x0008 | 0x0010) != 0 => None,
        2 => file5(&header, q, at, len).map(Step::File),
        5 => Some(Step::End(len)),
        _ => Some(Step::Skip(len)),
    }
}

/// A RAR 5 file header's own fields, from `q` (just past the common ones).
fn file5(header: &[u8], mut q: usize, at: u64, len: u64) -> Option<Member> {
    let file_flags = vint(header, &mut q)?;
    let size = vint(header, &mut q)?;
    vint(header, &mut q)?; // attributes
    if file_flags & 0x0002 != 0 {
        q += 4; // modification time
    }
    if file_flags & 0x0004 != 0 {
        q += 4; // data CRC32
    }
    // Solid: this entry's data continues the previous entry's dictionary.
    if vint(header, &mut q)? & 0x0040 != 0 {
        return None;
    }
    vint(header, &mut q)?; // host OS
    let name_len = usize::try_from(vint(header, &mut q)?).ok()?;
    let name = header.get(q..q.checked_add(name_len)?)?.to_vec();
    Some(Member {
        at,
        len,
        name,
        is_dir: file_flags & 0x0001 != 0,
        size,
    })
}

/// A vint the header holds only when its flag says so; zero when it does not.
fn optional_vint(b: &[u8], at: &mut usize, present: bool) -> Option<u64> {
    if present {
        vint(b, at)
    } else {
        Some(0)
    }
}

/// A RAR 5 variable-length integer: seven bits a byte, low first, high bit set on all but the
/// last (at most ten bytes).
fn vint(b: &[u8], at: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    for shift in (0..70).step_by(7) {
        let byte = *b.get(*at)?;
        *at += 1;
        v |= u64::from(byte & 0x7F).checked_shl(shift)?;
        if byte & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

fn le32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn read_at<R: Read + Seek>(r: &mut R, at: u64, buf: &mut [u8]) -> Option<()> {
    r.seek(SeekFrom::Start(at)).ok()?;
    r.read_exact(buf).ok()
}

fn read_vec<R: Read + Seek>(r: &mut R, at: u64, len: u64) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; usize::try_from(len).ok()?];
    read_at(r, at, &mut buf)?;
    Some(buf)
}

/// A RAR 5 writer for stored entries: the tests' fixtures and the fuzz seed.
#[cfg(test)]
mod build {
    pub(super) fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &b in bytes {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    fn vint(mut v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let b = (v & 0x7F) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                return out;
            }
            out.push(b | 0x80);
        }
    }

    /// A header block: CRC32 of what follows, the header size, the header.
    fn block(fields: &[u8]) -> Vec<u8> {
        let mut head = vint(fields.len() as u64);
        head.extend(fields);
        let mut out = crc32(&head).to_le_bytes().to_vec();
        out.extend(head);
        out
    }

    /// A stored entry named `name` holding `data`, as the big-file gate's grower writes one
    /// (`scripts/bigfiles/ballast.py::_rar5_entry`).
    pub(super) fn stored(name: &[u8], data: &[u8]) -> Vec<u8> {
        let n = data.len() as u64;
        // File header; data area present; its size; CRC present; unpacked size; attributes.
        let mut fields = [vint(2), vint(2), vint(n), vint(4), vint(n), vint(0x20)].concat();
        fields.extend(crc32(data).to_le_bytes());
        // Stored, Windows, the name.
        fields.extend([vint(0), vint(0), vint(name.len() as u64)].concat());
        fields.extend(name);
        let mut out = block(&fields);
        out.extend(data);
        out
    }

    /// A whole archive of stored entries: signature, main header, entries, end block.
    pub(super) fn archive(entries: &[(&[u8], &[u8])]) -> Vec<u8> {
        let mut out = super::RAR5.to_vec();
        out.extend(block(&[1, 0, 0]));
        for (name, data) in entries {
            out.extend(stored(name, data));
        }
        out.extend(block(&[5, 0, 0]));
        out
    }
}

/// A small RAR 5 comic (two stored pages), for the fuzzer's seed list.
#[cfg(test)]
pub(crate) fn fuzz_seed() -> Vec<u8> {
    let page = |shade: u8| {
        let img = image::RgbImage::from_pixel(8, 8, image::Rgb([shade, 40, 90]));
        let mut png = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .expect("png");
        png
    };
    build::archive(&[(b"page-002.png", &page(200)), (b"page-001.png", &page(20))])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn rar5_entry(name: &[u8], n: usize) -> Vec<u8> {
        build::stored(name, &vec![0u8; n])
    }

    /// The seed is an archive `rars` itself reads, and the walk finds its first page.
    #[test]
    fn the_seed_is_a_real_archive() {
        let seed = fuzz_seed();
        let whole = super::super::extract_n(&seed, 1, &PREFS).expect("rars reads the seed");
        assert_eq!(covers_seek(Cursor::new(&seed), 1, &PREFS), Some(whole));
    }

    const PREFS: CoverPrefs = CoverPrefs {
        prefer_cover: true,
        sort: true,
        skip_scanlation: true,
    };

    fn corpus_rars() -> Vec<(&'static str, Vec<u8>)> {
        ["real.rar", "sample.cbr", "archive-sample.rar"]
            .into_iter()
            .filter_map(|name| match crate::testcorpus::read(name) {
                Some(bytes) => Some((name, bytes)),
                None => {
                    eprintln!("NOT MEASURED: {name} absent");
                    None
                }
            })
            .collect()
    }

    /// Whatever the buffered read picks, the header walk picks and decodes too.
    #[test]
    fn the_walk_finds_the_covers_the_buffered_read_finds() {
        let prefs = PREFS;
        for (name, bytes) in corpus_rars() {
            for want in [1, 4] {
                let whole = super::super::extract_n(&bytes, want, &prefs);
                assert!(whole.is_some(), "{name}: no cover to compare");
                let walked = covers_seek(Cursor::new(&bytes), want, &prefs);
                assert_eq!(walked, whole, "{name}, {want} covers");
            }
        }
    }

    /// The big-file gate's shape: a large entry added last. The walk seeks past its data.
    #[test]
    fn a_large_entry_after_the_pages_is_skipped_not_read() {
        let prefs = PREFS;
        for (name, bytes) in corpus_rars() {
            if !bytes.starts_with(RAR5) {
                continue;
            }
            let layout = walk(
                &mut Cursor::new(&bytes),
                bytes.len() as u64,
                RAR5.len() as u64,
                block5,
            )
            .expect("walks");
            let Some((end, _)) = layout.end else { continue };
            let end = end as usize;
            let mut grown = bytes[..end].to_vec();
            grown.extend(rar5_entry(b"zzzz-ballast.bin", 3 << 20));
            grown.extend(&bytes[end..]);
            let whole = super::super::extract_n(&bytes, 1, &prefs);
            let walked = covers_seek(Cursor::new(&grown), 1, &prefs);
            assert!(whole.is_some(), "{name}: no cover to compare");
            assert_eq!(walked, whole, "{name}");
        }
    }

    #[test]
    fn a_damaged_archive_is_refused_without_panicking() {
        let prefs = PREFS;
        for (_, bytes) in corpus_rars() {
            for cut in (0..bytes.len().min(4096)).chain([bytes.len() / 2]) {
                let _ = covers_seek(Cursor::new(&bytes[..cut]), 1, &prefs);
            }
            let mut flipped = bytes.clone();
            for at in (0..flipped.len().min(2048)).step_by(7) {
                flipped[at] ^= 0x5A;
                let _ = covers_seek(Cursor::new(&flipped), 1, &prefs);
                flipped[at] ^= 0x5A;
            }
        }
        assert!(covers_seek(Cursor::new(b"not a rar".to_vec()), 1, &prefs).is_none());
    }
}
