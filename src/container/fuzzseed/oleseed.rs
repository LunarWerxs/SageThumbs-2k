#![cfg(test)]

//! Compound-file (OLE2) seeds: the bare container, named streams, an Outlook message and a 3ds Max scene.

use super::*;

/// A minimal but real OLE/CFB compound file: 512-byte header, one FAT sector, one directory
/// sector holding a Root Entry plus one named stream whose contents live in a third sector.
///
/// Worth the length. This is the container legacy Office files use, the FAT is a linked list
/// read out of the file's own bytes, and the directory is an array the header indexes into —
/// so the mutations that matter are chain cycles, out-of-range sector numbers and lengths that
/// exceed the file. None of that is reachable from a seed that is only a valid signature.
pub(super) fn synthetic_ole() -> Vec<u8> {
    // Recognisable filler, so a mutated read that wanders outside the stream is visible.
    let filler: Vec<u8> = (0..4096).map(|i| (i % 251) as u8).collect();
    synthetic_ole_with(&filler)
}

/// [`synthetic_ole`] carrying caller-supplied `\x05SummaryInformation` contents, so the 3ds Max
/// seed can put a real property set inside the same container instead of duplicating all of it.
pub(super) fn synthetic_ole_with(payload: &[u8]) -> Vec<u8> {
    synthetic_ole_named("\u{5}SummaryInformation", payload)
}

/// [`synthetic_ole_with`] over an arbitrary stream NAME - what the SolidWorks seed needs,
/// since its preview lives in `PreviewPNG` rather than the summary property set. ONE container
/// builder, two stream names, so the FAT/directory mutations are exercised identically for both.
pub(super) fn synthetic_ole_named(stream_name: &str, payload: &[u8]) -> Vec<u8> {
    const SECTOR: usize = 512;
    const ENDOFCHAIN: u32 = 0xFFFF_FFFE;
    const FREESECT: u32 = 0xFFFF_FFFF;
    const FIRST_DATA: u32 = 2;
    const MINI_CUTOFF: usize = 4096;

    // The stream is padded to at least `MINI_CUTOFF` and spans whole sectors, chained
    // 2 -> 3 -> ... Two reasons, both deliberate: a multi-hop chain is what makes `follow`
    // worth fuzzing at all (one wrong `next` and it is a cycle), and clearing the cutoff keeps
    // it on the main-FAT path rather than the mini-stream path, which would need a root
    // mini-stream this seed does not model.
    let stream_len = payload.len().max(MINI_CUTOFF).div_ceil(SECTOR) * SECTOR;
    let data_sectors = stream_len / SECTOR;

    let mut header = vec![0u8; SECTOR];
    header[0..8].copy_from_slice(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]);
    header[0x18..0x1A].copy_from_slice(&3u16.to_le_bytes()); // major version
    header[0x1C..0x1E].copy_from_slice(&0xFFFEu16.to_le_bytes()); // little-endian marker
    header[0x1E..0x20].copy_from_slice(&9u16.to_le_bytes()); // sector shift: 512
    header[0x20..0x22].copy_from_slice(&6u16.to_le_bytes()); // mini sector shift: 64
    header[0x2C..0x30].copy_from_slice(&1u32.to_le_bytes()); // FAT sector count
    header[0x30..0x34].copy_from_slice(&1u32.to_le_bytes()); // first directory sector
    header[0x38..0x3C].copy_from_slice(&(MINI_CUTOFF as u32).to_le_bytes()); // mini stream cutoff
    header[0x3C..0x40].copy_from_slice(&ENDOFCHAIN.to_le_bytes()); // first mini FAT
    header[0x44..0x48].copy_from_slice(&ENDOFCHAIN.to_le_bytes()); // first DIFAT
                                                                   // DIFAT[0] = sector 0 holds the FAT; the remaining 108 entries are free.
    header[0x4C..0x50].copy_from_slice(&0u32.to_le_bytes());
    for i in 1..109usize {
        let o = 0x4C + i * 4;
        header[o..o + 4].copy_from_slice(&FREESECT.to_le_bytes());
    }

    // Sector 0: the FAT. 0 = itself, 1 = the directory, then the data chain 2 -> 3 -> ... -> 9.
    let mut fat = vec![0u8; SECTOR];
    let put = |fat: &mut [u8], i: usize, v: u32| {
        fat[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    };
    put(&mut fat, 0, ENDOFCHAIN);
    put(&mut fat, 1, ENDOFCHAIN);
    for i in 0..data_sectors {
        let sector = FIRST_DATA as usize + i;
        let next = if i + 1 == data_sectors {
            ENDOFCHAIN
        } else {
            (sector + 1) as u32
        };
        put(&mut fat, sector, next);
    }
    for i in (FIRST_DATA as usize + data_sectors)..(SECTOR / 4) {
        put(&mut fat, i, FREESECT);
    }

    // Sector 1: the directory — four 128-byte entries, two of them used.
    let mut dir = vec![0u8; SECTOR];
    let mut entry = |slot: usize, name: &str, kind: u8, start: u32, size: u64| {
        let base = slot * 128;
        let utf16: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        for (i, c) in utf16.iter().enumerate() {
            dir[base + i * 2..base + i * 2 + 2].copy_from_slice(&c.to_le_bytes());
        }
        let name_len = (utf16.len() * 2) as u16;
        dir[base + 64..base + 66].copy_from_slice(&name_len.to_le_bytes());
        dir[base + 66] = kind; // 5 = root storage, 2 = stream
        dir[base + 67] = 1; // colour: black
        dir[base + 68..base + 72].copy_from_slice(&FREESECT.to_le_bytes()); // left sibling
        dir[base + 72..base + 76].copy_from_slice(&FREESECT.to_le_bytes()); // right sibling
        dir[base + 76..base + 80].copy_from_slice(&FREESECT.to_le_bytes()); // child
        dir[base + 116..base + 120].copy_from_slice(&start.to_le_bytes());
        dir[base + 120..base + 128].copy_from_slice(&size.to_le_bytes());
    };
    entry(0, "Root Entry", 5, ENDOFCHAIN, 0);
    entry(1, stream_name, 2, FIRST_DATA, stream_len as u64);
    // Root's child points at the stream entry, which is how the walk finds it.
    dir[76..80].copy_from_slice(&1u32.to_le_bytes());
    for slot in 2..4usize {
        dir[slot * 128 + 66] = 0; // unallocated
    }

    let mut stream = payload.to_vec();
    stream.resize(stream_len, 0);

    [header, fat, dir, stream].concat()
}

/// An Outlook `.msg`-shaped compound file: TWO directory entries sharing one stream name, and
/// every stream small enough to live in the MINISTREAM.
///
/// [`synthetic_ole`] cannot reach the code this exists for, and that is the whole reason it is
/// a separate seed rather than a parameter. That one has a single stream, padded past the mini
/// cutoff specifically to stay on the main-FAT path — so a mutation of it never touches the
/// miniFAT, never touches the root's ministream, and never makes `read_streams` collect a
/// second hit. All three are `read_streams`-only behaviour that shipped in 2.4.0 with no seed
/// behind it: the miniFAT is a second linked list read out of hostile bytes, the ministream is
/// a stream whose own chain has to be followed to slice mini-sectors out of, and the collector
/// CACHES both across hits, so a mutation that corrupts them once is then reused.
///
/// Real `.msg` files are exactly this shape — an attachment's long filename is a ~40-byte
/// stream and there is one per attachment, all identically named.
pub(super) fn synthetic_msg() -> Vec<u8> {
    const SECTOR: usize = 512;
    const MINI: usize = 64;
    const ENDOFCHAIN: u32 = 0xFFFF_FFFE;
    const FREESECT: u32 = 0xFFFF_FFFF;
    const ATTACH_NAME: &str = "__substg1.0_3707001F";
    const SUBJECT_NAME: &str = "__substg1.0_0037001F";
    // 0 = FAT, 1 = directory, 2 = miniFAT, 3 = the ministream's only sector.
    const MINIFAT_SECTOR: u32 = 2;
    const MINISTREAM_SECTOR: u32 = 3;

    let mut header = vec![0u8; SECTOR];
    header[0..8].copy_from_slice(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]);
    header[0x18..0x1A].copy_from_slice(&3u16.to_le_bytes()); // major version
    header[0x1C..0x1E].copy_from_slice(&0xFFFEu16.to_le_bytes()); // little-endian marker
    header[0x1E..0x20].copy_from_slice(&9u16.to_le_bytes()); // sector shift: 512
    header[0x20..0x22].copy_from_slice(&6u16.to_le_bytes()); // mini sector shift: 64
    header[0x2C..0x30].copy_from_slice(&1u32.to_le_bytes()); // FAT sector count
    header[0x30..0x34].copy_from_slice(&1u32.to_le_bytes()); // first directory sector
    header[0x38..0x3C].copy_from_slice(&4096u32.to_le_bytes()); // mini stream cutoff
    header[0x3C..0x40].copy_from_slice(&MINIFAT_SECTOR.to_le_bytes()); // first miniFAT sector
    header[0x40..0x44].copy_from_slice(&1u32.to_le_bytes()); // miniFAT sector count
    header[0x44..0x48].copy_from_slice(&ENDOFCHAIN.to_le_bytes()); // first DIFAT
    header[0x4C..0x50].copy_from_slice(&0u32.to_le_bytes()); // DIFAT[0] -> sector 0
    for i in 1..109usize {
        let o = 0x4C + i * 4;
        header[o..o + 4].copy_from_slice(&FREESECT.to_le_bytes());
    }

    // Sector 0: the FAT. Four sectors in use, each its own one-hop chain.
    let mut fat = vec![0u8; SECTOR];
    for i in 0..SECTOR / 4 {
        let v = if i < 4 { ENDOFCHAIN } else { FREESECT };
        fat[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }

    // Sector 3: the ministream, eight 64-byte mini sectors. The first three carry UTF-16LE
    // text; a mutated length field that overruns one of them lands in the recognisable filler.
    let mini_payloads: [&str; 3] = ["report.pdf", "photo.jpg", "Q3 numbers"];
    let mut ministream = vec![0xA5u8; SECTOR];
    for (slot, text) in mini_payloads.iter().enumerate() {
        let utf16: Vec<u8> = text
            .encode_utf16()
            .flat_map(|c| c.to_le_bytes())
            .collect::<Vec<u8>>();
        ministream[slot * MINI..slot * MINI + utf16.len()].copy_from_slice(&utf16);
    }

    // Sector 2: the miniFAT. Three one-hop mini chains, the rest free.
    let mut minifat = vec![0u8; SECTOR];
    for i in 0..SECTOR / 4 {
        let v = if i < mini_payloads.len() {
            ENDOFCHAIN
        } else {
            FREESECT
        };
        minifat[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }

    // Sector 1: the directory. Slots 1 and 2 deliberately share ATTACH_NAME — that duplicate
    // is what `read_streams` exists for and what `read_stream` would stop at.
    let mut dir = vec![0u8; SECTOR];
    let mut entry = |slot: usize, name: &str, kind: u8, start: u32, size: u64| {
        let base = slot * 128;
        let utf16: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        for (i, c) in utf16.iter().enumerate() {
            dir[base + i * 2..base + i * 2 + 2].copy_from_slice(&c.to_le_bytes());
        }
        dir[base + 64..base + 66].copy_from_slice(&((utf16.len() * 2) as u16).to_le_bytes());
        dir[base + 66] = kind; // 5 = root storage, 2 = stream
        dir[base + 67] = 1; // colour: black
        for off in [68, 72, 76] {
            dir[base + off..base + off + 4].copy_from_slice(&FREESECT.to_le_bytes());
        }
        dir[base + 116..base + 120].copy_from_slice(&start.to_le_bytes());
        dir[base + 120..base + 128].copy_from_slice(&size.to_le_bytes());
    };
    // The root's start sector IS the ministream, and its size is how far into it a mini
    // sector may be read from — both are file-supplied numbers the slicing trusts.
    entry(0, "Root Entry", 5, MINISTREAM_SECTOR, SECTOR as u64);
    entry(1, ATTACH_NAME, 2, 0, (mini_payloads[0].len() * 2) as u64);
    entry(2, ATTACH_NAME, 2, 1, (mini_payloads[1].len() * 2) as u64);
    entry(3, SUBJECT_NAME, 2, 2, (mini_payloads[2].len() * 2) as u64);
    dir[76..80].copy_from_slice(&1u32.to_le_bytes()); // root's child -> slot 1

    // Order IS the sector numbering the header and the entries above point at:
    // 0 = FAT, 1 = directory, 2 = miniFAT, 3 = ministream.
    [header, fat, dir, minifat, ministream].concat()
}

/// 3ds Max: the same compound file, with a real `SummaryInformation` property set whose
/// `PIDSI_THUMBNAIL` property is a `CF_DIB` clipboard blob.
///
/// This is the one seed built by composing another, and that is the point: `max::extract`'s
/// first act is `ole::read_stream`, so it can only be reached at all through a container that
/// actually resolves. Everything past that — the section offset, the property-pair table, the
/// `cb` length that the data slice is cut from — is arithmetic on file-supplied numbers.
pub(super) fn synthetic_max() -> Vec<u8> {
    const VT_CF: u32 = 0x0047;
    const CF_DIB: u32 = 8;
    const PIDSI_THUMBNAIL: u32 = 0x11;
    let dib = dib_8bpp(8, 8);

    let mut s = Vec::new();
    s.extend_from_slice(&0xFFFEu16.to_le_bytes()); // byte-order marker
    s.extend_from_slice(&0u16.to_le_bytes()); // format version
    s.extend_from_slice(&0u32.to_le_bytes()); // OS version
    s.extend_from_slice(&[0u8; 16]); // class id
    s.extend_from_slice(&1u32.to_le_bytes()); // one section
    s.extend_from_slice(&[0u8; 16]); // section format id
    let section = 48u32;
    s.extend_from_slice(&section.to_le_bytes()); // section offset — read at 44
    debug_assert_eq!(s.len(), section as usize);
    s.extend_from_slice(&0u32.to_le_bytes()); // section size (unread)
    s.extend_from_slice(&1u32.to_le_bytes()); // property count
    s.extend_from_slice(&PIDSI_THUMBNAIL.to_le_bytes());
    s.extend_from_slice(&16u32.to_le_bytes()); // property offset, relative to the section
    debug_assert_eq!(s.len(), section as usize + 16);
    s.extend_from_slice(&VT_CF.to_le_bytes());
    s.extend_from_slice(&((dib.len() + 4) as u32).to_le_bytes()); // cb = tag + data
    s.extend_from_slice(&CF_DIB.to_le_bytes());
    s.extend_from_slice(&dib);

    synthetic_ole_with(&s)
}
