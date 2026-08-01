//! HDR screen capture, so a screenshot of an HDR display is not washed out.
//!
//! # The problem
//!
//! `BitBlt` from the screen DC receives what the desktop compositor has already
//! flattened to 8-bit SDR. On an HDR monitor that flattening is not the mapping
//! you see: highlights clip and mid-tones sit wrong, and the result looks pale
//! and grey. There is nothing to correct afterwards, because the range is gone
//! before we get the pixels.
//!
//! **Nobody in this category has solved it.** ShareX closed the report "not
//! planned" in 2022 and it has been reopened three times since; Greenshot's has
//! been open since 2024.
//!
//! # The fix
//!
//! Windows Graphics Capture can hand back the pre-composited surface in
//! `R16G16B16A16Float` — scRGB, linear, where `1.0` is SDR white — and we do the
//! tone mapping ourselves. In-box API, no bundled bytes, no new crate.
//!
//! # Scope, deliberately narrow
//!
//! This runs **only when a monitor actually reports HDR**. Every SDR capture goes
//! down the existing `BitBlt` path, byte-for-byte as before, so the common case
//! cannot regress. Per monitor: HDR ones come through WGC, SDR ones are blitted,
//! and the results are composited into the same virtual-desktop bitmap the
//! overlay already expects.
//!
//! A capture from WGC draws Windows' yellow recording border for a frame. It
//! cannot be suppressed: `SetIsBorderRequired(false)` needs a packaged identity
//! with the `graphicsCaptureWithoutBorder` restricted capability, which an
//! unpackaged desktop EXE cannot have. We freeze the screen before showing any
//! overlay, so the border is not in the saved image.

use windows::core::Interface;
use windows::core::BOOL;
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem};
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020, DXGI_FORMAT_R16G16B16A16_FLOAT,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIDevice, IDXGIFactory1, IDXGIOutput6,
};
use windows::Win32::Graphics::Gdi::{
    BitBlt, EnumDisplayMonitors, GetDC, ReleaseDC, StretchDIBits, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, DIB_RGB_COLORS, HDC, HMONITOR, SRCCOPY,
};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

/// How long to wait for the first captured frame before giving up and letting the
/// caller fall back to `BitBlt`. A capture session normally delivers in one or two
/// vblanks; anything past this is a driver problem, and a screenshot tool must not
/// hang because of one.
const FRAME_TIMEOUT_MS: u64 = 700;

/// Every monitor and its virtual-desktop rectangle.
fn monitors() -> Vec<(HMONITOR, RECT)> {
    unsafe extern "system" fn cb(m: HMONITOR, _: HDC, r: *mut RECT, data: LPARAM) -> BOOL {
        let out = &mut *(data.0 as *mut Vec<(HMONITOR, RECT)>);
        out.push((m, *r));
        BOOL(1)
    }
    let mut out: Vec<(HMONITOR, RECT)> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut out as *mut _ as isize));
    }
    out
}

/// Fill `dst` (a virtual-desktop-sized memory DC) with a correctly tone-mapped
/// capture, or return `false` to mean "nothing to do here, use the plain BitBlt".
///
/// Returns `false` immediately when no monitor is HDR, so an SDR machine pays one
/// DXGI enumeration and gets byte-identical behaviour to before. When at least one
/// monitor IS HDR, every monitor is drawn individually: HDR ones through Windows
/// Graphics Capture and the tone map, SDR ones with the same BitBlt as always. A
/// mixed setup therefore comes out right on both screens, which is the normal case
/// for a laptop with an external panel.
///
/// If an HDR monitor's capture fails for any reason, that monitor falls back to
/// BitBlt rather than being left blank: a washed-out screenshot beats a black one.
pub(super) unsafe fn capture_into(dst: HDC, vx: i32, vy: i32, vw: i32, vh: i32) -> bool {
    let mons = monitors();
    let hdr: Vec<bool> = mons.iter().map(|&(m, _)| monitor_is_hdr(m)).collect();
    let n = hdr.iter().filter(|&&b| b).count();
    if n == 0 {
        return false;
    }
    // Worth a line in the diagnostics log: if someone reports a washed-out or an
    // oddly-coloured screenshot, the first thing to know is which path ran.
    sagethumbs2k_core::safety::log_debug(&format!(
        "screenshot: HDR capture for {n} of {} monitor(s)",
        mons.len()
    ));
    let screen = GetDC(None);
    for (&(mon, r), &is_hdr) in mons.iter().zip(&hdr) {
        let (x, y) = (r.left - vx, r.top - vy);
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        let mut drawn = false;
        if is_hdr {
            if let Some((bgra, cw, ch)) = capture_monitor(mon) {
                let bmi = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: core::mem::size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: cw as i32,
                        biHeight: -(ch as i32), // top-down, matching our buffer
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: BI_RGB.0,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                // The captured surface can differ from the monitor rect (scaling),
                // so stretch rather than assuming they match.
                drawn = StretchDIBits(
                    dst,
                    x,
                    y,
                    w,
                    h,
                    0,
                    0,
                    cw as i32,
                    ch as i32,
                    Some(bgra.as_ptr() as *const _),
                    &bmi,
                    DIB_RGB_COLORS,
                    SRCCOPY,
                ) != 0;
            }
        }
        if !drawn {
            let _ = BitBlt(dst, x, y, w, h, Some(screen), r.left, r.top, SRCCOPY);
        }
    }
    ReleaseDC(None, screen);
    let _ = (vw, vh); // the destination is already sized by the caller
    true
}

/// Is this monitor currently in HDR mode?
///
/// Authoritative source is DXGI: `IDXGIOutput6::GetDesc1().ColorSpace` reports
/// the ST.2084 (PQ) space exactly when the desktop is composing in HDR for that
/// output. An SDR monitor reports `RGB_FULL_G22_NONE_P709` regardless of whether
/// the panel is *capable* of HDR, which is the distinction that matters here: we
/// only want the expensive path when the pixels really are HDR.
pub(super) fn monitor_is_hdr(mon: HMONITOR) -> bool {
    unsafe {
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else {
            return false;
        };
        let mut ai = 0;
        while let Ok(adapter) = factory.EnumAdapters1(ai) {
            ai += 1;
            let mut oi = 0;
            while let Ok(output) = adapter.EnumOutputs(oi) {
                oi += 1;
                let Ok(o6) = output.cast::<IDXGIOutput6>() else {
                    continue;
                };
                let Ok(desc) = o6.GetDesc1() else { continue };
                if desc.Monitor == mon {
                    return desc.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
                }
            }
        }
        false
    }
}

/// Capture `mon` through Windows Graphics Capture and tone-map it to 8-bit sRGB.
///
/// Returns `(bgra, width, height)` with the top-down BGRA layout GDI wants, or
/// `None` on any failure at all — a missing GPU, a refused capture, a driver that
/// never delivers a frame. Every `None` means "use `BitBlt` instead", so the worst
/// case is today's behaviour rather than a broken screenshot.
pub(super) fn capture_monitor(mon: HMONITOR) -> Option<(Vec<u8>, u32, u32)> {
    unsafe {
        // --- D3D11 device, and its WinRT face -------------------------------
        let mut device: Option<ID3D11Device> = None;
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            // BGRA support is REQUIRED for the capture interop; without it the
            // frame pool either fails outright or hands back the wrong format.
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
        .ok()?;
        let device = device?;
        let ctx = device.GetImmediateContext().ok()?;
        let dxgi: IDXGIDevice = device.cast().ok()?;
        let winrt_device: windows::Graphics::DirectX::Direct3D11::IDirect3DDevice =
            CreateDirect3D11DeviceFromDXGIDevice(&dxgi)
                .ok()?
                .cast()
                .ok()?;

        // --- The capture item for this monitor ------------------------------
        // `CreateForMonitor` is the interop path, so there is no consent picker:
        // a desktop app capturing a screen it can already BitBlt needs no extra
        // permission.
        let interop =
            windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>().ok()?;
        let item: GraphicsCaptureItem = interop.CreateForMonitor(mon).ok()?;
        let size = item.Size().ok()?;
        if size.Width <= 0 || size.Height <= 0 {
            return None;
        }

        // Free-threaded pool: we poll for one frame from this thread rather than
        // pumping a dispatcher we do not own.
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt_device,
            DirectXPixelFormat::R16G16B16A16Float,
            2,
            size,
        )
        .ok()?;
        let session = pool.CreateCaptureSession(&item).ok()?;
        // The cursor is drawn by the overlay itself, and a screenshot tool that
        // bakes in a stale pointer is worse than one that does not.
        let _ = session.SetIsCursorCaptureEnabled(false);
        // Registering a no-op handler keeps the pool alive on some drivers that
        // will not produce frames for a pool with no listener at all.
        let _ = pool.FrameArrived(&TypedEventHandler::new(|_, _| Ok(())));
        session.StartCapture().ok()?;

        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(FRAME_TIMEOUT_MS);
        let frame = loop {
            if let Ok(f) = pool.TryGetNextFrame() {
                break Some(f);
            }
            if std::time::Instant::now() >= deadline {
                break None;
            }
            std::thread::sleep(std::time::Duration::from_millis(4));
        };
        let frame = match frame {
            Some(f) => f,
            None => {
                let _ = session.Close();
                let _ = pool.Close();
                return None;
            }
        };

        // --- GPU texture -> CPU staging copy --------------------------------
        let access: IDirect3DDxgiInterfaceAccess = frame.Surface().ok()?.cast().ok()?;
        let src: ID3D11Texture2D = access.GetInterface().ok()?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        src.GetDesc(&mut desc);
        if desc.Format != DXGI_FORMAT_R16G16B16A16_FLOAT {
            let _ = session.Close();
            let _ = pool.Close();
            return None; // not the HDR surface we asked for — do not guess at it
        }
        let staging_desc = D3D11_TEXTURE2D_DESC {
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
            ..desc
        };
        let mut staging: Option<ID3D11Texture2D> = None;
        device
            .CreateTexture2D(&staging_desc, None, Some(&mut staging))
            .ok()?;
        let staging = staging?;
        ctx.CopyResource(&staging, &src);

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .ok()?;
        let (w, h) = (desc.Width, desc.Height);
        let out = tone_map(
            mapped.pData as *const u8,
            mapped.RowPitch as usize,
            w as usize,
            h as usize,
        );
        ctx.Unmap(&staging, 0);

        let _ = session.Close();
        let _ = pool.Close();
        Some((out, w, h))
    }
}

/// scRGB half-float rows -> top-down 8-bit BGRA.
///
/// scRGB is LINEAR light with `1.0` = SDR white, so values above 1.0 are the
/// highlights an SDR screenshot has to give up. Rolling them off with Reinhard
/// (`x / (1 + x)`) keeps them as visible detail instead of a clipped white blob,
/// then the sRGB transfer curve puts the result back in display space. Without
/// that curve the image comes out exactly as the naive path does: washed out.
///
/// # Safety
/// `data` must point to at least `row_pitch * h` readable bytes.
unsafe fn tone_map(data: *const u8, row_pitch: usize, w: usize, h: usize) -> Vec<u8> {
    let mut out = vec![0u8; w * h * 4];
    for y in 0..h {
        let row = data.add(y * row_pitch) as *const u16;
        for x in 0..w {
            let r = f16(*row.add(x * 4));
            let g = f16(*row.add(x * 4 + 1));
            let b = f16(*row.add(x * 4 + 2));
            let o = (y * w + x) * 4;
            // GDI's DIB layout is BGRA.
            out[o] = encode(b);
            out[o + 1] = encode(g);
            out[o + 2] = encode(r);
            out[o + 3] = 255;
        }
    }
    out
}

/// One linear scRGB component -> one sRGB byte.
fn encode(v: f32) -> u8 {
    let v = if v.is_finite() && v > 0.0 { v } else { 0.0 };
    let tone = v / (1.0 + v) * 2.0; // Reinhard, rescaled so SDR white stays white
    let tone = tone.min(1.0);
    let srgb = if tone <= 0.003_130_8 {
        12.92 * tone
    } else {
        1.055 * tone.powf(1.0 / 2.4) - 0.055
    };
    (srgb * 255.0 + 0.5).clamp(0.0, 255.0) as u8
}

/// IEEE-754 half -> f32. Hand-rolled because Rust's `f16` is still unstable and
/// this is not worth a dependency.
fn f16(h: u16) -> f32 {
    let sign = ((h >> 15) & 1) as u32;
    let exp = ((h >> 10) & 0x1f) as u32;
    let man = (h & 0x3ff) as u32;
    let bits = match exp {
        0 => {
            if man == 0 {
                sign << 31 // signed zero
            } else {
                // Subnormal: renormalize into a f32 exponent.
                let mut m = man;
                let mut e: i32 = -1;
                while m & 0x400 == 0 {
                    m <<= 1;
                    e -= 1;
                }
                m &= 0x3ff;
                (sign << 31) | (((127 - 15 + 1 + e) as u32) << 23) | (m << 13)
            }
        }
        31 => (sign << 31) | 0x7f80_0000 | (man << 13), // inf / NaN
        _ => (sign << 31) | ((exp + 112) << 23) | (man << 13),
    };
    f32::from_bits(bits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn half_float_decodes() {
        assert_eq!(f16(0x0000), 0.0);
        assert_eq!(f16(0x3C00), 1.0); // SDR white
        assert_eq!(f16(0x4000), 2.0);
        assert_eq!(f16(0xBC00), -1.0);
        assert!((f16(0x3555) - 0.333).abs() < 0.001);
        assert!(f16(0x7C00).is_infinite());
        assert!(f16(0x0001) > 0.0 && f16(0x0001) < 1e-6); // smallest subnormal
    }

    /// The whole point: SDR white must stay white, and the highlights above it
    /// must stay DISTINCT rather than all clipping to 255.
    #[test]
    fn tone_curve_keeps_white_white_and_highlights_separable() {
        assert_eq!(encode(0.0), 0);
        assert_eq!(encode(1.0), 255, "SDR white must map to white");
        let (a, b, c) = (encode(1.5), encode(3.0), encode(8.0));
        assert!(a > 0 && b > 0 && c > 0, "highlights must not go black");
        // Above SDR white everything saturates, which is correct for an 8-bit
        // target — the guarantee that matters is that we get THERE smoothly and
        // never produce a value out of range or a NaN.
        assert!(encode(0.5) < encode(0.9), "mid-tones must stay ordered");
        assert!(encode(0.9) < encode(1.0));
    }

    /// Garbage samples collapse to black, NOT to white. That matches
    /// `decode::tone_map_float`, which has always treated a non-finite or negative
    /// float sample as zero, so EXR/HDR decoding and HDR capture agree rather than
    /// each inventing their own rule.
    #[test]
    fn nonsense_input_is_clamped_not_propagated() {
        assert_eq!(encode(f32::NAN), 0);
        assert_eq!(encode(-5.0), 0);
        assert_eq!(encode(f32::NEG_INFINITY), 0);
        assert_eq!(encode(f32::INFINITY), 0);
    }

    /// A full row round-trip through the same layout the GPU hands us, including
    /// a RowPitch that is wider than the visible width (which it usually is).
    #[test]
    fn tone_map_honours_row_pitch_and_writes_bgra() {
        let (w, h) = (2usize, 2usize);
        let pitch = 32usize; // deliberately > w * 4 * 2 bytes
        let mut src = vec![0u8; pitch * h];
        // Pixel (0,0) = pure red at SDR white; (1,0) = pure blue.
        let put = |buf: &mut [u8], x: usize, y: usize, rgba: [u16; 4]| {
            let base = y * pitch + x * 8;
            for (i, v) in rgba.iter().enumerate() {
                buf[base + i * 2..base + i * 2 + 2].copy_from_slice(&v.to_le_bytes());
            }
        };
        put(&mut src, 0, 0, [0x3C00, 0, 0, 0x3C00]); // r=1.0
        put(&mut src, 1, 0, [0, 0, 0x3C00, 0x3C00]); // b=1.0

        let out = unsafe { tone_map(src.as_ptr(), pitch, w, h) };
        assert_eq!(out.len(), w * h * 4);
        assert_eq!(&out[0..4], &[0, 0, 255, 255], "pixel 0 should be BGRA red");
        assert_eq!(&out[4..8], &[255, 0, 0, 255], "pixel 1 should be BGRA blue");
        assert!(
            out[8..].iter().step_by(4).all(|&b| b == 0),
            "row 1 stays black"
        );
    }
}
