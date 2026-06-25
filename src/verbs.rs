//! Context-menu verb actions (M6).
//!
//! Each action operates on a list of selected file paths (extracted from the
//! shell's IShellItemArray in command.rs). Conversion uses the `image` crate's
//! encoders and writes the result alongside the original.
//!
//! This module is a thin facade: the implementation is split across four
//! submodules and re-exported here so every existing caller keeps reaching the
//! items as `verbs::<Name>` unchanged.
//!
//! - [`menu`] — the `MenuItem` / `MENU` tree, `VerbAction` + its parameter enums,
//!   and the flattening helpers (`leaves`, `quick_items`).
//! - [`encode`] — decode → resize/flatten → encode primitives, the `Target` /
//!   `Resize` / `ConvertOpts` descriptors, and the per-file convert/transform/
//!   resize/email entry points.
//! - [`fileops`] — generic move/copy/sort helpers and the folder-mover verbs
//!   (files-to-folder, dimensions/tags-to-folders) + combine-to-CBZ.
//! - [`actions`] — `run_action` dispatch, clipboard, wallpaper, batch-rename,
//!   set-folder-icon, image-info, and the companion-app launchers.

mod actions;
mod encode;
mod fileops;
mod menu;

// ---- Public surface (matches each item's ORIGINAL visibility) -----------
// `#[allow(unused_imports)]`: several of these re-exports are consumed only by the
// sibling `sagethumbs2k-app` / `st2k` BINARY crates (and the test module), which the
// lib-only build can't see — so they read as "unused" here despite being load-bearing
// public API. (Items the lib itself uses — `is_image`, `run_action`, `MENU`,
// `leaves`, … — don't warn; the attribute just covers the bin-only ones.)

// Menu-tree model + flattening helpers.
#[allow(unused_imports)]
pub use menu::{
    audio_top_level, condensed_top_level, count_leaves, default_menu_tokens, id_for, leaves,
    ordered_top_level, quick_items, slot_for, top_level_audio_ok, CmdSlot, EmailSize, LeafId,
    MenuItem, QuickItem, RenamePattern, Transform, VerbAction, WallpaperMode, MENU, MENU_SEP_TOKEN,
    QUICK_KEYS,
};

// Encode / convert / resize primitives and descriptors.
#[allow(unused_imports)]
pub use encode::{
    compress_to_size, convert_file, convert_file_opts, convert_image_to_pdf_in, convert_to,
    convert_to_magick, convert_to_magick_in, resize_file, shrink_for_email, transform_file,
    ConvertOpts, Resize, Target,
};
pub(crate) use encode::{flatten_onto_white, read_capped};

// Folder/sort verbs + the CBZ archiver.
#[allow(unused_imports)]
pub use fileops::{combine_to_cbz, files_to_folder, sort_by_dimensions, tags_to_folders};

// Dispatch + the non-encode actions.
#[allow(unused_imports)]
pub use actions::{
    copy_to_clipboard, is_audio, is_image, prepare_wallpaper, prepare_wallpaper_in, run_action,
    run_action_detached, set_wallpaper, ActionReport,
};

// Crate-internal helpers surfaced ONLY for the in-crate `tests` module below
// (module-private in the monolith). `#[cfg(test)]` so they don't warn as unused
// in a normal (non-test) lib build — they're reached only via `super::*` in tests.
#[cfg(test)]
pub(crate) use actions::{rename_one, set_folder_icon, tag_base};
#[cfg(test)]
pub(crate) use encode::apply_resize;
#[cfg(test)]
pub(crate) use fileops::{combined_path, expand_template, sanitize_component};

// Imports the original monolithic file kept at module scope for the in-crate
// `tests` (test-only, so gated to avoid unused-import warnings in normal builds).
#[cfg(test)]
use core::ffi::c_void;
#[cfg(test)]
use std::iter::once;
#[cfg(test)]
use std::os::windows::ffi::OsStrExt;
#[cfg(test)]
use image::ImageFormat;
#[cfg(test)]
use windows::Win32::UI::WindowsAndMessaging::{
    SystemParametersInfoW, SPIF_SENDCHANGE, SPIF_UPDATEINIFILE, SPI_SETDESKWALLPAPER,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_png_to_jpg_alongside_original() {
        let dir = std::env::temp_dir().join("st2k_convert_test");
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("sample.png");

        let mut img = image::RgbaImage::new(32, 24);
        for p in img.pixels_mut() {
            *p = image::Rgba([200, 50, 50, 255]);
        }
        image::DynamicImage::ImageRgba8(img)
            .save_with_format(&png, ImageFormat::Png)
            .unwrap();

        let target = Target { format: ImageFormat::Jpeg, ext: "jpg", webp_quality: None };
        let out = convert_file(png.to_str().unwrap(), target).unwrap();
        assert_eq!(out, dir.join("sample.jpg"));
        assert!(out.exists());

        let decoded = image::open(&out).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (32, 24));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn convert_action_records_output_for_reveal() {
        // run_action(Convert) must record the produced file in ActionReport.output
        // so the Invoke handlers can reveal it; reveal() must be a safe no-op when
        // suppressed. (No st2k.exe sits next to the test binary, so this runs the
        // in-process path.)
        let dir = std::env::temp_dir().join("st2k_reveal_out");
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("r.png");
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(8, 8, image::Rgba([1, 2, 3, 255])))
            .save_with_format(&png, ImageFormat::Png)
            .unwrap();

        let target = Target { format: ImageFormat::Jpeg, ext: "jpg", webp_quality: None };
        let report = run_action(VerbAction::Convert(target), &[png.to_str().unwrap().to_string()]);
        assert_eq!(report.done, 1);
        let out = report.output.as_ref().expect("convert should record its output path");
        assert_eq!(out, &dir.join("r.jpg"));
        assert!(out.exists(), "the recorded output file should exist");

        // Suppressed reveal: must not spawn Explorer or panic.
        std::env::set_var("ST2K_NO_REVEAL", "1");
        report.reveal(&[]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn converts_to_modern_formats_and_rotates() {
        let dir = std::env::temp_dir().join("st2k_fmt_test");
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("s.png");
        let mut img = image::RgbaImage::new(40, 24);
        for p in img.pixels_mut() {
            *p = image::Rgba([20, 180, 90, 255]);
        }
        image::DynamicImage::ImageRgba8(img)
            .save_with_format(&png, ImageFormat::Png)
            .unwrap();
        let p = png.to_str().unwrap();

        for (format, ext) in [
            (ImageFormat::WebP, "webp"),
            (ImageFormat::Tiff, "tiff"),
            (ImageFormat::Ico, "ico"),
        ] {
            let out = convert_file(p, Target { format, ext, webp_quality: None })
                .unwrap_or_else(|e| panic!("convert to {ext} failed: {e:?}"));
            assert!(out.exists(), "{ext} output should exist");
        }

        let rot = transform_file(p, Transform::Right90).unwrap();
        let d = image::open(&rot).unwrap();
        assert_eq!((d.width(), d.height()), (24, 40), "90° rotation swaps dimensions");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resize_file_fits_and_keeps_format() {
        let dir = std::env::temp_dir().join("st2k_resize");
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("big.png");
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(2000, 1500)).save(&png).unwrap();

        // Fit within 800×600 → scaled down, aspect kept, still a PNG.
        let out = resize_file(png.to_str().unwrap(), Resize::Fit(800, 600)).unwrap();
        assert_eq!(out.extension().unwrap(), "png");
        let d = image::open(&out).unwrap();
        assert!(d.width() <= 800 && d.height() <= 600 && d.width() == 800, "got {}x{}", d.width(), d.height());

        // Never upscales a small image.
        let small = dir.join("small.png");
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(100, 100)).save(&small).unwrap();
        let out2 = resize_file(small.to_str().unwrap(), Resize::Fit(1920, 1080)).unwrap();
        assert_eq!(image::open(&out2).unwrap().width(), 100, "should not upscale");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn convert_new_raster_formats() {
        let dir = std::env::temp_dir().join("st2k_newfmt");
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("s.png");
        let mut img = image::RgbaImage::from_pixel(24, 16, image::Rgba([30, 140, 200, 255]));
        img.put_pixel(0, 0, image::Rgba([0, 0, 0, 0]));
        image::DynamicImage::ImageRgba8(img).save(&png).unwrap();

        for (format, ext) in [(ImageFormat::Tga, "tga"), (ImageFormat::Qoi, "qoi")] {
            let opts = ConvertOpts {
                target: Target { format, ext, webp_quality: None },
                jpeg_quality: 90,
                png_level: 6,
                webp_quality: None,
                resize: Resize::None,
            };
            let out = convert_file_opts(png.to_str().unwrap(), opts, &dir)
                .unwrap_or_else(|e| panic!("convert to {ext} failed: {e:?}"));
            assert!(out.exists() && image::open(&out).is_ok(), "{ext} should encode + reopen");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "webp-lossy")]
    #[test]
    fn lossy_webp_is_smaller_and_keeps_alpha() {
        let dir = std::env::temp_dir().join("st2k_webp_lossy");
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
            target: Target { format: ImageFormat::WebP, ext: "webp", webp_quality: None },
            jpeg_quality: 90,
            png_level: 6,
            webp_quality: None,
            resize: Resize::None,
        };
        let lossless = convert_file_opts(p, base, &dir).unwrap();
        let lossy = convert_file_opts(p, ConvertOpts { webp_quality: Some(60), ..base }, &dir).unwrap();

        // The lossy path actually ran (distinct bytes from the lossless encoder).
        let ls = std::fs::metadata(&lossless).unwrap().len();
        let ly = std::fs::metadata(&lossy).unwrap().len();
        assert_ne!(ly, ls, "lossy WebP ({ly}) should differ from lossless ({ls})");
        // Output is a valid WebP and alpha survives (not bit-exact for a lossy
        // codec, but the transparent corner stays mostly transparent).
        let a = image::open(&lossy).unwrap().to_rgba8().get_pixel(0, 0)[3];
        assert!(a < 128, "transparent pixel should stay mostly transparent, got alpha {a}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "webp-lossy")]
    #[test]
    fn oversized_lossy_webp_errors_cleanly() {
        // libwebp's 16383px limit: without the guard, encode() panics and (with
        // panic=abort) would kill this whole test binary. A clean Err = guard works.
        let dir = std::env::temp_dir().join("st2k_webp_big");
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("wide.png");
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(16384, 1)).save(&png).unwrap();
        let opts = ConvertOpts {
            target: Target { format: ImageFormat::WebP, ext: "webp", webp_quality: None },
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
        let dir = std::env::temp_dir().join("st2k_webp_logo");
        std::fs::create_dir_all(&dir).unwrap();
        let opts = ConvertOpts {
            target: Target { format: ImageFormat::WebP, ext: "webp", webp_quality: None },
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
        let dir = std::env::temp_dir().join("st2k_webp_alpha");
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("t.png");
        let mut img = image::RgbaImage::from_pixel(8, 8, image::Rgba([20, 200, 90, 255]));
        img.put_pixel(0, 0, image::Rgba([255, 0, 0, 0])); // fully transparent
        image::DynamicImage::ImageRgba8(img).save(&png).unwrap();

        let opts = ConvertOpts {
            target: Target { format: ImageFormat::WebP, ext: "webp", webp_quality: None },
            jpeg_quality: 90,
            png_level: 6,
            webp_quality: None,
            resize: Resize::None,
        };
        let out = convert_file_opts(png.to_str().unwrap(), opts, &dir).unwrap();
        let d = image::open(&out).unwrap().to_rgba8();
        assert_eq!(d.get_pixel(0, 0)[3], 0, "transparent pixel must stay transparent in WebP");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn convert_file_opts_resizes_and_converts() {
        let dir = std::env::temp_dir().join("st2k_cvopts");
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("big.png");
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(2000, 1000, image::Rgb([30, 140, 200])))
            .save(&png)
            .unwrap();

        let opts = ConvertOpts {
            target: Target { format: ImageFormat::Jpeg, ext: "jpg", webp_quality: None },
            jpeg_quality: 80,
            png_level: 6,
            webp_quality: None,
            resize: Resize::Fit(800, 600),
        };
        let out = convert_file_opts(png.to_str().unwrap(), opts, &dir).unwrap();
        assert!(out.exists(), "converted file should exist");
        let d = image::open(&out).unwrap();
        assert!(
            d.width() <= 800 && d.height() <= 600,
            "should fit within 800x600, got {}x{}",
            d.width(),
            d.height()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shrink_for_email_writes_smaller_jpeg() {
        let dir = std::env::temp_dir().join("st2k_email");
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("big.png");
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(2000, 1500, image::Rgb([30, 140, 200])))
            .save(&png)
            .unwrap();

        let out = shrink_for_email(png.to_str().unwrap(), EmailSize::Medium).unwrap();
        assert_eq!(out, dir.join("big (email).jpg"));
        let d = image::open(&out).unwrap();
        assert!(d.width() <= 1024 && d.height() <= 1024 && d.width() == 1024, "got {}x{}", d.width(), d.height());

        // Never upscales a tiny source.
        let small = dir.join("small.png");
        image::DynamicImage::ImageRgb8(image::RgbImage::new(100, 80)).save(&small).unwrap();
        let out2 = shrink_for_email(small.to_str().unwrap(), EmailSize::Large).unwrap();
        assert_eq!(image::open(&out2).unwrap().width(), 100, "should not upscale");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fitup_resize_upscales_explicit_dimensions() {
        // The Convert dialog's "Defined size" must GROW a smaller source (a
        // request); the preset Fit stays shrink-only.
        let dir = std::env::temp_dir().join("st2k_fitup");
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("small.png");
        image::DynamicImage::ImageRgb8(image::RgbImage::new(100, 50)).save(&src).unwrap();

        let opts = ConvertOpts {
            target: Target { format: ImageFormat::Png, ext: "png", webp_quality: None },
            jpeg_quality: 90,
            png_level: 6,
            webp_quality: None,
            resize: Resize::FitUp(400, 400),
        };
        let out = convert_file_opts(src.to_str().unwrap(), opts, &dir).unwrap();
        // 100×50 grown to fit 400×400, aspect preserved → 400×200.
        assert_eq!(
            { let i = image::open(&out).unwrap(); (i.width(), i.height()) },
            (400, 200),
            "FitUp should upscale to the requested box"
        );

        // Fit (the presets) still never upscales.
        let kept = apply_resize(image::DynamicImage::ImageRgb8(image::RgbImage::new(100, 50)), Resize::Fit(400, 400));
        assert_eq!((kept.width(), kept.height()), (100, 50));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sanitizes_filename_components() {
        assert_eq!(sanitize_component("Canon EOS R5"), "Canon EOS R5");
        assert_eq!(sanitize_component("a/b:c*?d"), "a-b-c--d");
        assert_eq!(sanitize_component("trailing.. "), "trailing");
        assert_eq!(sanitize_component("   "), "image");
    }

    #[test]
    fn set_folder_icon_writes_ini_and_ico() {
        let dir = std::env::temp_dir().join("st2k_foldericon");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("pic.png");
        // A non-square source — the icon should be padded to a square canvas.
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(300, 120, image::Rgb([200, 60, 60])))
            .save(&png)
            .unwrap();

        set_folder_icon(png.to_str().unwrap()).unwrap();

        let ico = dir.join("SageThumbsFolder.ico");
        let ini = dir.join("desktop.ini");
        assert!(ico.exists(), "icon file should be written");
        assert!(ini.exists(), "desktop.ini should be written");

        let icon = image::open(&ico).unwrap();
        assert_eq!((icon.width(), icon.height()), (256, 256), "icon is a 256² square");

        let ini_text = std::fs::read_to_string(&ini).unwrap();
        assert!(ini_text.contains("[.ShellClassInfo]"), "ini has the section");
        assert!(ini_text.contains("IconResource=SageThumbsFolder.ico,0"), "ini points at the icon");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn renames_by_exif_date_and_camera() {
        let dir = std::env::temp_dir().join("st2k_rename");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Build a JPEG carrying a hand-crafted EXIF APP1 (Model + DateTime).
        let jpg = dir.join("orig.jpg");
        std::fs::write(&jpg, jpeg_with_exif("TestCam", "2023:05:01 14:30:09")).unwrap();

        // By date taken → "<date>.jpg".
        let renamed = rename_one(jpg.to_str().unwrap(), RenamePattern::DateTaken).unwrap();
        assert!(renamed, "file with EXIF date should be renamed");
        assert!(dir.join("2023-05-01 14.30.09.jpg").exists(), "renamed to the capture date");
        assert!(!jpg.exists(), "original name is gone");

        // By camera + date on that same file → "<camera> <date>.jpg".
        let cur = dir.join("2023-05-01 14.30.09.jpg");
        rename_one(cur.to_str().unwrap(), RenamePattern::CameraDate).unwrap();
        assert!(dir.join("TestCam 2023-05-01 14.30.09.jpg").exists(), "renamed with camera prefix");

        // A file with no EXIF date is left untouched.
        let plain = dir.join("screenshot.png");
        image::DynamicImage::ImageRgb8(image::RgbImage::new(8, 8)).save(&plain).unwrap();
        assert!(!rename_one(plain.to_str().unwrap(), RenamePattern::DateTaken).unwrap(), "no date → skip");
        assert!(plain.exists(), "no-EXIF file keeps its name");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Build a baseline JPEG with a minimal little-endian EXIF APP1 holding IFD0
    /// `Model` (0x0110) and `DateTime` (0x0132). Mirrors the splice the strip test
    /// uses; just enough for `read_capture` to find both fields.
    fn jpeg_with_exif(model: &str, datetime: &str) -> Vec<u8> {
        let mut base = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(16, 12, image::Rgb([40, 90, 160])))
            .write_to(&mut std::io::Cursor::new(&mut base), image::ImageFormat::Jpeg)
            .unwrap();

        // ASCII values are NUL-terminated.
        let model_v: Vec<u8> = model.bytes().chain(std::iter::once(0)).collect();
        let dt_v: Vec<u8> = datetime.bytes().chain(std::iter::once(0)).collect();

        // IFD0 = count(2) + 2*entry(12) + next(4) = 30 bytes, starting at TIFF
        // offset 8 → data area begins at 38.
        let model_off: u32 = 38;
        let dt_off: u32 = model_off + model_v.len() as u32;

        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"II"); // little-endian
        tiff.extend_from_slice(&0x002Au16.to_le_bytes());
        tiff.extend_from_slice(&8u32.to_le_bytes()); // IFD0 at offset 8
        tiff.extend_from_slice(&2u16.to_le_bytes()); // 2 entries
        let entry = |tag: u16, count: u32, off: u32, t: &mut Vec<u8>| {
            t.extend_from_slice(&tag.to_le_bytes());
            t.extend_from_slice(&2u16.to_le_bytes()); // type ASCII
            t.extend_from_slice(&count.to_le_bytes());
            t.extend_from_slice(&off.to_le_bytes());
        };
        entry(0x0110, model_v.len() as u32, model_off, &mut tiff); // Model
        entry(0x0132, dt_v.len() as u32, dt_off, &mut tiff); // DateTime
        tiff.extend_from_slice(&0u32.to_le_bytes()); // no next IFD
        tiff.extend_from_slice(&model_v);
        tiff.extend_from_slice(&dt_v);

        let mut app1 = Vec::new();
        app1.extend_from_slice(b"Exif\0\0");
        app1.extend_from_slice(&tiff);
        let seg_len = (app1.len() + 2) as u16; // +2 for the length field itself

        let mut out = Vec::new();
        out.extend_from_slice(&base[0..2]); // SOI
        out.extend_from_slice(&[0xFF, 0xE1]); // APP1
        out.extend_from_slice(&seg_len.to_be_bytes());
        out.extend_from_slice(&app1);
        out.extend_from_slice(&base[2..]);
        out
    }

    #[test]
    fn quick_items_match_leaf_indices() {
        // Each quick item's reported index + its leaves must line up exactly with
        // the same verbs in the global `leaves()` list — otherwise a top-level quick
        // item would dispatch to the wrong action.
        let leaves = leaves();
        let items = quick_items();
        assert!(!items.is_empty(), "expected some quick items");
        // Convert… (a leaf) must be present now, not just groups.
        assert!(items.iter().any(|i| matches!(i, QuickItem::Leaf("menu_convert_dialog", _))));
        fn check(children: &[MenuItem], leaves: &[(&str, VerbAction)], i: &mut usize) {
            for c in children {
                match c {
                    MenuItem::Group(_, sub) => check(sub, leaves, i),
                    MenuItem::Verb(t, _) => {
                        assert_eq!(leaves[*i].0, *t, "quick leaf {i} should be {t}");
                        *i += 1;
                    }
                    MenuItem::Separator => {}
                }
            }
        }
        for item in items {
            match item {
                QuickItem::Group(title, children, start) => {
                    assert!(QUICK_KEYS.contains(&title));
                    let mut i = start as usize;
                    check(children, &leaves, &mut i);
                }
                QuickItem::Leaf(title, idx) => {
                    assert!(QUICK_KEYS.contains(&title));
                    assert_eq!(leaves[idx as usize].0, title, "quick leaf id must map to the verb");
                }
            }
        }
    }

    #[test]
    fn separators_dont_shift_leaf_indices() {
        // `leaves()` (which drives classic command-id dispatch) must skip
        // separators entirely, so adding dividers never renumbers a verb.
        fn count_verbs(items: &[MenuItem]) -> usize {
            items
                .iter()
                .map(|it| match it {
                    MenuItem::Group(_, c) => count_verbs(c),
                    MenuItem::Verb(..) => 1,
                    MenuItem::Separator => 0,
                })
                .sum()
        }
        assert_eq!(leaves().len(), count_verbs(MENU), "separators must not become leaves");
        assert!(MENU.iter().any(|it| matches!(it, MenuItem::Separator)), "menu should be grouped now");
        assert!(leaves().iter().all(|(t, _)| !t.is_empty()), "no blank leaf titles");
    }

    #[test]
    fn converts_to_native_pnm() {
        let dir = std::env::temp_dir().join("st2k_pnm");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("s.png");
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(20, 16, image::Rgb([180, 90, 40])))
            .save(&png)
            .unwrap();
        let opts = ConvertOpts {
            target: Target { format: ImageFormat::Pnm, ext: "ppm", webp_quality: None },
            jpeg_quality: 90,
            png_level: 6,
            webp_quality: None,
            resize: Resize::None,
        };
        let out = convert_file_opts(png.to_str().unwrap(), opts, &dir).expect("PNM should encode");
        assert!(out.exists() && image::open(&out).is_ok(), "PPM should reopen");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Needs ImageMagick (bundled on a full install, or on PATH). Run explicitly:
    //   cargo test --release -- --ignored converts_psd_via_magick
    #[test]
    #[ignore]
    fn converts_psd_via_magick() {
        if !crate::decode::magick_available() {
            return;
        }
        let dir = std::env::temp_dir().join("st2k_magenc");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("s.png");
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(40, 30, image::Rgb([30, 160, 90])))
            .save(&png)
            .unwrap();
        for ext in ["psd", "dds", "pcx", "jp2", "sgi"] {
            let out = dir.join(format!("o.{ext}"));
            convert_to_magick(png.to_str().unwrap(), &out, Resize::None, None)
                .unwrap_or_else(|e| panic!("magick {ext} failed: {e:?}"));
            assert!(out.exists() && std::fs::metadata(&out).unwrap().len() > 0, "{ext} should be written");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tag_rename_base_formats() {
        use crate::strip::AudioTags;
        let full = AudioTags {
            artist: Some("Daft Punk".into()),
            album: Some("Discovery".into()),
            title: Some("Aerodynamic".into()),
            track: Some(3),
            ..Default::default()
        };
        assert_eq!(tag_base(RenamePattern::ArtistTitle, &full).as_deref(), Some("Daft Punk - Aerodynamic"));
        assert_eq!(tag_base(RenamePattern::TrackTitle, &full).as_deref(), Some("03 - Aerodynamic")); // zero-padded

        // Missing artist/track → just the title; missing title → skip entirely.
        let title_only = AudioTags { title: Some("Untitled".into()), ..Default::default() };
        assert_eq!(tag_base(RenamePattern::ArtistTitle, &title_only).as_deref(), Some("Untitled"));
        assert_eq!(tag_base(RenamePattern::TrackTitle, &title_only).as_deref(), Some("Untitled"));
        assert_eq!(tag_base(RenamePattern::ArtistTitle, &AudioTags::default()), None);
    }

    #[test]
    fn read_audio_tags_roundtrips_via_lofty() {
        use lofty::config::WriteOptions;
        use lofty::tag::{Accessor, Tag, TagExt, TagType};

        let dir = std::env::temp_dir().join("st2k_tags");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("song.wav");
        std::fs::write(&wav, minimal_wav()).unwrap();

        // Write a RIFF INFO tag, then read it back through our reader.
        let mut tag = Tag::new(TagType::RiffInfo);
        tag.set_artist("The Artist".to_string());
        tag.set_title("The Song".to_string());
        tag.save_to_path(&wav, WriteOptions::default()).unwrap();

        let t = crate::strip::read_audio_tags(wav.to_str().unwrap());
        assert_eq!(t.artist.as_deref(), Some("The Artist"));
        assert_eq!(t.title.as_deref(), Some("The Song"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn combine_to_cbz_zips_pages_in_natural_order() {
        let dir = std::env::temp_dir().join("st2k_cbz");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Out-of-order names: natural sort must put 2 before 10.
        let names = ["10.png", "2.png", "1.png"];
        let mut paths = Vec::new();
        for n in names {
            let p = dir.join(n);
            image::DynamicImage::ImageRgba8(image::RgbaImage::new(8, 8)).save(&p).unwrap();
            paths.push(p.to_str().unwrap().to_string());
        }

        let out = combined_path(&paths[0], "cbz");
        combine_to_cbz(&paths, &out).unwrap();
        assert!(out.exists() && out.extension().unwrap() == "cbz");

        // Reopen the archive: 3 entries, in 1 → 2 → 10 page order.
        let f = std::fs::File::open(&out).unwrap();
        let mut zip = zip::ZipArchive::new(f).unwrap();
        assert_eq!(zip.len(), 3);
        let order: Vec<String> = (0..zip.len()).map(|i| zip.by_index(i).unwrap().name().to_string()).collect();
        assert_eq!(order, vec!["001_1.png", "002_2.png", "003_10.png"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn files_to_folder_creates_and_moves() {
        let dir = std::env::temp_dir().join("st2k_f2f");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut paths = Vec::new();
        for n in ["a.txt", "b.txt", "c.bin"] {
            let p = dir.join(n);
            std::fs::write(&p, b"x").unwrap();
            paths.push(p.to_str().unwrap().to_string());
        }

        let folder = files_to_folder(&paths, "My Group").unwrap();
        assert_eq!(folder, dir.join("My Group"));
        assert!(folder.join("a.txt").exists() && folder.join("b.txt").exists() && folder.join("c.bin").exists());
        // Originals moved out of the parent.
        assert!(!dir.join("a.txt").exists());

        // A second call with the same name makes a *fresh* folder, never merges.
        let p2 = dir.join("d.txt");
        std::fs::write(&p2, b"y").unwrap();
        let folder2 = files_to_folder(&[p2.to_str().unwrap().to_string()], "My Group").unwrap();
        assert_eq!(folder2, dir.join("My Group (2)"));
        // Illegal filename chars in the name are sanitized.
        let p3 = dir.join("e.txt");
        std::fs::write(&p3, b"z").unwrap();
        let folder3 = files_to_folder(&[p3.to_str().unwrap().to_string()], "a/b:c").unwrap();
        assert_eq!(folder3, dir.join("a-b-c"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sort_by_dimensions_buckets_by_size() {
        let dir = std::env::temp_dir().join("st2k_dims");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut paths = Vec::new();
        for (n, w, h) in [("a.png", 100, 100), ("b.png", 100, 100), ("c.png", 64, 48)] {
            let p = dir.join(n);
            image::DynamicImage::ImageRgba8(image::RgbaImage::new(w, h)).save(&p).unwrap();
            paths.push(p.to_str().unwrap().to_string());
        }

        let (moved, skipped) = sort_by_dimensions(&paths);
        assert_eq!((moved, skipped), (3, 0));
        assert!(dir.join("100x100").join("a.png").exists());
        assert!(dir.join("100x100").join("b.png").exists());
        assert!(dir.join("64x48").join("c.png").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn expands_tag_template() {
        use crate::strip::AudioTags;
        let t = AudioTags {
            artist: Some("A".into()),
            album: Some("B".into()),
            title: Some("T".into()),
            track: Some(5),
            ..Default::default()
        };
        assert_eq!(expand_template("$artist - $album", &t, "X"), "A - B");
        assert_eq!(expand_template("$track $title", &t, "X"), "05 T"); // track zero-padded
        // A missing tag is replaced by the fallback text.
        assert_eq!(expand_template("$artist", &AudioTags::default(), "Unknown"), "Unknown");
    }

    #[test]
    fn tags_to_folders_moves_and_copies_by_template() {
        let dir = std::env::temp_dir().join("st2k_ttf");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.wav");
        let b = dir.join("b.wav");
        tagged_wav(&a, "Alpha");
        tagged_wav(&b, "Beta");
        let dest = dir.join("sorted");

        // Move: two artists → two folders, originals gone.
        let files = vec![a.to_str().unwrap().to_string(), b.to_str().unwrap().to_string()];
        let (done, skipped) = tags_to_folders(&files, &dest, "$artist", "Unknown", true);
        assert_eq!((done, skipped), (2, 0));
        assert!(dest.join("Alpha").join("a.wav").exists());
        assert!(dest.join("Beta").join("b.wav").exists());
        assert!(!a.exists(), "move should remove the original");

        // Copy: original stays put.
        let c = dir.join("c.wav");
        tagged_wav(&c, "Gamma");
        let (done2, _) = tags_to_folders(&[c.to_str().unwrap().to_string()], &dest, "$artist", "Unknown", false);
        assert_eq!(done2, 1);
        assert!(c.exists(), "copy should keep the original");
        assert!(dest.join("Gamma").join("c.wav").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Write a minimal WAV and stamp a RIFF INFO artist + title tag on it.
    fn tagged_wav(path: &std::path::Path, artist: &str) {
        use lofty::config::WriteOptions;
        use lofty::tag::{Accessor, Tag, TagExt, TagType};
        std::fs::write(path, minimal_wav()).unwrap();
        let mut tag = Tag::new(TagType::RiffInfo);
        tag.set_artist(artist.to_string());
        tag.set_title("Song".to_string());
        tag.save_to_path(path, WriteOptions::default()).unwrap();
    }

    /// A tiny but valid 16-bit PCM mono WAV so `lofty` accepts it for tag writing.
    fn minimal_wav() -> Vec<u8> {
        let (rate, channels, bits) = (8000u32, 1u16, 16u16);
        let data: Vec<u8> = (0..32u16).flat_map(|i| ((i as i16) * 500).to_le_bytes()).collect();
        let byte_rate = rate * channels as u32 * (bits / 8) as u32;
        let block_align = channels * (bits / 8);
        let mut w = Vec::new();
        w.extend_from_slice(b"RIFF");
        w.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        w.extend_from_slice(b"WAVE");
        w.extend_from_slice(b"fmt ");
        w.extend_from_slice(&16u32.to_le_bytes());
        w.extend_from_slice(&1u16.to_le_bytes()); // PCM
        w.extend_from_slice(&channels.to_le_bytes());
        w.extend_from_slice(&rate.to_le_bytes());
        w.extend_from_slice(&byte_rate.to_le_bytes());
        w.extend_from_slice(&block_align.to_le_bytes());
        w.extend_from_slice(&bits.to_le_bytes());
        w.extend_from_slice(b"data");
        w.extend_from_slice(&(data.len() as u32).to_le_bytes());
        w.extend_from_slice(&data);
        w
    }

    #[test]
    fn prepares_wallpaper_image() {
        let dir = std::env::temp_dir().join("st2k_wp_prep");
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("w.png");
        let mut img = image::RgbaImage::new(40, 30);
        for p in img.pixels_mut() {
            *p = image::Rgba([10, 120, 220, 255]);
        }
        image::DynamicImage::ImageRgba8(img)
            .save_with_format(&src, ImageFormat::Png)
            .unwrap();

        // Write into the temp dir, NOT the real %APPDATA% (which would leave a
        // stale wallpaper.png in the user's profile and could clobber a
        // wallpaper they actually set via the verb).
        let out = prepare_wallpaper_in(&dir, src.to_str().unwrap()).unwrap();
        assert!(out.exists(), "wallpaper image should be written");
        assert_eq!(out, dir.join("wallpaper.png"));
        let d = image::open(&out).unwrap();
        assert_eq!((d.width(), d.height()), (40, 30));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Disruptive (changes the live desktop wallpaper, then restores it), so
    // `#[ignore]`d — run explicitly:
    //   cargo test --release -- --ignored sets_and_restores_wallpaper
    #[test]
    #[ignore]
    fn sets_and_restores_wallpaper() {
        use windows::Win32::UI::WindowsAndMessaging::{
            SPI_GETDESKWALLPAPER, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
        };
        unsafe fn current_wallpaper() -> String {
            let mut buf = [0u16; 520];
            let _ = SystemParametersInfoW(
                SPI_GETDESKWALLPAPER,
                buf.len() as u32,
                Some(buf.as_mut_ptr() as *mut c_void),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            );
            let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            String::from_utf16_lossy(&buf[..end])
        }
        unsafe {
            let original = current_wallpaper();

            let dir = std::env::temp_dir().join("st2k_wp_rt");
            std::fs::create_dir_all(&dir).unwrap();
            let src = dir.join("rt.png");
            let mut img = image::RgbaImage::new(32, 32);
            for p in img.pixels_mut() {
                *p = image::Rgba([200, 40, 40, 255]);
            }
            image::DynamicImage::ImageRgba8(img)
                .save_with_format(&src, ImageFormat::Png)
                .unwrap();

            set_wallpaper(src.to_str().unwrap(), WallpaperMode::Stretch).unwrap();
            let now = current_wallpaper().to_lowercase();
            assert!(
                now.contains("sagethumbs2k") && now.ends_with("wallpaper.png"),
                "wallpaper should now be ours, got '{now}'"
            );

            // Restore the user's original wallpaper.
            if !original.is_empty() {
                let wide: Vec<u16> = std::ffi::OsStr::new(&original)
                    .encode_wide()
                    .chain(once(0))
                    .collect();
                let _ = SystemParametersInfoW(
                    SPI_SETDESKWALLPAPER,
                    0,
                    Some(wide.as_ptr() as *mut c_void),
                    SPIF_UPDATEINIFILE | SPIF_SENDCHANGE,
                );
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
