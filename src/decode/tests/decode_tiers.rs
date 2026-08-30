//! The decode ladder: pure Rust, WIC, and the ImageMagick subprocess.
//! Each tier has to produce the same pixels in the same channel order and
//! the same orientation, or a file's thumbnail changes depending on which
//! tier happened to take it.

use super::*;

#[test]
#[ignore] // needs ImageMagick (magick.exe) installed; run explicitly
fn magick_subprocess_decodes() {
    // Feed a PNG straight to the ImageMagick tier (bypassing the image-first
    // tier) to prove the stdin->stdout subprocess plumbing works end-to-end.
    let png = png_bytes(50, 40, [30, 200, 90, 255]);
    let img = decode_via_magick_capped(&png, None).expect("magick should decode the PNG");
    assert_eq!((img.width(), img.height()), (50, 40));
}

#[test]
fn decode_full_rgba_order_and_orientation() {
    // The companion app's eyedropper samples `decode_full(...).to_rgba8()` and
    // its color readout hinges on the bytes being in **RGBA order, top row
    // first**. Verify with a 2×2 image of four known, distinct colors so a
    // channel swap or vertical flip would be caught. (Moved here from the
    // now-removed `lib::decode_to_rgba8`, a thin wrapper over this.)
    let mut img = image::RgbaImage::new(2, 2);
    img.put_pixel(0, 0, image::Rgba([200, 40, 30, 255])); // top-left red-ish
    img.put_pixel(1, 0, image::Rgba([20, 180, 90, 255])); // top-right green-ish
    img.put_pixel(0, 1, image::Rgba([30, 60, 210, 255])); // bottom-left blue-ish
    img.put_pixel(1, 1, image::Rgba([240, 230, 10, 255])); // bottom-right yellow
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .unwrap();

    let rgba = decode_full(&bytes).unwrap().to_rgba8();
    assert_eq!((rgba.width(), rgba.height()), (2, 2));
    let px = rgba.as_raw();
    // Row 0 first (top-down), each pixel RGBA in order.
    assert_eq!(&px[0..4], &[200, 40, 30, 255], "top-left");
    assert_eq!(&px[4..8], &[20, 180, 90, 255], "top-right");
    assert_eq!(
        &px[8..12],
        &[30, 60, 210, 255],
        "bottom-left (top-down row order)"
    );
    assert_eq!(&px[12..16], &[240, 230, 10, 255], "bottom-right");
}

#[test]
fn wic_path_decodes() {
    // Exercise the WIC plumbing directly (PNG is decodable by WIC even
    // though in production WIC is only the fallback). Needs COM on-thread.
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
        );
    }
    let bytes = png_bytes(40, 20, [10, 20, 200, 255]);
    let img = unsafe { wic_decode(&bytes) }.expect("WIC should decode PNG");
    assert_eq!((img.width(), img.height()), (40, 20));
}

/// Do the pure-Rust tier and the WIC tier AGREE on a plain JPEG?
///
/// This is the gating question for issue 3 (`docs/ISSUES.md`): routing large JPEGs through WIC
/// would let the codec scale during decode, which is the single biggest preview speed-up
/// available. It is only acceptable if the picture does not visibly change, and the two tiers
/// do not manage colour identically, so the answer has to be measured rather than assumed.
///
/// Reports the worst per-channel difference. Failing loudly with the number is the point: if
/// the tiers ever diverge, whoever reads this needs to see by how much, not just "differs".
#[test]
fn pure_rust_and_wic_agree_on_a_plain_jpeg() {
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
        );
    }
    // Smooth gradients plus hard edges: the combination that exposes both colour-management
    // differences and chroma-upsampling differences between two JPEG decoders.
    let src = image::RgbImage::from_fn(256, 256, |x, y| {
        if (x / 16 + y / 16) % 2 == 0 {
            image::Rgb([x as u8, y as u8, 200])
        } else {
            image::Rgb([220, (x ^ y) as u8, (255 - y) as u8])
        }
    });
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 92)
        .encode_image(&image::DynamicImage::ImageRgb8(src))
        .expect("encode jpeg");

    let pure = decode_preview(&jpeg).expect("pure-Rust tier decodes the jpeg");
    let via_wic =
        unsafe { wic::wic_decode_with_thumbnail(&jpeg, None) }.expect("WIC decodes the same jpeg");
    assert_eq!(
        (pure.width(), pure.height()),
        (via_wic.width(), via_wic.height()),
        "tiers must at least agree on size"
    );

    let (a, b) = (pure.to_rgb8(), via_wic.to_rgb8());
    let mut worst = 0u8;
    let mut total = 0u64;
    for (pa, pb) in a.pixels().zip(b.pixels()) {
        for c in 0..3 {
            let d = pa.0[c].abs_diff(pb.0[c]);
            worst = worst.max(d);
            total += d as u64;
        }
    }
    let mean = total as f64 / (a.pixels().len() * 3) as f64;
    println!("  pure-Rust vs WIC: worst channel delta {worst}, mean {mean:.2}");
    // A few levels is ordinary IDCT/upsampling disagreement between two conforming decoders.
    // A large delta would mean a real colour-management difference, and would make swapping
    // tiers a visible change rather than an invisible speed-up.
    assert!(
        worst <= 24 && mean <= 2.0,
        "tiers disagree too much to swap freely: worst {worst}, mean {mean:.2}"
    );
}

/// The capability probe has to actually DISTINGUISH, or it is a no-op that costs a file open.
///
/// A JPEG decoder reduces in the DCT domain and must be accepted; a PNG decoder has no such
/// mode and must be declined, because for a caller using this as a pre-pass ahead of a full
/// decode, accepting PNG means decoding the image twice. Measured before this existed: a 24 MP
/// PNG "scaled" decode cost 605 ms against 690 ms for the full one, i.e. it WAS the full one.
///
/// Both halves are asserted deliberately. A probe that says no to everything would pass a
/// JPEG-only test by accident and silently disable the optimisation everywhere.
///
/// **The target edge is deliberately NOT a clean fraction of the source.** The first version of
/// this test used 1024 -> 256, an exact quarter, and passed against a probe that was broken:
/// it asked the codec "can you produce exactly this size", and JPEG only offers halvings, so
/// every real photo (4000 px asked for 2048, offered 2000) was rejected and the fast path
/// silently died everywhere except at power-of-two ratios. A benchmark caught it, this test did
/// not. 1024 -> 400 is the awkward ratio that reproduces it.
#[test]
fn the_scaled_pre_pass_takes_jpeg_and_declines_png() {
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
        );
    }
    let dir = std::env::temp_dir();
    let pid = std::process::id();

    // Noise, not flat colour: a uniform image can compress to something a codec handles
    // unusually, and the question here is about the codec's capability, not the content.
    let mut img = image::RgbImage::new(1024, 768);
    for (x, y, p) in img.enumerate_pixels_mut() {
        *p = image::Rgb([(x % 251) as u8, (y % 241) as u8, ((x ^ y) % 239) as u8]);
    }
    let jpg = dir.join(format!("st2k_scaleprobe_{pid}.jpg"));
    image::DynamicImage::ImageRgb8(img)
        .save_with_format(&jpg, image::ImageFormat::Jpeg)
        .expect("stage temp jpeg");
    let png = dir.join(format!("st2k_scaleprobe_{pid}.png"));
    std::fs::write(&png, png_bytes(1024, 768, [10, 20, 200, 255])).expect("stage temp png");

    let jpg_p = jpg.to_string_lossy().into_owned();
    let png_p = png.to_string_lossy().into_owned();

    let scaled = crate::decode::wic_scaled_from_path_if_codec_scales(&jpg_p, 400)
        .expect("a JPEG codec reduces in the DCT domain and must be accepted");
    assert_eq!(scaled.width().max(scaled.height()), 400, "scaled to target");
    assert!(
        crate::decode::wic_scaled_from_path_if_codec_scales(&png_p, 400).is_none(),
        "a PNG codec cannot decode reduced, so the pre-pass must decline it rather than \
         decode the image a second time"
    );
    // The unconditional entry point still serves PNG — this probe narrows the PRE-pass only,
    // and the oversized rescue depends on WIC opening anything it can.
    assert!(
        crate::decode::wic_scaled_from_path(&png_p, 400).is_some(),
        "the plain scaled decode must still handle PNG"
    );

    // The BYTES twin, which is the one the shell provider can actually reach: it is handed an
    // IStream and never a filename, so the by-path probe above was unreachable from the surface
    // that draws every thumbnail in Explorer. Same three answers, over buffers.
    // Big enough to clear the size floor: the 1024x768 file above encodes to a few hundred KB,
    // which this path is SUPPOSED to decline. Noise so it cannot compress its way back under.
    let mut big = image::RgbImage::new(3000, 2000);
    for (x, y, p) in big.enumerate_pixels_mut() {
        *p = image::Rgb([
            (x.wrapping_mul(7) % 251) as u8,
            (y.wrapping_mul(13) % 241) as u8,
            ((x ^ y).wrapping_mul(3) % 239) as u8,
        ]);
    }
    let mut big_jpg = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(big)
        .write_to(&mut big_jpg, image::ImageFormat::Jpeg)
        .expect("encode big jpeg");
    let jpg_bytes = big_jpg.into_inner();
    assert!(
        jpg_bytes.len() >= 512 * 1024,
        "the fixture must clear the size floor to test the path at all, got {} bytes",
        jpg_bytes.len()
    );
    let png_file_bytes = std::fs::read(&png).expect("read staged png");
    let scaled_b = crate::decode::wic_scaled_from_bytes_if_codec_scales(&jpg_bytes, 400)
        .expect("a buffered JPEG must take the DCT-scaled path");
    assert_eq!(scaled_b.width().max(scaled_b.height()), 400);
    assert!(
        crate::decode::wic_scaled_from_bytes_if_codec_scales(&png_file_bytes, 400).is_none(),
        "PNG must decline: it has no reduced-size mode, so this would decode it twice"
    );
    // The size floor is load-bearing, not decoration. Below it a full decode is already fast
    // and the COM round trip could cost more than it saves, so a small JPEG must NOT be
    // diverted here — and a tiny one is exactly what most folders are full of.
    let mut small = image::RgbImage::new(64, 48);
    for (x, y, p) in small.enumerate_pixels_mut() {
        *p = image::Rgb([(x % 251) as u8, (y % 241) as u8, ((x ^ y) % 239) as u8]);
    }
    let mut small_jpg = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(small)
        .write_to(&mut small_jpg, image::ImageFormat::Jpeg)
        .expect("encode small jpeg");
    assert!(
        crate::decode::wic_scaled_from_bytes_if_codec_scales(small_jpg.get_ref(), 400).is_none(),
        "a JPEG under the size floor must stay on the pure-Rust tier"
    );

    let _ = std::fs::remove_file(&jpg);
    let _ = std::fs::remove_file(&png);
}

#[test]
fn wic_by_path_decodes_and_scales_without_buffering() {
    // The oversized rescue: WIC opens the FILE itself, so a document past
    // `limits::MAX_INPUT_BYTES` still thumbnails instead of getting the stock icon.
    // Staging a >256 MB file in a unit test is absurd, so this exercises the decode
    // (`wic_scaled_from_path`, which carries no size gate — its two callers apply their
    // own) and leaves the threshold itself to `oversized_wic_rescue`.
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
        );
    }
    let bytes = png_bytes(400, 200, [10, 20, 200, 255]);
    // Process-id suffixed so concurrent `cargo test` runs cannot race each other.
    let path = std::env::temp_dir().join(format!("st2k_wicpath_{}.png", std::process::id()));
    std::fs::write(&path, &bytes).expect("stage temp png");
    let p = path.to_string_lossy().into_owned();

    let img =
        crate::decode::wic_scaled_from_path(&p, 64).expect("WIC should decode a PNG off the file");
    // Scaled DURING decode: the long edge lands on the requested target, and the aspect
    // ratio survives. This is what makes the memory cost the thumbnail, not the document.
    assert_eq!(
        img.width().max(img.height()),
        64,
        "long edge scaled to target"
    );
    assert_eq!(img.height(), 32, "aspect ratio preserved");

    // A path WIC cannot open must decline rather than panic — that `None` is what lets the
    // caller fall through to its existing refusal.
    let missing = path.with_extension("does-not-exist");
    assert!(crate::decode::wic_scaled_from_path(&missing.to_string_lossy(), 64).is_none());

    let _ = std::fs::remove_file(&path);
}

#[test]
fn wic_thumbnail_scaling_keeps_rgba_channel_order() {
    // A SCALED WIC decode must come back in the same channel order as an unscaled one.
    // `IWICBitmapScaler` returns WIC's native BGRA rather than the 32bppRGBA it was fed,
    // so the raw bytes used to reach `RgbaImage::from_raw` transposed: every Explorer tile
    // smaller than its source (HEIC/AVIF/JPEG XR) rendered with red and blue swapped, while
    // the full-fidelity paths — which pass no target edge, hence no scaler — stayed correct.
    // The colour is deliberately asymmetric in R vs B so a swap cannot pass.
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
        );
    }
    const RGBA: [u8; 4] = [10, 20, 200, 255];
    let bytes = png_bytes(64, 64, RGBA);

    let unscaled = unsafe { wic_decode_with_thumbnail(&bytes, Some(64)) }
        .expect("WIC should decode PNG without scaling");
    assert_eq!((unscaled.width(), unscaled.height()), (64, 64));
    assert_eq!(
        unscaled.to_rgba8().get_pixel(32, 32).0,
        RGBA,
        "the no-scaler path must return the source colour"
    );

    let scaled = unsafe { wic_decode_with_thumbnail(&bytes, Some(16)) }
        .expect("WIC should decode PNG with scaling");
    assert_eq!((scaled.width(), scaled.height()), (16, 16));
    assert_eq!(
        scaled.to_rgba8().get_pixel(8, 8).0,
        RGBA,
        "a scaled WIC decode must keep RGBA order (a swap here reads [200, 20, 10, 255])"
    );
}

/// The WIC bomb guard bounds what we COPY OUT, and for a thumbnail that is not the source.
///
/// The rule this pins cost three real files to find: a 24000x14160 PNG (309 MB) was refused
/// outright, even though the requested thumbnail was 256x151 and WIC produces it by streaming
/// rows into the scaler — measured at 2.1 s with no measurable growth in the process working
/// set. Two of the three were UNDER the byte cap, so they were read and decoded for 12-24 s
/// before this guard threw the result away and Explorer drew the stock icon.
#[test]
fn the_wic_guard_bounds_the_output_when_scaling_and_the_source_when_not() {
    use crate::decode::limits::{MAX_DIM, MAX_PIXELS, MAX_SCALED_SOURCE_PIXELS};
    use crate::decode::wic::wic_source_within_limits;

    // The file that started it: past MAX_DIM on both edges and past MAX_PIXELS in total.
    assert!(
        wic_source_within_limits(24_000, 14_160, Some(256)),
        "a 340 MP source must be accepted for a 256 px thumbnail — the scaler decides what we \
         copy out, and that is 256x151 whatever arrives"
    );
    // ...and the SAME source is still refused when the caller wants every pixel, because then
    // the source really is what we materialize. This half is what keeps Convert/Image-info
    // bounded, and it is why the guard could not simply be relaxed.
    assert!(
        !wic_source_within_limits(24_000, 14_160, None),
        "a full-fidelity decode of the same source must stay refused"
    );

    // A source already within the requested edge is a full decode wearing a thumbnail's
    // clothes: no scaler engages, so it gets the full decode's ceiling.
    assert!(!wic_source_within_limits(20_000, 20_000, Some(32_768)));

    // The scaled ceiling is real, not absent — a decompression bomb costs time even when it
    // costs no memory, and time is what an isolated host actually runs out of.
    let half = (MAX_SCALED_SOURCE_PIXELS / 2) as u32;
    assert!(
        wic_source_within_limits(half, 2, Some(256)),
        "exactly AT the ceiling is allowed — the boundary belongs to the accepted side"
    );
    assert!(
        !wic_source_within_limits(half + 1, 2, Some(256)),
        "past MAX_SCALED_SOURCE_PIXELS a scaled decode must still be refused"
    );

    // Degenerate sizes are refused on either path.
    for cx in [None, Some(256)] {
        assert!(!wic_source_within_limits(0, 100, cx));
        assert!(!wic_source_within_limits(100, 0, cx));
    }

    // The unscaled path is untouched: exactly MAX_DIM square is the historical boundary.
    assert!(wic_source_within_limits(MAX_DIM, MAX_DIM, None));
    assert!(!wic_source_within_limits(MAX_DIM + 1, 1, None));
    assert_eq!(MAX_PIXELS * 4, MAX_SCALED_SOURCE_PIXELS);
}

/// The widened scaled-decode ceiling must NOT be reachable from inside `explorer.exe`.
///
/// `MAX_SCALED_SOURCE_PIXELS` lets a thumbnail request stream a source four times past
/// `MAX_PIXELS`, which is safe because thumbnails are drawn in an ISOLATED host — a hostile
/// file costs a throwaway `dllhost`, and the measured worst case there is ~4 s. The classic
/// context menu's preview tile is the one decode that runs in-process on Explorer's own UI
/// thread under `panic = "abort"`, so it must keep the strict guard.
///
/// It does, but only because `decode_menu_preview` -> `decode_cheap` -> `decode_any` passes
/// `None` for the target edge. That is a property of the call graph, and call graphs get
/// refactored, so this pins the BEHAVIOUR instead.
///
/// **The fixture is 20000x20, and the shape is the whole point.** It is past `MAX_DIM` on one
/// edge (so the strict guard refuses it) while being 0.4 MP in total (so the widened guard
/// accepts it AND a real decode is instant). An earlier version of this test used a 20000x20000
/// bomb with a deliberately invalid payload, reasoning that every guard rejects on the declared
/// size before inflating anything — and it passed even when `decode_any` was edited to pass
/// `Some(256)`, because the garbage payload failed to decode either way. It was asserting
/// nothing. Valid pixels are what make the failure mode observable: if the guard ever lets this
/// through, the decode SUCCEEDS and the assertion below fires.
#[test]
fn the_in_process_menu_path_never_gets_the_widened_ceiling() {
    use crate::decode::limits::{MAX_DIM, MAX_SCALED_SOURCE_PIXELS};

    const W: u32 = 20_000;
    const H: u32 = 20;
    // The fixture's two properties are decidable at compile time, so a bad edit should fail the
    // BUILD rather than wait for someone to run the tests — the same call `settings` makes for
    // its constant relationships.
    const _: () = assert!(
        W > MAX_DIM,
        "fixture must exceed the strict per-edge guard, or the split is not being tested"
    );
    const _: () = assert!(
        (W as u64) * (H as u64) < MAX_SCALED_SOURCE_PIXELS,
        "fixture must sit UNDER the widened ceiling, or this proves nothing about the split"
    );

    // The split itself, stated as the pure rule both paths consult.
    assert!(
        wic::wic_source_within_limits(W, H, Some(256)),
        "a thumbnail request may stream this source — it runs in an isolated host"
    );
    assert!(
        !wic::wic_source_within_limits(W, H, None),
        "a full decode of the same source must stay refused"
    );

    // Real, decodable pixels — see the note above about why an invalid payload proved nothing.
    let png = png_bytes(W, H, [20, 140, 90, 255]);
    // The `image` tier declines it first (its own Limits carry MAX_DIM), so this genuinely
    // reaches the WIC tier's guard rather than being rejected earlier for an unrelated reason.
    assert!(
        decode_with_image(&png).is_err(),
        "the image tier must decline it, or the WIC guard is not what this test measures"
    );
    assert!(
        decode_menu_preview(&png).is_err(),
        "the in-process context-menu preview must refuse a source past the strict guard — it          runs inside explorer.exe under panic=abort, where there is no isolated host to lose"
    );
}

/// A large camera JPEG must come out of the DCT-scaled fast path the right way up.
///
/// The fast path added for large-JPEG thumbnail performance returns early, and an early
/// return is a return PAST `apply_exif_orientation`. WIC hands back the codec's stored
/// pixels and never orients them itself, so every portrait phone photo over the 512 KiB
/// floor thumbnailed on its side, and Explorer then cached it that way.
///
/// The test is two-sided ON PURPOSE. Asserting only that the final image is portrait would
/// also pass if WIC declined and the ordinary tiers (which always oriented correctly) ran
/// instead, i.e. it would pass without ever measuring the path it exists to measure. So it
/// first pins that the fast path is genuinely taken AND that its raw output is unrotated,
/// and only then that the public entry point rotates it.
#[test]
fn the_scaled_jpeg_fast_path_still_applies_exif_orientation() {
    // The fast path IS WIC, so it needs COM on this thread like every other WIC test here.
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
        );
    }
    // Landscape, and noisy enough to clear the 512 KiB floor that gates the fast path.
    let base = noisy_jpeg_bytes(1400, 900);
    let bytes = with_exif_orientation(&base, 6); // 6 = rotate 90 CW
    assert!(
        bytes.len() >= 512 * 1024,
        "fixture is {} bytes, under the fast path's floor: it would prove nothing",
        bytes.len()
    );
    assert_eq!(
        exif_orientation(&bytes),
        Some(6),
        "the APP1 must be readable"
    );

    // 1) The fast path really runs for this input, and really returns unrotated pixels.
    let raw = wic_scaled_from_bytes_if_codec_scales(&bytes, 256)
        .expect("WIC must take the scaled path for a >512 KiB JPEG");
    assert!(
        raw.width() > raw.height(),
        "WIC is expected to return the stored, unoriented landscape pixels"
    );

    // 2) The public thumbnail entry point must nonetheless hand back a PORTRAIT tile.
    let out = decode_preview_thumbnail(&bytes, 256).expect("thumbnail decode must succeed");
    assert!(
        out.height() > out.width(),
        "orientation 6 must rotate the landscape source to portrait, got {}x{}",
        out.width(),
        out.height()
    );
}
