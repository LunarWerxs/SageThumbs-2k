//! ZIP-family container dispatch: EPUB (OPF cover cascade), FBZ (zipped FB2),
//! or a plain image zip / CBZ (first page by cover-selection).
//!
//! A178, accepted bound: every `ZipArchive::new` call here (and in `container/apk.rs`,
//! the other zip-based crate consumer) must fully parse the central directory before
//! ANY of our `MAX_LIST_ENTRIES`/`MAX_COVER` caps can run — that parse is the `zip`
//! crate's own constructor, and it has no bounded/streaming-directory option to hand
//! it one. A crafted archive of millions of tiny entries therefore costs the
//! directory parse regardless of what we cap afterward. No fix available at this
//! layer; recorded here rather than re-discovered per call site.

use std::io::{Cursor, Read, Seek};

use zip::ZipArchive;

use super::select::{pick_covers, Entry};

/// Stream a comic/image-zip cover from a SEEKABLE reader without buffering the whole
/// archive — the `zip` crate seeks to the central directory and reads only the chosen
/// entries. Used for oversized CBZ/ZIP (past the in-memory size cap), where the reader
/// is the shell's IStream. Project/Office packages CAN be that large (a multi-layer
/// Krita/ORA painting is oversized precisely because of its `data/layer*.png` entries;
/// a media-heavy deck likewise), and for those the generic image-pick grabs an
/// arbitrary layer/media image — ORA's `data/layer*.png` natural-sorts BEFORE the real
/// composite — so run the same dedicated-preview dispatch as the in-memory `extract`.
pub(crate) fn cover_from_reader<R: Read + Seek>(reader: R) -> Option<Vec<u8>> {
    covers_from_reader(reader, 1).and_then(|mut v| (!v.is_empty()).then(|| v.swap_remove(0)))
}

/// Up to `want` cover images from a seekable ZIP-family reader — the multi-image
/// generalization of [`cover_from_reader`], feeding the generic-archive contact
/// sheet. Runs the same [`dedicated_preview`] dispatch as the in-memory
/// [`extract`]: a project/Office/EPUB package yields its ONE real preview (a
/// collage of layer/media internals is never right), so only a plain image zip /
/// CBZ ever returns more than one image. Each returned entry is one bounded read;
/// the archive is never fully decompressed.
pub(crate) fn covers_from_reader<R: Read + Seek>(reader: R, want: usize) -> Option<Vec<Vec<u8>>> {
    let mut zip = ZipArchive::new(reader).ok()?;
    match dedicated_preview(&mut zip) {
        Dedicated::Final(cover) => cover.map(|c| vec![c]),
        Dedicated::FallThrough => covers_image_only(&mut zip, want),
    }
}

/// Outcome of the dedicated-preview dispatch.
enum Dedicated {
    /// This package kind OWNS the answer — take it verbatim, even when it's None
    /// (an Office doc with no stored thumbnail must not fall through to the
    /// generic image pick, which would grab embedded slide media).
    Final(Option<Vec<u8>>),
    /// Not a dedicated kind, or its declared cover didn't resolve: continue to the
    /// generic natural-first image pick.
    FallThrough,
}

/// The dedicated-preview cascade — project package, Office doc, EPUB, FBZ — shared
/// by BOTH the in-memory [`extract`] and the seekable [`covers_from_reader`].
///
/// It lives in one generic function on purpose: the two paths used to carry
/// separate copies, and the seekable one silently lacked the EPUB and FBZ arms, so
/// an EPUB big enough to take the streaming path (or any plain `.zip`-extension
/// book routed through the generic-archive probe) fell through to the generic
/// image pick and got an arbitrary interior image instead of its real cover.
fn dedicated_preview<R: Read + Seek>(zip: &mut ZipArchive<R>) -> Dedicated {
    // Art / CAD / 3D-print project files (Krita/OpenRaster/3MF/FreeCAD): a
    // ready-made embedded preview. Check first — otherwise the generic image-zip
    // path would grab an arbitrary layer/content image.
    if let Some(preview) = super::project::extract(zip) {
        return Dedicated::Final(Some(preview));
    }

    // Office documents (ODF / OOXML PowerPoint): a dedicated embedded preview. If
    // the package IS one of these, its thumbnail is the only sensible cover — take
    // it (or None) without falling through.
    if let Some(kind) = super::office::detect(zip) {
        return Dedicated::Final(super::office::extract(zip, kind));
    }

    // EPUB: identified by META-INF/container.xml -> OPF cover cascade. An EPUB with
    // no resolvable cover DOES fall through to first-image.
    if has_entry(zip, "META-INF/container.xml") {
        if let Some(cover) = super::epub::extract(zip) {
            return Dedicated::Final(Some(cover));
        }
    }

    // FBZ: a single .fb2 inside -> run the FB2 path on it.
    if let Some(name) = find_entry_ext(zip, ".fb2") {
        if let Some(data) = read_named(zip, &name) {
            if let Some(cover) = super::fb2::extract(&data) {
                return Dedicated::Final(Some(cover));
            }
        }
    }

    Dedicated::FallThrough
}

/// The generic CBZ / image-zip cover: natural-first cover image, one entry read.
pub(crate) fn cover_image_only<R: Read + Seek>(zip: &mut ZipArchive<R>) -> Option<Vec<u8>> {
    covers_image_only(zip, 1).and_then(|mut v| (!v.is_empty()).then(|| v.swap_remove(0)))
}

/// Aggregate decode ceiling for a CONTACT SHEET's cover picks (`want` > 1), mirroring
/// `sevenz::NON_SOLID_COVERS_BUDGET` (the aggregate pattern already retired there).
/// `read_index`'s [`super::MAX_COVER`] caps each entry independently, but nothing
/// previously shared a budget ACROSS the up-to-4 picks a contact sheet reads, so a
/// crafted archive of MAX_COVER-sized "cover" candidates could cost 128 MiB
/// synchronously for one shell thumbnail. A single-cover request (`want == 1`, the
/// ordinary `extract`/`cover_image_only` path) is left uncapped by this budget —
/// MAX_COVER alone was already the right bound there, and this only closes the gap
/// the aggregation itself opened.
const CONTACT_SHEET_COVERS_BUDGET: u64 = 8 * 1024 * 1024;

/// Up to `want` natural-first images (cover-named first), one bounded entry read
/// each. An entry that fails to read (corrupt / encrypted / unsupported method)
/// is skipped rather than failing the set — the sheet degrades gracefully.
///
/// Each pick is charged against [`CONTACT_SHEET_COVERS_BUDGET`] by its DECLARED size
/// (capped at MAX_COVER, matching what `read_index` would actually spend reading it)
/// BEFORE the read runs and regardless of whether that read then succeeds — so a run
/// of picks that all fail to decode still can't dodge the budget by never charging it.
pub(crate) fn covers_image_only<R: Read + Seek>(
    zip: &mut ZipArchive<R>,
    want: usize,
) -> Option<Vec<Vec<u8>>> {
    let entries = list_entries(zip);
    let mut remaining = if want > 1 {
        CONTACT_SHEET_COVERS_BUDGET
    } else {
        u64::MAX
    };
    let mut out = Vec::new();
    for idx in pick_covers(&entries, want) {
        let Some(entry) = entries.get(idx) else {
            continue;
        };
        let cost = entry.size.min(super::MAX_COVER);
        if cost > remaining {
            continue;
        }
        remaining -= cost;
        if let Some(bytes) = read_index(zip, idx) {
            out.push(bytes);
        }
    }
    (!out.is_empty()).then_some(out)
}

/// Extract the cover bytes from a ZIP-family container.
pub fn extract(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut zip = ZipArchive::new(Cursor::new(bytes)).ok()?;
    match dedicated_preview(&mut zip) {
        Dedicated::Final(cover) => cover,
        // CBZ / generic image zip: natural-first cover image.
        Dedicated::FallThrough => cover_image_only(&mut zip),
    }
}

fn has_entry<R: Read + Seek>(zip: &mut ZipArchive<R>, name: &str) -> bool {
    zip.by_name(name).is_ok()
}

fn find_entry_ext<R: Read + Seek>(zip: &mut ZipArchive<R>, dot_ext: &str) -> Option<String> {
    // Bounded like `list_entries` below: the FBZ probe now also runs on the
    // SEEKABLE path, and this allocates a String per entry.
    for i in 0..zip.len().min(super::MAX_LIST_ENTRIES) {
        // Skip a member that fails to open rather than abandoning the whole scan —
        // one corrupt entry shouldn't hide a valid match later in the archive.
        let Ok(f) = zip.by_index(i) else { continue };
        let name = f.name().to_string();
        if name.to_ascii_lowercase().ends_with(dot_ext) {
            return Some(name);
        }
    }
    None
}

pub(crate) fn list_entries<R: Read + Seek>(zip: &mut ZipArchive<R>) -> Vec<Entry> {
    // Bounded like `list_bytes` below: this also runs on every plain .zip Explorer
    // thumbnails now, so a directory declaring millions of entries must not drive
    // millions of allocations before pick_covers ever filters (the cover pick then
    // simply chooses among the first entries, same as the viewer's listing).
    let mut out = Vec::new();
    for i in 0..zip.len().min(super::MAX_LIST_ENTRIES) {
        if let Ok(f) = zip.by_index(i) {
            out.push(Entry {
                name: entry_display_name(&f),
                is_dir: f.is_dir(),
                size: f.size(),
            });
        }
    }
    out
}

/// Open a ZIP-family archive from bytes and list up to `max` central-directory entries (no
/// extraction). The `max` bound is applied WHILE collecting, so a crafted archive with millions of
/// tiny entries can't drive millions of `String` allocations.
pub(crate) fn list_bytes(bytes: &[u8], max: usize) -> Option<Vec<Entry>> {
    let mut zip = ZipArchive::new(Cursor::new(bytes)).ok()?;
    let mut out = Vec::new();
    for i in 0..zip.len().min(max) {
        if let Ok(f) = zip.by_index(i) {
            out.push(Entry {
                name: entry_display_name(&f),
                is_dir: f.is_dir(),
                size: f.size(),
            });
        }
    }
    Some(out)
}

/// The listing DISPLAY name for an entry. `zip` already applies the spec's own rule when it
/// parses the central directory (general-purpose bit 11 set -> UTF-8, unset -> CP437, via its
/// own embedded table), so a correctly-flagged archive already comes back right (see
/// `zip::types::CentralDirectoryHeader::from_le`). The gap this closes: some real-world writers
/// (older Info-ZIP on Linux, some 7-Zip configurations) store raw UTF-8 bytes but never set the
/// flag, so those names get CP437-decoded into mojibake even though the crate followed the flag
/// correctly. `read_named`/`by_name` lookups must keep using `f.name()` verbatim (it's the
/// crate's own index key); this override is for display/selection only.
// `ZipFile` gained a reader type parameter in zip 8 (`ZipFile<'a, R: Read + ?Sized>`), so this
// is generic over it rather than over one concrete reader. `?Sized` is required: the crate hands
// out `ZipFile<'_, dyn Read>` on some paths.
fn entry_display_name<R: std::io::Read + ?Sized>(f: &zip::read::ZipFile<'_, R>) -> String {
    prefer_utf8(f.name_raw(), f.name())
}

/// If `raw` decodes cleanly as UTF-8, prefer that over `decoded` (whatever the crate already
/// produced for the bit-11 branch it took). A genuine CP437 name contains at least one byte
/// `>= 0x80` that, standing alone or paired with its neighbours, essentially never forms valid
/// UTF-8 by chance, so this only ever fires for the unflagged-UTF-8 case it targets, and a real
/// CP437/plain-ASCII name passes through `decoded` unchanged.
fn prefer_utf8(raw: &[u8], decoded: &str) -> String {
    match std::str::from_utf8(raw) {
        Ok(s) => s.to_string(),
        Err(_) => decoded.to_string(),
    }
}

pub(crate) fn read_index<R: Read + Seek>(zip: &mut ZipArchive<R>, idx: usize) -> Option<Vec<u8>> {
    let f = zip.by_index(idx).ok()?;
    if f.size() > super::MAX_COVER {
        return None;
    }
    let mut buf = Vec::with_capacity(f.size().min(super::MAX_COVER) as usize);
    f.take(super::MAX_COVER).read_to_end(&mut buf).ok()?;
    (!buf.is_empty()).then_some(buf)
}

pub(crate) fn read_named<R: Read + Seek>(zip: &mut ZipArchive<R>, name: &str) -> Option<Vec<u8>> {
    let f = zip.by_name(name).ok()?;
    if f.size() > super::MAX_COVER {
        return None;
    }
    let mut buf = Vec::with_capacity(f.size().min(super::MAX_COVER) as usize);
    f.take(super::MAX_COVER).read_to_end(&mut buf).ok()?;
    (!buf.is_empty()).then_some(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn cbz_utf8_entry_name_extracts_and_lists_unchanged() {
        let name = "第01話/表紙.png";
        let image = image::RgbaImage::from_pixel(1, 1, image::Rgba([12, 34, 56, 255]));
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();

        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .start_file(name, zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(&png).unwrap();
        let bytes = writer.finish().unwrap().into_inner();

        assert_eq!(list_bytes(&bytes, 1).unwrap()[0].name, name);
        assert_eq!(extract(&bytes).as_deref(), Some(png.as_slice()));
        assert_eq!(
            covers_from_reader(Cursor::new(&bytes), 1).unwrap()[0].as_slice(),
            png.as_slice()
        );
    }

    /// A062: the per-entry MAX_COVER cap alone let a contact sheet's up-to-4 picks each
    /// spend their own full 32 MiB independently — 128 MiB synchronously for one shell
    /// thumbnail. Three picks here individually clear MAX_COVER easily but together
    /// clear the much smaller aggregate budget, so the fix must return FEWER than all
    /// three eligible covers, while a plain want=1 extraction (unaffected by the
    /// aggregate cap) still gets its one pick.
    #[test]
    fn contact_sheet_covers_respect_an_aggregate_budget_across_picks() {
        // ~3.34 MiB each: comfortably under MAX_COVER (32 MiB) individually, but two of
        // them already use most of CONTACT_SHEET_COVERS_BUDGET (8 MiB) and three exceed it.
        let page_bytes = vec![0xABu8; 3_500_000];
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for name in ["page01.png", "page02.png", "page03.png"] {
            writer
                .start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(&page_bytes).unwrap();
        }
        let bytes = writer.finish().unwrap().into_inner();

        // want=1 (the ordinary single-cover extraction) is unaffected by the contact-sheet
        // budget: one ~3.34 MiB pick is nowhere near either ceiling.
        let mut zip1 = ZipArchive::new(Cursor::new(&bytes)).unwrap();
        assert_eq!(
            covers_image_only(&mut zip1, 1).map(|v| v.len()),
            Some(1),
            "single-cover extraction must be unaffected by the contact-sheet aggregate budget"
        );

        // want=4 (a contact sheet): the aggregate budget must cap the returned set BELOW
        // the 3 that are individually eligible under MAX_COVER alone, proving the picks
        // now share one budget instead of each getting a fresh MAX_COVER allowance.
        let mut zip4 = ZipArchive::new(Cursor::new(&bytes)).unwrap();
        let out = covers_image_only(&mut zip4, 4).expect("the first pick alone must fit");
        assert_eq!(
            out.len(),
            2,
            "2 x ~3.34 MiB fits the 8 MiB budget, a 3rd does not; got {} covers",
            out.len()
        );
    }

    /// Bit-11-unset writers that stored raw UTF-8 anyway (real-world Linux zip/7-Zip output)
    /// must NOT come back mojibake: valid multi-byte UTF-8 wins over whatever CP437 fallback
    /// the crate produced for the unflagged branch.
    #[test]
    fn prefer_utf8_overrides_an_unflagged_utf8_name() {
        let raw = "第01話/表紙.png".as_bytes();
        // Stand-in for the crate's CP437 decode of those same bytes when bit 11 is clear;
        // deliberately a different string, so a pass here proves the override actually ran
        // rather than merely returning its second argument.
        assert_eq!(prefer_utf8(raw, "cp437-mojibake"), "第01話/表紙.png");
    }

    /// A genuine CP437 name (accented Western-European bytes with the high bit set, no UTF-8
    /// flag) is NOT valid UTF-8 on its own, so the override must leave the crate's already-
    /// correct CP437 decode alone instead of corrupting it.
    #[test]
    fn prefer_utf8_keeps_the_cp437_fallback_for_non_utf8_bytes() {
        // 0x82 is a lone UTF-8 continuation byte (invalid standing alone), and is CP437's 'é'.
        let raw = [b'r', 0x82, b's', b'u', b'm', 0x82, b'.', b't', b'x', b't'];
        let cp437_decoded = "r\u{e9}sum\u{e9}.txt";
        assert_eq!(prefer_utf8(&raw, cp437_decoded), cp437_decoded);
    }

    /// Plain ASCII must pass through unchanged regardless of which branch the crate took.
    #[test]
    fn prefer_utf8_is_a_no_op_for_ascii() {
        assert_eq!(prefer_utf8(b"readme.txt", "readme.txt"), "readme.txt");
    }
}
