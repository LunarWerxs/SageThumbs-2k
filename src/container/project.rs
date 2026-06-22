//! Embedded-preview extraction for ZIP-packaged "project files" — art apps and
//! 3D tools that bake a ready-made PNG/JPEG preview into the package. We just
//! open the ZIP and slice it out (no rendering, no codecs, no patent exposure —
//! it's a standard PNG/JPEG):
//!
//!   - Krita      `.kra`   → `mergedimage.png` | `preview.png`   (mimetype: krita)
//!   - OpenRaster `.ora`   → `Thumbnails/thumbnail.png`          (mimetype: openraster)
//!   - 3MF        `.3mf`   → `Metadata/thumbnail.png` | `.jpg`   (3D printing)
//!   - FreeCAD    `.fcstd` → `thumbnails/Thumbnail.png`
//!   - Fusion 360 `.f3d`   → `…/Previews/small.png`              (256² model render, CAD)
//!   - Sketch     `.sketch`→ `previews/preview.png`              (design)
//!   - Procreate  `.procreate` → `QuickLook/Thumbnail.png`       (Apple QuickLook)
//!   - Apple iWork `.key/.pages/.numbers` → `QuickLook/Thumbnail.jpg` | `preview.jpg`
//!   - CorelDRAW  `.cdr`   → `metadata/thumbnails/thumbnail.bmp` (X4+/2008+, ZIP/OPC;
//!     older RIFF-based .cdr aren't ZIPs and aren't covered)
//!   - Adobe XD   `.xd`    → `thumbnail.png` | `preview.png`     (mimetype: sparkler)
//!   - Visio      `.vsdx/.vsdm` → `docProps/thumbnail.emf`       (EMF, decoded by magick)
//!
//! Most have NO existing Windows thumbnailer. Works on compact installs (no
//! bundled ImageMagick) since the preview is already a raster image.

use super::util::{contains_ci, decodable_image};
use super::zipfmt::{read_named, Zip};

/// Extract a project-file preview, or None if this ZIP isn't one (or has none).
pub fn extract(zip: &mut Zip) -> Option<Vec<u8>> {
    // Krita / OpenRaster: keyed off their `mimetype` entry (like ODF).
    if let Some(mt) = read_named(zip, "mimetype") {
        if contains_ci(&mt, b"krita") {
            return try_paths(zip, &["mergedimage.png", "preview.png"]);
        }
        if contains_ci(&mt, b"openraster") {
            return try_paths(zip, &["Thumbnails/thumbnail.png", "mergedimage.png"]);
        }
        // Adobe XD: mimetype "application/vnd.adobe.sparkler.project…". Top-level
        // thumbnail.png (small) preferred, preview.png (larger) as fallback.
        if contains_ci(&mt, b"sparkler") {
            return try_paths(zip, &["thumbnail.png", "preview.png"]);
        }
    }
    // Autodesk Fusion 360 `.f3d` (ZIP): a 256×256 model render at
    // `FusionAssetName[Active]/Previews/small.png`. Matched by SUFFIX so the asset-name
    // folder prefix and Fusion version don't matter; the `Previews/small.png` tail is
    // distinctive (Sketch's preview is `previews/preview.png`, so no clash).
    if let Some(data) = read_suffix(zip, "previews/small.png") {
        if let Some(img) = decodable_image(data) {
            return Some(img);
        }
    }
    // 3MF + FreeCAD + design apps (Sketch / Procreate / Apple iWork): probe the
    // known preview paths. Each is distinctive enough not to false-positive on
    // other ZIPs (epub/cbz/office lack them); `preview.jpg` is probed LAST so a
    // more specific match always wins.
    try_paths(
        zip,
        &[
            "Metadata/thumbnail.png",
            "Metadata/thumbnail.jpg",
            "thumbnails/Thumbnail.png",
            "thumbnails/thumbnail.png",
            "previews/preview.png",              // Sketch
            "QuickLook/Thumbnail.png",           // Procreate
            "QuickLook/Thumbnail.jpg",           // Apple iWork (Keynote/Pages/Numbers)
            "metadata/thumbnails/thumbnail.bmp", // CorelDRAW (X4+/2008+, ZIP/OPC)
            "metadata/thumbnails/page1.bmp",     // CorelDRAW (alternate)
            "docProps/thumbnail.emf",            // Visio .vsdx/.vsdm (EMF preview; magick decodes it)
            "geogebra_thumbnail.png",            // GeoGebra .ggb (root-level, distinctive)
            "preview.jpg",                       // Apple iWork (root preview) — least specific, last
        ],
    )
}

fn try_paths(zip: &mut Zip, paths: &[&str]) -> Option<Vec<u8>> {
    for p in paths {
        if let Some(data) = read_named(zip, p) {
            if let Some(img) = decodable_image(data) {
                return Some(img);
            }
        }
    }
    None
}

/// Read the first entry whose (lowercased) name ends with `suffix_lc`. For containers
/// that key the preview off a stable path TAIL rather than a fixed full name — Fusion's
/// `FusionAssetName[Active]/Previews/small.png` has a varying folder prefix.
///
/// Fusion compresses that PNG with **zstd**, which the `zip` crate can't inflate without
/// a C lib — so we locate the entry via the RAW reader (a zstd member is invisible to the
/// normal `by_index`) and, when it's zstd, inflate it ourselves with the pure-Rust
/// `ruzstd`. Store/deflate members go back through the crate's normal decompressing read.
fn read_suffix(zip: &mut Zip, suffix_lc: &str) -> Option<Vec<u8>> {
    use std::io::Read;

    // Locate the entry via the RAW reader — a zstd member errors out of the normal
    // `by_index`, so it'd otherwise be invisible.
    let mut hit = None;
    for i in 0..zip.len() {
        let Ok(f) = zip.by_index_raw(i) else { continue };
        if f.name().to_ascii_lowercase().ends_with(suffix_lc) {
            hit = Some((i, f.name().to_string()));
            break;
        }
    }
    let (idx, name) = hit?;

    // Stored / Deflated: the crate's normal decompressing reader handles these.
    if let Some(d) = read_named(zip, &name) {
        return Some(d);
    }
    // None here means the crate couldn't inflate it — for Fusion that's the zstd-
    // compressed PNG. Read the raw entry bytes and inflate with the pure-Rust ruzstd.
    let mut f = zip.by_index_raw(idx).ok()?;
    let mut raw = Vec::new();
    f.by_ref().take(super::MAX_COVER).read_to_end(&mut raw).ok()?;
    let mut dec = ruzstd::StreamingDecoder::new(raw.as_slice()).ok()?;
    let mut out = Vec::new();
    dec.by_ref().take(super::MAX_COVER).read_to_end(&mut out).ok()?;
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};

    fn tiny_png() -> Vec<u8> {
        let mut buf = Vec::new();
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(2, 2))
            .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        buf
    }

    fn make_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(Cursor::new(&mut buf));
            for (name, data) in entries {
                w.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
                w.write_all(data).unwrap();
            }
            w.finish().unwrap();
        }
        buf
    }

    fn extract_bytes(bytes: &[u8]) -> Option<Vec<u8>> {
        let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        extract(&mut zip)
    }

    #[test]
    fn extracts_project_previews_and_ignores_plain_zips() {
        let png = tiny_png();

        // Krita + OpenRaster: keyed off mimetype.
        let kra = make_zip(&[("mimetype", b"application/x-krita"), ("mergedimage.png", &png)]);
        assert!(extract_bytes(&kra).unwrap().starts_with(&[0x89, b'P', b'N', b'G']));
        let ora = make_zip(&[("mimetype", b"image/openraster"), ("Thumbnails/thumbnail.png", &png)]);
        assert!(extract_bytes(&ora).unwrap().starts_with(&[0x89, b'P', b'N', b'G']));

        // 3MF: no mimetype, preview under Metadata/.
        let mf = make_zip(&[("3D/3dmodel.model", b"<model/>"), ("Metadata/thumbnail.png", &png)]);
        assert!(extract_bytes(&mf).is_some());

        // FreeCAD path.
        let fc = make_zip(&[("Document.xml", b"<doc/>"), ("thumbnails/Thumbnail.png", &png)]);
        assert!(extract_bytes(&fc).is_some());

        // Fusion 360 .f3d: preview under a varying asset-name folder, matched by suffix.
        let f3d = make_zip(&[
            ("Components/part.brep", b"x"),
            ("FusionAssetName[Active]/Previews/small.png", &png),
        ]);
        assert!(extract_bytes(&f3d).is_some(), "fusion f3d preview");

        // Sketch: previews/preview.png
        let sk = make_zip(&[("document.json", b"{}"), ("previews/preview.png", &png)]);
        assert!(extract_bytes(&sk).is_some(), "sketch preview");

        // Procreate: QuickLook/Thumbnail.png
        let pr = make_zip(&[("Document.archive", b"x"), ("QuickLook/Thumbnail.png", &png)]);
        assert!(extract_bytes(&pr).is_some(), "procreate preview");

        // Apple iWork: a root preview.jpg (use png bytes — decodable_image sniffs content).
        let iwork = make_zip(&[("Index.zip", b"x"), ("preview.jpg", &png)]);
        assert!(extract_bytes(&iwork).is_some(), "iWork preview");

        // CorelDRAW (X4+ ZIP): metadata/thumbnails/thumbnail.bmp.
        let cdr = make_zip(&[("content/riffData.cdr", b"x"), ("metadata/thumbnails/thumbnail.bmp", &png)]);
        assert!(extract_bytes(&cdr).is_some(), "coreldraw preview");

        // Adobe XD: keyed off the "sparkler" mimetype.
        let xd = make_zip(&[("mimetype", b"application/vnd.adobe.sparkler.project+dcxucf"), ("thumbnail.png", &png)]);
        assert!(extract_bytes(&xd).is_some(), "adobe xd preview");

        // Visio: docProps/thumbnail.emf (a minimal blob carrying the EMF signature).
        let mut emf = vec![0x01, 0x00, 0x00, 0x00];
        emf.resize(40, 0);
        emf.extend_from_slice(b" EMF");
        let vsdx = make_zip(&[("[Content_Types].xml", b"<Types/>"), ("docProps/thumbnail.emf", emf.as_slice())]);
        assert!(extract_bytes(&vsdx).is_some(), "visio emf preview");

        // A plain image zip (CBZ-style) must NOT be treated as a project file.
        let cbz = make_zip(&[("001.png", &png)]);
        assert!(extract_bytes(&cbz).is_none());
    }
}
