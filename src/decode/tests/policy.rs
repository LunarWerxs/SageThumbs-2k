#![cfg(test)]

//! Policy ceilings and how a picture is fitted to the box.
//! The ImageMagick limits have to agree with policy.xml, the scaler has to leave a file's
//! own small picture at its size, and it has to keep enlarging a stand-in for a larger one.

use super::*;

#[test]
fn magick_time_limits_agree() {
    // ImageMagick's own `-limit time` is ELAPSED seconds, so it has to track the external
    // watchdog's WALL backstop, not its CPU budget — pinning it to the CPU number would let
    // magick self-abort a merely-starved decode and reintroduce issue #9 from inside the
    // child. Bump one, this test catches the others (the silent "watchdog waits 120s but
    // magick still kills at 20s" trap).
    //
    // policy.xml is a CEILING rather than a setting: a command-line `-limit` above it is
    // clamped to it, silently. So it has to sit at or above the LONGEST wall backstop any
    // caller runs under, or the full-fidelity decode that issue #41 needed would be cut
    // back to the tile tier's 120 s by the policy file alone.
    let ceiling = limits::MAGICK_POLICY_TIME_LIMIT.parse::<u64>().unwrap();
    // The LONGEST backstop is the full-fidelity one (a compile-time assert beside the constants
    // in decode.rs pins that ordering), so clearing it clears every caller.
    assert!(
        ceiling >= limits::MAGICK_FULL_FIDELITY_WALL_SECS,
        "policy.xml's time ceiling ({ceiling}) is below a wall backstop we actually use",
    );
    assert_eq!(
        MAGICK_TIMEOUT,
        std::time::Duration::from_secs(limits::MAGICK_WALL_SECS)
    );
    assert_eq!(
        MAGICK_CPU_BUDGET,
        std::time::Duration::from_secs(limits::MAGICK_CPU_SECS)
    );
    // That the CPU budget is tighter than the wall backstop is pinned at compile time,
    // by the `const _: () = assert!(...)` beside the constants in decode.rs.
}

#[test]
fn magick_limits_match_policy_xml() {
    // policy.xml ships to disk beside magick.exe, so it can't read the consts at
    // runtime — pin it here. Change a magick `-limit` and you must change
    // scripts/packaging/imagemagick-policy.xml to match (and vice-versa).
    let policy = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/scripts/packaging/imagemagick-policy.xml"
    ))
    .expect("imagemagick-policy.xml must be readable");
    for (name, value) in [
        ("memory", limits::MAGICK_MEMORY_LIMIT),
        ("map", limits::MAGICK_MAP_LIMIT),
        ("time", limits::MAGICK_POLICY_TIME_LIMIT),
    ] {
        let needle = format!("name=\"{name}\" value=\"{value}\"");
        assert!(
            policy.contains(&needle),
            "imagemagick-policy.xml is missing `{needle}` — it drifted from decode::limits",
        );
    }
}

#[test]
fn fits_box_and_preserves_aspect() {
    // 200x100 -> must fit in 96x96, longest side fills the box -> 96x48.
    let d = decode_thumbnail_opts(&png_bytes(200, 100, [255, 0, 0, 255]), 96, false).unwrap();
    assert!(d.width <= 96 && d.height <= 96);
    assert_eq!((d.width, d.height), (96, 48));
    assert_eq!(d.rgba.len(), (d.width * d.height * 4) as usize);
    assert!(d.rgba[0] > 200 && d.rgba[3] == 255); // still red, opaque
}

#[test]
fn a_files_own_picture_is_never_drawn_larger_than_it_is() {
    // The file's pixels ARE the picture, so a tile never exceeds them: Windows draws a 32 px
    // PNG at 32 px in the middle of the cell, and a desktop of small pictures blown up to their
    // tiles is what an uninstall note of 2026-09-15 called "modified". This covers the sprite
    // size that used to nearest-upscale, the mid size that used to fill the box, the aspect
    // that used to survive the enlargement, and the far-too-small case that always stayed
    // native; all of them now come back exactly as they are.
    for (w, h, cx) in [
        (16u32, 16u32, 256u32),
        (100, 50, 256),
        (200, 200, 256),
        (200, 100, 256),
        (100, 100, 1024),
    ] {
        let d = decode_thumbnail_opts(&png_bytes(w, h, [10, 20, 30, 255]), cx, false).unwrap();
        assert_eq!(
            (d.width, d.height),
            (w, h),
            "{w}x{h} into a {cx} box must stay {w}x{h}: the file's own picture is never enlarged"
        );
        assert_eq!(d.rgba.len(), (w * h * 4) as usize);
    }
    // A large image still shrinks to fit.
    let big = png_bytes(800, 600, [10, 20, 30, 255]);
    let d = decode_thumbnail_opts(&big, 256, false).unwrap();
    assert!(d.width <= 256 && d.height <= 256 && d.width.max(d.height) == 256);
}

#[test]
fn garbage_bytes_fail_cleanly() {
    assert!(decode_thumbnail_opts(&[0u8, 1, 2, 3, 4, 5, 6, 7], 96, false).is_err());
}

#[test]
fn a_stand_in_for_a_larger_picture_still_fills_the_tile() {
    // Issue #25 stays fixed: Photoshop bakes a ~160 px preview into a document of any size,
    // and a preview drawn at its own size misstates the file and draws as a smaller tile than
    // its neighbours. The fixture's header declares a 100x100 canvas and its baked preview is
    // 160 px, so the decode is visibly NOT the picture, and it is enlarged to the box as before
    // (Lanczos3, aspect kept; the fixture is square).
    let (psd, _) = crate::container::psd_testutil::synthetic_psd(3, true, 0);
    let d = decode_thumbnail_opts(&psd, 256, false).unwrap();
    assert_eq!(
        (d.width, d.height),
        (256, 256),
        "a baked preview standing in for the document must still fill the tile"
    );
    // An icon is drawn at whatever size the view wants, as Windows draws one, so a 32 px .ico
    // keeps the crisp integer enlargement instead of sitting as a dot in a 256 px cell.
    let mut ico = Vec::new();
    image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
        32,
        32,
        image::Rgba([10, 20, 30, 255]),
    ))
    .write_to(&mut std::io::Cursor::new(&mut ico), image::ImageFormat::Ico)
    .unwrap();
    let d = decode_thumbnail_opts(&ico, 256, false).unwrap();
    assert_eq!(
        (d.width, d.height),
        (256, 256),
        "an icon file still scales to the tile"
    );
}

#[test]
fn metafile_min_density_bumps_small_emf_only() {
    // Minimal EMF header: iType=1 (EMR_HEADER), rclBounds(16), rclFrame(16, .01mm), " EMF".
    let mut emf = vec![0u8; 88];
    emf[0..4].copy_from_slice(&1i32.to_le_bytes());
    emf[40..44].copy_from_slice(b" EMF");
    let set_frame = |b: &mut [u8], w: i32, h: i32| {
        b[24..28].copy_from_slice(&0i32.to_le_bytes()); // left
        b[28..32].copy_from_slice(&0i32.to_le_bytes()); // top
        b[32..36].copy_from_slice(&w.to_le_bytes()); // right
        b[36..40].copy_from_slice(&h.to_le_bytes()); // bottom
    };
    // ~0.67 inch (1693 units of .01 mm) → ~64px at 96 DPI → bump toward a 512px long edge.
    set_frame(&mut emf, 1693, 1000);
    let d = metafile_min_density(&emf).expect("small metafile → density bump");
    assert!((760..=772).contains(&d), "density ~768, got {d}");
    // A 10-inch frame (~960px at 96 DPI) is already large → no override.
    set_frame(&mut emf, 25400, 20000);
    assert_eq!(metafile_min_density(&emf), None, "large metafile untouched");
    // A tiny declared frame would compute a huge density; it must be CAPPED so magick's reader
    // can't be handed a value it chokes on (the pre-1.0.1 WMF crash class).
    set_frame(&mut emf, 100, 80); // ~0.04 in → uncapped would be ~13000
    assert_eq!(
        metafile_min_density(&emf),
        Some(1200),
        "tiny-frame density is capped"
    );
    // Placeable WMF is deliberately NOT bumped — its header bbox/Inch can disagree with the
    // metafile body, which is exactly what made a crafted WMF crash magick.
    let mut wmf = vec![0u8; 22];
    wmf[0..4].copy_from_slice(&[0xD7, 0xCD, 0xC6, 0x9A]);
    wmf[10..12].copy_from_slice(&72i16.to_le_bytes()); // bbox right
    wmf[12..14].copy_from_slice(&54i16.to_le_bytes()); // bbox bottom
    wmf[14..16].copy_from_slice(&1440u16.to_le_bytes()); // Inch
    assert_eq!(
        metafile_min_density(&wmf),
        None,
        "WMF left at intrinsic size"
    );
    assert_eq!(metafile_min_density(b"not a metafile at all ......"), None);
}

#[test]
fn svg_small_scales_up_to_min() {
    let svg = |w: u32, h: u32| {
        format!(
                r#"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}"><rect width="{w}" height="{h}" fill="rgb(20,120,200)"/></svg>"#
            )
            .into_bytes()
    };
    // Small icon/logo → vector rendered UP to the 512px long edge (crisp), aspect preserved.
    let img = render_svg(&svg(24, 24)).expect("small svg renders");
    assert_eq!((img.width(), img.height()), (512, 512));
    let img = render_svg(&svg(48, 24)).expect("small wide svg renders");
    assert_eq!((img.width(), img.height()), (512, 256));
    // Already-large-enough SVG is left at its intrinsic size.
    let img = render_svg(&svg(800, 600)).expect("normal svg renders");
    assert_eq!((img.width(), img.height()), (800, 600));
    // Oversized SVG still clamps down to the 2048 ceiling.
    let img = render_svg(&svg(4000, 3000)).expect("huge svg renders");
    assert_eq!(img.width(), 2048);
}

/// The PDF raster edge has been wrong twice, in opposite directions, so pin the rule.
///
/// It must never render SMALLER than the historical fixed 1024 (that would be a quality
/// regression), never render LARGER than the tile actually asked for (that was the red-team
/// finding: deriving it from the user's global ceiling made a 32 px icon request rasterize a
/// 2560 px page), and must follow a genuinely large request up so PDFs are not the one format
/// that upscales a too-small source once the ceiling exceeds 1024.
#[test]
fn pdf_raster_edge_follows_the_request_but_never_drops_below_1024() {
    use crate::decode::pdf_raster_edge;

    // Small icon views ask for far less than 1024; rasterizing lower would look worse than
    // the behaviour that shipped, so the floor holds.
    for cx in [1, 32, 96, 256, 768, 1024] {
        assert_eq!(
            pdf_raster_edge(Some(cx)),
            1024,
            "cx={cx} must still rasterize at the historical 1024 floor",
        );
    }
    // A genuinely large request is followed, which is the issue #26.5 half of the fix.
    assert_eq!(pdf_raster_edge(Some(1025)), 1025);
    assert_eq!(pdf_raster_edge(Some(2560)), 2560);
    // Full-fidelity callers (Convert, Image info) pass None and keep the historical edge.
    assert_eq!(pdf_raster_edge(None), 1024);
    // And it must NOT track the user's global ceiling: at the top setting, a tiny request is
    // still a tiny request. This is the assertion that fails if the regression comes back.
    assert!(
        pdf_raster_edge(Some(32)) < crate::settings::THUMB_MAX,
        "a small request must not rasterize at the global ceiling",
    );
}
