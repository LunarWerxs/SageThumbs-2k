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
//! `R16G16B16A16Float` — scRGB, linear, where `1.0` is 80 nits — and we do the
//! tone mapping ourselves. In-box API, no bundled bytes, no new crate.
//!
//! Windows composes SDR content (the desktop, every ordinary window) at the
//! user's **SDR white level**, the "SDR content brightness" slider: 200 to 480
//! nits is common, so desktop white arrives as scRGB 2.5 to 6.0, not 1.0. Until
//! issue #67 the tone curve assumed 1.0, which clipped the whole desktop to white
//! and lifted every mid-tone. Each HDR monitor's white level is now read from
//! `DisplayConfigGetDeviceInfo` and divided out, so SDR content comes out as it
//! would from an SDR desktop; only what is brighter than SDR white (HDR video,
//! games) is rolled off, toward the monitor's peak, keeping its hue.
//!
//! The float capture itself is kept beside the 8-bit picture (see `hdrsave.rs`),
//! so the HDR save formats get the real range, not the tone-mapped one.
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
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig,
    DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL, DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
    DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_DEVICE_INFO_TYPE, DISPLAYCONFIG_MODE_INFO,
    DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SDR_WHITE_LEVEL, DISPLAYCONFIG_SOURCE_DEVICE_NAME,
    QDC_ONLY_ACTIVE_PATHS,
};
use windows::Win32::Foundation::{ERROR_SUCCESS, HMODULE, LUID};
use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020, DXGI_FORMAT_R16G16B16A16_FLOAT,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIDevice, IDXGIFactory1, IDXGIOutput6, DXGI_OUTPUT_DESC1,
};
use windows::Win32::Graphics::Gdi::{
    BitBlt, EnumDisplayMonitors, GetDC, ReleaseDC, StretchDIBits, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, DIB_RGB_COLORS, HDC, HMONITOR, SRCCOPY,
};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

use super::hdrsave::{HdrShot, HdrTile, SCRGB_NITS};
use super::hdrtone::{same_device, tone_map, Levels};

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
/// capture and return the HDR monitors' float captures, or return `None` to mean
/// "nothing to do here, use the plain BitBlt".
///
/// Returns `None` immediately when no monitor is HDR, so an SDR machine pays one
/// DXGI enumeration and gets byte-identical behaviour to before. When at least one
/// monitor IS HDR, every monitor is drawn individually: HDR ones through Windows
/// Graphics Capture and the tone map, SDR ones with the same BitBlt as always. A
/// mixed setup therefore comes out right on both screens, which is the normal case
/// for a laptop with an external panel.
///
/// If an HDR monitor's capture fails for any reason, that monitor falls back to
/// BitBlt rather than being left blank (a washed-out screenshot beats a black one),
/// and has no tile in the result.
pub(super) unsafe fn capture_into(dst: HDC, vx: i32, vy: i32, vw: i32, vh: i32) -> Option<HdrShot> {
    let mons = monitors();
    let hdr: Vec<Option<DXGI_OUTPUT_DESC1>> = mons.iter().map(|&(m, _)| hdr_output(m)).collect();
    let n = hdr.iter().flatten().count();
    if n == 0 {
        return None;
    }
    // Worth a line in the diagnostics log: if someone reports a washed-out or an
    // oddly-coloured screenshot, the first thing to know is which path ran.
    st2k_base::safety::log_debugf!(
        "screenshot: HDR capture for {n} of {} monitor(s)",
        mons.len()
    );
    // One D3D11 device for the WHOLE capture (item 145's D3D half) instead of one per HDR
    // monitor: creating a device is the single most expensive step in this path, and a
    // multi-monitor HDR rig previously paid it again for every monitor on every screenshot.
    // `None` here (a missing/refused GPU) means every HDR monitor below falls back to BitBlt,
    // same as before — `capture_monitor` no longer has a device of its own to try.
    let gpu = create_capture_device();
    let screen = GetDC(None);
    let mut shot = HdrShot::default();
    for (&(mon, r), out) in mons.iter().zip(&hdr) {
        let dest = RECT {
            left: r.left - vx,
            top: r.top - vy,
            right: r.right - vx,
            bottom: r.bottom - vy,
        };
        let tile = match (out, gpu.as_ref()) {
            (Some(desc), Some((device, ctx, winrt))) => {
                capture_monitor(mon, levels_of(desc), device, ctx, winrt)
                    .and_then(|c| draw_capture(dst, dest, c))
            }
            _ => None,
        };
        match tile {
            Some(t) => shot.tiles.push(t),
            None => {
                let _ = BitBlt(
                    dst,
                    dest.left,
                    dest.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    Some(screen),
                    r.left,
                    r.top,
                    SRCCOPY,
                );
            }
        }
    }
    ReleaseDC(None, screen);
    let _ = (vw, vh); // the destination is already sized by the caller
    Some(shot)
}

/// One HDR monitor's capture: the tone-mapped 8-bit picture and the scRGB it came from.
struct Captured {
    bgra: Vec<u8>,
    w: u32,
    h: u32,
    /// `w * h` RGB triples of scRGB half floats.
    rgb: Vec<u16>,
    levels: Levels,
}

/// Draw `c`'s 8-bit picture into `dst` over `dest` and hand back its float tile, or `None`
/// when GDI refused the draw (the caller then blits that monitor).
unsafe fn draw_capture(dst: HDC, dest: RECT, c: Captured) -> Option<HdrTile> {
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: core::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: c.w as i32,
            biHeight: -(c.h as i32), // top-down, matching our buffer
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    // The captured surface can differ from the monitor rect (scaling), so stretch rather
    // than assuming they match.
    let drawn = StretchDIBits(
        dst,
        dest.left,
        dest.top,
        dest.right - dest.left,
        dest.bottom - dest.top,
        0,
        0,
        c.w as i32,
        c.h as i32,
        Some(c.bgra.as_ptr() as *const _),
        &bmi,
        DIB_RGB_COLORS,
        SRCCOPY,
    ) != 0;
    drawn.then_some(HdrTile {
        dest,
        src_w: c.w as usize,
        src_h: c.h as usize,
        rgb: c.rgb,
        white_nits: c.levels.white * SCRGB_NITS,
    })
}

/// This monitor's DXGI output description, when it is currently in HDR mode.
///
/// Authoritative source is DXGI: `IDXGIOutput6::GetDesc1().ColorSpace` reports
/// the ST.2084 (PQ) space exactly when the desktop is composing in HDR for that
/// output. An SDR monitor reports `RGB_FULL_G22_NONE_P709` regardless of whether
/// the panel is *capable* of HDR, which is the distinction that matters here: we
/// only want the expensive path when the pixels really are HDR.
fn hdr_output(mon: HMONITOR) -> Option<DXGI_OUTPUT_DESC1> {
    unsafe {
        let factory = CreateDXGIFactory1::<IDXGIFactory1>().ok()?;
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
                    return (desc.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020)
                        .then_some(desc);
                }
            }
        }
        None
    }
}

/// The display levels of the HDR output `desc` describes.
fn levels_of(desc: &DXGI_OUTPUT_DESC1) -> Levels {
    let white = unsafe { sdr_white_level(&desc.DeviceName) };
    let levels = Levels::new(white, desc.MaxLuminance);
    st2k_base::safety::log_debugf!(
        "screenshot: HDR monitor SDR white {:.0} nits, peak {:.0} nits",
        levels.white * SCRGB_NITS,
        levels.peak * SCRGB_NITS
    );
    levels
}

/// The raw `SDRWhiteLevel` (1000 = 80 nits) of the display whose GDI device name is
/// `device`: its active path is found by source name, then its target is asked.
unsafe fn sdr_white_level(device: &[u16; 32]) -> Option<u32> {
    let (mut np, mut nm) = (0u32, 0u32);
    if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm) != ERROR_SUCCESS {
        return None;
    }
    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
    let got = QueryDisplayConfig(
        QDC_ONLY_ACTIVE_PATHS,
        &mut np,
        paths.as_mut_ptr(),
        &mut nm,
        modes.as_mut_ptr(),
        None,
    );
    if got != ERROR_SUCCESS {
        return None;
    }
    paths.truncate(np as usize);
    let path = paths.iter().find(|p| {
        let mut req = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
            header: info_header::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>(
                DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                p.sourceInfo.adapterId,
                p.sourceInfo.id,
            ),
            ..Default::default()
        };
        DisplayConfigGetDeviceInfo(&mut req.header) == 0
            && same_device(&req.viewGdiDeviceName, device)
    })?;
    let mut req = DISPLAYCONFIG_SDR_WHITE_LEVEL {
        header: info_header::<DISPLAYCONFIG_SDR_WHITE_LEVEL>(
            DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL,
            path.targetInfo.adapterId,
            path.targetInfo.id,
        ),
        SDRWhiteLevel: 0,
    };
    (DisplayConfigGetDeviceInfo(&mut req.header) == 0).then_some(req.SDRWhiteLevel)
}

/// The header of a `DisplayConfigGetDeviceInfo` request whose packet is a `T`.
fn info_header<T>(
    kind: DISPLAYCONFIG_DEVICE_INFO_TYPE,
    adapter: LUID,
    id: u32,
) -> DISPLAYCONFIG_DEVICE_INFO_HEADER {
    DISPLAYCONFIG_DEVICE_INFO_HEADER {
        r#type: kind,
        size: core::mem::size_of::<T>() as u32,
        adapterId: adapter,
        id,
    }
}

/// Closes the WinRT capture session + frame pool when it drops, covering every
/// exit from [`capture_monitor`] instead of a hand-repeated `Close()` pair at each early
/// return. `GraphicsCaptureSession`/`Direct3D11CaptureFramePool` are `IClosable` objects
/// backing a live OS capture stream (and the undismissable yellow recording-border indicator
/// this module's own doc comment describes) — relying on `Drop`/`Release` alone left 3 of the
/// function's 5 exit paths (the staging-texture allocation, the `Map` call, and the
/// not-quite-that-format check) leaking a session+pool on a machine where the GPU work
/// reliably fails (low VRAM, a driver quirk), for as long as the screenshot process stays
/// open — which, with the editor left open annotating, can be a while.
struct CaptureGuard {
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
}

impl Drop for CaptureGuard {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

/// Capture `mon` through Windows Graphics Capture and tone-map it to 8-bit sRGB at
/// `levels`, using the D3D11 device/context/WinRT-face the caller created once for the whole
/// multi-monitor capture (item 145's D3D half — see [`capture_into`]).
///
/// Returns the top-down BGRA layout GDI wants plus the scRGB it came from, or `None` on any
/// failure at all — a refused capture session, a driver that never delivers a frame. Every
/// `None` means "use `BitBlt` instead", so the worst case is today's behaviour rather than a
/// broken screenshot.
fn capture_monitor(
    mon: HMONITOR,
    levels: Levels,
    device: &ID3D11Device,
    ctx: &ID3D11DeviceContext,
    winrt_device: &windows::Graphics::DirectX::Direct3D11::IDirect3DDevice,
) -> Option<Captured> {
    unsafe {
        let (pool, session) = start_capture_session(mon, winrt_device)?;
        let guard = CaptureGuard { pool, session };

        let frame = wait_for_frame(&guard.pool)?;

        // --- GPU texture -> CPU staging copy --------------------------------
        let (src, desc) = frame_src_texture(&frame)?;
        if desc.Format != DXGI_FORMAT_R16G16B16A16_FLOAT {
            return None; // not the HDR surface we asked for — do not guess at it
        }
        let staging = staged_copy(device, ctx, &src, &desc)?;

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .ok()?;
        let (w, h) = (desc.Width, desc.Height);
        let (bgra, rgb) = tone_map(
            mapped.pData as *const u8,
            mapped.RowPitch as usize,
            w as usize,
            h as usize,
            levels,
        );
        ctx.Unmap(&staging, 0);

        // `guard` drops here (and on every early return above), closing the session + pool
        // exactly once no matter which path was taken.
        Some(Captured {
            bgra,
            w,
            h,
            rgb,
            levels,
        })
    }
}

/// D3D11 device, and its WinRT face. BGRA support is REQUIRED for the capture interop;
/// without it the frame pool either fails outright or hands back the wrong format.
unsafe fn create_capture_device() -> Option<(
    ID3D11Device,
    ID3D11DeviceContext,
    windows::Graphics::DirectX::Direct3D11::IDirect3DDevice,
)> {
    let mut device: Option<ID3D11Device> = None;
    D3D11CreateDevice(
        None,
        D3D_DRIVER_TYPE_HARDWARE,
        HMODULE::default(),
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
    Some((device, ctx, winrt_device))
}

/// The capture item for `mon` (`CreateForMonitor` is the interop path, so there is no
/// consent picker: a desktop app capturing a screen it can already BitBlt needs no extra
/// permission), and a started free-threaded capture session against it.
unsafe fn start_capture_session(
    mon: HMONITOR,
    winrt_device: &windows::Graphics::DirectX::Direct3D11::IDirect3DDevice,
) -> Option<(Direct3D11CaptureFramePool, GraphicsCaptureSession)> {
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
        winrt_device,
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
    Some((pool, session))
}

/// Poll `pool` until a frame arrives or [`FRAME_TIMEOUT_MS`] elapses.
unsafe fn wait_for_frame(pool: &Direct3D11CaptureFramePool) -> Option<Direct3D11CaptureFrame> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(FRAME_TIMEOUT_MS);
    loop {
        if let Ok(f) = pool.TryGetNextFrame() {
            return Some(f);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(4));
    }
}

/// `frame`'s underlying D3D11 texture and its description.
unsafe fn frame_src_texture(
    frame: &Direct3D11CaptureFrame,
) -> Option<(ID3D11Texture2D, D3D11_TEXTURE2D_DESC)> {
    let access: IDirect3DDxgiInterfaceAccess = frame.Surface().ok()?.cast().ok()?;
    let src: ID3D11Texture2D = access.GetInterface().ok()?;
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    src.GetDesc(&mut desc);
    Some((src, desc))
}

/// Copy `src` into a new CPU-readable staging texture matching `desc`.
unsafe fn staged_copy(
    device: &ID3D11Device,
    ctx: &ID3D11DeviceContext,
    src: &ID3D11Texture2D,
    desc: &D3D11_TEXTURE2D_DESC,
) -> Option<ID3D11Texture2D> {
    let staging_desc = D3D11_TEXTURE2D_DESC {
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
        ..*desc
    };
    let mut staging: Option<ID3D11Texture2D> = None;
    device
        .CreateTexture2D(&staging_desc, None, Some(&mut staging))
        .ok()?;
    let staging = staging?;
    ctx.CopyResource(&staging, src);
    Some(staging)
}
