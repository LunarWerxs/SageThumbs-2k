//! Embedded-preview extraction for ZIP-packaged "project files" — art apps and
//! 3D tools that bake a ready-made PNG/JPEG preview into the package. We just
//! open the ZIP and slice it out (no rendering, no codecs, no patent exposure —
//! it's a standard PNG/JPEG):
//!
//!   - Krita      `.kra`   → `mergedimage.png` | `preview.png`   (mimetype: krita)
//!   - OpenRaster `.ora`   → `Thumbnails/thumbnail.png`          (mimetype: openraster)
//!   - 3MF        `.3mf`   → `Metadata/thumbnail.png` | `.jpg`   (3D printing)
//!   - FreeCAD    `.fcstd` → `thumbnails/Thumbnail.png`
//!
//! Most have NO existing Windows thumbnailer. Works on compact installs (no
//! bundled ImageMagick) since the preview is already a raster image.

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
    }
    // 3MF + FreeCAD: probe the known preview paths. Distinctive enough not to
    // false-positive on other ZIPs (epub/cbz/office lack them).
    try_paths(
        zip,
        &[
            "Metadata/thumbnail.png",
            "Metadata/thumbnail.jpg",
            "thumbnails/Thumbnail.png",
            "thumbnails/thumbnail.png",
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

fn contains_ci(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w.eq_ignore_ascii_case(needle))
}

/// Only accept bytes that look like a raster format our tiers can render.
fn decodable_image(data: Vec<u8>) -> Option<Vec<u8>> {
    let ok = data.starts_with(&[0xFF, 0xD8, 0xFF]) // JPEG
        || data.starts_with(&[0x89, b'P', b'N', b'G']) // PNG
        || data.starts_with(b"GIF8")
        || data.starts_with(b"BM")
        || (data.len() >= 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WEBP");
    ok.then_some(data)
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

        // A plain image zip (CBZ-style) must NOT be treated as a project file.
        let cbz = make_zip(&[("001.png", &png)]);
        assert!(extract_bytes(&cbz).is_none());
    }
}
