#![cfg(test)]

//! WebP: lossy size, alpha and transparency.

use super::*;

#[cfg(feature = "webp-lossy")]
#[test]
fn lossy_webp_is_smaller_and_keeps_alpha() {
    let dir = std::env::temp_dir().join(format!("st2k_webp_lossy_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let png = dir.join("photo.png");
    // A noisy gradient (photo-like) with a transparent corner.
    let mut img = image::RgbaImage::new(128, 128);
    for (x, y, p) in img.enumerate_pixels_mut() {
        *p = image::Rgba([(x * 2) as u8, (y * 2) as u8, ((x + y) * 3) as u8, 255]);
    }
    img.put_pixel(0, 0, image::Rgba([0, 0, 0, 0]));
    image::DynamicImage::ImageRgba8(img).save(&png).unwrap();
    let p = png.to_str().unwrap();

    let base = ConvertOpts {
        watermark: None,
        target: Target {
            format: ImageFormat::WebP,
            ext: "webp",
            webp_quality: None,
        },
        jpeg_quality: 90,
        png_level: 6,
        webp_quality: None,
        resize: Resize::None,
    };
    let lossless = convert_file_opts(p, base.clone(), &dir).unwrap();
    let lossy = convert_file_opts(
        p,
        ConvertOpts {
            watermark: None,
            webp_quality: Some(60),
            ..base
        },
        &dir,
    )
    .unwrap();

    // The lossy path actually ran (distinct bytes from the lossless encoder).
    let ls = std::fs::metadata(&lossless).unwrap().len();
    let ly = std::fs::metadata(&lossy).unwrap().len();
    assert_ne!(
        ly, ls,
        "lossy WebP ({ly}) should differ from lossless ({ls})"
    );
    // Output is a valid WebP and alpha survives (not bit-exact for a lossy
    // codec, but the transparent corner stays mostly transparent).
    let a = image::open(&lossy).unwrap().to_rgba8().get_pixel(0, 0)[3];
    assert!(
        a < 128,
        "transparent pixel should stay mostly transparent, got alpha {a}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(feature = "webp-lossy")]
#[test]
fn oversized_lossy_webp_errors_cleanly() {
    // libwebp's 16383px limit: without the guard, encode() panics and (with
    // panic=abort) would kill this whole test binary. A clean Err = guard works.
    let dir = std::env::temp_dir().join(format!("st2k_webp_big_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let png = dir.join("wide.png");
    image::DynamicImage::ImageRgba8(image::RgbaImage::new(16384, 1))
        .save(&png)
        .unwrap();
    let opts = ConvertOpts {
        watermark: None,
        target: Target {
            format: ImageFormat::WebP,
            ext: "webp",
            webp_quality: None,
        },
        jpeg_quality: 90,
        png_level: 6,
        webp_quality: Some(75),
        resize: Resize::None,
    };
    assert!(
        convert_file_opts(png.to_str().unwrap(), opts, &dir).is_err(),
        "oversized lossy WebP must error, not panic"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn webp_keeps_real_logo_transparency() {
    let src = "assets/sg2k_logo.png";
    if !std::path::Path::new(src).exists() {
        return; // running outside the crate root
    }
    let dir = std::env::temp_dir().join(format!("st2k_webp_logo_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let opts = ConvertOpts {
        watermark: None,
        target: Target {
            format: ImageFormat::WebP,
            ext: "webp",
            webp_quality: None,
        },
        jpeg_quality: 90,
        png_level: 6,
        webp_quality: None,
        resize: Resize::None,
    };
    let out = convert_file_opts(src, opts, &dir).unwrap();
    let d = image::open(&out).unwrap().to_rgba8();
    let transparent = d.pixels().filter(|p| p[3] < 255).count();
    let total = (d.width() * d.height()) as usize;
    assert!(
        transparent > total / 100,
        "WebP of the transparent logo should keep transparency: {transparent}/{total} below-opaque"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn webp_convert_preserves_transparency() {
    let dir = std::env::temp_dir().join(format!("st2k_webp_alpha_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let png = dir.join("t.png");
    let mut img = image::RgbaImage::from_pixel(8, 8, image::Rgba([20, 200, 90, 255]));
    img.put_pixel(0, 0, image::Rgba([255, 0, 0, 0])); // fully transparent
    image::DynamicImage::ImageRgba8(img).save(&png).unwrap();

    let opts = ConvertOpts {
        watermark: None,
        target: Target {
            format: ImageFormat::WebP,
            ext: "webp",
            webp_quality: None,
        },
        jpeg_quality: 90,
        png_level: 6,
        webp_quality: None,
        resize: Resize::None,
    };
    let out = convert_file_opts(png.to_str().unwrap(), opts, &dir).unwrap();
    let d = image::open(&out).unwrap().to_rgba8();
    assert_eq!(
        d.get_pixel(0, 0)[3],
        0,
        "transparent pixel must stay transparent in WebP"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
