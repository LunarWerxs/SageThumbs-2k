//! `IPropertyStore` — surface the metadata we already extract (image dimensions + EXIF
//! camera, audio artist/album/title/track) into Explorer's **Details pane, hover info-tips,
//! and sortable/groupable columns** for the formats Windows can't read natively. The same
//! data the right-click "Image info" tile shows, now where the shell wants it.
//!
//! READ-ONLY: `SetValue`/`Commit` are refused. This coclass loads **in-process** into
//! `explorer.exe` AND `SearchIndexer.exe`, so — exactly like the thumbnail provider — every COM
//! entry point is wrapped in [`safety::guard`], the crate is `panic = "abort"` with a
//! `catch_unwind` at the boundary, and the file probe is bounded. A malformed/hostile file must
//! never crash the host: on any failure we just expose no properties.
//!
//! We initialize via `IInitializeWithFile` (the shell hands us the file PATH). The thumbnail
//! provider uses `IInitializeWithStream`, but the property host's stream carries no name, so the
//! path-based extractors (`read_info`/`read_audio_tags`) need the real path. Properties are built
//! LAZILY on the first query, so the indexer pays nothing until something actually asks.

use core::cell::RefCell;
use core::mem::ManuallyDrop;

use windows_implement::implement;
use windows::core::{Error, Result, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    E_FAIL, E_INVALIDARG, FILETIME, PROPERTYKEY, STG_E_ACCESSDENIED, SYSTEMTIME,
};
use windows::Win32::System::Com::CoTaskMemAlloc;
use windows::Win32::System::Variant::VT_LPWSTR;
use windows::Win32::Storage::EnhancedStorage::{
    PKEY_Audio_EncodingBitrate, PKEY_GPS_LatitudeDecimal, PKEY_GPS_LongitudeDecimal,
    PKEY_Image_BitDepth, PKEY_Image_Dimensions, PKEY_Image_HorizontalResolution,
    PKEY_Image_HorizontalSize, PKEY_Image_VerticalResolution, PKEY_Image_VerticalSize,
    PKEY_Media_Duration, PKEY_Media_Year, PKEY_Music_AlbumTitle, PKEY_Music_Artist,
    PKEY_Music_Genre, PKEY_Music_TrackNumber, PKEY_Photo_CameraManufacturer, PKEY_Photo_CameraModel,
    PKEY_Photo_DateTaken, PKEY_Title, PKEY_Video_FrameHeight, PKEY_Video_FrameWidth,
};
use windows::Win32::System::Com::StructuredStorage::{
    InitPropVariantFromFileTime, InitPropVariantFromStringVector, PROPVARIANT, PROPVARIANT_0,
    PROPVARIANT_0_0, PROPVARIANT_0_0_0,
};
use windows::Win32::System::Time::{SystemTimeToFileTime, TzSpecificLocalTimeToSystemTime};
use windows::Win32::UI::Shell::PropertiesSystem::{
    IInitializeWithFile, IInitializeWithFile_Impl, IPropertyStore, IPropertyStore_Impl,
};

use crate::safety;

/// Hard wall-clock cap on the in-process file probe (see [`PropertyStore_Impl::build_props`]).
/// The bounded probe normally returns in well under this; the budget only ever elapses for a
/// pathological in-cap file — a slow ImageMagick decode tail, a network / cloud-placeholder
/// stall — and on expiry we expose NO properties rather than freeze Explorer / SearchIndexer /
/// the host's file-open dialog. Same discipline as the preview handler's `decode_preview_budgeted`.
const PROBE_BUDGET: core::time::Duration = core::time::Duration::from_secs(3);

#[implement(IPropertyStore, IInitializeWithFile)]
pub struct PropertyStore {
    _ref: crate::ModuleRef,
    path: RefCell<Option<String>>,
    /// Built lazily from the file on the first query, then cached for this instance.
    props: RefCell<Option<Vec<(PROPERTYKEY, PROPVARIANT)>>>,
}

impl Default for PropertyStore {
    #[allow(clippy::default_constructed_unit_structs)]
    fn default() -> Self {
        Self {
            _ref: crate::ModuleRef::default(),
            path: RefCell::new(None),
            props: RefCell::new(None),
        }
    }
}

impl IInitializeWithFile_Impl for PropertyStore_Impl {
    fn Initialize(&self, pszfilepath: &PCWSTR, _grfmode: u32) -> Result<()> {
        safety::guard(|| {
            let path = unsafe { pszfilepath.to_string() }.map_err(|_| Error::from(E_FAIL))?;
            *self.path.try_borrow_mut().map_err(|_| Error::from(E_FAIL))? = Some(path);
            Ok(())
        })
    }
}

impl IPropertyStore_Impl for PropertyStore_Impl {
    fn GetCount(&self) -> Result<u32> {
        safety::guard_val(|| self.with_props(|p| Ok(p.len() as u32)))
    }

    fn GetAt(&self, iprop: u32, pkey: *mut PROPERTYKEY) -> Result<()> {
        safety::guard_val(|| {
            if pkey.is_null() {
                return Err(Error::from(E_INVALIDARG));
            }
            self.with_props(|p| {
                let entry = p.get(iprop as usize).ok_or_else(|| Error::from(E_INVALIDARG))?;
                unsafe { *pkey = entry.0 };
                Ok(())
            })
        })
    }

    fn GetValue(&self, key: *const PROPERTYKEY) -> Result<PROPVARIANT> {
        safety::guard_val(|| {
            if key.is_null() {
                return Err(Error::from(E_INVALIDARG));
            }
            let want = unsafe { *key };
            self.with_props(|p| {
                // A property store returns an EMPTY variant (not an error) for keys it
                // doesn't carry — that's how the shell probes which properties exist.
                Ok(p
                    .iter()
                    .find(|(k, _)| k.fmtid == want.fmtid && k.pid == want.pid)
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default())
            })
        })
    }

    fn SetValue(&self, _key: *const PROPERTYKEY, _propvar: *const PROPVARIANT) -> Result<()> {
        Err(Error::from(STG_E_ACCESSDENIED)) // read-only
    }

    fn Commit(&self) -> Result<()> {
        Ok(()) // read-only: nothing to flush
    }
}

impl PropertyStore_Impl {
    /// Run `f` against the (lazily built, cached) property list.
    fn with_props<T>(&self, f: impl FnOnce(&[(PROPERTYKEY, PROPVARIANT)]) -> Result<T>) -> Result<T> {
        let mut slot = self.props.try_borrow_mut().map_err(|_| Error::from(E_FAIL))?;
        if slot.is_none() {
            *slot = Some(self.build_props());
        }
        f(slot.as_ref().unwrap())
    }

    /// Extract the properties from the file. Never fails loudly — returns whatever it could read.
    fn build_props(&self) -> Vec<(PROPERTYKEY, PROPVARIANT)> {
        let mut out = Vec::new();
        let Some(path) = self.path.borrow().clone() else {
            return out;
        };

        // Probe the file OFF the host thread under a wall-clock budget. This coclass loads
        // IN-PROCESS into Explorer, SearchIndexer, AND a host app's file-open dialog, so the
        // probe must never stall the caller: an oversized file is skipped (`read_info_bounded`),
        // and a slow in-cap decode is abandoned at `PROBE_BUDGET`, exposing no properties rather
        // than freezing the shell (selecting a large upload in Chrome's file picker used to lock
        // the browser here). Only PLAIN data (`ImageInfo` + `AudioTags`, both `Send`) crosses
        // back; the `PROPVARIANT`s below are built on THIS COM thread.
        let Some((info, tags)) = probe_budgeted(path.clone()) else {
            safety::log_debug(&format!(
                "PropStore::build_props: probe over budget or unreadable -> 0 props for {path}"
            ));
            return out;
        };

        let ext = std::path::Path::new(&path)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_default();
        let is_video = matches!(crate::formats::category(&ext), crate::formats::Category::Video);

        // Image dimensions + EXIF camera (same probe "Image info" uses, under the decode guards).
        if info.width > 0 && info.height > 0 {
            out.push((
                PKEY_Image_Dimensions,
                pv_lpwstr(&format!("{} x {}", info.width, info.height)),
            ));
            out.push((PKEY_Image_HorizontalSize, PROPVARIANT::from(info.width)));
            out.push((PKEY_Image_VerticalSize, PROPVARIANT::from(info.height)));
            // For the video formats that reach us (flv/ogv) the same geometry IS the frame
            // size — surface it under the video keys too, so the pane labels it correctly.
            if is_video {
                out.push((PKEY_Video_FrameWidth, PROPVARIANT::from(info.width)));
                out.push((PKEY_Video_FrameHeight, PROPVARIANT::from(info.height)));
            }
        }
        if let Some(make) = info.make.filter(|s| !s.is_empty()) {
            out.push((PKEY_Photo_CameraManufacturer, pv_lpwstr(&make)));
        }
        if let Some(model) = info.model.filter(|s| !s.is_empty()) {
            out.push((PKEY_Photo_CameraModel, pv_lpwstr(&model)));
        }
        // EXIF capture date → System.Photo.DateTaken (VT_FILETIME). Only when it parses.
        if let Some(dt) = info.datetime.as_deref().and_then(datetime_to_propvariant) {
            out.push((PKEY_Photo_DateTaken, dt));
        }
        if info.bit_depth > 0 {
            out.push((PKEY_Image_BitDepth, PROPVARIANT::from(info.bit_depth)));
        }
        if info.dpi_x > 0.0 {
            out.push((PKEY_Image_HorizontalResolution, PROPVARIANT::from(info.dpi_x)));
        }
        if info.dpi_y > 0.0 {
            out.push((PKEY_Image_VerticalResolution, PROPVARIANT::from(info.dpi_y)));
        }
        if let Some((lat, lon)) = info.gps {
            out.push((PKEY_GPS_LatitudeDecimal, PROPVARIANT::from(lat)));
            out.push((PKEY_GPS_LongitudeDecimal, PROPVARIANT::from(lon)));
        }

        // Audio tags (lofty + our ASF parser) — probed alongside `info` above. Empty for non-audio.
        if let Some(artist) = tags.artist.filter(|s| !s.is_empty()) {
            out.push((PKEY_Music_Artist, pv_lpwstr_vec(&artist))); // multi-value key
        }
        if let Some(album) = tags.album.filter(|s| !s.is_empty()) {
            out.push((PKEY_Music_AlbumTitle, pv_lpwstr(&album)));
        }
        if let Some(title) = tags.title.filter(|s| !s.is_empty()) {
            out.push((PKEY_Title, pv_lpwstr(&title)));
        }
        if let Some(track) = tags.track.filter(|&t| t > 0) {
            out.push((PKEY_Music_TrackNumber, PROPVARIANT::from(track)));
        }
        if let Some(genre) = tags.genre.filter(|s| !s.is_empty()) {
            out.push((PKEY_Music_Genre, pv_lpwstr_vec(&genre))); // multi-value key
        }
        if let Some(year) = tags.year.filter(|&y| y > 0) {
            out.push((PKEY_Media_Year, PROPVARIANT::from(year)));
        }
        // System.Media.Duration is in 100-nanosecond units (VT_UI8); ms × 10 000.
        if tags.duration_ms > 0 {
            out.push((PKEY_Media_Duration, PROPVARIANT::from(tags.duration_ms.saturating_mul(10_000))));
        }
        // System.Audio.EncodingBitrate is bits-per-second (VT_UI4); kbps × 1000.
        if tags.bitrate_kbps > 0 {
            out.push((PKEY_Audio_EncodingBitrate, PROPVARIANT::from(tags.bitrate_kbps.saturating_mul(1000))));
        }

        safety::log_debug(&format!(
            "PropStore::build_props: dims {}x{} -> {} props",
            info.width,
            info.height,
            out.len()
        ));
        out
    }
}

/// Build a `VT_LPWSTR` PROPVARIANT — the canonical type for single-string `System.*` properties.
/// `PROPVARIANT::from(&str)` makes a `VT_BSTR`; the Details pane coerces and displays that, but the
/// Windows SEARCH INDEXER rejects `VT_BSTR` for these keys, so property/`kind:` search never finds
/// the file. The string is `CoTaskMemAlloc`'d and OWNED by the variant — its `Drop`
/// (`PropVariantClear`) `CoTaskMemFree`s it. (Constructed the same way the `windows` crate builds
/// its own integer `From` impls; there is no single-string `InitPropVariantFromString` in this
/// crate version, only the vector form.)
fn pv_lpwstr(s: &str) -> PROPVARIANT {
    let wide: Vec<u16> = s.encode_utf16().chain(core::iter::once(0)).collect();
    unsafe {
        let p = CoTaskMemAlloc(wide.len() * 2) as *mut u16;
        if p.is_null() {
            return PROPVARIANT::default();
        }
        core::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len());
        PROPVARIANT {
            Anonymous: PROPVARIANT_0 {
                Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                    vt: VT_LPWSTR,
                    wReserved1: 0,
                    wReserved2: 0,
                    wReserved3: 0,
                    Anonymous: PROPVARIANT_0_0_0 { pwszVal: PWSTR(p) },
                }),
            },
        }
    }
}

/// Build a `VT_VECTOR | VT_LPWSTR` PROPVARIANT for the multi-value string keys. System.Music.Artist
/// and System.Music.Genre carry the `PDTF_MULTIPLEVALUES` schema flag, so a scalar string is the
/// wrong canonical type for the index — these must be a string vector (one element here, since our
/// extractors yield a single value). `InitPropVariantFromStringVector` copies the strings.
fn pv_lpwstr_vec(s: &str) -> PROPVARIANT {
    let wide: Vec<u16> = s.encode_utf16().chain(core::iter::once(0)).collect();
    let arr = [PCWSTR(wide.as_ptr())];
    unsafe { InitPropVariantFromStringVector(Some(&arr)) }.unwrap_or_default()
}

/// Build a `VT_FILETIME` PROPVARIANT from an EXIF datetime (`"YYYY:MM:DD HH:MM:SS"`, also
/// tolerating `-`/`/` date separators and trailing sub-seconds). Returns `None` for a
/// malformed or never-set (all-zero) stamp.
///
/// EXIF `DateTimeOriginal` is the camera's LOCAL wall-clock with no timezone. `System.Photo.DateTaken`
/// is a UTC `FILETIME` that the shell converts back to local for display — so we must convert the
/// local components to UTC FIRST (`TzSpecificLocalTimeToSystemTime`, using the machine's current
/// zone), or the displayed time would be shifted by the local UTC offset. With the conversion, the
/// Details pane shows the original wall-clock — matching Windows' own photo property handler.
fn datetime_to_propvariant(s: &str) -> Option<PROPVARIANT> {
    let (date, time) = s.split_once(' ')?;
    let d: Vec<&str> = date.split([':', '-', '/']).collect();
    let t: Vec<&str> = time.split([':', '.']).collect();
    if d.len() != 3 || t.len() < 3 {
        return None;
    }
    let num = |x: &str| x.trim().parse::<u16>().ok();
    let local = SYSTEMTIME {
        wYear: num(d[0])?,
        wMonth: num(d[1])?,
        wDay: num(d[2])?,
        wHour: num(t[0])?,
        wMinute: num(t[1])?,
        wSecond: num(t[2])?,
        wDayOfWeek: 0,
        wMilliseconds: 0,
    };
    if local.wYear == 0 || local.wMonth == 0 || local.wDay == 0 {
        return None; // a camera that never had its clock set writes 0000:00:00
    }
    let mut utc = SYSTEMTIME::default();
    unsafe { TzSpecificLocalTimeToSystemTime(None, &local, &mut utc) }.ok()?;
    let mut ft = FILETIME::default();
    unsafe { SystemTimeToFileTime(&utc, &mut ft) }.ok()?;
    unsafe { InitPropVariantFromFileTime(&ft) }.ok()
}

/// Run the bounded file probe ([`crate::strip::read_info_bounded`] + audio tags) on a detached
/// worker, returning its result only if it finishes within [`PROBE_BUDGET`]. On timeout returns
/// `None` and leaves the worker to finish and exit on its own (its send into the dropped channel
/// just errors), so the calling shell thread blocks for at most the budget. The worker takes its
/// OWN COM apartment for the WIC/WinRT decode tiers the dimension probe leans on for RAW/HEIC —
/// the host thread's apartment doesn't extend to a thread we spawned. `ImageInfo`/`AudioTags` are
/// plain `Send` data; no COM object crosses the channel.
fn probe_budgeted(path: String) -> Option<(crate::strip::ImageInfo, crate::strip::AudioTags)> {
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // Pin the DLL for this detached worker's whole lifetime. On timeout we return but
        // leave this thread running, and `DllCanUnloadNow` does NOT count it — so when the
        // host (e.g. a file-open dialog in Chrome) releases the property object on CLOSE, the
        // DLL could unload mid-probe → crash-on-close. Mirrors run_action_detached.
        #[allow(clippy::default_constructed_unit_structs)]
        let _module = crate::ModuleRef::default();
        // S_OK/S_FALSE took a COM ref → balance it with CoUninitialize; RPC_E_CHANGED_MODE
        // (already an MTA thread) did not, so it must NOT be balanced. Mirrors parallel.rs.
        let inited = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
        let probed = (
            crate::strip::read_info_bounded(&path),
            crate::strip::read_audio_tags(&path),
        );
        // All WIC objects the decode created are already dropped inside `read_info_bounded`,
        // so the apartment carries no live COM ref here.
        if inited {
            unsafe { CoUninitialize() };
        }
        let _ = tx.send(probed);
    });
    rx.recv_timeout(PROBE_BUDGET).ok()
}
