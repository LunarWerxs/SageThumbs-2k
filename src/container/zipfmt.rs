//! ZIP-family container dispatch: EPUB (OPF cover cascade), FBZ (zipped FB2),
//! or a plain image zip / CBZ (first page by cover-selection).

use std::io::{Cursor, Read};

use zip::ZipArchive;

use super::select::{pick_cover, Entry};

pub(crate) type Zip<'a> = ZipArchive<Cursor<&'a [u8]>>;

/// Extract the cover bytes from a ZIP-family container.
pub fn extract(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut zip = ZipArchive::new(Cursor::new(bytes)).ok()?;

    // Art / CAD / 3D-print project files (Krita/OpenRaster/3MF/FreeCAD): a
    // ready-made embedded preview. Check first — otherwise the generic image-zip
    // path below would grab an arbitrary layer/content image.
    if let Some(preview) = super::project::extract(&mut zip) {
        return Some(preview);
    }

    // Office documents (ODF / OOXML PowerPoint): a dedicated embedded preview. If
    // the package IS one of these, its thumbnail is the only sensible cover —
    // return it (or None) without falling through to the generic image-zip path
    // (which would otherwise grab an arbitrary content image).
    if let Some(kind) = super::office::detect(&mut zip) {
        return super::office::extract(&mut zip, kind);
    }

    // EPUB: identified by META-INF/container.xml -> OPF cover cascade.
    if has_entry(&mut zip, "META-INF/container.xml") {
        if let Some(cover) = super::epub::extract(&mut zip) {
            return Some(cover);
        }
        // EPUB with no resolvable cover: fall through to first-image.
    }

    // FBZ: a single .fb2 inside -> run the FB2 path on it.
    if let Some(name) = find_entry_ext(&mut zip, ".fb2") {
        if let Some(data) = read_named(&mut zip, &name) {
            if let Some(cover) = super::fb2::extract(&data) {
                return Some(cover);
            }
        }
    }

    // CBZ / generic image zip: natural-first cover image.
    let entries = list_entries(&mut zip);
    let idx = pick_cover(&entries)?;
    read_index(&mut zip, idx)
}

fn has_entry(zip: &mut Zip, name: &str) -> bool {
    zip.by_name(name).is_ok()
}

fn find_entry_ext(zip: &mut Zip, dot_ext: &str) -> Option<String> {
    for i in 0..zip.len() {
        let f = zip.by_index(i).ok()?;
        let name = f.name().to_string();
        if name.to_ascii_lowercase().ends_with(dot_ext) {
            return Some(name);
        }
    }
    None
}

pub(crate) fn list_entries(zip: &mut Zip) -> Vec<Entry> {
    let mut out = Vec::new();
    for i in 0..zip.len() {
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

pub(crate) fn read_index(zip: &mut Zip, idx: usize) -> Option<Vec<u8>> {
    let f = zip.by_index(idx).ok()?;
    if f.size() > super::MAX_COVER {
        return None;
    }
    let mut buf = Vec::with_capacity(f.size().min(super::MAX_COVER) as usize);
    f.take(super::MAX_COVER).read_to_end(&mut buf).ok()?;
    (!buf.is_empty()).then_some(buf)
}

pub(crate) fn read_named(zip: &mut Zip, name: &str) -> Option<Vec<u8>> {
    let f = zip.by_name(name).ok()?;
    if f.size() > super::MAX_COVER {
        return None;
    }
    let mut buf = Vec::with_capacity(f.size().min(super::MAX_COVER) as usize);
    f.take(super::MAX_COVER).read_to_end(&mut buf).ok()?;
    (!buf.is_empty()).then_some(buf)
}
