//! Embedded album / cover art from audio files (MP3, FLAC, Ogg/Opus/Speex,
//! MP4/M4A, WMA, APE, WavPack, Musepack, WAV, AIFF) via the `lofty` crate.
//! Windows 11 doesn't thumbnail several of these at all (Ogg/Opus/APE/…), so we
//! pull the front-cover picture (or the first one) and hand its bytes to the
//! normal image tiers — same flow as an ebook cover.
//!
//! FOUR families can't ride lofty for art and get their own extractors here, in the order
//! `extract_reader` tries them:
//! APEv2 "Cover Art (Front)" (lofty reads the tag but not the cover item);
//! ASF/WMA — lofty has NO ASF support at all (its `FileType` enum has no Wma/Asf
//! variant), so a real WMP/foobar-tagged `.wma` never reaches a picture via lofty, and
//! `asf_cover` parses the `WM/Picture` attribute out of the ASF header directly;
//! DSD `.dsf`, whose trailing ID3v2 tag lofty 0.22 also cannot reach;
//! and MP4-family audio (.m4a/.m4b/.m4p/ALAC), where lofty returns no picture for the
//! iTunes `covr` atom, so `mp4_cover` reuses the video side's atom walk. That last one was
//! costing about 113 ms per thumbnail instead of 0.2 ms, because with no branch here the
//! cover was only ever found by the brute-force embedded-JPEG scan at the end of the
//! decode chain (2026-09-05 audit F38).
//!
//! `extract_reader` takes a seekable reader so the thumbnail provider can hand us
//! the shell's IStream directly: lofty seeks to the metadata/art and reads only
//! that, never the whole (possibly multi-gigabyte audiobook) file.

use std::io::{Cursor, Read, Seek, SeekFrom};

use lofty::config::ParseOptions;
use lofty::file::TaggedFileExt;
use lofty::picture::PictureType;
use lofty::probe::Probe;

use super::util::{le16, le32, le64};

/// Album art from a byte slice (used by the generic cover path / examples).
pub fn extract(bytes: &[u8]) -> Option<Vec<u8>> {
    extract_reader(Cursor::new(bytes))
}

/// Album art from any seekable reader. lofty parses tags by seeking, so a huge
/// file costs only the reads needed to reach the picture block. Failing an
/// embedded cover, raw-PCM WAV/AIFF get a drawn waveform (see `waveform`); every
/// other format with no art returns `None` → the shell shows the default icon.
pub fn extract_reader<R: Read + Seek>(mut reader: R) -> Option<Vec<u8>> {
    // APEv2 cover art FIRST: lofty reads the APEv2 tag but does NOT expose its
    // "Cover Art (Front)" item as a picture, so Musepack (.mpc, APEv2-only for art)
    // — and any APEv2-cover WavPack/Monkey's-Audio — would otherwise return nothing.
    // This reads only the tag region at the file end (memory-light), then seeks back
    // for lofty. Absent on non-APEv2 files (fast footer reject) → lofty path runs.
    if let Some(cover) = apev2_cover(&mut reader) {
        return Some(cover);
    }
    // ASF/WMA SECOND: lofty can't identify ASF at all, so it would just error out
    // below. Hand-parse the `WM/Picture` attribute. Non-ASF input bails immediately
    // (GUID mismatch) → the lofty path runs as before.
    if let Some(cover) = asf_cover(&mut reader) {
        return Some(cover);
    }
    // DSD (.dsf) THIRD: lofty 0.22 has no DSF reader either, so hand-parse the DSD
    // header's pointer to its trailing ID3v2 tag and pull the cover out. Non-DSF
    // input bails on the magic → the lofty path runs as before.
    if let Some(cover) = dsf_cover(&mut reader) {
        return Some(cover);
    }
    // MP4-family audio (.m4a/.m4b/.m4p/ALAC) FOURTH, ahead of lofty, because lofty does
    // not return the `covr` artwork for these files and the atom read that does is both
    // exact and effectively free.
    //
    // 2026-09-05 audit F38 found this as a 45x thumbnail slowdown on `sample.m4a` and
    // `sample.m4b`, and the measurement is worth recording because the shape recurs:
    // `mp4::cover_art` finds the file's 143 KB JPEG in 0.19 ms, `lofty_cover` answers None,
    // and with no branch here the whole decode fell through every remaining tier to the
    // brute-force embedded-JPEG scan, which finds the same picture in about 113 ms. Nothing
    // was broken in the visible sense: the right cover appeared, just 45 times slower than
    // its recorded baseline, which is exactly the class of regression a correctness test
    // cannot see.
    //
    // Placed before lofty rather than after, so the cheap exact answer wins on every file
    // that has one, and non-MP4 input falls straight through on the `ftyp` mismatch.
    if let Some(cover) = mp4_cover(&mut reader) {
        return Some(cover);
    }
    // lofty for every other tagged format. Borrowed (`&mut`) so we keep ownership
    // of the reader for the waveform fallback below.
    if let Some(cover) = lofty_cover(&mut reader) {
        return Some(cover);
    }
    // No embedded art: draw a waveform for the raw-PCM families (WAV/AIFF). A
    // recognizable shape beats a blank icon; `None` for anything else.
    reader.seek(SeekFrom::Start(0)).ok()?;
    super::waveform::render_from_reader(&mut reader)
}

/// Total bytes lofty may pull out of the reader while parsing a tag, regardless of what
/// any frame/tag header inside it CLAIMS its own size to be. See [`BudgetedReader`] and
/// `lofty_cover`. A notch above `MAX_COVER`, matching the sibling `MAX_APE_TAG`/
/// `MAX_ASF_HEADER` tag-parsing budgets (`audio/ape.rs`, `audio/asf.rs`).
const MAX_LOFTY_READ: u64 = super::MAX_COVER + 1024 * 1024;

/// Caps the total bytes lofty can actually materialize while parsing, independent of a
/// frame's declared content length. `Probe::read()` parses the WHOLE tag — including any
/// embedded picture — into memory before `lofty_cover`'s `MAX_COVER` check below ever
/// runs, so a hostile file that declares an oversized picture/frame would otherwise be
/// fully read into memory first and only rejected after the fact (the cap bounds what's
/// KEPT, not what gets ALLOCATED). Seeking stays free (no allocation, so `Ok`), and once
/// the budget is spent, reads report EOF (`Ok(0)`) rather than erroring — lofty then
/// finishes its normal parse with a short buffer, and the existing size check downstream
/// rejects it exactly as it would any other oversized picture.
struct BudgetedReader<'a> {
    inner: &'a mut dyn super::ReadSeek,
    read_so_far: u64,
    budget: u64,
}

impl Read for BudgetedReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.read_so_far >= self.budget {
            return Ok(0);
        }
        let allowed = (self.budget - self.read_so_far).min(buf.len() as u64) as usize;
        let n = self.inner.read(&mut buf[..allowed])?;
        self.read_so_far += n as u64;
        Ok(n)
    }
}

impl Seek for BudgetedReader<'_> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(pos)
    }
}

/// Rank an ID3 `APIC` picture-type byte: LOWER is a better thumbnail. Shared by the
/// hand-rolled ID3 and ASF readers, which use the same numbering (`WM/Picture` carries
/// the ID3 type byte verbatim), and mirrored by [`lofty_pic_rank`] for lofty's enum.
///
/// The ranking is what stops a junk picture winning. Types **1 and 2 are literally
/// "32x32 file icon" and "other file icon"** — a tagger is entitled to put a favicon
/// there, and picking it renders a music file as a 32 px smudge. Everything that is not
/// a cover or an icon (band logo, artist photo, leaflet) ranks between the two: better
/// than an icon, worse than the actual sleeve.
pub(super) fn id3_pic_rank(t: u8) -> u8 {
    match t {
        3 => 0,     // Cover (front) — what a thumbnail wants
        0 => 1,     // Other — unlabelled art, usually the cover anyway
        1 | 2 => 3, // file icons — last resort, never over real art
        _ => 2,     // back cover / leaflet / logo / artist / …
    }
}

/// [`id3_pic_rank`] for lofty's typed enum.
fn lofty_pic_rank(t: PictureType) -> u8 {
    match t {
        PictureType::CoverFront => 0,
        PictureType::Other => 1,
        PictureType::Icon | PictureType::OtherIcon => 3,
        _ => 2,
    }
}

/// The `covr` artwork of MP4-family audio (.m4a/.m4b/.m4p and Apple Lossless), read by the
/// same atom walk the video side already uses ([`crate::mp4::cover_art`]) rather than a
/// second parser of the same container.
///
/// Cheap rejection first: an MP4 has `ftyp` as its first box, so four bytes settle whether
/// this branch applies at all and every non-MP4 caller pays one short read. Without that,
/// putting this ahead of lofty would make it the thing that touches every audio file.
///
/// Size and content are already enforced inside `cover_art` (its own 32 MB ceiling plus an
/// image-signature check), so this deliberately does not re-check them; duplicating the cap
/// here would be a second number to drift.
fn mp4_cover<R: Read + Seek>(reader: &mut R) -> Option<Vec<u8>> {
    reader.seek(SeekFrom::Start(0)).ok()?;
    let mut head = [0u8; 12];
    // A short file is not an MP4, and a read error is not this branch's business to report.
    reader.read_exact(&mut head).ok()?;
    if &head[4..8] != b"ftyp" {
        return None;
    }
    reader.seek(SeekFrom::Start(0)).ok()?;
    crate::mp4::cover_art(reader)
}

/// Best album art via lofty's tag reader — the front cover, and the LARGEST one when a
/// file carries several. `None` if the format is unidentified, untagged, or has no
/// picture.
///
/// **Taking the first picture is a real bug, not a tidiness point** (found 2026-08-21).
/// A tag can hold any number of them, and the corpus `.flac`/`.wav` both carry a 1x1
/// white PNG ahead of the real 512x384 sleeve, so "first" rendered every such file as a
/// blank white tile. Real-world files hit this constantly: ID3 type 1 is a 32x32 file
/// icon and plenty of taggers write one alongside the cover. So: rank by picture type,
/// then take the biggest inside the winning rank.
fn lofty_cover(reader: &mut dyn super::ReadSeek) -> Option<Vec<u8>> {
    reader.seek(SeekFrom::Start(0)).ok()?;
    let bounded = BudgetedReader {
        inner: reader,
        read_so_far: 0,
        budget: MAX_LOFTY_READ,
    };
    // Properties (duration/bitrate/…) are never used here — only tags/pictures are — so
    // skip reading them too: on a large audio file they'd otherwise spend most of the
    // read budget on data we throw away instead of on the tag we actually want.
    let tagged = Probe::new(bounded)
        .guess_file_type()
        .ok()?
        .options(ParseOptions::new().read_properties(false))
        .read()
        .ok()?;
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag())?;
    // `min_by_key` keeps the FIRST of any tie, so a file with one picture — or several
    // equally-ranked, equally-sized ones — behaves exactly as it did before.
    let pic = tag.pictures().iter().min_by_key(|p| {
        (
            lofty_pic_rank(p.pic_type()),
            std::cmp::Reverse(p.data().len()),
        )
    })?;
    let data = pic.data();
    // The art may itself be WebP/AVIF/JXL; the downstream image tiers decode what
    // they can and fall back to the default icon otherwise — we just bound size.
    (!data.is_empty() && data.len() as u64 <= super::MAX_COVER).then(|| data.to_vec())
}

mod ape;
mod asf;
mod id3;

// Parent-hub import model: the three hand-rolled parsers lofty can't cover live in
// their own files; the hub keeps the lofty path and the dispatch.
use ape::apev2_cover;
use asf::asf_cover;
use id3::dsf_cover;

pub(crate) use asf::asf_tags;
pub use asf::AudioTags;
// Test-only re-exports so `container::fuzzseed` can aim at the APEv2 / DSF-ID3v2 cover-art
// sub-parsers directly. The format modules themselves (`ape`, `id3`) stay private to `audio`.
#[cfg(test)]
pub(crate) use ape::fuzzapi as ape_fuzzapi;
#[cfg(test)]
pub(crate) use id3::fuzzapi as id3_fuzzapi;

/// Cheap magic sniff so we only run lofty on actual audio containers. (Cover art
/// in MP3 lives in ID3v2, which sits at the file start, so "ID3" covers MP3.)
pub fn looks_like_audio(b: &[u8]) -> bool {
    b.starts_with(b"ID3")                                       // MP3 (ID3v2)
        || b.starts_with(b"fLaC")                               // FLAC
        || b.starts_with(b"OggS")                               // Ogg: Vorbis/Opus/Speex
        || b.starts_with(b"MAC ")                               // Monkey's Audio (APE)
        || b.starts_with(b"wvpk")                               // WavPack
        || b.starts_with(b"MPCK")                               // Musepack SV8
        || b.starts_with(b"MP+")                                // Musepack SV7
        || b.starts_with(b"DSD ")                               // DSD stream (.dsf — ID3v2 cover)
        // MP4/M4A audio: ftyp with an AUDIO brand. Crucially this excludes the
        // image MP4 brands (heic/heix/mif1/avif/…) so HEIC/AVIF still take the
        // normal image path instead of being misrouted here.
        || (b.len() >= 12
            && &b[4..8] == b"ftyp"
            && matches!(&b[8..12], b"M4A " | b"M4B " | b"M4P " | b"mp42" | b"mp41" | b"isom" | b"iso2" | b"dash"))
        || (b.len() >= 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WAVE") // WAV
        || (b.len() >= 12 && &b[0..4] == b"FORM" && matches!(&b[8..12], b"AIFF" | b"AIFC")) // AIFF
        || b.starts_with(&[0x30, 0x26, 0xB2, 0x75]) // ASF / WMA
}

#[cfg(test)]
mod tests;
