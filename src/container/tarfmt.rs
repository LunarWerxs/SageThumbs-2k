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
        // GNU tar's base-256 large-size encoding (high bit of the first byte set) isn't
        // ASCII-octal, and a header this parser can't make sense of at all is likewise
        // possible in a crafted/truncated file. Either way, stop the walk here instead of
        // `?`-abandoning the whole extract: entries already collected from earlier, valid
        // headers are still real cover candidates, and discarding them for one later
        // unreadable header is worse than just not reading past it.
        let Some(size) = tar_octal(&header[124..136]) else {
            break;
        };
        let data_off = pos + 512;
        let Some(after) = data_off.checked_add(size) else {
            break;
        };
        if after > bytes.len() {
            break;
        }
        let name = tar_name(header);
        // typeflag at offset 156: '0' or NUL = a regular file.
        let is_regular = matches!(header[156], b'0' | 0);
        if is_regular && !name.is_empty() && super::is_image_name(&name) {
            entries.push(Entry {
                name,
                is_dir: false,
                size: size as u64,
            });
            ranges.push((data_off, size));
        }
        // Advance past this entry's (padded) data.
        let Some(next) = data_off.checked_add(size.div_ceil(512) * 512) else {
            break;
        };
        pos = next;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Build one 512-byte TAR header with a regular-file typeflag. `size_field` is written
    /// verbatim into the octal size bytes so a test can hand in something `tar_octal` can't
    /// parse (e.g. GNU base-256, high bit set) without going through the ASCII-octal encoder.
    fn tar_header(name: &str, size_field: &[u8; 12]) -> [u8; 512] {
        let mut h = [0u8; 512];
        h[0..name.len()].copy_from_slice(name.as_bytes());
        h[124..136].copy_from_slice(size_field);
        h[156] = b'0'; // regular file
        h
    }

    fn octal_size_field(size: usize) -> [u8; 12] {
        let mut f = [b' '; 12];
        let s = format!("{:o}\0", size); // NUL-terminated octal, like real tar writers
        f[..s.len()].copy_from_slice(s.as_bytes());
        f
    }

    fn push_padded(bytes: &mut Vec<u8>, header: [u8; 512], data: &[u8]) {
        bytes.extend_from_slice(&header);
        bytes.extend_from_slice(data);
        let pad = data.len().div_ceil(512) * 512 - data.len();
        bytes.extend(std::iter::repeat_n(0u8, pad));
    }

    /// A one-line PNG signature is enough: `extract` only slices bytes back out, it never
    /// decodes them, and `is_image_name` gates purely on the `.png` extension.
    const FAKE_PNG: &[u8] = b"not a real png but extract() doesn't care";

    /// A bad size field on a LATER entry (GNU base-256: high bit of the first size byte set,
    /// which `usize::from_str_radix(_, 8)` can't parse) used to propagate through `?` and
    /// abandon the whole walk, discarding the valid earlier entry with it. It must instead
    /// just stop the walk there and still return the cover already found.
    #[test]
    fn bad_size_field_on_a_later_entry_stops_the_walk_but_keeps_earlier_covers() {
        let mut bytes = Vec::new();
        push_padded(
            &mut bytes,
            tar_header("page1.png", &octal_size_field(FAKE_PNG.len())),
            FAKE_PNG,
        );

        // A GNU base-256 size field: byte 0 has its high bit set, so it is not valid
        // ASCII-octal at all -> tar_octal returns None for it.
        let mut bad_size = [0u8; 12];
        bad_size[0] = 0x80;
        bytes.extend_from_slice(&tar_header("page2.png", &bad_size));
        bytes.extend(std::iter::repeat_n(0u8, 512)); // pretend-data block, never reached

        assert_eq!(extract(&bytes).as_deref(), Some(FAKE_PNG));
    }

    /// With no bad entries at all, the walk still needs to find and return the sole cover:
    /// this is the control case the fix must not regress.
    #[test]
    fn extract_returns_the_only_page_when_the_archive_is_well_formed() {
        let mut bytes = Vec::new();
        push_padded(
            &mut bytes,
            tar_header("page1.png", &octal_size_field(FAKE_PNG.len())),
            FAKE_PNG,
        );
        assert_eq!(extract(&bytes).as_deref(), Some(FAKE_PNG));
    }
}
