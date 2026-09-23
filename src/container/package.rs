//! Package icons: NuGet `.nupkg` and Visual Studio / VS Code `.vsix`, the icon the package's
//! own manifest names.
//!
//! Both are zips with a small XML manifest at the root. A `.nupkg` carries `<id>.nuspec`,
//! whose `metadata/icon` element is the path of an icon packed inside it (the older
//! `iconUrl` points at the web and is never fetched). A `.vsix` carries
//! `extension.vsixmanifest`, whose `Metadata/Icon` names the icon, with the
//! `Microsoft.VisualStudio.Services.Icons.Default` asset as the fallback VS Code's packager
//! also writes. Checked against real packages from nuget.org (Serilog 4.0.0) and Open VSX
//! (EditorConfig 0.16.4), 2026-09-22.
//!
//! A package that IS one of these but names no icon keeps the stock icon: a package is full
//! of images that are not its picture, which is why the generic image pick never runs for it.

use std::io::{Read, Seek};

use zip::ZipArchive;

use super::util::decodable_image;
use super::zipfmt::read_named;

/// A manifest bigger than this is not one a packager wrote.
const MAX_MANIFEST: u64 = 1024 * 1024;

/// `Some(icon or None)` when the zip is a NuGet or VSIX package, `None` when it is not one.
pub fn extract<R: Read + Seek>(zip: &mut ZipArchive<R>) -> Option<Option<Vec<u8>>> {
    let (manifest, is_vsix) = if zip.by_name("extension.vsixmanifest").is_ok() {
        ("extension.vsixmanifest".to_string(), true)
    } else {
        // `<id>.nuspec` at the root, the only `.nuspec` a package has.
        let name = zip
            .file_names()
            .take(super::MAX_LIST_ENTRIES)
            .find(|n| !n.contains('/') && n.to_ascii_lowercase().ends_with(".nuspec"))?
            .to_string();
        (name, false)
    };
    let xml = zip
        .by_name(&manifest)
        .ok()
        .and_then(|f| crate::decode::read_bounded(f, MAX_MANIFEST).ok());
    let icon = xml
        .and_then(|x| icon_path(&x, is_vsix))
        .and_then(|path| read_entry(zip, &path))
        .and_then(decodable_image);
    Some(icon)
}

/// The icon's path inside the package, as the manifest gives it.
pub(super) fn icon_path(xml: &[u8], vsix: bool) -> Option<String> {
    let text = std::str::from_utf8(xml.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(xml)).ok()?;
    let doc = roxmltree::Document::parse(text).ok()?;
    let named = |n: &roxmltree::Node, name: &str| n.is_element() && n.tag_name().name() == name;
    let text_of = |parent: &str, child: &str| {
        doc.descendants()
            .filter(|n| named(n, parent))
            .flat_map(|n| n.children())
            .find(|n| named(n, child))
            .and_then(|n| n.text())
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
    };
    if !vsix {
        return text_of("metadata", "icon");
    }
    text_of("Metadata", "Icon").or_else(|| {
        doc.descendants()
            .filter(|n| named(n, "Asset"))
            .find(|n| n.attribute("Type") == Some("Microsoft.VisualStudio.Services.Icons.Default"))
            .and_then(|n| n.attribute("Path"))
            .map(str::to_string)
    })
}

/// Read `path` as the manifest wrote it: backslashes are folder separators to both
/// packagers, and the entry's case may differ from the manifest's.
fn read_entry<R: Read + Seek>(zip: &mut ZipArchive<R>, path: &str) -> Option<Vec<u8>> {
    let path = path.replace('\\', "/");
    let path = path.trim_start_matches("./").trim_start_matches('/');
    if let Some(bytes) = read_named(zip, path) {
        return Some(bytes);
    }
    let name = zip
        .file_names()
        .take(super::MAX_LIST_ENTRIES)
        .find(|n| n.eq_ignore_ascii_case(path))?
        .to_string();
    read_named(zip, &name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};

    fn png() -> Vec<u8> {
        let mut buf = Vec::new();
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(3, 3))
            .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        buf
    }

    fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            for (name, data) in entries {
                w.start_file(*name, zip::write::SimpleFileOptions::default())
                    .unwrap();
                w.write_all(data).unwrap();
            }
            w.finish().unwrap();
        }
        buf.into_inner()
    }

    fn run(bytes: &[u8]) -> Option<Option<Vec<u8>>> {
        extract(&mut ZipArchive::new(Cursor::new(bytes)).unwrap())
    }

    const NUSPEC: &[u8] = br#"<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://schemas.microsoft.com/packaging/2013/05/nuspec.xsd">
  <metadata><id>Demo</id><icon>images\Icon.png</icon></metadata>
</package>"#;

    #[test]
    fn a_nupkg_icon_is_read_through_backslashes_and_case() {
        let p = png();
        let pkg = zip_of(&[
            ("Demo.nuspec", NUSPEC),
            ("images/icon.png", &p),
            ("lib/a.png", &p),
        ]);
        assert_eq!(run(&pkg), Some(Some(p.clone())));
        // A package without an icon is still a package: no generic pick.
        let bare = br#"<package><metadata><id>x</id><iconUrl>https://e/x.png</iconUrl></metadata></package>"#;
        let pkg = zip_of(&[("x.nuspec", bare), ("lib/a.png", &p)]);
        assert_eq!(run(&pkg), Some(None));
    }

    #[test]
    fn a_vsix_icon_is_read_from_metadata_or_the_asset_list() {
        let p = png();
        let meta = br#"<PackageManifest Version="2.0.0" xmlns="http://schemas.microsoft.com/developer/vsx-schema/2011"><Metadata><Icon>extension/icon.png</Icon></Metadata></PackageManifest>"#;
        let pkg = zip_of(&[("extension.vsixmanifest", meta), ("extension/icon.png", &p)]);
        assert_eq!(run(&pkg), Some(Some(p.clone())));
        let asset = br#"<PackageManifest><Metadata/><Assets><Asset Type="Microsoft.VisualStudio.Services.Icons.Default" Path="extension/logo.png"/></Assets></PackageManifest>"#;
        let pkg = zip_of(&[
            ("extension.vsixmanifest", asset),
            ("extension/logo.png", &p),
        ]);
        assert_eq!(run(&pkg), Some(Some(p)));
    }

    #[test]
    fn other_zips_are_not_packages() {
        assert_eq!(run(&zip_of(&[("a.png", &png())])), None);
        // A `.nuspec` inside a folder is some other zip's file, not a package manifest.
        assert_eq!(run(&zip_of(&[("docs/x.nuspec", NUSPEC)])), None);
    }

    #[test]
    fn real_packages_carry_their_icon() {
        for name in ["real.nupkg", "real.vsix"] {
            let Some(bytes) = crate::testcorpus::read(name) else {
                eprintln!("NOT MEASURED: {name} absent");
                continue;
            };
            let icon = run(&bytes)
                .flatten()
                .unwrap_or_else(|| panic!("{name} icon"));
            let img = crate::decode::decode_preview(&icon).expect("icon decodes");
            assert!(img.width() >= 32, "{name}");
        }
    }
}
