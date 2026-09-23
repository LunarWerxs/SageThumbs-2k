#![cfg(test)]

//! Seeds for the 2026-09-22 long-tail formats: animated cursors, Valve and Khronos textures,
//! DXF previews, SIXEL, NuGet/VSIX packages, XMind maps, and Photoshop's stored composite.
//! Built by each module's own builder where it has one, so the seed and the module's tests
//! cannot drift apart.

use super::*;

/// A two-frame `.ani` whose frames are real ICO files, shown in reverse order.
pub(super) fn synthetic_ani() -> Vec<u8> {
    let ico = |rgb: [u8; 3]| {
        let img = image::RgbaImage::from_pixel(4, 4, image::Rgba([rgb[0], rgb[1], rgb[2], 255]));
        let mut out = std::io::Cursor::new(Vec::new());
        let _ = image::DynamicImage::ImageRgba8(img).write_to(&mut out, image::ImageFormat::Ico);
        out.into_inner()
    };
    ani::synth(&[ico([200, 0, 0]), ico([0, 0, 200])], Some(&[1, 0]))
}

/// VTF 7.3 with a resource directory, DXT5, five levels: the level-offset sum, the resource
/// walk and the block decode are all reachable.
pub(super) fn synthetic_vtf() -> Vec<u8> {
    vtf::synth(15, 16, 16, 5, true)
}

/// VTF 7.2 (no resource directory), BGRA8888: the pre-7.3 offset path.
pub(super) fn synthetic_vtf_72() -> Vec<u8> {
    vtf::synth(12, 8, 4, 3, false)
}

/// KTX 1, uncompressed RGB with padded rows, stored bottom-up.
pub(super) fn synthetic_ktx() -> Vec<u8> {
    ktx::synth_rgb(5, 3, [10, 20, 30], [200, 100, 50], true)
}

/// A DXF whose `THUMBNAILIMAGE` section carries a small 24 bpp DIB.
pub(super) fn synthetic_dxf() -> Vec<u8> {
    dxf::synth(&dxf::dib_24(6, 4, [40, 80, 120]))
}

/// A SIXEL picture after a line of terminal chatter: raster attributes, an RGB and an HLS
/// register, repeats, carriage returns and a second band.
pub(super) fn synthetic_sixel() -> Vec<u8> {
    b"\n\x1b[2 I\x1bP0;1;0q\"1;1;8;12#1;2;100;20;0#2;1;120;50;80#1!8~$#2!4?!4F-#1~~#2!6N\x1b\\"
        .to_vec()
}

/// A NuGet package whose `.nuspec` names its icon with a backslash path.
pub(super) fn synthetic_nupkg() -> Vec<u8> {
    stored_zip(&[
        (
            "Demo.nuspec",
            br#"<package xmlns="http://schemas.microsoft.com/packaging/2013/05/nuspec.xsd"><metadata><id>Demo</id><icon>images\icon.png</icon></metadata></package>"#,
        ),
        ("images/icon.png", &png(12, 12)),
        ("lib/other.png", &png(4, 4)),
    ])
}

/// A VS Code `.vsix` whose manifest names its icon in `Metadata/Icon`.
pub(super) fn synthetic_vsix() -> Vec<u8> {
    stored_zip(&[
        (
            "extension.vsixmanifest",
            br#"<PackageManifest Version="2.0.0" xmlns="http://schemas.microsoft.com/developer/vsx-schema/2011"><Metadata><Icon>extension/icon.png</Icon></Metadata><Assets><Asset Type="Microsoft.VisualStudio.Services.Icons.Default" Path="extension/icon.png"/></Assets></PackageManifest>"#,
        ),
        ("extension/icon.png", &png(12, 12)),
    ])
}

/// An XMind 2020+ map: its JSON parts, an inserted picture, and the map's thumbnail.
pub(super) fn synthetic_xmind() -> Vec<u8> {
    stored_zip(&[
        ("content.json", b"[{\"rootTopic\":{\"title\":\"x\"}}]"),
        ("metadata.json", b"{}"),
        ("resources/inserted.png", &png(4, 4)),
        ("Thumbnails/thumbnail.png", &png(16, 16)),
    ])
}

/// A transparent RGB Photoshop document whose stored composite is PackBits: the section walk,
/// the row table and the row reads of the Quick preview's big-document path (issue #46).
pub(super) fn synthetic_psd_merged() -> Vec<u8> {
    psdmerged::synth((24, 18), (3, 4, 8), false, true, true, |x, y, c| {
        ((x * 9 + y * 5 + u32::from(c) * 40) % 256) as u16
    })
}

/// A 16-bit Photoshop document saved without its composite: its layers in an `Lr16` block,
/// ZIP with prediction, one masked, one clipped, two inside a group. The flatten's record walk,
/// block search, inflate and blend.
pub(super) fn synthetic_psd_layers() -> Vec<u8> {
    psdmerged::synth_layered()
}
