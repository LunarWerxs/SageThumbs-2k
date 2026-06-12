//! Embedded album / cover art from audio files (MP3, FLAC, Ogg/Opus/Speex,
//! MP4/M4A, WMA, APE, WavPack, Musepack, WAV, AIFF) via the `lofty` crate.
//! Windows 11 doesn't thumbnail several of these at all (Ogg/Opus/APE/…), so we
//! pull the front-cover picture (or the first one) and hand its bytes to the
//! normal image tiers — same flow as an ebook cover.
//!
//! `extract_reader` takes a seekable reader so the thumbnail provider can hand us
//! the shell's IStream directly: lofty seeks to the metadata/art and reads only
//! that, never the whole (possibly multi-gigabyte audiobook) file.

use std::io::{Cursor, Read, Seek};

use lofty::file::TaggedFileExt;
use lofty::picture::PictureType;
use lofty::probe::Probe;

/// Album art from a byte slice (used by the generic cover path / examples).
pub fn extract(bytes: &[u8]) -> Option<Vec<u8>> {
    extract_reader(Cursor::new(bytes))
}

/// Album art from any seekable reader. lofty parses tags by seeking, so a huge
/// file costs only the reads needed to reach the picture block.
pub fn extract_reader<R: Read + Seek>(reader: R) -> Option<Vec<u8>> {
    let tagged = Probe::new(reader).guess_file_type().ok()?.read().ok()?;
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag())?;
    let pics = tag.pictures();
    let pic = pics
        .iter()
        .find(|p| p.pic_type() == PictureType::CoverFront)
        .or_else(|| pics.first())?;
    let data = pic.data();
    // The art may itself be WebP/AVIF/JXL; the downstream image tiers decode what
    // they can and fall back to the default icon otherwise — we just bound size.
    (!data.is_empty() && data.len() as u64 <= super::MAX_COVER).then(|| data.to_vec())
}

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
        // MP4/M4A audio: ftyp with an AUDIO brand. Crucially this excludes the
        // image MP4 brands (heic/heix/mif1/avif/…) so HEIC/AVIF still take the
        // normal image path instead of being misrouted here.
        || (b.len() >= 12
            && &b[4..8] == b"ftyp"
            && matches!(&b[8..12], b"M4A " | b"M4B " | b"M4P " | b"mp42" | b"mp41" | b"isom" | b"iso2" | b"dash"))
        || (b.len() >= 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WAVE") // WAV
        || (b.len() >= 12 && &b[0..4] == b"FORM" && matches!(&b[8..12], b"AIFF" | b"AIFC")) // AIFF
        || b.starts_with(&[0x30, 0x26, 0xB2, 0x75])             // ASF / WMA
}
