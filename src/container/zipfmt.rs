//! ZIP-family container dispatch: EPUB (OPF cover cascade), FBZ (zipped FB2),
//! or a plain image zip / CBZ (first page by cover-selection).

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

/// Up to `want` natural-first images (cover-named first), one bounded entry read
/// each. An entry that fails to read (corrupt / encrypted / unsupported method)
/// is skipped rather than failing the set — the sheet degrades gracefully.
pub(crate) fn covers_image_only<R: Read + Seek>(
    zip: &mut ZipArchive<R>,
    want: usize,
) -> Option<Vec<Vec<u8>>> {
    let entries = list_entries(zip);
    let out: Vec<Vec<u8>> =
        pick_covers(&entries, want).into_iter().filter_map(|idx| read_index(zip, idx)).collect();
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
                name: f.name().to_string(),
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
            out.push(Entry { name: f.name().to_string(), is_dir: f.is_dir(), size: f.size() });
        }
    }
    Some(out)
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
