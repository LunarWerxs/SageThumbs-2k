//! End-to-end probe: ask the shell to extract a thumbnail for a fresh `.qoi`
//! (a format Windows can't thumbnail) and check our handler ran.
//!
//! NOTE: thumbnails are now registered CLASSICALLY (regsvr32/HKLM via
//! `register.rs`), not via the package — the packaged-thumbnail path could not
//! be confirmed through this probe. So run this
//! after an *elevated* `regsvr32 <release>\sagethumbs2k.dll`; it then verifies
//! the classic thumbnail handler loads. Kept as the headless shell-integration
//! oracle for the (admin-gated) thumbnail confirmation.
//!
//! `#[ignore]`d because it requires the handler registered first and mutates
//! shell state. Orchestrated externally:
//!   1. register-dev.ps1 (Add-AppxPackage -Register … -ExternalLocation release)
//!   2. cargo test --release --test packaged_load -- --ignored --nocapture
//!   3. unregister
//!
//! It asks the shell to extract a thumbnail for a fresh `.qoi` — a format
//! Windows has no built-in thumbnailer for, and (unlike `.tga`, which a
//! Photoshop UserChoice association can shadow) nothing else on this machine
//! claims — with SIIGBF_THUMBNAILONLY (no icon fallback). Success ⟹ our
//! IThumbnailProvider was loaded and ran.
//!
//! HONEST-FAILURE NOTE: this test has no "precondition absent → silent skip"
//! path. If the handler isn't registered, `GetImage(THUMBNAILONLY)` returns an
//! error and we PANIC (with the HRESULT) rather than returning quietly — so a
//! missing registration can never masquerade as a pass. The `#[ignore]` is what
//! keeps it out of the default run; once you opt in with `--ignored`, it must
//! genuinely pass.
#![cfg(windows)]

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;

use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::SIZE;
use windows::Win32::Graphics::Gdi::{DeleteObject, GetObjectW, BITMAP};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::Shell::{
    SHCreateItemFromParsingName, IShellItem, IShellItemImageFactory, SIIGBF_THUMBNAILONLY,
};

#[test]
#[ignore]
fn packaged_thumbnail_handler_loads_in_shell() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        // Fresh, uniquely-named .qoi — a format no other app on this machine
        // claims (unlike .tga, which Photoshop's UserChoice association shadows),
        // so the shell resolves to our handler. Unique name dodges the cache.
        let dir = std::env::temp_dir().join("st2k_pkgload");
        std::fs::create_dir_all(&dir).unwrap();
        let probe = dir.join(format!("probe_{}.qoi", std::process::id()));
        let mut img = image::RgbaImage::new(64, 48);
        for p in img.pixels_mut() {
            *p = image::Rgba([20, 40, 220, 255]); // distinctive blue
        }
        image::DynamicImage::ImageRgba8(img)
            .save_with_format(&probe, image::ImageFormat::Qoi)
            .unwrap();

        let wide: Vec<u16> = probe.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let item: IShellItem =
            SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None).expect("shell item");
        let factory: IShellItemImageFactory = item.cast().expect("IShellItemImageFactory");

        let size = SIZE { cx: 96, cy: 96 };
        // THUMBNAILONLY: fail rather than fall back to a generic icon — so a
        // success specifically means a thumbnail provider produced an image.
        let result = factory.GetImage(size, SIIGBF_THUMBNAILONLY);

        match result {
            Ok(hbmp) => {
                let mut bm = BITMAP::default();
                GetObjectW(
                    hbmp.into(),
                    std::mem::size_of::<BITMAP>() as i32,
                    Some(&mut bm as *mut _ as *mut c_void),
                );
                println!(
                    "PACKAGED-LOAD OK: got {}x{} {}bpp thumbnail for .qoi",
                    bm.bmWidth, bm.bmHeight, bm.bmBitsPixel
                );
                let _ = DeleteObject(hbmp.into());
                assert!(bm.bmWidth > 0 && bm.bmHeight > 0);
            }
            Err(e) => {
                let _ = std::fs::remove_dir_all(&dir);
                panic!("PACKAGED-LOAD FAILED: GetImage(THUMBNAILONLY) -> {e:?} (handler did not load)");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
