#![cfg(test)]

use super::*;

/// The load-bearing test of this whole module.
///
/// A seed that its own parser rejects is worse than no seed: the fuzzer mutates it happily,
/// every iteration dies at the magic check, and the suite stays green while testing nothing.
/// So assert that each seed is actually recognised — not that it produces a good picture,
/// just that it gets past the front door into the code being fuzzed.
#[test]
fn every_seed_reaches_its_parser() {
    let by = |name: &str| {
        seeds()
            .into_iter()
            .find(|(n, _)| *n == name)
            .map(|(_, b)| b)
            .unwrap_or_else(|| panic!("seed {name} missing"))
    };
    assert!(psd::header_dims(&by("psd")).is_some(), "psd header");
    assert!(psd::extract(&by("psd")).is_some(), "psd resource thumbnail");
    assert!(ilbm::looks_like_ilbm(&by("ilbm")), "ilbm magic");
    assert!(ilbm::extract(&by("ilbm")).is_some(), "ilbm planar decode");
    assert!(cdr::looks_like_cdr(&by("cdr")), "cdr magic");
    assert!(cdr::extract(&by("cdr")).is_some(), "cdr DISP DIB");
    assert!(icns::extract(&by("icns")).is_some(), "icns png member");
    assert!(pdn::extract(&by("pdn")).is_some(), "pdn base64 thumb");
    assert!(psp::looks_like_psp(&by("psp")), "psp magic");
    assert!(psp::extract(&by("psp")).is_some(), "psp composite bank");
    assert!(c4d::looks_like_c4d(&by("c4d")), "c4d magic");
    assert!(c4d::extract(&by("c4d")).is_some(), "c4d scene preview");
    assert!(eps::extract(&by("eps-dos")).is_some(), "dos-eps tiff");
    assert!(eps::is_eps(&by("epsi")), "epsi magic");
    assert!(
        eps::extract_ascii_preview(&by("epsi")).is_some(),
        "epsi ascii preview"
    );
    assert!(ole::looks_like_ole(&by("ole")), "ole magic");
    assert!(
        ole::read_stream(&by("ole"), "\u{5}SummaryInformation").is_some(),
        "ole directory walk + FAT chain"
    );
    // The `.msg` seed has to prove BOTH of the things it exists for, or a mutation run
    // against it is measuring nothing: two hits on one name (the collector loop) and
    // contents that came back out of the ministream (the miniFAT + root chain + cache).
    let msg_attachments = ole::read_streams(&by("msg"), "__substg1.0_3707001F", 64)
        .expect("msg seed is a compound file");
    assert_eq!(
        msg_attachments.len(),
        2,
        "msg seed must resolve BOTH identically-named attachment streams"
    );
    assert_eq!(
        msg_attachments
            .iter()
            .map(|s| s.len())
            .collect::<Vec<_>>()
            .as_slice(),
        &[20, 18],
        "msg attachment streams must be sliced out of the ministream at their real lengths"
    );
    assert!(max::looks_like_max(&by("max")), "max magic");
    assert!(
        max::extract(&by("max")).is_some(),
        "max summary-information thumbnail"
    );
    assert!(fb2::looks_like_fb2(&by("fb2")), "fb2 magic");
    assert!(fb2::extract(&by("fb2")).is_some(), "fb2 binary cover");
    assert!(gcode::extract(&by("gcode")).is_some(), "gcode thumbnail");
    assert!(
        affinity::looks_like_affinity(&by("affinity")),
        "affinity magic"
    );
    assert!(
        affinity::extract(&by("affinity")).is_some(),
        "affinity embedded png"
    );
    assert!(indd::looks_like_indd(&by("indd")), "indd magic");
    assert!(indd::extract(&by("indd")).is_some(), "indd xmp thumbnail");
    assert!(mobi::extract(&by("mobi")).is_some(), "mobi cover record");
    assert!(blend::extract(&by("blend")).is_some(), "blend TEST preview");
    assert!(dwg::looks_like_dwg(&by("dwg")), "dwg magic");
    assert!(dwg::extract(&by("dwg")).is_some(), "dwg preview table");
    // `clip` is asserted one step short of the others, and deliberately. Its database is a
    // real SQLite file but not a Clip Studio one, so `extract` correctly finds no preview
    // row — which would be indistinguishable from the wrapper failing. Assert instead that
    // the wrapper resolves onto a genuine SQLite header, i.e. that the b-tree walk (the
    // code a crafted file actually attacks) is reached.
    assert!(
        clip::locates_sqlite(&by("clip")),
        "clip CSFCHUNK wrapper should resolve onto the SQLite payload"
    );
    assert!(apk::looks_like_apk(&by("apk")), "apk manifest sniff");
    assert!(
        apk::extract(&by("apk")).is_some(),
        "apk AXML icon attribute -> zip entry"
    );
    assert!(apk::looks_like_apk(&by("xapk")), "xapk wrapper sniff");
    assert!(
        apk::extract(&by("xapk")).is_some(),
        "xapk wrapper -> inner base.apk -> icon"
    );
    assert!(xcf::looks_like_xcf(&by("xcf")), "xcf magic");
    assert!(
        xcf::extract(&by("xcf")).is_some(),
        "xcf property/layer/hierarchy/level/tile walk"
    );
    assert!(skp::looks_like_skp(&by("skp")), "skp header");
    assert!(skp::extract(&by("skp")).is_some(), "skp embedded png carve");
    assert!(rhino::looks_like_3dm(&by("rhino")), "3dm header");
    assert!(
        rhino::extract(&by("rhino")).is_some(),
        "3dm compressed preview chunk"
    );
    assert!(
        waveform::render_from_reader(&mut std::io::Cursor::new(by("wav"))).is_some(),
        "wav pcm waveform render"
    );
    {
        let bytes = by("project");
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("valid zip seed");
        assert!(
            project::extract(&mut zip).is_some(),
            "project krita mimetype preview"
        );
    }
    {
        let mut zip =
            zip::ZipArchive::new(std::io::Cursor::new(by("pxo"))).expect("valid zip seed");
        assert!(
            project::extract(&mut zip).is_some(),
            "pxo pixelorama mimetype preview"
        );
    }
    {
        let mut zip =
            zip::ZipArchive::new(std::io::Cursor::new(by("spla"))).expect("valid zip seed");
        assert!(spla::extract(&mut zip).is_some(), "spla frame-0 render");
    }
    for (label, why) in [
        ("mcworld", "minecraft world_icon.jpeg at the root"),
        ("mcaddon", "minecraft pack_icon.png one folder down"),
    ] {
        let mut zip =
            zip::ZipArchive::new(std::io::Cursor::new(by(label))).expect("valid zip seed");
        assert!(project::extract(&mut zip).is_some(), "{why}");
    }
    assert!(
        aseprite::looks_like_aseprite(&by("aseprite")),
        "aseprite magic"
    );
    assert!(
        aseprite::extract(&by("aseprite")).is_some(),
        "aseprite frame-0 composite"
    );
    assert!(bgcode::looks_like_bgcode(&by("bgcode")), "bgcode magic");
    assert!(
        bgcode::extract(&by("bgcode")).is_some(),
        "bgcode thumbnail block"
    );
    assert!(sfw::looks_like_sfw(&by("sfw")), "sfw magic");
    assert!(sfw::extract(&by("sfw")).is_some(), "sfw unwrapped JPEG");
    assert!(pix::looks_like_alias_pix(&by("pix")), "alias pix header");
    assert!(
        pix::extract(&by("pix")).is_some(),
        "alias pix run-length fill"
    );
    assert!(
        solidworks::extract(&by("solidworks")).is_some(),
        "solidworks PreviewPNG stream"
    );
    assert!(ani::extract(&by("ani")).is_some(), "ani seq-picked frame");
    assert!(
        vtf::extract(&by("vtf")).is_some(),
        "vtf 7.3 resource offset"
    );
    assert!(vtf::extract(&by("vtf-72")).is_some(), "vtf 7.2 level chain");
    assert!(
        ktx::extract(&by("ktx")).is_some(),
        "ktx padded bottom-up rows"
    );
    assert!(dxf::extract(&by("dxf")).is_some(), "dxf THUMBNAILIMAGE dib");
    assert!(
        sixel::extract(&by("sixel")).is_some(),
        "sixel after a preamble"
    );
    for (label, why) in [("nupkg", "nuspec icon"), ("vsix", "vsixmanifest icon")] {
        let mut zip =
            zip::ZipArchive::new(std::io::Cursor::new(by(label))).expect("valid zip seed");
        assert!(package::extract(&mut zip).flatten().is_some(), "{why}");
    }
    {
        let mut zip =
            zip::ZipArchive::new(std::io::Cursor::new(by("xmind"))).expect("valid zip seed");
        assert!(project::extract(&mut zip).is_some(), "xmind map thumbnail");
    }
    assert!(
        audio::ape_fuzzapi::cover_from_items_result(&by("apev2-item"), 1).is_some(),
        "apev2 cover item"
    );
    assert!(
        audio::id3_fuzzapi::front_cover_result(&by("dsf-id3v2-apic")).is_some(),
        "dsf/id3v2 APIC front cover"
    );
    assert!(
        djvu::extract(&by("djvu")).is_some(),
        "djvu bilevel page render"
    );
    {
        let bytes = by("epub");
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("valid zip seed");
        assert!(epub::extract(&mut zip).is_some(), "epub cover-image item");
    }
    assert!(
        sevenz::extract(&by("sevenz")).is_some(),
        "7z stored-entry cover decode"
    );
    assert_eq!(
        sevenz::list(&by("sevenz"), 8).map(|v| v.len()),
        Some(1),
        "7z metadata listing"
    );
    {
        let bytes = by("office-ooxml");
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("valid zip seed");
        let kind = office::detect(&mut zip).expect("ooxml package detected");
        assert!(
            office::extract(&mut zip, kind).is_some(),
            "office docProps thumbnail"
        );
    }
    assert!(
        tarfmt::extract(&by("tar-cover")).is_some(),
        "tar cover entry"
    );
}

/// The dispatcher has to route them too — that is the path the shell actually takes, and a
/// seed only its own parser accepts would leave `extract_cover` untested for that format.
#[test]
fn the_dispatcher_routes_every_cover_bearing_seed() {
    // `clip` is absent on purpose — see `every_seed_reaches_its_parser`: its database is a
    // real SQLite but carries no Clip Studio preview row, so there is no cover to route.
    // `apk`/`xapk` here prove the ordering contract too: both are zips, so they
    // only route to the launcher-icon path while the apk arm sits BEFORE the
    // generic `is_zip` branch in `extract_cover`. `wav` is absent too: waveform
    // rendering is reached only through `audio_art_from_reader`, never through
    // `extract_cover`'s magic dispatch.
    for name in [
        "psd",
        "ilbm",
        "cdr",
        "icns",
        "pdn",
        "psp",
        "c4d",
        "max",
        "fb2",
        "gcode",
        "affinity",
        "indd",
        "mobi",
        "blend",
        "dwg",
        "apk",
        "xapk",
        "xcf",
        "skp",
        "rhino",
        "project",
        "sevenz",
        "pxo",
        "spla",
        "aseprite",
        "bgcode",
        "sfw",
        "pix",
        "solidworks",
        "ani",
        "vtf",
        "vtf-72",
        "ktx",
        "dxf",
        "sixel",
        "nupkg",
        "vsix",
        "xmind",
    ] {
        let bytes = seeds()
            .into_iter()
            .find(|(n, _)| *n == name)
            .map(|(_, b)| b)
            .expect("seed");
        assert!(
            super::super::extract_cover(&bytes).is_some(),
            "extract_cover should route the {name} seed"
        );
    }
}
