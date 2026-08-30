//! Transparency, and refusing to show a thumbnail that is really blank.
//! A fully transparent result is a failure worth reporting; a zeroed alpha
//! channel on an opaque image is not, and telling them apart is the whole
//! job here.

use super::*;

#[test]
fn fully_transparent_thumbnail_is_rejected_blank() {
    // A fully-transparent decode is invisible → reject so Explorer shows the icon.
    let clear = png_bytes(32, 32, [0, 0, 0, 0]);
    assert!(
        decode_thumbnail_opts(&clear, 256, false).is_err(),
        "fully-transparent thumbnail must be rejected as blank"
    );
    // Anything with visible pixels is fine.
    let opaque = png_bytes(32, 32, [10, 20, 30, 255]);
    assert!(decode_thumbnail_opts(&opaque, 256, false).is_ok());
}

/// Issue #17's case, and the behaviour we deliberately KEPT after trying the alternative.
///
/// A PNG whose alpha is entirely zero but whose RGB still holds a picture is shown OPAQUE,
/// so the user sees their artwork. It looks wrong when the hidden RGB is black, which is what
/// was reported — but gating this off for PNG was tried and reverted: it replaced an ugly
/// thumbnail with no thumbnail, and a tile you can recognise beats a generic file icon.
#[test]
fn a_zeroed_alpha_png_is_shown_opaque_not_rejected() {
    let hidden = png_bytes(32, 32, [255, 210, 0, 0]);
    let d = decode_thumbnail_opts(&hidden, 256, false)
        .expect("a zeroed-alpha PNG with real colour must still produce a thumbnail");
    assert!(
        d.rgba.chunks_exact(4).all(|px| px[3] == 255),
        "it must come back fully opaque, or the shell composites it away to nothing"
    );
    assert!(
        d.rgba.chunks_exact(4).any(|px| px[0] != 0 || px[1] != 0),
        "and it must still carry the picture that was hidden under the zeroed alpha"
    );
}

#[test]
fn tone_map_rescues_all_zero_alpha_float() {
    // A VFX render pass (emission/AOV EXR) can carry real RGB with the whole
    // alpha channel at 0. Tone-mapping must surface the RGB opaque instead of
    // producing a fully-transparent image the blank-thumbnail watchdog rejects.
    let mut buf = image::Rgba32FImage::new(2, 2);
    for p in buf.pixels_mut() {
        *p = image::Rgba([0.5f32, 0.25, 1.5, 0.0]);
    }
    let out = tone_map_float(&DynamicImage::ImageRgba32F(buf)).to_rgba8();
    assert!(
        out.pixels().all(|p| p.0[3] == 255),
        "all-zero alpha must be rescued to opaque"
    );
    assert!(out.pixels().all(|p| p.0[0] > 0), "RGB content must survive");

    // PARTIAL alpha is compositing intent and must be preserved verbatim.
    let mut buf = image::Rgba32FImage::new(2, 1);
    buf.put_pixel(0, 0, image::Rgba([1.0f32, 1.0, 1.0, 1.0]));
    buf.put_pixel(1, 0, image::Rgba([1.0f32, 1.0, 1.0, 0.0]));
    let out = tone_map_float(&DynamicImage::ImageRgba32F(buf)).to_rgba8();
    assert_eq!(out.get_pixel(0, 0).0[3], 255);
    assert_eq!(
        out.get_pixel(1, 0).0[3],
        0,
        "partial transparency must survive untouched"
    );
}

#[test]
fn zero_alpha_exr_thumbnails_end_to_end() {
    // The full chain for a real all-transparent EXR: image-crate decode ->
    // Rgba32F -> tone_map_float rescue -> fit_to_box -> the fully-transparent
    // watchdog must NOT fire (this exact shape showed a default icon before).
    let mut buf = image::Rgba32FImage::new(8, 8);
    for p in buf.pixels_mut() {
        *p = image::Rgba([0.8f32, 0.2, 0.1, 0.0]);
    }
    let mut exr = Vec::new();
    DynamicImage::ImageRgba32F(buf)
        .write_to(
            &mut std::io::Cursor::new(&mut exr),
            image::ImageFormat::OpenExr,
        )
        .unwrap();
    let out = decode_thumbnail_opts(&exr, 64, false)
        .expect("zero-alpha EXR must thumbnail, not be rejected as blank");
    assert!(out.rgba.chunks_exact(4).any(|px| px[3] != 0));
}

#[test]
fn exr_paths_decode_scaled_off_the_file_handle() {
    let dir = std::env::temp_dir();
    let exr = dir.join(format!("st2k_exr_path_{}.exr", std::process::id()));
    std::fs::write(&exr, ramp_exr_bytes(600, 400)).expect("write temp exr");
    let path = exr.to_string_lossy().into_owned();

    // step = floor(600 / 64) = 9 -> ceil(600/9) x ceil(400/9) = 67x45.
    let img = decode_preview_streamed(&path, 64).expect("EXR must take the streaming path");
    assert_eq!((img.width(), img.height()), (67, 45));
    // Tone-mapped to 8-bit sRGB, like the `image` tier's float output.
    assert!(matches!(img, DynamicImage::ImageRgba8(_)));
    // `decode_preview_path` agrees with it.
    let same = decode_preview_path(&path, 64).expect("path decode");
    assert_eq!((same.width(), same.height()), (67, 45));
    let _ = std::fs::remove_file(&exr);

    // A non-EXR is left entirely to the ordinary bounded read + tiered decode.
    let png = dir.join(format!("st2k_exr_path_{}.png", std::process::id()));
    image::RgbaImage::new(9, 7)
        .save(&png)
        .expect("write temp png");
    let png_path = png.to_string_lossy().into_owned();
    assert!(decode_preview_streamed(&png_path, 64).is_none());
    assert_eq!(
        decode_preview_path(&png_path, 64)
            .map(|i| (i.width(), i.height()))
            .ok(),
        Some((9, 7))
    );
    let _ = std::fs::remove_file(&png);
}
