//! Generic file/folder operations shared by the folder-mover verbs: collision-free
//! move/copy, name sanitizing, the skwire-style "files → folder", "dimensions →
//! folders", and "tags → folders" sorters, plus the combine-to-CBZ archiver.

use core::ffi::c_void;
use std::iter::once;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use windows::core::{Error, Result, PCWSTR};
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::UI::Shell::{SHChangeNotify, StrCmpLogicalW, SHCNE_UPDATEDIR, SHCNF_PATHW};

use super::actions::is_image;
use super::encode::{read_capped, reserve, write_atomic, OutSlot};
use crate::decode;

/// Case-insensitive whole-path comparison (Windows file names are case-folding,
/// so `Photo.JPG` and `photo.jpg` are the same file — don't bump the counter or
/// rename onto self).
pub(crate) fn same_path(a: &Path, b: &Path) -> bool {
    a.as_os_str().eq_ignore_ascii_case(b.as_os_str())
}

/// Strip characters Windows forbids in a filename, and trailing dots/spaces
/// (which Explorer also rejects). Never returns empty.
pub(crate) fn sanitize_component(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '-',
            c if (c as u32) < 0x20 => '-',
            c => c,
        })
        .collect();
    let cleaned = cleaned.trim().trim_end_matches(['.', ' ']).trim();
    if cleaned.is_empty() {
        "image".to_string()
    } else {
        cleaned.to_string()
    }
}

/// `combined.<ext>` (deduped) next to the first selected file, ATOMICALLY reserved.
///
/// Goes through [`super::encode::reserve`] rather than a `while cand.exists()` loop: two Combine
/// runs fired at the same folder in quick succession both compute the identical free name, and
/// the second `write_atomic` rename then lands on top of the first's finished file. `reserve`
/// claims the name with `create_new`, so the second run walks on to `combined (2).<ext>`. Hold
/// the returned slot until the write completes — dropping it early releases the reservation.
pub(crate) fn combined_path(first: &str, ext: &str) -> super::encode::OutSlot {
    let dir = Path::new(first)
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    super::encode::reserve(|n| {
        if n == 0 {
            dir.join(format!("combined.{ext}"))
        } else {
            dir.join(format!("combined ({}).{ext}", n + 1))
        }
    })
}

/// A path's **file name** as a NUL-terminated UTF-16 buffer — the pre-encoded
/// sort key for [`natural_key_cmp`], built once per element (not once per
/// comparison) so `sort_by_cached_key` doesn't re-encode UTF-16 on every compare.
fn natural_sort_key(p: &str) -> Vec<u16> {
    let fname = Path::new(p)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(p);
    fname.encode_utf16().chain(once(0)).collect()
}

/// Natural (logical) compare of two pre-encoded file-name keys — so page2 sorts
/// before page10, matching Explorer (Win32 `StrCmpLogicalW`). Used to order CBZ
/// pages. Inputs are the NUL-terminated buffers from [`natural_sort_key`].
fn natural_key_cmp(a: &[u16], b: &[u16]) -> std::cmp::Ordering {
    unsafe { StrCmpLogicalW(PCWSTR(a.as_ptr()), PCWSTR(b.as_ptr())) }.cmp(&0)
}

/// Bytes read from the head of a page when probing its dimensions for
/// `ComicInfo.xml`. Every raster header we care about sits far inside this: the
/// only one that scans is JPEG (to its SOF marker), and 64 KiB clears a normal
/// one comfortably. Deliberately NOT a whole-file read - a 300-page CBZ would
/// otherwise be read twice end to end just to fill in two optional attributes.
const PAGE_PROBE_BYTES: usize = 64 * 1024;

/// Width/height of a page from its header alone, or `None` if the prefix does not
/// carry them (the `ImageWidth`/`ImageHeight` attributes are optional, so a page
/// we can't measure cheaply simply goes without).
fn page_dims_from_head(path: &str) -> Option<(u32, u32)> {
    use std::io::Read;
    let f = std::fs::File::open(path).ok()?;
    let mut head = Vec::new();
    f.take(PAGE_PROBE_BYTES as u64)
        .read_to_end(&mut head)
        .ok()?;
    image::ImageReader::new(std::io::Cursor::new(&head))
        .with_guessed_format()
        .ok()
        .and_then(|r| r.into_dimensions().ok())
        .or_else(|| crate::container::real_dims(&head))
}

/// Build the `ComicInfo.xml` sidecar for a CBZ.
///
/// Comic readers - Kavita, Komga, YACReader, ComicRack's descendants - all read
/// this file for page count and per-page facts; without it they have to guess by
/// unzipping. `Image` is a **0-based** index into the page order. `Type` is left
/// off every page but the first, because `Story` is the schema default and
/// repeating it just makes the file bigger.
///
/// Nothing user-supplied reaches this string: only integers and fixed tokens, so
/// there is no text to XML-escape and no injection surface. Keep it that way if a
/// future version starts writing a `<Title>` from the file name.
fn comic_info_xml(dims: &[Option<(u32, u32)>]) -> String {
    use std::fmt::Write as _;
    let mut s = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\r\n<ComicInfo ");
    s.push_str("xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" ");
    s.push_str("xmlns:xsd=\"http://www.w3.org/2001/XMLSchema\">\r\n");
    let _ = writeln!(s, "  <PageCount>{}</PageCount>\r", dims.len());
    s.push_str("  <Pages>\r\n");
    for (i, d) in dims.iter().enumerate() {
        let _ = write!(s, "    <Page Image=\"{i}\"");
        if i == 0 {
            s.push_str(" Type=\"FrontCover\"");
        }
        if let Some((w, h)) = d {
            let _ = write!(s, " ImageWidth=\"{w}\" ImageHeight=\"{h}\"");
        }
        s.push_str(" />\r\n");
    }
    s.push_str("  </Pages>\r\n</ComicInfo>\r\n");
    s
}

/// Combine the selected images into one CBZ (a ZIP of images, the standard comic
/// archive). Pages are natural-sorted by file name and stored **uncompressed**
/// (images are already compressed; STORE avoids a pointless re-deflate). Written
/// to a temp file + renamed so a failed write leaves no partial `.cbz`.
///
/// A `ComicInfo.xml` goes in as the **first** entry, which is what the community
/// CBZ RFC asks for so a reader can pull metadata without scanning the whole
/// central directory. It is the one deflated entry: it is text, and it is small.
pub fn combine_to_cbz(imgs: &[String], out: &Path) -> Result<()> {
    use std::io::Write;

    // Pre-encode each file name to UTF-16 ONCE (the sort key), then natural-sort
    // by the cached buffers — `StrCmpLogicalW` never re-allocates per comparison.
    let mut keyed: Vec<(Vec<u16>, &String)> =
        imgs.iter().map(|p| (natural_sort_key(p), p)).collect();
    keyed.sort_by(|a, b| natural_key_cmp(&a.0, &b.0));
    let sorted: Vec<&String> = keyed.into_iter().map(|(_, p)| p).collect();

    // Header-only probe, before anything is written: the sidecar has to be the
    // FIRST zip entry, so the per-page facts must all be known up front.
    let dims: Vec<Option<(u32, u32)>> = sorted
        .iter()
        .map(|p| page_dims_from_head(p.as_str()))
        .collect();

    write_atomic(out, |tmp| {
        let file = std::fs::File::create(tmp).map_err(|_| Error::from(E_FAIL))?;
        let mut zw = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zw.start_file(
            "ComicInfo.xml",
            zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated),
        )
        .map_err(|_| Error::from(E_FAIL))?;
        zw.write_all(comic_info_xml(&dims).as_bytes())
            .map_err(|_| Error::from(E_FAIL))?;
        for (i, p) in sorted.iter().enumerate() {
            let bytes = read_capped(p)?;
            let stem = Path::new(p.as_str())
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("page");
            // Zero-padded index prefix keeps page order stable in any reader.
            let name = format!("{:03}_{stem}", i + 1);
            zw.start_file(name, opts).map_err(|_| Error::from(E_FAIL))?;
            zw.write_all(&bytes).map_err(|_| Error::from(E_FAIL))?;
        }
        zw.finish().map_err(|_| Error::from(E_FAIL))?;
        Ok(())
    })
}

/// Atomically reserve a collision-free destination for `stem[.ext]` (`src`'s
/// extension) inside `dir` (`name (2).ext` if taken), or `None` if the natural
/// (uncounted) name is already `src` itself (nothing to reserve — the caller
/// treats that as "already in place"). Each candidate past the first is claimed
/// with `create_new` ([`reserve`]) instead of a `while dest.exists()` check, so
/// an external writer (Explorer, another `st2k`, an AV scan) landing a file in the
/// gap between the check and the move/copy/rename can never collide with us — the
/// single race-prone picker this replaces. Shared with
/// [`super::actions::rename_one`] (in-place rename has the identical race: the
/// target dir just happens to equal `src`'s own parent).
pub(crate) fn reserve_dest(src: &Path, dir: &Path, stem: &str) -> Result<Option<OutSlot>> {
    let ext = src.extension().and_then(|e| e.to_str()).map(str::to_string);
    let natural = match &ext {
        Some(e) => dir.join(format!("{stem}.{e}")),
        None => dir.join(stem),
    };
    if same_path(&natural, src) {
        return Ok(None); // already in place, no rename/move needed
    }
    let (stem, dir) = (stem.to_string(), dir.to_path_buf());
    Ok(Some(reserve(move |n| {
        // n=0 is the plain name; a collision then counts "(2), (3), …" (Explorer's
        // own duplicate-naming convention — the first colliding copy is "(2)", not
        // "(1)"), so a collision at n bumps to count n+1.
        let nm = match (&ext, n) {
            (Some(e), 0) => format!("{stem}.{e}"),
            (Some(e), n) => format!("{stem} ({}).{e}", n + 1),
            (None, 0) => stem.clone(),
            (None, n) => format!("{stem} ({})", n + 1),
        };
        dir.join(nm)
    })))
}

/// Move `src` into directory `dir`, dodging name collisions. `dir` must exist.
/// Retries briefly past a transient Explorer lock. (Same-volume move — a
/// cross-volume source just fails and is skipped by the caller.)
fn move_into(src: &Path, dir: &Path) -> Result<PathBuf> {
    let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let Some(slot) = reserve_dest(src, dir, stem)? else {
        return Ok(src.to_path_buf());
    };
    // `rename` overwrites the reserved placeholder atomically; only release the slot
    // from its zero-byte-cleanup drop AFTER that succeeds, so a failed rename still
    // leaves the empty placeholder to be cleaned up, and a legitimately empty `src`
    // isn't mistaken for an abandoned reservation and deleted right after landing.
    crate::fsutil::rename_retrying(src, slot.path()).map_err(|_| Error::from(E_FAIL))?;
    Ok(slot.release())
}

/// Copy `src` into directory `dir`, dodging name collisions. `dir` must exist.
fn copy_into(src: &Path, dir: &Path) -> Result<PathBuf> {
    let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let Some(slot) = reserve_dest(src, dir, stem)? else {
        return Ok(src.to_path_buf()); // copying onto itself → nothing to do
    };
    std::fs::copy(src, slot.path()).map_err(|_| Error::from(E_FAIL))?;
    Ok(slot.release())
}

/// Tell the shell to refresh `dir` (so a new subfolder / moved files appear).
fn refresh_dir(dir: &Path) {
    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(once(0)).collect();
    unsafe {
        SHChangeNotify(
            SHCNE_UPDATEDIR,
            SHCNF_PATHW,
            Some(wide.as_ptr() as *const c_void),
            None,
        );
    }
}

/// Create a fresh folder named `folder_name` (sanitized, deduped) next to the
/// first selected file and move every selected file into it. Shared by the DLL's
/// single-file path and the companion app's multi-file dialog. Returns the folder.
pub fn files_to_folder(paths: &[String], folder_name: &str) -> Result<PathBuf> {
    if paths.is_empty() {
        return Err(Error::from(E_FAIL));
    }
    let name = sanitize_component(folder_name);
    let parent = Path::new(&paths[0])
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();

    // A *fresh* folder (dedupe) — "create a folder & move files in", never silently
    // merge into an unrelated existing folder.
    let mut dir = parent.join(&name);
    let mut n = 2u32;
    while dir.exists() {
        dir = parent.join(format!("{name} ({n})"));
        n += 1;
    }
    std::fs::create_dir_all(&dir).map_err(|_| Error::from(E_FAIL))?;

    // Count successful moves so a TOTAL failure (permissions, cross-volume, locked files)
    // surfaces as an error instead of a fake success — callers build the user-facing report
    // (and the dialog message) off this Result. (A partial move still succeeds: the common
    // failure modes fail every file, which the 0-moved check catches.)
    let moved = paths
        .iter()
        .filter(|p| move_into(Path::new(p), &dir).is_ok())
        .count();
    refresh_dir(&parent);
    if moved == 0 {
        // Nothing landed in the new folder — clean up the empty dir we just made.
        let _ = std::fs::remove_dir(&dir);
        return Err(Error::from(E_FAIL));
    }
    Ok(dir)
}

/// Read an image's pixel dimensions: a cheap header read first, falling back to a
/// full decode for formats the `image` crate can't probe (HEIC/RAW/containers).
fn dims(path: &str) -> Option<(u32, u32)> {
    if let Ok(r) = image::ImageReader::open(path).and_then(|r| r.with_guessed_format()) {
        if let Ok(d) = r.into_dimensions() {
            return Some(d);
        }
    }
    let bytes = read_capped(path).ok()?;
    // Container header probe (PSD canvas size) — the real document dimensions,
    // without the full-fidelity decode below spawning ImageMagick per file.
    if let Some(d) = crate::container::real_dims(&bytes) {
        return Some(d);
    }
    let img = decode::decode_full(&bytes).ok()?;
    Some((img.width(), img.height()))
}

/// Move each selected image into a `WIDTHxHEIGHT` subfolder of its own parent
/// folder (skwire "Dimensions 2 Folders"). Returns (moved, skipped).
pub fn sort_by_dimensions(paths: &[String]) -> (usize, usize) {
    let mut moved = 0usize;
    let mut skipped = 0usize;
    let mut touched: Vec<PathBuf> = Vec::new();
    let images: Vec<&String> = paths.iter().filter(|p| is_image(p.as_str())).collect();
    // Probe dimensions IN PARALLEL first — `dims()` can fall back to a full decode (up
    // to an ImageMagick subprocess per exotic RAW/HEIC file), and every other multi-file
    // verb already fans that out via `parallel::map`. The moves stay serial below: they're
    // cheap, and two files with equal dims share a target dir (no create/move races).
    let probed = crate::parallel::map(&images, |_, p| dims(p.as_str()));
    for (p, d) in images.iter().zip(probed) {
        let src = Path::new(p.as_str());
        let parent = src.parent().unwrap_or_else(|| Path::new("."));
        match d {
            Some((w, h)) => {
                let dir = parent.join(format!("{w}x{h}"));
                if std::fs::create_dir_all(&dir).is_ok() && move_into(src, &dir).is_ok() {
                    moved += 1;
                    if !touched.iter().any(|t| t == parent) {
                        touched.push(parent.to_path_buf());
                    }
                } else {
                    skipped += 1;
                }
            }
            None => skipped += 1,
        }
    }
    for dir in &touched {
        refresh_dir(dir);
    }
    (moved, skipped)
}

/// Expand a folder-name template against one file's tags. Tokens: `$artist`,
/// `$album`, `$title`, `$track` (zero-padded). A missing tag becomes `missing`.
pub(crate) fn expand_template(
    template: &str,
    tags: &crate::strip::AudioTags,
    missing: &str,
) -> String {
    let or = |o: &Option<String>| o.clone().unwrap_or_else(|| missing.to_string());
    let track = tags
        .track
        .map(|n| format!("{n:02}"))
        .unwrap_or_else(|| missing.to_string());
    template
        .replace("$artist", &or(&tags.artist))
        .replace("$album", &or(&tags.album))
        .replace("$title", &or(&tags.title))
        .replace("$track", &track)
}

/// Turn an expanded template into a relative folder path: split on `/` or `\`
/// (so `$artist\$album` nests), sanitize each segment, drop empties. None if
/// nothing usable remains.
fn template_relpath(expanded: &str) -> Option<PathBuf> {
    let mut rel = PathBuf::new();
    for part in expanded.split(['/', '\\']) {
        let p = part.trim();
        if !p.is_empty() {
            rel.push(sanitize_component(p));
        }
    }
    (!rel.as_os_str().is_empty()).then_some(rel)
}

/// Sort audio files into `dest/<template>/…` folders by their tags (skwire
/// "Tags 2 Folders"). `move_files` chooses move vs copy. Returns (done, skipped).
pub fn tags_to_folders(
    files: &[String],
    dest: &Path,
    template: &str,
    missing: &str,
    move_files: bool,
) -> (usize, usize) {
    let mut done = 0usize;
    let mut skipped = 0usize;
    let mut touched = false;
    for p in files {
        let tags = crate::strip::read_audio_tags(p);
        let Some(rel) = template_relpath(&expand_template(template, &tags, missing)) else {
            skipped += 1;
            continue;
        };
        let dir = dest.join(rel);
        if std::fs::create_dir_all(&dir).is_err() {
            skipped += 1;
            continue;
        }
        let src = Path::new(p);
        let ok = if move_files {
            move_into(src, &dir).is_ok()
        } else {
            copy_into(src, &dir).is_ok()
        };
        if ok {
            done += 1;
            touched = true;
        } else {
            skipped += 1;
        }
    }
    if touched {
        refresh_dir(dest);
    }
    (done, skipped)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(dir: &Path, name: &str, w: u32, h: u32) -> String {
        let p = dir.join(name);
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(w, h, image::Rgb([1, 2, 3])))
            .save(&p)
            .unwrap();
        p.to_string_lossy().into_owned()
    }

    /// Kavita/Komga/YACReader read `ComicInfo.xml`, and the community CBZ RFC
    /// wants it FIRST so a reader does not have to scan the archive for it.
    #[test]
    fn cbz_leads_with_comicinfo_and_describes_every_page() {
        let dir = std::env::temp_dir().join(format!("st2k_cbzinfo_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // Deliberately out of lexical order: page 2 must land before page 10.
        let imgs = vec![
            png(&dir, "page10.png", 30, 40),
            png(&dir, "page2.png", 10, 20),
            png(&dir, "page1.png", 50, 60),
        ];
        let out = dir.join("book.cbz");
        combine_to_cbz(&imgs, &out).unwrap();

        let f = std::fs::File::open(&out).unwrap();
        let mut zip = zip::ZipArchive::new(f).unwrap();
        assert_eq!(zip.len(), 4, "3 pages + the sidecar");
        assert_eq!(
            zip.by_index(0).unwrap().name(),
            "ComicInfo.xml",
            "the sidecar must be the first entry"
        );

        let xml = {
            use std::io::Read;
            let mut e = zip.by_index(0).unwrap();
            let mut s = String::new();
            e.read_to_string(&mut s).unwrap();
            s
        };
        assert!(xml.contains("<PageCount>3</PageCount>"), "{xml}");
        assert!(
            xml.contains(
                r#"<Page Image="0" Type="FrontCover" ImageWidth="50" ImageHeight="60" />"#
            ),
            "page 1 is the cover and carries its real size: {xml}"
        );
        assert!(
            xml.contains(r#"<Page Image="1" ImageWidth="10" ImageHeight="20" />"#),
            "natural sort must put page2 second: {xml}"
        );
        assert!(
            xml.contains(r#"<Page Image="2" ImageWidth="30" ImageHeight="40" />"#),
            "page10 sorts last, not second: {xml}"
        );
        assert!(
            !xml.contains("Image=\"3\""),
            "the sidecar counted itself as a page"
        );

        assert_eq!(zip.by_index(1).unwrap().name(), "001_page1.png");
        assert_eq!(zip.by_index(2).unwrap().name(), "002_page2.png");
        assert_eq!(zip.by_index(3).unwrap().name(), "003_page10.png");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A page we cannot measure from its header prefix still gets a `<Page>` row,
    /// just without the two optional size attributes.
    #[test]
    fn unmeasurable_page_still_gets_a_row() {
        let xml = comic_info_xml(&[Some((8, 9)), None]);
        assert!(xml.contains("<PageCount>2</PageCount>"));
        assert!(xml.contains(r#"<Page Image="1" />"#), "{xml}");
    }
}
