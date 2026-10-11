//! Serif Affinity (Photo / Designer / Publisher) `.afphoto`/`.afdesign`/`.afpub`
//! and the unified V2/V3 `.af`. These proprietary containers embed a standard
//! PNG preview; we scan for it and slice it out (no rendering, no codecs). There
//! is no official Windows thumbnailer for these — long-requested.
//!
//! Container magic is `00 FF 4B 41` (`\0\xFFKA`) with a varying revision/flags
//! byte after, so we gate only on the 4-byte prefix. The preview is selected as
//! the LAST embedded PNG whose longest edge is ≤ 512 (the thumbnail) — picking
//! "largest" grabs a full-res layer, "smallest" grabs a tiny layer icon.
//!
//! Affinity writes the preview AFTER the document's compressed data: in the corpus
//! file the PNG ends on the file's last byte. So a file too big to hold in memory
//! (a layered document converted from Photoshop is the usual one) is read from its
//! end by [`extract_seek`]; without that it had no thumbnail at all.

use std::io::{Read, Seek, SeekFrom};

use super::util::find;

const PNG_SIG: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
const IEND: [u8; 8] = [0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82];

/// Cap the O(n·m) byte scan so a huge/hostile `.af` (often hundreds of MB)
/// can't run away. Only the first and the last 64 MiB are searched for the PNG
/// signature, and each IEND search span extends at most a further 64 MiB, bounding
/// pathological inputs. Derived from the canonical cover budget rather than a
/// separately picked magic number.
const MAX_SCAN: usize = super::MAX_COVER as usize * 2;

/// True if `head` looks like an Affinity container.
pub fn looks_like_affinity(head: &[u8]) -> bool {
    head.starts_with(&[0x00, 0xFF, 0x4B, 0x41])
}

/// Extract the embedded thumbnail PNG, or None.
pub fn extract(bytes: &[u8]) -> Option<Vec<u8>> {
    let (best_le512, last_any) = candidates(bytes);
    best_le512.or(last_any).map(|p| p.to_vec())
}

/// How much of a big file's end [`extract_seek`] reads first. The preview ends at or near
/// the last byte, so this one small read finds it in the usual case; the full windows are
/// the fallback.
const TAIL_PROBE: usize = 1024 * 1024;

/// [`extract`] for a file too big to hold in memory, off a seekable reader: the same windows
/// the buffered scan searches (the last [`MAX_SCAN`] bytes and the first), each read on its
/// own and ranked the way the buffered scan ranks them (the last thumbnail-sized PNG wins,
/// then the last PNG of any size), so a big document gets the picture a small one gets.
///
/// One difference: a PNG that starts in the head window and ends past it is found by the
/// buffered scan, whose IEND search runs on, and missed here, because the head read stops
/// at [`MAX_SCAN`] to keep all three reads inside the provider's read budget.
pub(super) fn extract_seek<R: Read + Seek>(r: &mut R) -> Option<Vec<u8>> {
    let len = r.seek(SeekFrom::End(0)).ok()?;
    let probe = read_window(r, len, len.saturating_sub(TAIL_PROBE as u64), TAIL_PROBE)?;
    if let (Some(thumb), _) = candidates(&probe) {
        return Some(thumb.to_vec());
    }
    let tail_at = len.saturating_sub(MAX_SCAN as u64);
    let tail = read_window(r, len, tail_at, MAX_SCAN)?;
    // The head stops where the tail starts, so no PNG is counted twice.
    let head_len = usize::try_from(tail_at).map_or(MAX_SCAN, |n| n.min(MAX_SCAN));
    let head = read_window(r, len, 0, head_len)?;
    let (tail_thumb, tail_any) = candidates(&tail);
    let (head_thumb, head_any) = candidates(&head);
    tail_thumb
        .or(head_thumb)
        .or(tail_any)
        .or(head_any)
        .map(<[u8]>::to_vec)
}

/// `len` bytes of `r` from offset `at`, clipped to the file's end, or None if fewer arrive.
/// The provider's stream reads short once its budget or deadline runs out, and a ranking
/// over a cut window would pick the wrong PNG, so a short window declines the thumbnail.
/// The buffer is reserved fallibly: this runs in the shell's thumbnail host, where a
/// refused allocation must decline the thumbnail rather than abort the process.
fn read_window<R: Read + Seek>(r: &mut R, file_len: u64, at: u64, len: usize) -> Option<Vec<u8>> {
    let want = usize::try_from(file_len.saturating_sub(at)).map_or(len, |n| n.min(len));
    r.seek(SeekFrom::Start(at)).ok()?;
    let mut out = Vec::new();
    out.try_reserve_exact(want).ok()?;
    r.by_ref().take(want as u64).read_to_end(&mut out).ok()?;
    (out.len() == want).then_some(out)
}

/// The two picks [`extract`] chooses between: the last PNG whose longest edge is ≤ 512 (the
/// thumbnail) and the last valid PNG of any size (the fallback).
fn candidates(bytes: &[u8]) -> (Option<&[u8]>, Option<&[u8]>) {
    let mut best_le512: Option<&[u8]> = None; // last PNG with max edge ≤ 512
    let mut last_any: Option<&[u8]> = None; // fallback: last valid PNG

    // Bound the signature search to two MAX_SCAN windows, the head and the tail,
    // so we can't walk an arbitrarily large file. A big layered document may keep
    // its thumbnail near the end, past the head window; the tail window is
    // scanned after the head so a thumbnail there still wins ("last ≤512 wins").
    // Each IEND search below still scans at most a MAX_SCAN window.
    let sig_limit = bytes.len().min(MAX_SCAN);
    scan_window(bytes, 0, sig_limit, &mut best_le512, &mut last_any);
    if bytes.len() > MAX_SCAN {
        let tail_start = (bytes.len() - MAX_SCAN).max(sig_limit);
        scan_window(
            bytes,
            tail_start,
            bytes.len(),
            &mut best_le512,
            &mut last_any,
        );
    }
    (best_le512, last_any)
}

/// Scan `start..limit` for PNG signatures, recording candidates as `scan_step` does.
fn scan_window<'a>(
    bytes: &'a [u8],
    start: usize,
    limit: usize,
    best_le512: &mut Option<&'a [u8]>,
    last_any: &mut Option<&'a [u8]>,
) {
    let mut i = start;
    while i + PNG_SIG.len() <= limit {
        match scan_step(bytes, i, best_le512, last_any) {
            Some(next) => i = next,
            None => break, // no terminator → no more complete PNGs
        }
    }
}

/// Advance the scan from `i` by one step, recording any bounded PNG candidate;
/// returns the next offset, or None once no more complete PNGs can follow.
fn scan_step<'a>(
    bytes: &'a [u8],
    i: usize,
    best_le512: &mut Option<&'a [u8]>,
    last_any: &mut Option<&'a [u8]>,
) -> Option<usize> {
    if bytes[i..i + 8] != PNG_SIG {
        return Some(i + 1);
    }
    // Found a PNG header; find its IEND terminator (incl. CRC) within a
    // bounded window after the header (don't linearly scan to EOF).
    let span_end = (i + 8).saturating_add(MAX_SCAN).min(bytes.len());
    // No terminator (→ None) means no more complete PNGs can follow.
    let rel = find(&bytes[i + 8..span_end], IEND.as_slice())?;
    let end = i + 8 + rel + IEND.len();
    let png = &bytes[i..end];
    // Bound the cover we hand back (shared CBXMEM cap): a hostile `.af`
    // could carry an enormous embedded PNG that's decoded downstream
    // under panic=abort. Oversized candidates are simply skipped.
    if png.len() >= 57 && png.len() as u64 <= crate::container::MAX_COVER {
        *last_any = Some(png);
        if let Some((w, h)) = ihdr_dims(png) {
            if w.max(h) <= 512 {
                *best_le512 = Some(png);
            }
        }
    }
    Some(end) // resume after this PNG (don't rescan inside it)
}

/// PNG IHDR dimensions (bytes 12..16 must be "IHDR"; width/height big-endian).
fn ihdr_dims(png: &[u8]) -> Option<(u32, u32)> {
    if png.get(12..16)? != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes(png.get(16..20)?.try_into().ok()?);
    let h = u32::from_be_bytes(png.get(20..24)?.try_into().ok()?);
    Some((w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut b = Vec::new();
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(w, h))
            .write_to(&mut std::io::Cursor::new(&mut b), image::ImageFormat::Png)
            .unwrap();
        b
    }

    #[test]
    fn picks_the_thumbnail_png_not_a_full_res_layer() {
        let big = png(1200, 800); // a full-res layer — must be skipped
        let thumb = png(256, 256); // the ≤512 thumbnail — must win
        let mut bytes = vec![0x00, 0xFF, 0x4B, 0x41, 0x09, 0x00]; // Affinity magic
        bytes.extend_from_slice(&big);
        bytes.extend_from_slice(b"some junk between");
        bytes.extend_from_slice(&thumb);

        let got = extract(&bytes).expect("embedded png");
        let d = image::load_from_memory(&got).unwrap();
        assert_eq!(
            (d.width(), d.height()),
            (256, 256),
            "should pick the ≤512 preview"
        );
        assert!(extract(b"no png in here").is_none());
    }

    /// A layered document keeps a layer icon up front and the real thumbnail past the
    /// head scan window; the thumbnail must still win over the earlier small PNG.
    #[test]
    fn thumb_past_64mib_is_found() {
        let icon = png(16, 16); // a layer icon in the head — must lose
        let thumb = png(256, 256); // the thumbnail, stored past MAX_SCAN — must win
        let mut bytes = vec![0x00, 0xFF, 0x4B, 0x41, 0x09, 0x00]; // Affinity magic
        bytes.extend_from_slice(&icon);
        bytes.resize(MAX_SCAN + 1024, 0);
        bytes.extend_from_slice(&thumb);

        let got = extract(&bytes).expect("embedded png");
        let d = image::load_from_memory(&got).unwrap();
        assert_eq!((d.width(), d.height()), (256, 256));
    }

    /// A file too big to hold is read from its end, and gets the same pick the whole buffer
    /// gives: the thumbnail in the last bytes (found by the small probe), a thumbnail just
    /// past the probe (found in the tail window), and the 64 MiB layout above with padding
    /// after the thumbnail, so the probe misses it and the tail window must outrank the head.
    #[test]
    fn extract_seek_picks_what_extract_picks() {
        let magic = [0x00, 0xFF, 0x4B, 0x41, 0x09, 0x00];
        let layer = png(1200, 800);
        let thumb = png(256, 256);
        let icon = png(16, 16);

        let mut at_end = magic.to_vec();
        at_end.extend_from_slice(&layer);
        at_end.extend_from_slice(&thumb);

        let mut past_probe = magic.to_vec();
        past_probe.extend_from_slice(&thumb);
        past_probe.resize(TAIL_PROBE * 2, 0);
        past_probe.extend_from_slice(&layer);

        let mut past_head_window = magic.to_vec();
        past_head_window.extend_from_slice(&icon);
        past_head_window.resize(MAX_SCAN + 1024, 0);
        past_head_window.extend_from_slice(&thumb);
        past_head_window.resize(past_head_window.len() + TAIL_PROBE + 1024, 0);

        for (name, bytes) in [
            ("at the end", at_end),
            ("past the probe", past_probe),
            ("past the head window", past_head_window),
        ] {
            let seek = extract_seek(&mut std::io::Cursor::new(&bytes));
            assert_eq!(seek, extract(&bytes), "{name}: seek and buffer differ");
            let d = image::load_from_memory(&seek.expect("a preview")).unwrap();
            assert_eq!((d.width(), d.height()), (256, 256), "{name}");
        }
        assert!(extract_seek(&mut std::io::Cursor::new(b"no png in here")).is_none());
    }

    /// Serves bytes below `cut` and reads short past it, as the provider's block stream does
    /// once its read budget or deadline runs out.
    struct CutReader {
        inner: std::io::Cursor<Vec<u8>>,
        cut: u64,
    }

    impl Read for CutReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let left = self.cut.saturating_sub(self.inner.position());
            let n = usize::try_from(left).map_or(buf.len(), |l| l.min(buf.len()));
            self.inner.read(&mut buf[..n])
        }
    }

    impl Seek for CutReader {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(pos)
        }
    }

    /// A read cut short declines the thumbnail. Before, the cut tail came back as a window
    /// and the scan picked the layer icon at the file's start.
    #[test]
    fn a_short_read_declines_rather_than_picking_from_a_cut_window() {
        let mut bytes = vec![0x00, 0xFF, 0x4B, 0x41, 0x09, 0x00];
        bytes.extend_from_slice(&png(16, 16));
        bytes.resize(TAIL_PROBE * 3, 0);
        bytes.extend_from_slice(&png(256, 256));
        let whole = extract_seek(&mut std::io::Cursor::new(bytes.clone())).expect("a preview");
        let d = image::load_from_memory(&whole).unwrap();
        assert_eq!((d.width(), d.height()), (256, 256));

        let mut cut = CutReader {
            inner: std::io::Cursor::new(bytes),
            cut: TAIL_PROBE as u64,
        };
        assert_eq!(extract_seek(&mut cut), None);
    }

    /// `extract` used to carry its own copy of `find` with no empty-needle guard
    /// (`windows(0)` panics in std, which under `panic = "abort"` would kill the shell
    /// host). It now calls `super::util::find`, which is guarded; this proves the guard
    /// is actually reachable through this module rather than merely present in `util`.
    #[test]
    fn find_helper_does_not_panic_on_an_empty_needle() {
        assert_eq!(find(b"anything", b""), None);
    }
}
