//! Embedded album / cover art from audio files (MP3, FLAC, Ogg/Opus/Speex,
//! MP4/M4A, WMA, APE, WavPack, Musepack, WAV, AIFF) via the `lofty` crate.
//! Windows 11 doesn't thumbnail several of these at all (Ogg/Opus/APE/…), so we
//! pull the front-cover picture (or the first one) and hand its bytes to the
//! normal image tiers — same flow as an ebook cover.
//!
//! Two formats can't ride lofty for art and get hand-rolled extractors here:
//! APEv2 "Cover Art (Front)" (lofty reads the tag but not the cover item), and
//! ASF/WMA — lofty has NO ASF support at all (its `FileType` enum has no Wma/Asf
//! variant), so a real WMP/foobar-tagged `.wma` never reaches a picture via lofty.
//! `asf_cover` parses the `WM/Picture` attribute out of the ASF header directly.
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

/// Front-cover (or first) album art via lofty's tag reader. `None` if the format
/// is unidentified, untagged, or carries no picture.
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

mod ape;
mod asf;
mod id3;

// Parent-hub import model: the three hand-rolled parsers lofty can't cover live in
// their own files; the hub keeps the lofty path and the dispatch.
use ape::apev2_cover;
use asf::asf_cover;
use id3::dsf_cover;

pub(crate) use asf::{asf_tags, AsfTags};

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
mod tests {
    use super::*;
    // The ASF object GUIDs + the name comparator the synthetic-header builders below need.
    use super::asf::{
        name_eq, ASF_CONTENT_DESC_GUID, ASF_ECD_GUID, ASF_HDR_EXT_GUID, ASF_HEADER_GUID,
        ASF_MDLIB_GUID, ASF_META_GUID,
    };

    /// The size cap on what lofty is allowed to READ must be enforced regardless of how
    /// much data the underlying stream actually offers — a hostile file's declared
    /// picture/frame size is exactly the kind of backing-reader size this stands in for.
    /// Small budget so the test stays instant; the mechanism is size-independent.
    #[test]
    fn budgeted_reader_stops_at_the_budget_regardless_of_backing_size() {
        let huge = vec![0xABu8; 10_000]; // far more than the reader is allowed to hand out
        let mut cursor = Cursor::new(huge);
        let budget = 256u64;
        let mut bounded = BudgetedReader {
            inner: &mut cursor,
            read_so_far: 0,
            budget,
        };
        let mut out = Vec::new();
        bounded.read_to_end(&mut out).unwrap();
        assert_eq!(
            out.len() as u64,
            budget,
            "read_to_end must stop AT the budget, not the underlying reader's real size \
             (this is what stands between an attacker-declared oversized picture and lofty \
             actually materializing all of it before the MAX_COVER check runs)"
        );
    }

    /// Seeking must stay free — an EOF-once-spent read budget would be useless if a big
    /// seek (e.g. lofty jumping to a trailing APEv2/ID3v1 footer) also ate into it.
    #[test]
    fn budgeted_reader_seeking_does_not_spend_the_read_budget() {
        let data = vec![0xCDu8; 10_000];
        let mut cursor = Cursor::new(data);
        let budget = 10u64;
        let mut bounded = BudgetedReader {
            inner: &mut cursor,
            read_so_far: 0,
            budget,
        };
        bounded.seek(SeekFrom::Start(9_000)).unwrap();
        let mut byte = [0u8; 1];
        assert_eq!(
            bounded.read(&mut byte).unwrap(),
            1,
            "a seek must not have spent the budget"
        );
    }

    /// Smallest bytes that pass `looks_like_raster` as a JPEG (so the extracted art
    /// is "decodable" without shipping a real image fixture).
    const FAKE_JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F'];

    fn utf16(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    /// A `WM/Picture` byte-array value (front cover) wrapping `image`.
    fn wm_picture(image: &[u8]) -> Vec<u8> {
        let mut v = vec![3u8]; // picture type 3 = front cover
        v.extend_from_slice(&(image.len() as u32).to_le_bytes());
        v.extend_from_slice(&utf16("image/jpeg")); // MIME
        v.extend_from_slice(&[0, 0]); // NUL
        v.extend_from_slice(&[0, 0]); // empty description + NUL
        v.extend_from_slice(image);
        v
    }

    fn asf_object(guid: [u8; 16], payload: &[u8]) -> Vec<u8> {
        let mut o = guid.to_vec();
        o.extend_from_slice(&((24 + payload.len()) as u64).to_le_bytes());
        o.extend_from_slice(payload);
        o
    }

    /// Wrap nested objects in a Header Extension Object (where real files put the
    /// Metadata / Metadata Library Objects): reserved GUID(16) + reserved u16(6) +
    /// data-size u32 + nested.
    fn hdr_ext(nested: &[u8]) -> Vec<u8> {
        let mut payload = vec![0u8; 16]; // reserved GUID (contents irrelevant — we skip it)
        payload.extend_from_slice(&6u16.to_le_bytes());
        payload.extend_from_slice(&(nested.len() as u32).to_le_bytes());
        payload.extend_from_slice(nested);
        asf_object(ASF_HDR_EXT_GUID, &payload)
    }

    /// Wrap one header sub-object in a complete ASF Header Object.
    fn asf_file(sub: &[u8]) -> Vec<u8> {
        let mut h = ASF_HEADER_GUID.to_vec();
        h.extend_from_slice(&((30 + sub.len()) as u64).to_le_bytes()); // header object size
        h.extend_from_slice(&1u32.to_le_bytes()); // number of header objects
        h.extend_from_slice(&[1, 2]); // reserved
        h.extend_from_slice(sub);
        h
    }

    /// Picture in the Extended Content Description Object (the small-cover path,
    /// `value-len` is u16). This is where mutagen/encoders put covers ≤ 64 KiB.
    fn ecd_payload(name: &str, value: &[u8]) -> Vec<u8> {
        let nm = {
            let mut n = utf16(name);
            n.extend_from_slice(&[0, 0]);
            n
        };
        let mut p = 1u16.to_le_bytes().to_vec(); // descriptor count
        p.extend_from_slice(&(nm.len() as u16).to_le_bytes());
        p.extend_from_slice(&nm);
        p.extend_from_slice(&1u16.to_le_bytes()); // value type 1 = byte array
        p.extend_from_slice(&(value.len() as u16).to_le_bytes());
        p.extend_from_slice(value);
        p
    }

    /// Picture in the Metadata Library Object (the full-size path, `data-len` is u32).
    fn mdlib_payload(name: &str, value: &[u8]) -> Vec<u8> {
        let nm = {
            let mut n = utf16(name);
            n.extend_from_slice(&[0, 0]);
            n
        };
        let mut p = 1u16.to_le_bytes().to_vec(); // record count
        p.extend_from_slice(&0u16.to_le_bytes()); // language list index
        p.extend_from_slice(&0u16.to_le_bytes()); // stream number
        p.extend_from_slice(&(nm.len() as u16).to_le_bytes());
        p.extend_from_slice(&1u16.to_le_bytes()); // data type 1 = byte array
        p.extend_from_slice(&(value.len() as u32).to_le_bytes());
        p.extend_from_slice(&nm);
        p.extend_from_slice(value);
        p
    }

    /// Build a 4-byte ID3v2 synchsafe size (high bit of each byte is zero).
    fn synchsafe_bytes(n: u32) -> [u8; 4] {
        [
            ((n >> 21) & 0x7f) as u8,
            ((n >> 14) & 0x7f) as u8,
            ((n >> 7) & 0x7f) as u8,
            (n & 0x7f) as u8,
        ]
    }

    /// A minimal `.dsf`: the 28-byte `DSD ` header pointing at a trailing ID3v2.4 tag
    /// whose single `APIC` frame (front cover) wraps `image`.
    fn dsf_with_cover(image: &[u8]) -> Vec<u8> {
        let mut apic = vec![0u8]; // text encoding: latin1
        apic.extend_from_slice(b"image/jpeg\0"); // MIME
        apic.push(3); // picture type: front cover
        apic.push(0); // empty description (latin1 NUL)
        apic.extend_from_slice(image);
        let mut frame = b"APIC".to_vec();
        frame.extend_from_slice(&synchsafe_bytes(apic.len() as u32));
        frame.extend_from_slice(&[0, 0]); // frame flags
        frame.extend_from_slice(&apic);
        let mut id3 = b"ID3".to_vec();
        id3.extend_from_slice(&[4, 0, 0]); // v2.4.0, no flags
        id3.extend_from_slice(&synchsafe_bytes(frame.len() as u32));
        id3.extend_from_slice(&frame);
        let mut f = b"DSD ".to_vec();
        f.extend_from_slice(&28u64.to_le_bytes()); // DSD chunk size
        f.extend_from_slice(&(28 + id3.len() as u64).to_le_bytes()); // total file size
        f.extend_from_slice(&28u64.to_le_bytes()); // metadata pointer → right after the header
        f.extend_from_slice(&id3);
        f
    }

    /// DSD `.dsf` carries its cover in a trailing ID3v2 tag that lofty can't read; the
    /// hand-rolled `dsf_cover` must pull the front-cover APIC out.
    #[test]
    fn dsf_cover_reads_id3v2_apic() {
        assert_eq!(
            extract(&dsf_with_cover(FAKE_JPEG)),
            Some(FAKE_JPEG.to_vec())
        );
    }

    #[test]
    fn asf_cover_reads_wm_picture_from_ecd() {
        let pic = wm_picture(FAKE_JPEG);
        let file = asf_file(&asf_object(ASF_ECD_GUID, &ecd_payload("WM/Picture", &pic)));
        assert_eq!(asf_cover(&mut Cursor::new(file)), Some(FAKE_JPEG.to_vec()));
    }

    #[test]
    fn asf_cover_reads_wm_picture_from_metadata_library() {
        // Real files nest the Metadata Library Object inside the Header Extension
        // Object — the case that the first implementation missed.
        let pic = wm_picture(FAKE_JPEG);
        let mdlib = asf_object(ASF_MDLIB_GUID, &mdlib_payload("WM/Picture", &pic));
        let file = asf_file(&hdr_ext(&mdlib));
        assert_eq!(asf_cover(&mut Cursor::new(file)), Some(FAKE_JPEG.to_vec()));
    }

    #[test]
    fn asf_cover_reads_wm_picture_from_metadata_object() {
        // The older Metadata Object (same record layout) also nests in Header Extension.
        let pic = wm_picture(FAKE_JPEG);
        let meta = asf_object(ASF_META_GUID, &mdlib_payload("WM/Picture", &pic));
        let file = asf_file(&hdr_ext(&meta));
        assert_eq!(asf_cover(&mut Cursor::new(file)), Some(FAKE_JPEG.to_vec()));
    }

    #[test]
    fn asf_cover_ignores_non_picture_attributes_and_non_asf() {
        // A WM/Picture whose payload isn't a decodable raster → rejected.
        let junk = wm_picture(&[0u8; 16]);
        let file = asf_file(&asf_object(
            ASF_MDLIB_GUID,
            &mdlib_payload("WM/Picture", &junk),
        ));
        assert_eq!(asf_cover(&mut Cursor::new(file)), None);
        // A non-picture attribute name → ignored.
        let pic = wm_picture(FAKE_JPEG);
        let file = asf_file(&asf_object(ASF_ECD_GUID, &ecd_payload("WM/Author", &pic)));
        assert_eq!(asf_cover(&mut Cursor::new(file)), None);
        // Non-ASF bytes bail immediately.
        assert_eq!(
            asf_cover(&mut Cursor::new(b"ID3\x04not an asf file".to_vec())),
            None
        );
    }

    #[test]
    fn name_matcher_is_exact() {
        let mut wp = utf16("WM/Picture");
        assert!(name_eq(&wp, b"WM/Picture"));
        wp.extend_from_slice(&[0, 0]); // trailing NUL is allowed
        assert!(name_eq(&wp, b"WM/Picture"));
        assert!(!name_eq(&utf16("WM/PictureX"), b"WM/Picture"));
        assert!(!name_eq(&utf16("WM/Author"), b"WM/Picture"));
        assert!(!name_eq(b"WM/Picture", b"WM/Picture")); // ASCII (not UTF-16) → no
    }

    /// Multiple header sub-objects (the tag tests need Content Description + ECD).
    fn asf_file_n(subs: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = subs.iter().flatten().copied().collect();
        let mut h = ASF_HEADER_GUID.to_vec();
        h.extend_from_slice(&((30 + body.len()) as u64).to_le_bytes());
        h.extend_from_slice(&(subs.len() as u32).to_le_bytes());
        h.extend_from_slice(&[1, 2]);
        h.extend_from_slice(&body);
        h
    }

    fn utf16z(s: &str) -> Vec<u8> {
        let mut v = utf16(s);
        v.extend_from_slice(&[0, 0]);
        v
    }

    /// Content Description Object payload (Title + Author; other fields empty).
    fn cd_payload(title: &str, author: &str) -> Vec<u8> {
        let (t, a) = (utf16z(title), utf16z(author));
        let mut p = (t.len() as u16).to_le_bytes().to_vec();
        p.extend_from_slice(&(a.len() as u16).to_le_bytes());
        p.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // copyright/description/rating lengths = 0
        p.extend_from_slice(&t);
        p.extend_from_slice(&a);
        p
    }

    /// Extended Content Description payload of Unicode-string attributes.
    fn ecd_str_payload(pairs: &[(&str, &str)]) -> Vec<u8> {
        let mut p = (pairs.len() as u16).to_le_bytes().to_vec();
        for (name, val) in pairs {
            let (nm, vv) = (utf16z(name), utf16z(val));
            p.extend_from_slice(&(nm.len() as u16).to_le_bytes());
            p.extend_from_slice(&nm);
            p.extend_from_slice(&0u16.to_le_bytes()); // value type 0 = Unicode string
            p.extend_from_slice(&(vv.len() as u16).to_le_bytes());
            p.extend_from_slice(&vv);
        }
        p
    }

    #[test]
    fn asf_tags_reads_content_description_and_wm_attrs() {
        let cd = asf_object(ASF_CONTENT_DESC_GUID, &cd_payload("My Song", "The Artist"));
        let ecd = asf_object(
            ASF_ECD_GUID,
            &ecd_str_payload(&[("WM/AlbumTitle", "The Album"), ("WM/TrackNumber", "7/12")]),
        );
        let tags = asf_tags(&mut Cursor::new(asf_file_n(&[cd, ecd]))).unwrap();
        assert_eq!(tags.title.as_deref(), Some("My Song"));
        assert_eq!(tags.artist.as_deref(), Some("The Artist"));
        assert_eq!(tags.album.as_deref(), Some("The Album"));
        assert_eq!(tags.track, Some(7));
    }

    #[test]
    fn asf_tags_reads_attrs_nested_in_metadata_library() {
        // Album/track can live in the Header-Extension-nested Metadata Library too.
        let mdlib = asf_object(
            ASF_MDLIB_GUID,
            &mdlib_str_payload("WM/AlbumTitle", "Nested Album"),
        );
        let tags = asf_tags(&mut Cursor::new(asf_file_n(&[hdr_ext(&mdlib)]))).unwrap();
        assert_eq!(tags.album.as_deref(), Some("Nested Album"));
    }

    /// One Metadata Library record holding a Unicode-string attribute.
    fn mdlib_str_payload(name: &str, val: &str) -> Vec<u8> {
        let (nm, vv) = (utf16z(name), utf16z(val));
        let mut p = 1u16.to_le_bytes().to_vec(); // record count
        p.extend_from_slice(&0u16.to_le_bytes()); // language list index
        p.extend_from_slice(&0u16.to_le_bytes()); // stream number
        p.extend_from_slice(&(nm.len() as u16).to_le_bytes());
        p.extend_from_slice(&0u16.to_le_bytes()); // data type 0 = Unicode string
        p.extend_from_slice(&(vv.len() as u32).to_le_bytes());
        p.extend_from_slice(&nm);
        p.extend_from_slice(&vv);
        p
    }

    #[test]
    fn asf_tags_prefers_track_author_over_album_artist() {
        // Real files store the ECD before the Content Description Object, so without
        // care WM/AlbumArtist would win. The track Author must win regardless of order.
        let ecd = asf_object(
            ASF_ECD_GUID,
            &ecd_str_payload(&[("WM/AlbumArtist", "Various Artists")]),
        );
        let cd = asf_object(ASF_CONTENT_DESC_GUID, &cd_payload("Song", "Real Artist"));
        let tags = asf_tags(&mut Cursor::new(asf_file_n(&[ecd, cd]))).unwrap();
        assert_eq!(tags.artist.as_deref(), Some("Real Artist"));
    }

    #[test]
    fn asf_tags_none_for_non_asf() {
        assert!(asf_tags(&mut Cursor::new(b"ID3\x04 not an asf file".to_vec())).is_none());
    }
}
