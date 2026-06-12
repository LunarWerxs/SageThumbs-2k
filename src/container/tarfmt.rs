//! CBT comic archives: an (uncompressed) TAR of page images. Hand-parsed — TAR
//! is a flat sequence of 512-byte headers, each followed by the file's bytes
//! padded up to a 512-byte boundary. We collect the image entries and reuse the
//! shared cover-selection (natural sort / "cover" preference / junk skip).

use super::select::{pick_cover, Entry};

pub fn extract(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut entries: Vec<Entry> = Vec::new();
    let mut ranges: Vec<(usize, usize)> = Vec::new(); // (data offset, size), parallel to entries
    let mut pos = 0usize;
    while pos + 512 <= bytes.len() {
        let header = &bytes[pos..pos + 512];
        if header.iter().all(|&b| b == 0) {
            break; // zero block ends the archive
        }
        let size = tar_octal(&header[124..136])?;
        let data_off = pos + 512;
        if data_off.checked_add(size)? > bytes.len() {
            break;
        }
        let name = tar_name(header);
        // typeflag at offset 156: '0' or NUL = a regular file.
        let is_regular = matches!(header[156], b'0' | 0);
        if is_regular && !name.is_empty() && super::is_image_name(&name) {
            entries.push(Entry { name, is_dir: false, size: size as u64 });
            ranges.push((data_off, size));
        }
        // Advance past this entry's (padded) data.
        pos = data_off.checked_add(size.div_ceil(512) * 512)?;
    }

    let idx = pick_cover(&entries)?;
    let (off, size) = ranges[idx];
    if size as u64 > super::MAX_COVER {
        return None;
    }
    bytes.get(off..off + size).map(<[u8]>::to_vec)
}

fn tar_name(header: &[u8]) -> String {
    let raw = &header[0..100];
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    String::from_utf8_lossy(&raw[..end]).into_owned()
}

/// A TAR numeric field: ASCII octal, space/NUL padded.
fn tar_octal(field: &[u8]) -> Option<usize> {
    let s = std::str::from_utf8(field).ok()?;
    let s = s.trim_matches(|c| c == '\0' || c == ' ');
    if s.is_empty() {
        return Some(0);
    }
    usize::from_str_radix(s, 8).ok()
}
