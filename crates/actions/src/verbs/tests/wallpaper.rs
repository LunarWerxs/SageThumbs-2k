#![cfg(test)]

//! Set as wallpaper, and putting the old one back.

use super::*;

#[test]
fn prepares_wallpaper_image() {
    let dir = std::env::temp_dir().join(format!("st2k_wp_prep_{}", std::process::id()));
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

// Run explicitly: cargo test --release -- --ignored sets_and_restores_wallpaper
#[test]
#[ignore = "changes the live desktop wallpaper (then restores it); run with --ignored"]
pub(super) fn sets_and_restores_wallpaper() {
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

        let dir = std::env::temp_dir().join(format!("st2k_wp_rt_{}", std::process::id()));
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
