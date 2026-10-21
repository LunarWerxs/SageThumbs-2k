//! HDR screenshot files (issue #67): the capture's linear light, kept beside the 8-bit picture
//! the editor annotates, written as one of the two HDR save formats.
//!
//! - **PNG HDR**: 16-bit RGB in BT.2020 primaries with the PQ (SMPTE ST 2084) transfer, tagged
//!   with a `cICP` chunk (9, 16, 0, 1) as the PNG Third Edition specifies, plus a `cLLI` chunk
//!   with the image's content light levels. The `png` crate reads both chunks but does not write
//!   them, so they go in by hand between IHDR and IDAT.
//! - **JPEG XR HDR**: scRGB half-float through WIC's in-box JPEG XR encoder, lossless. Windows'
//!   own Game Bar saves HDR screenshots this way, so everything that opens those opens these.
//!
//! Nothing here needs the `hdr-capture` feature: a capture with no HDR monitor still saves in
//! the HDR formats, its 8-bit pixels placed at BT.2408's reference SDR white, so the format the
//! user picked never fails just because this capture happened to be SDR.

use windows::core::{Error, Result, GUID, PWSTR};
use windows::Win32::Foundation::{E_FAIL, E_OUTOFMEMORY, RECT};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_ContainerFormatWmp, GUID_WICPixelFormat64bppRGBAHalf,
    IWICBitmapEncoder, IWICBitmapFrameEncode, IWICImagingFactory, WICBitmapEncoderNoCache,
};
use windows::Win32::System::Com::StructuredStorage::{IPropertyBag2, PROPBAG2};
use windows::Win32::System::Com::{
    CoCreateInstance, IStream, CLSCTX_INPROC_SERVER, STATFLAG_NONAME, STATSTG, STREAM_SEEK_SET,
};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Shell::SHCreateMemStream;

/// scRGB's unit: `1.0` is 80 nits.
pub(super) const SCRGB_NITS: f32 = 80.0;

/// BT.2408's reference white for SDR content in an HDR signal, in nits: where the pixels of a
/// capture with no HDR monitor sit in an HDR file.
pub(super) const SDR_REFERENCE_WHITE_NITS: f32 = 203.0;

/// The `cICP` payload for BT.2020 primaries (9), the PQ transfer (16), RGB with no matrix (0),
/// full range (1).
pub(super) const CICP_BT2020_PQ: [u8; 4] = [9, 16, 0, 1];

/// One HDR monitor's capture: scRGB half-floats (RGB, no alpha) at the size Windows Graphics
/// Capture delivered, and where that monitor sits in the overlay's client space.
#[cfg_attr(
    not(feature = "hdr-capture"),
    allow(dead_code, reason = "built only by the hdr-capture feature's hdr.rs")
)]
pub(super) struct HdrTile {
    /// The monitor's rectangle in overlay-client coordinates.
    pub(super) dest: RECT,
    pub(super) src_w: usize,
    pub(super) src_h: usize,
    /// `src_w * src_h` RGB triples of IEEE half floats, scRGB.
    pub(super) rgb: Vec<u16>,
    /// The monitor's SDR white ("SDR content brightness") in nits.
    pub(super) white_nits: f32,
}

/// Every HDR monitor's capture for one screenshot. No tiles when every HDR monitor's capture
/// fell back to `BitBlt`.
#[derive(Default)]
pub(super) struct HdrShot {
    pub(super) tiles: Vec<HdrTile>,
}

/// Linear light in nits, BT.709 primaries, one RGB triple per pixel, top-down. A channel can
/// be negative: scRGB carries colours outside BT.709 that way, and BT.2020 has room for them.
pub(super) struct HdrImage {
    pub(super) w: usize,
    pub(super) h: usize,
    pub(super) rgb: Vec<f32>,
}

impl HdrTile {
    fn contains(&self, x: i32, y: i32) -> bool {
        let d = self.dest;
        x >= d.left && x < d.right && y >= d.top && y < d.bottom
    }

    /// The linear light at overlay-client point `(x, y)`, in nits. The nearest sample when
    /// the captured surface's size differs from the monitor rectangle (the same stretch the
    /// 8-bit picture got from `StretchDIBits`).
    fn nits_at(&self, x: i32, y: i32) -> [f32; 3] {
        let d = self.dest;
        let (dw, dh) = ((d.right - d.left).max(1), (d.bottom - d.top).max(1));
        let sx = ((x - d.left) as usize * self.src_w / dw as usize).min(self.src_w - 1);
        let sy = ((y - d.top) as usize * self.src_h / dh as usize).min(self.src_h - 1);
        let i = (sy * self.src_w + sx) * 3;
        [0, 1, 2].map(|c| finite(half_to_f32(self.rgb[i + c])) * SCRGB_NITS)
    }
}

impl HdrShot {
    fn tile_at(&self, x: i32, y: i32) -> Option<&HdrTile> {
        self.tiles
            .iter()
            .find(|t| t.contains(x, y) && t.src_w > 0 && t.src_h > 0)
    }

    /// Where 8-bit pixels outside every HDR monitor (an SDR monitor beside an HDR one) sit:
    /// the first HDR monitor's SDR white, so the two screens keep the brightness they had.
    fn sdr_white_nits(&self) -> Option<f32> {
        self.tiles.first().map(|t| t.white_nits)
    }
}

/// A float that is not finite reads as black, as everywhere else in the capture path.
fn finite(v: f32) -> f32 {
    if v.is_finite() {
        v
    } else {
        0.0
    }
}

/// The 256 sRGB byte values as linear light, `0.0..=1.0`.
fn srgb_to_linear_table() -> [f32; 256] {
    std::array::from_fn(|i| {
        let c = i as f32 / 255.0;
        if c <= 0.040_45 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    })
}

/// The HDR image of the region `sel` (overlay-client coordinates).
///
/// `composed` and `plain` are the region's top-down BGRA with and without the annotations.
/// A pixel the editor left alone takes the HDR capture's value; a pixel an annotation changed
/// takes its 8-bit value at that monitor's SDR white, so arrows and text keep the colour and
/// brightness they had in the editor instead of being reinterpreted as HDR. Outside every HDR
/// monitor (an SDR screen, or a capture with no HDR at all) the 8-bit value sits at SDR white.
/// `None` when the buffers do not match the region's size.
pub(super) fn region_image(
    hdr: Option<&HdrShot>,
    sel: RECT,
    composed: &[u8],
    plain: &[u8],
) -> Option<HdrImage> {
    let (w, h) = (
        usize::try_from(sel.right - sel.left).ok()?,
        usize::try_from(sel.bottom - sel.top).ok()?,
    );
    let n = w.checked_mul(h)?;
    if composed.len() < n * 4 || plain.len() < n * 4 {
        return None;
    }
    let lut = srgb_to_linear_table();
    let sdr_white = hdr
        .and_then(HdrShot::sdr_white_nits)
        .unwrap_or(SDR_REFERENCE_WHITE_NITS);
    let mut rgb = Vec::with_capacity(n * 3);
    for i in 0..n {
        let (x, y) = (sel.left + (i % w) as i32, sel.top + (i / w) as i32);
        let o = i * 4;
        let tile = hdr.and_then(|s| s.tile_at(x, y));
        let edited = composed[o..o + 3] != plain[o..o + 3];
        let px = match tile {
            Some(t) if !edited => t.nits_at(x, y),
            _ => {
                let white = tile.map_or(sdr_white, |t| t.white_nits);
                // BGRA in, RGB out.
                [2, 1, 0].map(|c| lut[composed[o + c] as usize] * white)
            }
        };
        rgb.extend_from_slice(&px);
    }
    Some(HdrImage { w, h, rgb })
}

/// The SMPTE ST 2084 (PQ) inverse EOTF: absolute luminance in nits to a `0.0..=1.0` signal.
fn pq(nits: f32) -> f32 {
    const M1: f32 = 2610.0 / 16384.0;
    const M2: f32 = 2523.0 / 4096.0 * 128.0;
    const C1: f32 = 3424.0 / 4096.0;
    const C2: f32 = 2413.0 / 4096.0 * 32.0;
    const C3: f32 = 2392.0 / 4096.0 * 32.0;
    let y = (finite(nits) / 10_000.0).clamp(0.0, 1.0);
    let p = y.powf(M1);
    ((C1 + C2 * p) / (1.0 + C3 * p)).powf(M2)
}

/// Linear BT.709 RGB to linear BT.2020 RGB (ITU-R BT.2087), clamped at zero: a colour BT.2020
/// cannot hold either is out of every display's gamut.
fn bt709_to_bt2020([r, g, b]: [f32; 3]) -> [f32; 3] {
    [
        0.627_404 * r + 0.329_283 * g + 0.043_313 * b,
        0.069_097 * r + 0.919_540 * g + 0.011_362 * b,
        0.016_391 * r + 0.088_013 * g + 0.895_595 * b,
    ]
    .map(|v| finite(v).max(0.0))
}

/// The `cLLI` payload: MaxCLL (the brightest pixel's brightest channel) and MaxFALL (the
/// image's average of each pixel's brightest channel), big-endian, in 0.0001-nit units.
fn clli(max_cll: f32, max_fall: f32) -> [u8; 8] {
    let unit = |nits: f32| (nits.clamp(0.0, 10_000.0) * 10_000.0).round() as u32;
    let mut out = [0u8; 8];
    out[..4].copy_from_slice(&unit(max_cll).to_be_bytes());
    out[4..].copy_from_slice(&unit(max_fall).to_be_bytes());
    out
}

/// The PNG HDR samples (16-bit big-endian RGB, BT.2020, PQ) and the image's `cLLI` payload.
fn pq_samples(img: &HdrImage) -> (Vec<u8>, [u8; 8]) {
    let mut out = Vec::with_capacity(img.rgb.len() * 2);
    let (mut max_cll, mut sum) = (0.0f32, 0.0f64);
    for &px in img.rgb.as_chunks::<3>().0 {
        let c = bt709_to_bt2020(px);
        let brightest = c[0].max(c[1]).max(c[2]);
        max_cll = max_cll.max(brightest);
        sum += f64::from(brightest);
        for v in c {
            let s = (pq(v) * 65_535.0 + 0.5) as u16;
            out.extend_from_slice(&s.to_be_bytes());
        }
    }
    let pixels = (img.rgb.len() / 3).max(1);
    (out, clli(max_cll, (sum / pixels as f64) as f32))
}

/// Encode `img` as PNG HDR. See the module docs.
pub(super) fn encode_png_pq(img: &HdrImage) -> Option<Vec<u8>> {
    let (samples, light) = pq_samples(img);
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(
        &mut out,
        u32::try_from(img.w).ok()?,
        u32::try_from(img.h).ok()?,
    );
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Sixteen);
    // Fast: 16-bit PQ samples barely compress at any level, and the save runs while the user
    // waits on the editor.
    enc.set_compression(png::Compression::Fast);
    let mut writer = enc.write_header().ok()?;
    // Both chunks must precede IDAT (PNG Third Edition, 11.3.2.6 and 11.3.2.8).
    writer.write_chunk(png::chunk::cICP, &CICP_BT2020_PQ).ok()?;
    writer.write_chunk(png::chunk::cLLI, &light).ok()?;
    writer.write_image_data(&samples).ok()?;
    writer.finish().ok()?;
    Some(out)
}

/// IEEE half to f32. Hand-rolled because Rust's `f16` is still unstable and this is not worth
/// a dependency.
pub(super) fn half_to_f32(h: u16) -> f32 {
    let sign = ((h >> 15) & 1) as u32;
    let exp = ((h >> 10) & 0x1f) as u32;
    let man = (h & 0x3ff) as u32;
    let bits = match exp {
        0 if man == 0 => sign << 31, // signed zero
        0 => {
            // Subnormal: renormalize into an f32 exponent.
            let mut m = man;
            let mut e: i32 = 0;
            while m & 0x400 == 0 {
                m <<= 1;
                e -= 1;
            }
            m &= 0x3ff;
            (sign << 31) | (((127 - 15 + 1 + e) as u32) << 23) | (m << 13)
        }
        31 => (sign << 31) | 0x7f80_0000 | (man << 13), // inf / NaN
        _ => (sign << 31) | ((exp + 112) << 23) | (man << 13),
    };
    f32::from_bits(bits)
}

/// f32 to IEEE half, rounding to nearest even; past the half range is infinity, below it zero.
fn f32_to_half(v: f32) -> u16 {
    let bits = v.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xff) as i32;
    let man = bits & 0x7f_ffff;
    if exp == 0xff {
        return sign | 0x7c00 | if man != 0 { 0x200 } else { 0 };
    }
    let e = exp - 127 + 15;
    if e >= 0x1f {
        return sign | 0x7c00;
    }
    // A normal half keeps 10 of the 23 mantissa bits; a subnormal one shifts the implicit 1 in.
    let (m, shift, base) = if e > 0 {
        (man, 13u32, (e as u32) << 10)
    } else if e >= -10 {
        (man | 0x80_0000, (14 - e) as u32, 0)
    } else {
        return sign;
    };
    let kept = m >> shift;
    let rem = m & ((1 << shift) - 1);
    let half = 1 << (shift - 1);
    let up = u32::from(rem > half || (rem == half && kept & 1 == 1));
    // A carry out of the mantissa rolls into the exponent, which is the right answer.
    sign | (base + kept + up) as u16
}

/// The JPEG XR pixels: scRGB half-float RGBA (alpha 1.0), little-endian, top-down.
fn scrgb_halves(img: &HdrImage) -> Vec<u8> {
    const ONE: u16 = 0x3c00;
    let mut out = Vec::with_capacity(img.rgb.len() / 3 * 8);
    for &px in img.rgb.as_chunks::<3>().0 {
        for v in px {
            out.extend_from_slice(&f32_to_half(finite(v) / SCRGB_NITS).to_le_bytes());
        }
        out.extend_from_slice(&ONE.to_le_bytes());
    }
    out
}

/// Encode `img` as JPEG XR HDR through WIC. See the module docs.
pub(super) fn encode_jxr(img: &HdrImage) -> Option<Vec<u8>> {
    // The overlay's thread has COM already; the editor-less instant capture may not.
    let _com = st2k_base::parallel::ComGuard::sta();
    match unsafe { jxr_bytes(img) } {
        Ok(bytes) => Some(bytes),
        Err(e) => {
            st2k_base::safety::log(&format!("screenshot: JPEG XR encode failed: {e}"));
            None
        }
    }
}

unsafe fn jxr_bytes(img: &HdrImage) -> Result<Vec<u8>> {
    let (w, h) = (
        u32::try_from(img.w).map_err(|_| Error::from(E_FAIL))?,
        u32::try_from(img.h).map_err(|_| Error::from(E_FAIL))?,
    );
    let factory: IWICImagingFactory =
        CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
    let stream = SHCreateMemStream(None).ok_or_else(|| Error::from(E_OUTOFMEMORY))?;
    let encoder = factory.CreateEncoder(&GUID_ContainerFormatWmp, std::ptr::null())?;
    encoder.Initialize(&stream, WICBitmapEncoderNoCache)?;
    write_jxr_frame(&encoder, w, h, img)?;
    encoder.Commit()?;
    stream_bytes(&stream)
}

/// The one frame of a JPEG XR file: lossless, `w` x `h`, scRGB half-float.
unsafe fn write_jxr_frame(
    encoder: &IWICBitmapEncoder,
    w: u32,
    h: u32,
    img: &HdrImage,
) -> Result<()> {
    let (mut frame, mut options) = (None, None);
    encoder.CreateNewFrame(&mut frame, &mut options)?;
    let frame = frame.ok_or_else(|| Error::from(E_FAIL))?;
    if let Some(bag) = &options {
        set_lossless(bag)?;
    }
    frame.Initialize(options.as_ref())?;
    frame.SetSize(w, h)?;
    set_half_float_format(&frame)?;
    frame.WritePixels(h, w * 8, &scrgb_halves(img))?;
    frame.Commit()
}

/// Ask `frame` for scRGB half-float RGBA, and refuse when the encoder would convert to
/// something else rather than save a lie.
unsafe fn set_half_float_format(frame: &IWICBitmapFrameEncode) -> Result<()> {
    let want: GUID = GUID_WICPixelFormat64bppRGBAHalf;
    let mut format = want;
    frame.SetPixelFormat(&mut format)?;
    if format == want {
        Ok(())
    } else {
        Err(Error::from(E_FAIL))
    }
}

/// Ask the JPEG XR encoder for a lossless file: an HDR capture is chosen for fidelity.
unsafe fn set_lossless(bag: &IPropertyBag2) -> Result<()> {
    let mut name: Vec<u16> = "Lossless\0".encode_utf16().collect();
    let option = PROPBAG2 {
        pstrName: PWSTR(name.as_mut_ptr()),
        ..Default::default()
    };
    let value = VARIANT::from(true);
    bag.Write(1, &option, &value)
}

/// Everything written to an in-memory stream.
unsafe fn stream_bytes(stream: &IStream) -> Result<Vec<u8>> {
    let mut stat = STATSTG::default();
    stream.Stat(&mut stat, STATFLAG_NONAME)?;
    let len = u32::try_from(stat.cbSize).map_err(|_| Error::from(E_OUTOFMEMORY))?;
    stream.Seek(0, STREAM_SEEK_SET, None)?;
    let mut bytes = vec![0u8; len as usize];
    let mut read = 0u32;
    stream
        .Read(bytes.as_mut_ptr().cast(), len, Some(&mut read))
        .ok()?;
    bytes.truncate(read as usize);
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(w: i32, h: i32) -> RECT {
        RECT {
            left: 0,
            top: 0,
            right: w,
            bottom: h,
        }
    }

    #[test]
    fn half_float_decodes() {
        assert_eq!(half_to_f32(0x0000), 0.0);
        assert_eq!(half_to_f32(0x3C00), 1.0);
        assert_eq!(half_to_f32(0x4000), 2.0);
        assert_eq!(half_to_f32(0xBC00), -1.0);
        assert!((half_to_f32(0x3555) - 0.333).abs() < 0.001);
        assert!(half_to_f32(0x7C00).is_infinite());
        assert!(half_to_f32(0x0001) > 0.0 && half_to_f32(0x0001) < 1e-6);
    }

    /// The JPEG XR path writes halves: known encodings, rounding, and the range ends.
    #[test]
    fn half_float_encodes_and_round_trips() {
        assert_eq!(f32_to_half(0.0), 0x0000);
        assert_eq!(f32_to_half(1.0), 0x3C00);
        assert_eq!(f32_to_half(2.5), 0x4100);
        assert_eq!(f32_to_half(-1.0), 0xBC00);
        assert_eq!(f32_to_half(65_504.0), 0x7BFF);
        assert_eq!(f32_to_half(1e6), 0x7C00, "past the range is infinity");
        assert_eq!(f32_to_half(1e-9), 0x0000, "below the range is zero");
        assert_eq!(
            f32_to_half(half_to_f32(0x0001)),
            0x0001,
            "smallest subnormal"
        );
        for h in [
            0x0001u16, 0x03FF, 0x0400, 0x3555, 0x3C01, 0x4B00, 0x7BFF, 0xC120,
        ] {
            assert_eq!(f32_to_half(half_to_f32(h)), h, "{h:#06x}");
        }
    }

    /// Reference points of the ST 2084 curve.
    #[test]
    fn pq_matches_st2084() {
        assert_eq!(pq(0.0), (3424.0f32 / 4096.0).powf(2523.0 / 4096.0 * 128.0));
        assert!((pq(10_000.0) - 1.0).abs() < 1e-6);
        for (nits, signal) in [(100.0, 0.5081), (203.0, 0.5807), (1000.0, 0.7518)] {
            assert!(
                (pq(nits) - signal).abs() < 5e-4,
                "{nits} nits gave {}",
                pq(nits)
            );
        }
        assert_eq!(pq(f32::NAN), pq(0.0));
    }

    /// A neutral stays neutral through the primaries change, so white is still white.
    #[test]
    fn grey_survives_the_bt2020_matrix() {
        let [r, g, b] = bt709_to_bt2020([100.0, 100.0, 100.0]);
        assert!((r - 100.0).abs() < 0.01 && (g - 100.0).abs() < 0.01 && (b - 100.0).abs() < 0.01);
        assert_eq!(bt709_to_bt2020([-5.0, 0.0, 0.0]), [0.0, 0.0, 0.0]);
    }

    /// A capture with no HDR monitor puts its white at BT.2408's reference white.
    #[test]
    fn sdr_capture_sits_at_reference_white() {
        let bgra = [255, 255, 255, 255, 0, 0, 0, 255];
        let img = region_image(None, rect(2, 1), &bgra, &bgra).expect("image");
        assert_eq!((img.w, img.h), (2, 1));
        assert_eq!(&img.rgb[..3], &[SDR_REFERENCE_WHITE_NITS; 3]);
        assert_eq!(&img.rgb[3..], &[0.0; 3]);
    }

    /// The HDR capture shows through where the editor left a pixel alone; an annotated pixel
    /// takes its 8-bit colour at the monitor's SDR white, as it looked in the editor.
    #[test]
    fn annotations_land_at_the_monitors_sdr_white() {
        // A 2x1 monitor at overlay x 1..3 whose SDR white is 240 nits, holding a 1000-nit
        // highlight and a 0.5 scRGB grey.
        let tile = HdrTile {
            dest: RECT {
                left: 1,
                top: 0,
                right: 3,
                bottom: 1,
            },
            src_w: 2,
            src_h: 1,
            rgb: [12.5f32, 12.5, 12.5, 0.5, 0.5, 0.5]
                .map(f32_to_half)
                .to_vec(),
            white_nits: 240.0,
        };
        let shot = HdrShot { tiles: vec![tile] };
        // Region = overlay x 0..3: an SDR pixel, the highlight, and the grey that an
        // annotation painted pure red.
        let plain = [10, 10, 10, 255, 255, 255, 255, 255, 180, 180, 180, 255];
        let mut composed = plain;
        composed[8..11].copy_from_slice(&[0, 0, 255]);
        let img = region_image(Some(&shot), rect(3, 1), &composed, &plain).expect("image");
        let lut = srgb_to_linear_table();
        assert_eq!(
            &img.rgb[0..3],
            &[lut[10] * 240.0; 3],
            "SDR pixel at the HDR monitor's white"
        );
        assert_eq!(
            &img.rgb[3..6],
            &[1000.0; 3],
            "the highlight keeps its 1000 nits"
        );
        assert_eq!(
            &img.rgb[6..9],
            &[240.0, 0.0, 0.0],
            "the annotation at SDR white"
        );
    }

    /// Buffers that do not match the region are refused, not read past.
    #[test]
    fn mismatched_buffers_are_refused() {
        assert!(region_image(None, rect(4, 4), &[0; 8], &[0; 8]).is_none());
        let inverted = RECT {
            left: 5,
            top: 0,
            right: 1,
            bottom: 1,
        };
        assert!(region_image(None, inverted, &[0; 64], &[0; 64]).is_none());
    }

    /// The PNG chunks in file order.
    fn chunks(png: &[u8]) -> Vec<([u8; 4], Vec<u8>)> {
        let mut out = Vec::new();
        let mut at = 8;
        while at + 8 <= png.len() {
            let len = u32::from_be_bytes(png[at..at + 4].try_into().expect("len")) as usize;
            let kind: [u8; 4] = png[at + 4..at + 8].try_into().expect("type");
            out.push((kind, png[at + 8..at + 8 + len].to_vec()));
            at += 12 + len;
        }
        out
    }

    /// PNG HDR: a 16-bit RGB PNG whose `cICP` (BT.2020, PQ) and `cLLI` come before IDAT, that
    /// decodes back with the tag and the PQ code values of what was captured.
    #[test]
    fn png_hdr_is_tagged_bt2020_pq_and_decodes_back() {
        let img = HdrImage {
            w: 2,
            h: 1,
            rgb: vec![203.0, 203.0, 203.0, 1000.0, 1000.0, 1000.0],
        };
        let bytes = encode_png_pq(&img).expect("encoded");
        assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
        let found = chunks(&bytes);
        let at = |k: &[u8; 4]| found.iter().position(|(t, _)| t == k);
        let (ihdr, cicp, clli_at, idat) = (at(b"IHDR"), at(b"cICP"), at(b"cLLI"), at(b"IDAT"));
        assert_eq!(found[ihdr.expect("IHDR")].1[8..10], [16, 2], "16-bit RGB");
        assert_eq!(found[cicp.expect("cICP")].1, CICP_BT2020_PQ);
        assert!(
            cicp < idat && clli_at < idat,
            "cICP and cLLI must precede IDAT"
        );
        let light = &found[clli_at.expect("cLLI")].1;
        let max_cll = u32::from_be_bytes(light[..4].try_into().expect("MaxCLL"));
        assert!(
            (max_cll as f32 / 10_000.0 - 1000.0).abs() < 1.0,
            "MaxCLL {max_cll}"
        );

        let mut reader = png::Decoder::new(std::io::Cursor::new(&bytes))
            .read_info()
            .expect("decodes");
        let cicp = reader
            .info()
            .coding_independent_code_points
            .expect("the decoder sees cICP");
        assert_eq!(
            (cicp.color_primaries, cicp.transfer_function),
            (9, 16),
            "BT.2020 primaries, PQ"
        );
        let mut buf = vec![0; reader.output_buffer_size().expect("size")];
        reader.next_frame(&mut buf).expect("pixels");
        let sample = |i: usize| u16::from_be_bytes([buf[i * 2], buf[i * 2 + 1]]);
        let want = |nits: f32| (pq(nits) * 65_535.0 + 0.5) as u16;
        for c in 0..3 {
            assert!(
                sample(c).abs_diff(want(203.0)) <= 2,
                "SDR white channel {c}"
            );
            assert!(
                sample(3 + c).abs_diff(want(1000.0)) <= 2,
                "highlight channel {c}"
            );
        }
    }

    /// JPEG XR HDR: WIC writes it, and WIC reads the same picture back.
    #[test]
    fn jxr_hdr_encodes_and_decodes_back() {
        let img = HdrImage {
            w: 3,
            h: 2,
            rgb: (0..18).map(|i| i as f32 * 40.0).collect(),
        };
        let bytes = encode_jxr(&img).expect("WIC's JPEG XR encoder is in-box");
        assert!(bytes.starts_with(b"II\xbc"), "JPEG XR magic");
        // WIC decodes on a COM thread; the encoder dropped its own apartment.
        let _com = st2k_base::parallel::ComGuard::sta();
        let back = st2k_codecs::decode::decode_full(&bytes).expect("decodes");
        assert_eq!((back.width(), back.height()), (3, 2));
    }
}
