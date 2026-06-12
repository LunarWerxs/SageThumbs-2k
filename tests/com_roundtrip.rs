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
    SHCreateMemStream, IThumbnailProvider, WTS_ALPHATYPE, WTSAT_ARGB, WTSAT_UNKNOWN,
};

const CLSID_THUMBNAIL_PROVIDER: GUID = GUID::from_u128(0x7B2E6A14_9C3D_4F8A_B1E7_2A5D9F0C6E31);

type DllGetClassObjectFn =
    unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> HRESULT;

/// The cdylib sits one dir above the test exe (…/debug/sagethumbs2k.dll vs
/// …/debug/deps/<test>.exe).
fn dll_path() -> std::path::PathBuf {
    let exe = std::env::current_exe().unwrap();
    exe.parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("sagethumbs2k.dll")
}

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
        [self.bgra[i], self.bgra[i + 1], self.bgra[i + 2], self.bgra[i + 3]]
    }
}

unsafe fn get_thumbnail(bytes: &[u8], cx: u32) -> Result<Thumb> {
    // Returns a Result (not catch_unwind) so the harness behaves identically
    // whether the DLL is built with panic=unwind (debug) or panic=abort (release).
    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

    let path = dll_path();
    assert!(path.exists(), "cdylib not built at {path:?} — run `cargo build` first");
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let module: HMODULE = LoadLibraryW(PCWSTR(wide.as_ptr()))?;

    let proc = GetProcAddress(module, s!("DllGetClassObject"))
        .ok_or_else(|| Error::from(E_FAIL))?;
    let dll_get_class_object: DllGetClassObjectFn = std::mem::transmute(proc);

    // Class factory, exactly as the shell does it.
    let mut factory_ptr: *mut c_void = std::ptr::null_mut();
    dll_get_class_object(&CLSID_THUMBNAIL_PROVIDER, &IClassFactory::IID, &mut factory_ptr).ok()?;
    assert!(!factory_ptr.is_null(), "null class factory");
    let factory = IClassFactory::from_raw(factory_ptr);

    // Create the object asking for the initializer interface.
    let init: IInitializeWithStream = factory.CreateInstance(None)?;

    // Feed the image bytes as an IStream.
    let stream: IStream = SHCreateMemStream(Some(bytes)).ok_or_else(|| Error::from(E_FAIL))?;
    init.Initialize(&stream, 0)?;

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

    Ok(Thumb { w, h, bgra, alpha: alpha.0 })
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
    let png = encode(solid(200, 100, [255, 0, 0, 255]), ImageFormat::Png);
    let t = unsafe { get_thumbnail(&png, 96) }.unwrap();
    assert_eq!((t.w, t.h), (96, 48), "200x100 should fit 96-box as 96x48");
    assert_eq!(t.alpha, WTSAT_ARGB.0, "should report premultiplied ARGB");
    let [b, g, r, a] = t.px(0, 0);
    assert!(r > 200 && g < 60 && b < 60 && a == 255, "expected red, got BGRA {:?}", [b, g, r, a]);
}

#[test]
fn dib_is_top_down() {
    // Red top half, blue bottom half. A top-down DIB keeps red at row 0.
    let mut img = RgbaImage::new(200, 100);
    for y in 0..100u32 {
        for x in 0..200u32 {
            let c = if y < 50 { [255, 0, 0, 255] } else { [0, 0, 255, 255] };
            img.put_pixel(x, y, Rgba(c));
        }
    }
    let png = encode(img, ImageFormat::Png);
    let t = unsafe { get_thumbnail(&png, 64) }.unwrap();
    let top = t.px(0, 0);
    let bottom = t.px(0, t.h - 1);
    assert!(top[2] > 180 && top[0] < 70, "top row should be red, got BGRA {top:?}");
    assert!(bottom[0] > 180 && bottom[2] < 70, "bottom row should be blue, got BGRA {bottom:?}");
}

#[test]
fn alpha_is_premultiplied() {
    // Straight R=200, A=128 -> premultiplied R ≈ 200*128/255 ≈ 100.
    let png = encode(solid(80, 80, [200, 0, 0, 128]), ImageFormat::Png);
    let t = unsafe { get_thumbnail(&png, 64) }.unwrap();
    assert_eq!(t.alpha, WTSAT_ARGB.0);
    let [b, _g, r, a] = t.px(0, 0);
    assert_eq!(a, 128, "alpha preserved");
    assert!((r as i32 - 100).abs() < 20, "R should be premultiplied ~100, got {r}");
    assert!(b < 20, "blue ~0, got {b}");
}

#[test]
fn jpeg_also_decodes_through_com() {
    let jpg = encode(solid(120, 90, [0, 200, 0, 255]), ImageFormat::Jpeg);
    let t = unsafe { get_thumbnail(&jpg, 96) }.unwrap();
    assert_eq!((t.w, t.h), (96, 72), "120x90 should fit 96-box as 96x72");
    assert!(t.px(0, 0)[1] > 150, "green channel should dominate");
}

#[test]
fn garbage_returns_error_not_crash() {
    // GetThumbnail should return a failure HRESULT (not crash the host) for
    // undecodable input.
    let result = unsafe { get_thumbnail(&[0, 1, 2, 3, 4, 5, 6, 7], 96) };
    assert!(result.is_err(), "garbage input should yield a failed GetThumbnail");
}
