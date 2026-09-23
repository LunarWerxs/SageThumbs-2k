#![cfg(test)]

//! The fuzz targets: every per-format extractor behind one signature, by name.

use super::*;

/// One parser entry point: a stable name and a closure that must never panic on any input.
pub(super) type Target = (&'static str, fn(&[u8]));

/// Every per-format extractor that takes raw bytes, aimed at directly rather than through
/// `extract_cover`'s magic dispatch.
///
/// Going through the dispatcher is not the same test: a mutation has to leave the magic intact
/// to be routed anywhere at all, so the mutations that reach a given parser are exactly the ones
/// that did NOT touch its header. Calling each parser directly removes that filter.
///
/// The `looks_like_*` sniffers are included deliberately. They run on attacker-controlled heads
/// before anything else does, several of them index fixed offsets, and being cheap is not the
/// same as being total.
pub(crate) fn targets() -> Vec<Target> {
    vec![
        ("psd::extract", |b| {
            let _ = psd::extract(b);
        }),
        ("psd::header_dims", |b| {
            let _ = psd::header_dims(b);
        }),
        ("psd::has_alpha", |b| {
            let _ = psd::has_alpha(b);
        }),
        ("psd::thumbnail_from_resources", |b| {
            let _ = psd::thumbnail_from_resources(b);
        }),
        ("ilbm::extract", |b| {
            let _ = ilbm::extract(b);
        }),
        ("ilbm::looks_like_ilbm", |b| {
            let _ = ilbm::looks_like_ilbm(b);
        }),
        ("cdr::extract", |b| {
            let _ = cdr::extract(b);
        }),
        ("cdr::looks_like_cdr", |b| {
            let _ = cdr::looks_like_cdr(b);
        }),
        ("icns::extract", |b| {
            let _ = icns::extract(b);
        }),
        ("pdn::extract", |b| {
            let _ = pdn::extract(b);
        }),
        ("psp::extract", |b| {
            let _ = psp::extract(b);
        }),
        ("psp::extract_best", |b| {
            let _ = psp::extract_best(b);
        }),
        ("psp::looks_like_psp", |b| {
            let _ = psp::looks_like_psp(b);
        }),
        ("c4d::extract", |b| {
            let _ = c4d::extract(b);
        }),
        ("c4d::looks_like_c4d", |b| {
            let _ = c4d::looks_like_c4d(b);
        }),
        ("eps::extract", |b| {
            let _ = eps::extract(b);
        }),
        ("eps::extract_ascii_preview", |b| {
            let _ = eps::extract_ascii_preview(b);
        }),
        ("eps::is_eps", |b| {
            let _ = eps::is_eps(b);
        }),
        // The OLE compound-file reader follows a FAT chain out of the file's own bytes, which
        // is the classic shape for a cycle or a wild index. Both stream names a real caller
        // asks for, since the directory walk that resolves them is the risky part.
        ("ole::read_stream(thumbnail)", |b| {
            let _ = ole::read_stream(b, "\x05SummaryInformation");
        }),
        ("ole::read_stream(missing)", |b| {
            let _ = ole::read_stream(b, "NoSuchStreamName");
        }),
        // `read_streams` is NOT the same code path as `read_stream` with a different cap: it
        // keeps collecting after the first hit, and it CACHES the ministream and the miniFAT
        // across targets so the second attachment doesn't re-walk them. That cache is state
        // carried between iterations of a loop driven by hostile directory entries, which is
        // its own bug class and had no target at all when it shipped in 2.4.0 — the `.msg`
        // attachment list is the only caller and it asks for up to 64.
        ("ole::read_streams(msg-attach)", |b| {
            let _ = ole::read_streams(b, "__substg1.0_3707001F", 64);
        }),
        ("ole::read_streams(missing)", |b| {
            let _ = ole::read_streams(b, "NoSuchStreamName", 64);
        }),
        ("ole::looks_like_ole", |b| {
            let _ = ole::looks_like_ole(b);
        }),
        ("dwg::extract", |b| {
            let _ = dwg::extract(b);
        }),
        ("dwg::looks_like_dwg", |b| {
            let _ = dwg::looks_like_dwg(b);
        }),
        ("indd::extract", |b| {
            let _ = indd::extract(b);
        }),
        ("max::extract", |b| {
            let _ = max::extract(b);
        }),
        ("mobi::extract", |b| {
            let _ = mobi::extract(b);
        }),
        ("fb2::extract", |b| {
            let _ = fb2::extract(b);
        }),
        ("gcode::extract", |b| {
            let _ = gcode::extract(b);
        }),
        ("affinity::extract", |b| {
            let _ = affinity::extract(b);
        }),
        ("blend::extract", |b| {
            let _ = blend::extract(b);
        }),
        // The hand-rolled read-only SQLite reader behind `.clip`. It walks a b-tree out of
        // untrusted page bytes, so a crafted file can make that graph a cycle.
        ("clip::extract", |b| {
            let _ = clip::extract(b);
        }),
        // Android packages: binary-XML + resources.arsc parsing, both driven by
        // file-supplied offsets/counts/strides — exactly the shape mutations attack.
        ("apk::extract", |b| {
            let _ = apk::extract(b);
        }),
        ("apk::looks_like_apk", |b| {
            let _ = apk::looks_like_apk(b);
        }),
        ("xcf::extract", |b| {
            let _ = xcf::extract(b);
        }),
        ("xcf::looks_like_xcf", |b| {
            let _ = xcf::looks_like_xcf(b);
        }),
        ("skp::extract", |b| {
            let _ = skp::extract(b);
        }),
        ("skp::looks_like_skp", |b| {
            let _ = skp::looks_like_skp(b);
        }),
        ("rhino::extract", |b| {
            let _ = rhino::extract(b);
        }),
        ("rhino::looks_like_3dm", |b| {
            let _ = rhino::looks_like_3dm(b);
        }),
        ("djvu::extract", |b| {
            let _ = djvu::extract(b);
        }),
        // Generic 7-Zip (.7z/cb7) metadata parse + one-entry decode.
        ("sevenz::extract", |b| {
            let _ = sevenz::extract(b);
        }),
        ("sevenz::list", |b| {
            let _ = sevenz::list(b, 64);
        }),
        // Raw-PCM waveform rendering (`.wav`/`.aiff` with no embedded cover art). Takes a
        // `Read + Seek` rather than raw bytes, so a mutated buffer is fed in through a Cursor —
        // same input surface Explorer's shell IStream drives in production.
        ("waveform::render_from_reader", |b| {
            let _ = waveform::render_from_reader(&mut std::io::Cursor::new(b));
        }),
        // The ZIP-packaged "project" family (Krita/OpenRaster/3MF/FreeCAD/Fusion/…). Only a
        // structurally valid zip reaches the parser at all (a mutated central directory just
        // fails `ZipArchive::new`), so this exercises `project::extract`'s own path probing
        // rather than the zip crate — the same shape `apk`'s targets already accept.
        ("project::extract", |b| {
            if let Ok(mut zip) = zip::ZipArchive::new(std::io::Cursor::new(b)) {
                let _ = project::extract(&mut zip);
            }
        }),
        // SpriteLoop `.spla`: the manifest parse and the frame-0 compositor, on a structurally
        // valid zip whose manifest JSON and part PNGs are what gets mutated (`synthetic_spla`).
        ("spla::extract", |b| {
            if let Ok(mut zip) = zip::ZipArchive::new(std::io::Cursor::new(b)) {
                let _ = spla::extract(&mut zip);
            }
        }),
        // Aseprite: the chunk walk plus the layer/cel composite, including the zlib cel path.
        ("aseprite::extract", |b| {
            let _ = aseprite::extract(b);
        }),
        // PrusaSlicer binary G-code: the block walk and the thumbnail pick.
        ("bgcode::extract", |b| {
            let _ = bgcode::extract(b);
        }),
        // Seattle FilmWorks: the marker walk that rebuilds a JPEG, then the JPEG decode.
        ("sfw::extract", |b| {
            let _ = sfw::extract(b);
        }),
        // Alias PIX: the run-length accounting that stands in for a signature, then the fill.
        ("pix::extract", |b| {
            let _ = pix::extract(b);
        }),
        // Animated cursors: the RIFF chunk walk, the `seq ` pick and the icon-directory check.
        ("ani::extract", |b| {
            let _ = ani::extract(b);
        }),
        // Photoshop's stored composite: the section walk, the row table and the row reads.
        ("psdmerged::from_reader", |b| {
            let _ = psdmerged::from_reader(std::io::Cursor::new(b), 64);
        }),
        // Valve textures: the header, the 7.3 resource walk and the level-offset sum.
        ("vtf::extract", |b| {
            let _ = vtf::extract(b);
        }),
        // Khronos KTX 1: the key/value walk, the row repack and the flip.
        ("ktx::extract", |b| {
            let _ = ktx::extract(b);
        }),
        // DXF: the backwards section search and the hex-pair decode.
        ("dxf::extract", |b| {
            let _ = dxf::extract(b);
        }),
        // SIXEL: both interpreter passes, the palette and the paint budget.
        ("sixel::extract", |b| {
            let _ = sixel::extract(b);
        }),
        // NuGet / VSIX manifests, on the XML directly: a zip's CRC check would otherwise stop
        // every mutation of the manifest before it reached the parser.
        ("package::icon_path", |b| {
            let _ = package::icon_path(b, false);
            let _ = package::icon_path(b, true);
        }),
        ("package::extract", |b| {
            let _ = zip::ZipArchive::new(std::io::Cursor::new(b))
                .map(|mut zip| package::extract(&mut zip));
        }),
        // SolidWorks: the `PreviewPNG` stream lookup, over the OLE reader.
        ("solidworks::extract", |b| {
            let _ = solidworks::extract(b);
        }),
        // APEv2 "Cover Art (Front)" item parsing, on raw item bytes rather than through the
        // Read+Seek footer wrapper (see `synthetic_apev2_item`).
        (
            "audio::apev2_cover_from_items",
            audio::ape_fuzzapi::cover_from_items,
        ),
        // DSF's trailing ID3v2 tag's APIC frame, on the frame bytes directly.
        ("audio::id3v2_front_cover", audio::id3_fuzzapi::front_cover),
        // EPUB: container.xml -> OPF -> manifest/guide/brute-force cover cascade, then the
        // xhtml-wrapper-follow step.
        ("epub::extract", |b| {
            if let Ok(mut zip) = zip::ZipArchive::new(std::io::Cursor::new(b)) {
                let _ = epub::extract(&mut zip);
            }
        }),
        // Office (ODF/OOXML) detection + thumbnail extraction. Chained like the real
        // dispatcher: `extract` only ever runs on a kind `detect` itself committed to.
        ("office::detect", |b| {
            if let Ok(mut zip) = zip::ZipArchive::new(std::io::Cursor::new(b)) {
                let _ = office::detect(&mut zip);
            }
        }),
        ("office::extract", |b| {
            if let Ok(mut zip) = zip::ZipArchive::new(std::io::Cursor::new(b)) {
                if let Some(kind) = office::detect(&mut zip) {
                    let _ = office::extract(&mut zip, kind);
                }
            }
        }),
        // CBT (TAR-of-images) comic archives.
        ("tarfmt::extract", |b| {
            let _ = tarfmt::extract(b);
        }),
    ]
}
