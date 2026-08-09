//! OpenDocument (.odt/.ods/.odp…) and OOXML PowerPoint (.pptx/.pptm/.potx)
//! thumbnails. Both are ZIP packages carrying a ready-made preview image we just
//! extract — same pattern as ebook covers, no rendering.
//!
//! - ODF: a spec-mandated `Thumbnails/thumbnail.png` (OASIS ODF Part 2).
//! - OOXML: a `docProps/thumbnail.*` part referenced from `_rels/.rels`.
//!   PowerPoint embeds one; Word/Excel only when the user opts in, so it's
//!   commonly absent there — we return None and the shell shows the icon.

use std::io::{Read, Seek};

use zip::ZipArchive;

use super::util::{contains_ci, decodable_image};
use super::zipfmt::read_named;

pub enum Kind {
    Odf,
    Ooxml,
}

/// Identify an Office package by its signature entries, or None. Generic over the
/// reader so the oversized-file streamed path shares it (see `project::extract`).
pub fn detect<R: Read + Seek>(zip: &mut ZipArchive<R>) -> Option<Kind> {
    // ODF: a `mimetype` entry whose content is an OpenDocument media type.
    if let Some(mt) = read_named(zip, "mimetype") {
        if contains_ci(&mt, b"opendocument") {
            return Some(Kind::Odf);
        }
    }
    // OOXML: the Open Packaging Conventions content-types part, PLUS at least one part
    // belonging to an actual Office application. The content-types entry alone is too weak
    // a signal to claim the package: saying "this is OOXML" is a COMMITMENT — the caller
    // takes our answer as final and never falls through to the generic cover pick (see
    // `zipfmt::Dedicated::Final`). A comic archive that happens to carry a stray
    // `[Content_Types].xml` would therefore lose its cover page entirely and show the stock
    // icon. Requiring an owning part keeps genuine documents on the dedicated path while
    // letting anything else fall through to the image pick.
    if zip.by_name("[Content_Types].xml").is_ok() && has_ooxml_part(zip) {
        return Some(Kind::Ooxml);
    }
    None
}

/// Does this package contain a part that only a real OOXML document would have? Checks the
/// application part prefixes (Word/PowerPoint/Excel/Visio) and the shared metadata folder.
fn has_ooxml_part<R: Read + Seek>(zip: &mut ZipArchive<R>) -> bool {
    const PREFIXES: [&str; 5] = ["word/", "ppt/", "xl/", "visio/", "docProps/"];
    (0..zip.len()).any(|i| {
        zip.name_for_index(i)
            .is_some_and(|n| PREFIXES.iter().any(|p| n.starts_with(p)))
    })
}

/// Extract the embedded preview, or None for an Office doc that has none.
pub fn extract<R: Read + Seek>(zip: &mut ZipArchive<R>, kind: Kind) -> Option<Vec<u8>> {
    match kind {
        Kind::Odf => decodable_image(read_named(zip, "Thumbnails/thumbnail.png")?),
        Kind::Ooxml => ooxml_thumbnail(zip),
    }
}

fn ooxml_thumbnail<R: Read + Seek>(zip: &mut ZipArchive<R>) -> Option<Vec<u8>> {
    // Resolve the thumbnail relationship's target from the package-root rels.
    if let Some(rels) = read_named(zip, "_rels/.rels") {
        if let Some(target) = thumbnail_target(&rels) {
            let name = target.trim_start_matches('/');
            if let Some(data) = read_named(zip, name) {
                return decodable_image(data);
            }
        }
    }
    // Fallback to the conventional path desktop PowerPoint uses.
    for name in [
        "docProps/thumbnail.jpeg",
        "docProps/thumbnail.jpg",
        "docProps/thumbnail.png",
    ] {
        if let Some(data) = read_named(zip, name) {
            return decodable_image(data);
        }
    }
    None
}

/// Pull the `Target` of the `<Relationship>` whose `Type` ends in
/// `…/metadata/thumbnail`. A light string parse — the rels file is tiny.
///
/// The match is scoped to the **Type** attribute's own value. It used to test the whole
/// `<Relationship …>` tag text for the substring `/thumbnail"`, which any OTHER attribute
/// could satisfy — most easily `Target="media/thumbnail"` on an unrelated relationship
/// (an image, an OLE object). That relationship's target then became the document's
/// "official" thumbnail, so Explorer showed an arbitrary embedded part as the cover and
/// the real `docProps/thumbnail.*` fallback was never reached.
fn thumbnail_target(rels_xml: &[u8]) -> Option<String> {
    let xml = std::str::from_utf8(rels_xml).ok()?;
    for rel in xml.split("<Relationship") {
        let is_thumb = attr(rel, "Type").is_some_and(|t| {
            let t = t.trim_end_matches('/');
            t.ends_with("/thumbnail")
        });
        if is_thumb {
            if let Some(t) = attr(rel, "Target") {
                return Some(t);
            }
        }
    }
    None
}

/// Read one XML attribute's value out of a tag's text. Accepts either quote style — XML
/// permits both, and the thumbnail-relationship match previously tested for `'` as well,
/// so honoring only `"` here would have quietly dropped support for single-quoted rels.
fn attr(s: &str, key: &str) -> Option<String> {
    for quote in ['"', '\''] {
        let pat = format!("{key}={quote}");
        if let Some(at) = s.find(&pat) {
            let start = at + pat.len();
            if let Some(rel_end) = s[start..].find(quote) {
                return Some(s[start..start + rel_end].to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    use zip::write::SimpleFileOptions;

    fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            for (name, data) in entries {
                w.start_file(*name, SimpleFileOptions::default()).unwrap();
                w.write_all(data).unwrap();
            }
            w.finish().unwrap();
        }
        buf.into_inner()
    }

    /// The thumbnail relationship is identified by its Type. An unrelated relationship
    /// whose TARGET merely ends in "/thumbnail" must not be mistaken for it — that made
    /// an arbitrary embedded part the document's cover and skipped the real fallback.
    #[test]
    fn thumbnail_relationship_matches_on_type_not_target() {
        let decoy = br#"<?xml version="1.0"?><Relationships>
          <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/thumbnail"/>
        </Relationships>"#;
        assert_eq!(thumbnail_target(decoy), None, "Target must not qualify");

        let real = br#"<?xml version="1.0"?><Relationships>
          <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/package/2006/relationships/metadata/thumbnail" Target="docProps/thumbnail.jpeg"/>
        </Relationships>"#;
        assert_eq!(
            thumbnail_target(real).as_deref(),
            Some("docProps/thumbnail.jpeg")
        );

        // Single-quoted attributes are valid XML and were accepted before; keep them.
        let single =
            b"<Relationship Type='http://x/metadata/thumbnail' Target='docProps/thumbnail.png'/>";
        assert_eq!(
            thumbnail_target(single).as_deref(),
            Some("docProps/thumbnail.png")
        );

        // Attribute order must not matter (Target before Type).
        let reordered = br#"<Relationship Target="docProps/thumbnail.jpg" Type="http://x/metadata/thumbnail"/>"#;
        assert_eq!(
            thumbnail_target(reordered).as_deref(),
            Some("docProps/thumbnail.jpg")
        );
    }

    /// `[Content_Types].xml` alone must NOT claim a package as OOXML: claiming it is
    /// final, so a comic archive carrying that name would lose its cover entirely.
    #[test]
    fn ooxml_detection_requires_an_office_part() {
        let decoy = zip_of(&[
            ("[Content_Types].xml", b"<Types/>".as_slice()),
            ("page001.jpg", b"\xFF\xD8\xFF\xE0not-really".as_slice()),
        ]);
        let mut zip = ZipArchive::new(Cursor::new(decoy)).unwrap();
        assert!(
            detect(&mut zip).is_none(),
            "a comic zip with a stray content-types entry is not an Office package"
        );

        let real = zip_of(&[
            ("[Content_Types].xml", b"<Types/>".as_slice()),
            ("ppt/presentation.xml", b"<p/>".as_slice()),
        ]);
        let mut zip = ZipArchive::new(Cursor::new(real)).unwrap();
        assert!(matches!(detect(&mut zip), Some(Kind::Ooxml)));

        // docProps/ alone also identifies a genuine package (Word/Excel/PowerPoint all
        // write it, and it is where the conventional thumbnail fallback lives).
        let docprops = zip_of(&[
            ("[Content_Types].xml", b"<Types/>".as_slice()),
            ("docProps/app.xml", b"<a/>".as_slice()),
        ]);
        let mut zip = ZipArchive::new(Cursor::new(docprops)).unwrap();
        assert!(matches!(detect(&mut zip), Some(Kind::Ooxml)));
    }
}
