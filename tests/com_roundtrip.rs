//! End-to-end COM test that drives the built DLL exactly the way Explorer
//! does — no registration, no admin, no Explorer:
//!
//!   LoadLibrary(DLL) -> DllGetClassObject -> IClassFactory::CreateInstance
//!   -> QI IInitializeWithStream -> Initialize(IStream) -> QI IThumbnailProvider
//!   -> GetThumbnail -> read back the HBITMAP (size, top-down, colors, alpha).
//!
//! This is the automated proof that the shell handshake + DIB output are
//! correct, which compile checks and decode-only unit tests can't give.
//!
//! IMPORTANT: run via `scripts/test.ps1` (or `cargo build` before `cargo test`).
//! Plain `cargo test` does NOT refresh target/<profile>/sagethumbs2k.dll, so the
//! LoadLibrary below could otherwise pick up a stale cdylib.
#![cfg(windows)]

mod common;

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;

use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
use windows::core::{s, Error, Interface, Result, GUID, HRESULT, PCWSTR};
use windows::Win32::Foundation::{E_FAIL, HMODULE};
use windows::Win32::Graphics::Gdi::{DeleteObject, GetObjectW, BITMAP, HBITMAP};
use windows::Win32::System::Com::{
    CoInitializeEx, IClassFactory, IStream, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::UI::Shell::PropertiesSystem::IInitializeWithStream;
use windows::Win32::UI::Shell::{
    IThumbnailProvider, SHCreateMemStream, WTSAT_ARGB, WTSAT_UNKNOWN, WTS_ALPHATYPE,
};

const CLSID_THUMBNAIL_PROVIDER: GUID = GUID::from_u128(0x7B2E6A14_9C3D_4F8A_B1E7_2A5D9F0C6E31);

type DllGetClassObjectFn =
    unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> HRESULT;

/// A returned thumbnail: width, height, tightly-packed BGRA bytes, alpha tag.
struct Thumb {
    w: usize,
    h: usize,
    bgra: Vec<u8>,
    alpha: i32,
}

impl Thumb {
    /// BGRA quad at (x, y).
    fn px(&self, x: usize, y: usize) -> [u8; 4] {
        let i = (y * self.w + x) * 4;
        [
            self.bgra[i],
            self.bgra[i + 1],
            self.bgra[i + 2],
            self.bgra[i + 3],
        ]
    }
}

/// Throwaway HKCU subkey the DLL's settings reads are redirected to, so this test measures
/// the CODE and not whatever the developer running it happens to have ticked.
///
/// It used to read the real `HKCU\Software\SageThumbs2K`, which made it quietly
/// machine-dependent: turning on "Checkerboard behind transparent thumbnails" composites the
/// tile onto an opaque backdrop, and `alpha_is_premultiplied` then fails with A=255 on a
/// machine where nothing is wrong. Same shape for `FormatBadge`. The sibling COM tests
/// (`settings_gate`, `explorer_command`, `context_menu_latency`) already redirect this way.
const TEST_SETTINGS_ROOT: &str = r"Software\SageThumbs2K-test\com_roundtrip";

/// Point settings at the throwaway root before the DLL's `OnceLock` resolves it. Must run
/// before the first settings read of the process, which is why it lives in the one helper
/// every test in this file goes through.
fn isolate_settings() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: set before any thread but this one has touched the environment, and the
        // value is only read (once) by `settings::hkcu_root`.
        unsafe { common::set_test_env("ST2K_SETTINGS_ROOT", TEST_SETTINGS_ROOT) };
    });
}

/// Serializes every test in this file.
///
/// They all share ONE settings root (the DLL resolves `ST2K_SETTINGS_ROOT` once per process),
/// so a test that flips `FormatBadge` or `ThumbChecker` is flipping it for whatever else is
/// running at that moment — which is how `alpha_is_premultiplied` started failing against a
/// checkerboard it never asked for. These tests take under a second in total, so serializing
/// them costs nothing worth having.
fn settings_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    // A poisoned lock means some other test panicked; that is its failure to report, not a
    // reason to fail this one too.
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

unsafe fn get_thumbnail(bytes: &[u8], cx: u32) -> Result<Thumb> {
    let stream: IStream = SHCreateMemStream(Some(bytes)).ok_or_else(|| Error::from(E_FAIL))?;
    get_thumbnail_from_stream(&stream, cx)
}

/// The same handshake over a caller-supplied stream, so a test can hand the DLL a stream
/// with a threading model of its choosing (see `ApartmentStream` at the end of this file).
unsafe fn get_thumbnail_from_stream(stream: &IStream, cx: u32) -> Result<Thumb> {
    isolate_settings();
    // Returns a Result (not catch_unwind) so the harness behaves identically
    // whether the DLL is built with panic=unwind (debug) or panic=abort (release).
    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

    let path = common::dll_path();
    assert!(
        path.exists(),
        "cdylib not built at {path:?} — run `cargo build` first"
    );
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let module: HMODULE = LoadLibraryW(PCWSTR(wide.as_ptr()))?;

    let proc =
        GetProcAddress(module, s!("DllGetClassObject")).ok_or_else(|| Error::from(E_FAIL))?;
    let dll_get_class_object: DllGetClassObjectFn = std::mem::transmute(proc);

    // Class factory, exactly as the shell does it.
    let mut factory_ptr: *mut c_void = std::ptr::null_mut();
    dll_get_class_object(
        &CLSID_THUMBNAIL_PROVIDER,
        &IClassFactory::IID,
        &mut factory_ptr,
    )
    .ok()?;
    assert!(!factory_ptr.is_null(), "null class factory");
    let factory = IClassFactory::from_raw(factory_ptr);

    // Create the object asking for the initializer interface.
    let init: IInitializeWithStream = factory.CreateInstance(None)?;

    // Feed the bytes as an IStream, exactly as the shell does.
    init.Initialize(stream, 0)?;

    // QI across to the thumbnail interface and ask for the bitmap.
    let provider: IThumbnailProvider = init.cast()?;
    let mut hbmp = HBITMAP::default();
    let mut alpha: WTS_ALPHATYPE = WTSAT_UNKNOWN;
    provider.GetThumbnail(cx, &mut hbmp, &mut alpha)?;
    assert!(!hbmp.is_invalid(), "GetThumbnail returned a null HBITMAP");

    // Inspect the bitmap. Must be a 32bpp DIB section (bmBits non-null).
    let mut bm = BITMAP::default();
    let n = GetObjectW(
        hbmp.into(),
        std::mem::size_of::<BITMAP>() as i32,
        Some(&mut bm as *mut _ as *mut c_void),
    );
    assert!(n != 0, "GetObjectW failed");
    assert_eq!(bm.bmBitsPixel, 32, "thumbnail must be 32bpp");
    assert!(!bm.bmBits.is_null(), "thumbnail must be a DIB section");

    let w = bm.bmWidth as usize;
    let h = bm.bmHeight as usize;
    let stride = bm.bmWidthBytes as usize;
    let src = std::slice::from_raw_parts(bm.bmBits as *const u8, stride * h);
    let mut bgra = vec![0u8; w * 4 * h];
    for y in 0..h {
        bgra[y * w * 4..(y + 1) * w * 4].copy_from_slice(&src[y * stride..y * stride + w * 4]);
    }
    let _ = DeleteObject(hbmp.into());

    Ok(Thumb {
        w,
        h,
        bgra,
        alpha: alpha.0,
    })
}

fn solid(w: u32, h: u32, rgba: [u8; 4]) -> RgbaImage {
    let mut img = RgbaImage::new(w, h);
    for p in img.pixels_mut() {
        *p = Rgba(rgba);
    }
    img
}

fn encode(img: RgbaImage, fmt: ImageFormat) -> Vec<u8> {
    let dynimg = if fmt == ImageFormat::Jpeg {
        DynamicImage::ImageRgb8(DynamicImage::ImageRgba8(img).to_rgb8())
    } else {
        DynamicImage::ImageRgba8(img)
    };
    let mut bytes = Vec::new();
    dynimg
        .write_to(&mut std::io::Cursor::new(&mut bytes), fmt)
        .unwrap();
    bytes
}

#[test]
fn png_fits_box_preserves_aspect_and_color() {
    let _settings = settings_lock();
    let png = encode(solid(200, 100, [255, 0, 0, 255]), ImageFormat::Png);
    let t = unsafe { get_thumbnail(&png, 96) }.unwrap();
    assert_eq!((t.w, t.h), (96, 48), "200x100 should fit 96-box as 96x48");
    assert_eq!(t.alpha, WTSAT_ARGB.0, "should report premultiplied ARGB");
    let [b, g, r, a] = t.px(0, 0);
    assert!(
        r > 200 && g < 60 && b < 60 && a == 255,
        "expected red, got BGRA {:?}",
        [b, g, r, a]
    );
}

#[test]
fn dib_is_top_down() {
    let _settings = settings_lock();
    // Red top half, blue bottom half. A top-down DIB keeps red at row 0.
    let mut img = RgbaImage::new(200, 100);
    for y in 0..100u32 {
        for x in 0..200u32 {
            let c = if y < 50 {
                [255, 0, 0, 255]
            } else {
                [0, 0, 255, 255]
            };
            img.put_pixel(x, y, Rgba(c));
        }
    }
    let png = encode(img, ImageFormat::Png);
    let t = unsafe { get_thumbnail(&png, 64) }.unwrap();
    let top = t.px(0, 0);
    let bottom = t.px(0, t.h - 1);
    assert!(
        top[2] > 180 && top[0] < 70,
        "top row should be red, got BGRA {top:?}"
    );
    assert!(
        bottom[0] > 180 && bottom[2] < 70,
        "bottom row should be blue, got BGRA {bottom:?}"
    );
}

#[test]
fn alpha_is_premultiplied() {
    let _settings = settings_lock();
    // Straight R=200, A=128 -> premultiplied R ≈ 200*128/255 ≈ 100.
    let png = encode(solid(80, 80, [200, 0, 0, 128]), ImageFormat::Png);
    let t = unsafe { get_thumbnail(&png, 64) }.unwrap();
    assert_eq!(t.alpha, WTSAT_ARGB.0);
    let [b, _g, r, a] = t.px(0, 0);
    assert_eq!(a, 128, "alpha preserved");
    assert!(
        (r as i32 - 100).abs() < 20,
        "R should be premultiplied ~100, got {r}"
    );
    assert!(b < 20, "blue ~0, got {b}");
}

#[test]
fn jpeg_also_decodes_through_com() {
    let _settings = settings_lock();
    let jpg = encode(solid(120, 90, [0, 200, 0, 255]), ImageFormat::Jpeg);
    let t = unsafe { get_thumbnail(&jpg, 96) }.unwrap();
    assert_eq!((t.w, t.h), (96, 72), "120x90 should fit 96-box as 96x72");
    assert!(t.px(0, 0)[1] > 150, "green channel should dominate");
}

/// A synthetic opaque RGB PSD: header + empty color-mode data + one 1036
/// image-resource block holding a red JPEG thumbnail + `tail` zero bytes
/// of "layer data" (mirrors `container::psd::testutil`, which an external test
/// crate can't reach).
///
/// The preview is **160 px, the size Photoshop actually writes**, and that is now
/// load-bearing rather than cosmetic. Since issue #33 the head-preview fast path only
/// commits to the prefix when the baked preview can serve the request, so the 4 px
/// thumbnail this used to carry would send a 96 px tile off to render the composite —
/// and this test would then be exercising the opposite of the path in its name.
fn synthetic_psd_with_thumb(tail: usize) -> Vec<u8> {
    let jpeg = encode(solid(160, 160, [200, 50, 50, 255]), ImageFormat::Jpeg);
    let mut data = Vec::new();
    data.extend_from_slice(&1u32.to_be_bytes()); // format = JPEG
    data.extend_from_slice(&[0u8; 20]); // w/h/widthbytes/totalsize/sizeafter
    data.extend_from_slice(&[0, 24]); // bits/pixel
    data.extend_from_slice(&[0, 1]); // planes
    data.extend_from_slice(&jpeg);

    let mut res = Vec::new();
    res.extend_from_slice(b"8BIM");
    res.extend_from_slice(&1036u16.to_be_bytes());
    res.extend_from_slice(&[0, 0]); // empty Pascal name + pad
    res.extend_from_slice(&(data.len() as u32).to_be_bytes());
    res.extend_from_slice(&data);
    if data.len() & 1 == 1 {
        res.push(0);
    }

    let mut psd = Vec::new();
    psd.extend_from_slice(b"8BPS");
    psd.extend_from_slice(&[0, 1]); // version 1 (PSD)
    psd.extend_from_slice(&[0u8; 6]); // reserved
    psd.extend_from_slice(&[0, 3]); // channels (opaque RGB)
    psd.extend_from_slice(&100u32.to_be_bytes()); // height
    psd.extend_from_slice(&100u32.to_be_bytes()); // width
    psd.extend_from_slice(&[0, 8]); // depth
    psd.extend_from_slice(&[0, 3]); // color mode = RGB
    psd.extend_from_slice(&0u32.to_be_bytes()); // color-mode data length
    psd.extend_from_slice(&(res.len() as u32).to_be_bytes()); // resources length
    psd.extend_from_slice(&res);
    psd.extend_from_slice(&vec![0u8; tail]);
    psd
}

#[test]
fn big_psd_thumbnails_through_com_via_the_head_prefix() {
    let _settings = settings_lock();
    // 8 MB of layer data behind the baked thumbnail: end-to-end through the
    // real DLL, the head-preview fast path must still produce the red JPEG
    // thumbnail (the streamsrc unit tests assert the byte-level prefix; this
    // proves the full shell handshake on the same shape).
    let psd = synthetic_psd_with_thumb(8 << 20);
    let t = unsafe { get_thumbnail(&psd, 96) }.unwrap();
    assert!(t.w > 0 && t.h > 0);
    let [b, g, r, _a] = t.px(t.w / 2, t.h / 2);
    assert!(
        r > 150 && g < 110 && b < 110,
        "expected the red baked thumbnail, got BGRA {:?}",
        [b, g, r]
    );
}

/// A PSD whose baked preview and whose merged composite are DIFFERENT COLOURS, so which one
/// a thumbnail came from is unambiguous from a single pixel.
///
/// Preview: red stripes. Merged image data: green stripes. Both striped rather than flat, so
/// the answer never turns on the blank-composite tie-break — this test is about the SIZE
/// decision and nothing else. 64x64 with a 64 px preview, which serves a 256 px request
/// exactly (`MAX_UPSCALE_FACTOR`) and cannot serve 1024.
fn psd_with_distinct_preview_and_composite() -> Vec<u8> {
    let mut prev = RgbaImage::new(64, 64);
    for (_, y, p) in prev.enumerate_pixels_mut() {
        *p = if (y / 4) % 2 == 0 {
            Rgba([200, 50, 50, 255])
        } else {
            Rgba([60, 10, 10, 255])
        };
    }
    let jpeg = encode(prev, ImageFormat::Jpeg);

    let mut data = Vec::new();
    data.extend_from_slice(&1u32.to_be_bytes()); // format = JPEG
    data.extend_from_slice(&64u32.to_be_bytes()); // width
    data.extend_from_slice(&64u32.to_be_bytes()); // height
    data.extend_from_slice(&(64u32 * 3).to_be_bytes()); // widthbytes
    data.extend_from_slice(&(64u32 * 3 * 64).to_be_bytes()); // totalsize
    data.extend_from_slice(&(jpeg.len() as u32).to_be_bytes()); // sizeafter
    data.extend_from_slice(&[0, 24]); // bits/pixel
    data.extend_from_slice(&[0, 1]); // planes
    data.extend_from_slice(&jpeg);

    let mut res = Vec::new();
    res.extend_from_slice(b"8BIM");
    res.extend_from_slice(&1036u16.to_be_bytes());
    res.extend_from_slice(&[0, 0]);
    res.extend_from_slice(&(data.len() as u32).to_be_bytes());
    res.extend_from_slice(&data);
    if data.len() & 1 == 1 {
        res.push(0);
    }

    let mut psd = Vec::new();
    psd.extend_from_slice(b"8BPS");
    psd.extend_from_slice(&[0, 1]);
    psd.extend_from_slice(&[0u8; 6]);
    psd.extend_from_slice(&[0, 3]); // channels: opaque RGB
    psd.extend_from_slice(&64u32.to_be_bytes()); // height
    psd.extend_from_slice(&64u32.to_be_bytes()); // width
    psd.extend_from_slice(&[0, 8]); // depth
    psd.extend_from_slice(&[0, 3]); // colour mode = RGB
    psd.extend_from_slice(&0u32.to_be_bytes()); // colour-mode data
    psd.extend_from_slice(&(res.len() as u32).to_be_bytes());
    psd.extend_from_slice(&res);
    psd.extend_from_slice(&0u32.to_be_bytes()); // layer + mask info: empty
    psd.extend_from_slice(&[0, 0]); // image-data compression = raw
                                    // Planar: all R, then all G, then all B — GREEN stripes.
    for channel in [[40u8, 10], [200, 60], [80, 20]] {
        for y in 0..64u32 {
            let v = if (y / 4) % 2 == 0 {
                channel[0]
            } else {
                channel[1]
            };
            psd.extend_from_slice(&[v; 64]);
        }
    }
    psd
}

/// **Issue #33, end to end through the real DLL, at the surface that reported it.**
///
/// The Explorer preview pane asked `GetThumbnail` for 2048 px and got Photoshop's 160 px
/// baked thumbnail, every time, however long it waited. The fix makes the size of the request
/// decide which source answers it — so on one file, changing only `cx`, the colour of the
/// returned tile must change too.
///
/// Driven through `LoadLibrary` -> `IInitializeWithStream` -> `IThumbnailProvider` like every
/// other test here, because the bug lived in the handshake between the stream cascade (which
/// decides what bytes exist) and the decoder (which decides what to do with them). A
/// decode-only test could not have caught it: the cascade had already thrown the composite
/// away before the decoder was asked.
#[test]
fn a_large_request_gets_the_psd_composite_not_the_baked_preview() {
    let _settings = settings_lock();
    if !sagethumbs2k_core::decode::magick_available() {
        // Loud, because a skip that reads as a pass is worse than no test at all.
        eprintln!(
            "SKIPPED a_large_request_gets_the_psd_composite_not_the_baked_preview: no ImageMagick"
        );
        return;
    }
    let psd = psd_with_distinct_preview_and_composite();

    // Small enough for the baked preview to serve: the fast path stands, and the tile is RED.
    let t = unsafe { get_thumbnail(&psd, 256) }.unwrap();
    let [b, g, r, _] = t.px(t.w / 2, 2);
    assert!(
        r > g && r > b,
        "a 256 px tile must still come from the baked preview (red), got BGRA {:?}",
        [b, g, r]
    );

    // Too large for it: the composite must be reached, and the tile is GREEN.
    let t = unsafe { get_thumbnail(&psd, 1024) }.unwrap();
    let [b, g, r, _] = t.px(t.w / 2, 2);
    assert!(
        g > r && g > b,
        "a 1024 px request must render the merged composite (green), got BGRA {:?}",
        [b, g, r]
    );
}

#[test]
fn garbage_returns_error_not_crash() {
    let _settings = settings_lock();
    // GetThumbnail should return a failure HRESULT (not crash the host) for
    // undecodable input.
    let result = unsafe { get_thumbnail(&[0, 1, 2, 3, 4, 5, 6, 7], 96) };
    assert!(
        result.is_err(),
        "garbage input should yield a failed GetThumbnail"
    );
}

/// The optional format badge (`FormatBadge`, off by default) must reach a REAL thumbnail
/// through the shell's own COM path. The unit tests in `badge.rs` prove the drawing; this
/// proves the WIRING — that the provider recovers the file's extension from the stream the
/// shell marshals in, and stamps the finished tile.
///
/// Uses a FILE-backed stream on purpose. The badge needs a file name, and a memory stream
/// has none (`IStream::Stat` leaves `pwcsName` null), so the whole feature is invisible to
/// the `SHCreateMemStream` helper the other tests use — a test built on that would have
/// passed while proving nothing. Explorer always hands us a named, file-backed stream.
///
/// Restores the previous registry value on the way out, including if the assertions fail,
/// so a test run cannot leave badges silently switched on for the developer.
#[test]
fn format_badge_stamps_a_real_thumbnail_when_enabled() {
    let _settings = settings_lock();
    use windows::Win32::System::Com::{STGM_READ, STGM_SHARE_DENY_NONE};
    use windows::Win32::UI::Shell::SHCreateStreamOnFileEx;
    use windows_registry::CURRENT_USER;

    let corpus = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../test-corpus/sample.png");
    if !corpus.exists() {
        eprintln!("skipping: no ../test-corpus/sample.png");
        return;
    }

    unsafe fn tile(path: &std::path::Path, cx: u32) -> Vec<u8> {
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let stream: IStream = SHCreateStreamOnFileEx(
            PCWSTR(wide.as_ptr()),
            STGM_READ.0 | STGM_SHARE_DENY_NONE.0,
            0,
            false,
            None,
        )
        .expect("file stream");

        let path_dll = common::dll_path();
        let wide_dll: Vec<u16> = path_dll
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let module: HMODULE = LoadLibraryW(PCWSTR(wide_dll.as_ptr())).expect("LoadLibrary");
        let proc = GetProcAddress(module, s!("DllGetClassObject")).expect("DllGetClassObject");
        let dll_get_class_object: DllGetClassObjectFn = std::mem::transmute(proc);
        let mut factory_ptr: *mut c_void = std::ptr::null_mut();
        dll_get_class_object(
            &CLSID_THUMBNAIL_PROVIDER,
            &IClassFactory::IID,
            &mut factory_ptr,
        )
        .ok()
        .expect("class object");
        let factory = IClassFactory::from_raw(factory_ptr);
        let init: IInitializeWithStream = factory.CreateInstance(None).expect("create");
        init.Initialize(&stream, 0).expect("Initialize");
        let provider: IThumbnailProvider = init.cast().expect("QI");
        let mut hbmp = HBITMAP::default();
        let mut alpha: WTS_ALPHATYPE = WTSAT_UNKNOWN;
        provider
            .GetThumbnail(cx, &mut hbmp, &mut alpha)
            .expect("GetThumbnail");

        let mut bm = BITMAP::default();
        GetObjectW(
            hbmp.into(),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bm as *mut _ as *mut c_void),
        );
        let (w, h, stride) = (
            bm.bmWidth as usize,
            bm.bmHeight as usize,
            bm.bmWidthBytes as usize,
        );
        let src = std::slice::from_raw_parts(bm.bmBits as *const u8, stride * h);
        let mut out = vec![0u8; w * 4 * h];
        for y in 0..h {
            out[y * w * 4..(y + 1) * w * 4].copy_from_slice(&src[y * stride..y * stride + w * 4]);
        }
        let _ = DeleteObject(hbmp.into());
        out
    }

    // The THROWAWAY root, not the developer's real one. Flipping the live key worked, but it
    // meant a test run reached into the settings of whoever ran it and put them back by hand
    // afterwards — which is only correct while nothing panics in between.
    isolate_settings();
    let key = CURRENT_USER
        .create(TEST_SETTINGS_ROOT)
        .expect("settings key");
    let _ = key.set_u32("FormatBadge", 0);
    let off = unsafe { tile(&corpus, 256) };
    let _ = key.set_u32("FormatBadge", 1);
    let on = unsafe { tile(&corpus, 256) };
    let _ = key.remove_value("FormatBadge");

    assert_eq!(
        off.len(),
        on.len(),
        "the badge must not change the tile size"
    );
    assert_ne!(
        off, on,
        "FormatBadge=1 produced a byte-identical tile - the badge never reached the provider"
    );

    // Confined to the bottom-right: the top-left eighth is the picture the user asked for.
    let w = 256usize;
    let clean =
        (0..32).all(|y| (0..32).all(|x| off[(y * w + x) * 4..][..4] == on[(y * w + x) * 4..][..4]));
    assert!(clean, "the badge bled into the top-left of the thumbnail");

    // `FormatBadgeStyle` must reach the provider as well. Text and icon draw different
    // pixels, so a knob that never arrived shows up as two identical tiles.
    let _ = key.set_u32("FormatBadge", 1);
    let _ = key.set_u32("FormatBadgeStyle", 1);
    let icon = unsafe { tile(&corpus, 256) };
    let _ = key.set_u32("FormatBadgeStyle", 0);
    let text = unsafe { tile(&corpus, 256) };
    let _ = key.remove_value("FormatBadge");
    let _ = key.remove_value("FormatBadgeStyle");
    assert_ne!(
        icon, text,
        "FormatBadgeStyle produced a byte-identical tile - the style never reached the provider"
    );
}

/// `ThumbChecker` is the other setting that rewrites the finished bitmap, and it has a
/// property worth pinning: it must make the tile OPAQUE. A half-transparent thumbnail that
/// still reports `WTSAT_ARGB` with real alpha would mean the checkerboard was drawn on top
/// of the picture rather than under it.
#[test]
fn thumb_checker_fills_transparency_and_leaves_an_opaque_tile() {
    let _settings = settings_lock();
    use windows_registry::CURRENT_USER;

    isolate_settings();
    let key = CURRENT_USER
        .create(TEST_SETTINGS_ROOT)
        .expect("settings key");

    // Half transparent, half opaque. NOT a fully transparent image: `decode/thumb.rs`
    // deliberately forces one of those opaque (DDS texture maps and render passes use alpha
    // for something other than transparency), so it would prove nothing here.
    let mut img = RgbaImage::new(80, 80);
    for y in 0..80u32 {
        for x in 0..80u32 {
            let px = if x < 40 {
                [0, 0, 0, 0]
            } else {
                [200, 0, 0, 255]
            };
            img.put_pixel(x, y, Rgba(px));
        }
    }
    let png = encode(img, ImageFormat::Png);

    let _ = key.set_u32("ThumbChecker", 0);
    let plain = unsafe { get_thumbnail(&png, 64) }.expect("thumbnail");
    assert_eq!(
        plain.px(2, 2)[3],
        0,
        "transparent stays transparent when off"
    );

    let _ = key.set_u32("ThumbChecker", 1);
    let checked = unsafe { get_thumbnail(&png, 64) }.expect("thumbnail");
    let _ = key.remove_value("ThumbChecker");

    let [b, g, r, a] = checked.px(2, 2);
    assert_eq!(a, 255, "the checkerboard must leave the tile opaque");
    assert!(
        b > 190 && g > 190 && r > 190,
        "the see-through half should now be checkerboard grey, got BGRA {:?}",
        [b, g, r, a]
    );
    // ...and the opaque half must be untouched by it.
    let [rb, rg, rr, ra] = checked.px(checked.w - 3, 2);
    assert_eq!(ra, 255);
    assert!(
        rr > 180 && rg < 60 && rb < 60,
        "the opaque half must still be the picture, got BGRA {:?}",
        [rb, rg, rr, ra]
    );
}

// ---------------------------------------------------------------------------------------------
// Issue #35: video through the shell handshake, on BOTH host threading models.
//
// The shell's block-stream video tier (`video::frame_from_block_stream`, the one AVI/WMV
// decode through) used to run inline on the thumbnail thread, and a decoder that wedged
// there hung Explorer's whole thumbnail pipeline until a reboot. It now runs on a worker
// that reaches the shell stream through the Global Interface Table while the shell thread
// waits PUMPING. These tests prove that design against the real DLL and the real COM
// handshake, with a stream whose threading model is chosen on purpose.
// ---------------------------------------------------------------------------------------------

use std::sync::Mutex;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    E_INVALIDARG, E_NOTIMPL, E_POINTER, STG_E_ACCESSDENIED, S_FALSE, S_OK,
};
use windows::Win32::System::Com::{
    ISequentialStream_Impl, IStream_Impl, COINIT_MULTITHREADED, LOCKTYPE, STATFLAG, STATSTG, STGC,
    STGTY_STREAM, STREAM_SEEK, STREAM_SEEK_CUR, STREAM_SEEK_END, STREAM_SEEK_SET,
};
use windows_implement::implement;

/// An in-memory read-only `IStream` that is deliberately NOT free-threaded: it aggregates no
/// free-threaded marshaler, so a thread in another apartment is handed a PROXY and every one
/// of its reads marshals back to the apartment that created the stream. That is the property
/// Explorer's thumbnail stream has, and the property the issue #35 worker must survive:
/// created on an STA thread, it can only be read from a worker while that STA thread pumps.
/// `SHCreateMemStream` would prove nothing here, since it may be free-threaded and hand the
/// worker a direct pointer.
#[implement(IStream)]
struct ApartmentStream {
    bytes: Vec<u8>,
    pos: Mutex<u64>,
}

impl ApartmentStream {
    fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            pos: Mutex::new(0),
        }
    }
}

impl ISequentialStream_Impl for ApartmentStream_Impl {
    fn Read(&self, pv: *mut c_void, cb: u32, pcbread: *mut u32) -> HRESULT {
        if pv.is_null() {
            return E_POINTER;
        }
        let mut pos = self.pos.lock().unwrap();
        let start = (*pos as usize).min(self.bytes.len());
        let n = (cb as usize).min(self.bytes.len() - start);
        unsafe { std::ptr::copy_nonoverlapping(self.bytes.as_ptr().add(start), pv as *mut u8, n) };
        *pos += n as u64;
        if !pcbread.is_null() {
            unsafe { *pcbread = n as u32 };
        }
        if n == cb as usize {
            S_OK
        } else {
            S_FALSE
        }
    }
    fn Write(&self, _pv: *const c_void, _cb: u32, _pcbwritten: *mut u32) -> HRESULT {
        STG_E_ACCESSDENIED
    }
}

impl IStream_Impl for ApartmentStream_Impl {
    fn Seek(&self, dlibmove: i64, dworigin: STREAM_SEEK, plibnewposition: *mut u64) -> Result<()> {
        let mut pos = self.pos.lock().unwrap();
        let base: i128 = match dworigin {
            STREAM_SEEK_SET => 0,
            STREAM_SEEK_CUR => *pos as i128,
            STREAM_SEEK_END => self.bytes.len() as i128,
            _ => return Err(Error::from(E_INVALIDARG)),
        };
        let np = base + dlibmove as i128;
        if np < 0 {
            return Err(Error::from(E_INVALIDARG));
        }
        *pos = np as u64;
        if !plibnewposition.is_null() {
            unsafe { *plibnewposition = *pos };
        }
        Ok(())
    }
    fn Stat(&self, pstatstg: *mut STATSTG, _grfstatflag: &STATFLAG) -> Result<()> {
        if pstatstg.is_null() {
            return Err(Error::from(E_POINTER));
        }
        unsafe {
            *pstatstg = STATSTG {
                r#type: STGTY_STREAM.0 as u32,
                cbSize: self.bytes.len() as u64,
                ..Default::default()
            };
        }
        Ok(())
    }
    fn SetSize(&self, _libnewsize: u64) -> Result<()> {
        Err(Error::from(E_NOTIMPL))
    }
    fn CopyTo(
        &self,
        _pstm: windows::core::Ref<'_, IStream>,
        _cb: u64,
        _pcbread: *mut u64,
        _pcbwritten: *mut u64,
    ) -> Result<()> {
        Err(Error::from(E_NOTIMPL))
    }
    fn Commit(&self, _grfcommitflags: &STGC) -> Result<()> {
        Ok(())
    }
    fn Revert(&self) -> Result<()> {
        Ok(())
    }
    fn LockRegion(&self, _liboffset: u64, _cb: u64, _dwlocktype: &LOCKTYPE) -> Result<()> {
        Err(Error::from(E_NOTIMPL))
    }
    fn UnlockRegion(&self, _liboffset: u64, _cb: u64, _dwlocktype: u32) -> Result<()> {
        Err(Error::from(E_NOTIMPL))
    }
    fn Clone(&self) -> Result<IStream> {
        Err(Error::from(E_NOTIMPL))
    }
}

fn fixture_video(name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("video")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The AVI fixture is MPEG-4 Part 2, an inbox decoder on consumer Windows. A Server image
/// may lack it, so the decode tests skip rather than fail there.
fn mpeg4_part2_decoder_present() -> bool {
    use windows::Win32::Media::MediaFoundation::MFVideoFormat_MP4V;
    sagethumbs2k_core::video::media_foundation_available()
        && sagethumbs2k_core::vcodec::decoder_installed(MFVideoFormat_MP4V) == Some(true)
}

/// ffmpeg's `testsrc` pattern is colourful: require real variety, the way the shell-surface
/// proof script does, so a quietly failing handler's flat grey tile cannot pass.
fn assert_testsrc_frame(t: &Thumb) {
    assert!(t.w > 0 && t.h > 0, "empty thumbnail");
    let mut seen = std::collections::HashSet::new();
    for y in (0..t.h).step_by((t.h / 16).max(1)) {
        for x in (0..t.w).step_by((t.w / 16).max(1)) {
            let [b, g, r, _] = t.px(x, y);
            seen.insert((r / 32, g / 32, b / 32));
        }
    }
    assert!(
        seen.len() >= 4,
        "thumbnail is effectively blank ({} distinct sampled colours)",
        seen.len()
    );
}

/// An AVI has no MP4/MKV index our own parsers read, so through the shell it can ONLY
/// thumbnail via the block-stream tier: Media Foundation seeking its own index over our
/// block cache. The stream is created on this STA thread, so the worker holds a proxy and
/// each of its reads is dispatched HERE, which only happens while this thread pumps. A
/// thumbnail coming back at all therefore proves the pumping wait; a non-pumping wait would
/// deadlock into the 8 s budget and this would fail on both the result and the clock.
#[test]
fn video_avi_thumbnails_via_the_block_stream_worker_from_an_sta_bound_stream() {
    let _settings = settings_lock();
    if !mpeg4_part2_decoder_present() {
        eprintln!("no MPEG-4 Part 2 decoder on this Windows - skipped");
        return;
    }
    let started = Instant::now();
    let stream: IStream = ApartmentStream::new(fixture_video("mpeg4-160x120.avi")).into();
    let t = unsafe { get_thumbnail_from_stream(&stream, 96) }
        .expect("the AVI must thumbnail through the shell handshake from an STA-bound stream");
    assert!(
        started.elapsed() < Duration::from_secs(6),
        "took {:?}: the worker's marshaled reads were not being served",
        started.elapsed()
    );
    assert_testsrc_frame(&t);
}

/// The thumbnail host (dllhost) is an MTA: the worker gets the same pointer back from the
/// table and the wait is a plain wait. Same file, same tier, the other threading model.
#[test]
fn video_avi_thumbnails_via_the_block_stream_worker_on_an_mta_thread() {
    let _settings = settings_lock();
    if !mpeg4_part2_decoder_present() {
        eprintln!("no MPEG-4 Part 2 decoder on this Windows - skipped");
        return;
    }
    let bytes = fixture_video("mpeg4-160x120.avi");
    let t = std::thread::spawn(move || {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .expect("MTA init");
        let stream: IStream = ApartmentStream::new(bytes).into();
        unsafe { get_thumbnail_from_stream(&stream, 96) }
    })
    .join()
    .expect("worker thread")
    .expect("the AVI must thumbnail through the shell handshake from an MTA thread");
    assert_testsrc_frame(&t);
}

/// The reporter's file shape: H.264 in a profile the Windows decoder does not implement.
/// Through the real shell handshake the DLL must decline at once, with the always-on log
/// line that names why, instead of handing the stream to a decoder that (on Windows 10)
/// wedges. Timing is the assertion that no decoder was asked: every path that asks one is
/// bounded by an 8 s budget, and the old behaviour paid that budget twice.
#[test]
fn video_h264_444_is_refused_at_once_through_the_shell_handshake() {
    let _settings = settings_lock();
    let bytes = fixture_video("h264-high444-320x240.mp4");
    let log = sagethumbs2k_core::safety::log_file();
    let before = log
        .as_ref()
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.len() as usize)
        .unwrap_or(0);
    let started = Instant::now();
    let r = unsafe { get_thumbnail(&bytes, 96) };
    let took = started.elapsed();
    assert!(r.is_err(), "a 4:4:4 clip must not thumbnail");
    assert!(
        took < Duration::from_secs(2),
        "the refusal took {took:?}; an 8 s wait means a decoder was asked after all"
    );
    if let Some(p) = log {
        let text = std::fs::read(&p).unwrap_or_default();
        let from = if before <= text.len() { before } else { 0 };
        let tail = String::from_utf8_lossy(&text[from..]);
        assert!(
            tail.contains("issue #35"),
            "the always-on refusal line must land in the log; new lines were: {tail}"
        );
    }
}
