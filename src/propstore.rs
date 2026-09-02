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
//!
//! The coclass is registered `ThreadingModel=Both` (`register.rs`), so an MTA host such as
//! `SearchIndexer.exe` may call it from several threads at once with no COM serialisation.
//! Its two pieces of state therefore sit behind `Mutex`es, taken with `try_lock`: a call that
//! would have to wait (or that arrives re-entrantly on the same thread) gets `E_FAIL` for
//! that one query rather than blocking an indexing thread or racing a `RefCell` borrow flag.

use core::mem::ManuallyDrop;
use std::sync::Mutex;

use windows::core::{Error, Result, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    E_FAIL, E_INVALIDARG, E_POINTER, FILETIME, PROPERTYKEY, STG_E_ACCESSDENIED, SYSTEMTIME,
};
use windows::Win32::Storage::EnhancedStorage::{
    PKEY_Audio_EncodingBitrate, PKEY_GPS_LatitudeDecimal, PKEY_GPS_LongitudeDecimal,
    PKEY_Image_BitDepth, PKEY_Image_Dimensions, PKEY_Image_HorizontalResolution,
    PKEY_Image_HorizontalSize, PKEY_Image_VerticalResolution, PKEY_Image_VerticalSize,
    PKEY_Media_Duration, PKEY_Media_Year, PKEY_Music_AlbumTitle, PKEY_Music_Artist,
    PKEY_Music_Genre, PKEY_Music_TrackNumber, PKEY_Photo_CameraManufacturer,
    PKEY_Photo_CameraModel, PKEY_Photo_DateTaken, PKEY_Title, PKEY_Video_FrameHeight,
    PKEY_Video_FrameWidth,
};
use windows::Win32::System::Com::CoTaskMemAlloc;
use windows::Win32::System::Com::StructuredStorage::{
    InitPropVariantFromFileTime, InitPropVariantFromStringVector, PROPVARIANT, PROPVARIANT_0,
    PROPVARIANT_0_0, PROPVARIANT_0_0_0,
};
use windows::Win32::System::Time::{SystemTimeToFileTime, TzSpecificLocalTimeToSystemTime};
use windows::Win32::System::Variant::VT_LPWSTR;
use windows::Win32::UI::Shell::PropertiesSystem::{
    IInitializeWithFile, IInitializeWithFile_Impl, IPropertyStore, IPropertyStore_Impl,
};
use windows_implement::implement;

use crate::safety;

/// Hard wall-clock cap on one in-process metadata query (see
/// [`PropertyStore_Impl::build_props`]).  Explorer and SearchIndexer call property handlers on
/// their UI/indexing paths, so this must be a *small* latency budget, not the multi-second
/// budget appropriate for an explicit preview.  On expiry we return no properties; the shell can
/// continue immediately and a later property-store instance can try again.
const PROBE_BUDGET: core::time::Duration = core::time::Duration::from_millis(250);
/// Timed-out metadata reads cannot be cancelled safely. Keep a slow remote/provider file from
/// leaving an unbounded trail of detached workers while Explorer enumerates a directory.
const MAX_ACTIVE_PROBES: usize = 2;

/// How long one detached worker may hold its slot before the slot is reclaimed for a new
/// probe (see [`safety::LeasePool`] for why a lease and not a counter: two hung reads used
/// to exhaust both slots for the life of the host, blanking the Details pane for every file
/// after them). Generous on purpose: it bounds the damage from a hung read without cutting
/// short a slow one that would have succeeded.
const PROBE_LEASE_MS: u64 = 30_000;

/// The probe slots. A worker that finishes normally releases its slot immediately; one that
/// hangs loses it at lease expiry.
static PROBE_POOL: safety::LeasePool<MAX_ACTIVE_PROBES> = safety::LeasePool::new(PROBE_LEASE_MS);

/// `Mutex`, not `RefCell`: see the module doc. Nothing here is `Sync` (a `PROPVARIANT` holds
/// raw pointers), which is the same as before; the lock is what makes concurrent calls from
/// an MTA host fail cleanly instead of racing the borrow flag.
#[implement(IPropertyStore, IInitializeWithFile)]
pub struct PropertyStore {
    _ref: crate::ModuleRef,
    path: Mutex<Option<String>>,
    /// Built lazily from the file on the first query, then cached for this instance.
    props: Mutex<Option<Vec<(PROPERTYKEY, PROPVARIANT)>>>,
}

impl Default for PropertyStore {
    #[allow(clippy::default_constructed_unit_structs)]
    fn default() -> Self {
        Self {
            _ref: crate::ModuleRef::default(),
            path: Mutex::new(None),
            props: Mutex::new(None),
        }
    }
}

impl IInitializeWithFile_Impl for PropertyStore_Impl {
    fn Initialize(&self, pszfilepath: &PCWSTR, _grfmode: u32) -> Result<()> {
        safety::guard(|| {
            // `PCWSTR::as_wide` (called inside `to_string`) runs an unconditional `wcslen`
            // on the raw pointer — a null here is a hard access violation, not a catchable
            // Rust panic, unlike the other raw-pointer args in this file (GetAt/GetValue both
            // null-check first).
            if pszfilepath.is_null() {
                return Err(Error::from(E_POINTER));
            }
            let path = unsafe { pszfilepath.to_string() }.map_err(|_| Error::from(E_FAIL))?;
            // A host is free to re-Initialize one PropertyStore instance across several files
            // (a documented, real shell pattern) — without clearing the cache here,
            // `with_props`'s `get_or_insert_with` only ever builds it on the FIRST query, so
            // every query after that silently answers with the PREVIOUS file's metadata under
            // the new file's path, with no error to signal it. The cache lock is taken FIRST
            // and held across both writes: a query on another thread (ThreadingModel=Both,
            // no COM serialisation) locks only `props`, so this makes the path swap and the
            // clear one step — nothing can read the old cache under the new path. Same lock
            // order as `with_props` -> `build_props` (props, then path), so no deadlock.
            let mut props = self.props.try_lock().map_err(|_| Error::from(E_FAIL))?;
            *self.path.try_lock().map_err(|_| Error::from(E_FAIL))? = Some(path);
            *props = None;
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
                let entry = p
                    .get(iprop as usize)
                    .ok_or_else(|| Error::from(E_INVALIDARG))?;
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
                Ok(p.iter()
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
    /// Run `f` against the (lazily built, cached) property list. `try_lock`: a second thread
    /// (or a re-entrant call) arriving while the list is being built gets `E_FAIL` for that
    /// query instead of blocking an indexing thread; a poisoned lock is unreachable under
    /// `panic = "abort"` and is treated the same way.
    fn with_props<T>(
        &self,
        f: impl FnOnce(&[(PROPERTYKEY, PROPVARIANT)]) -> Result<T>,
    ) -> Result<T> {
        let mut slot = self.props.try_lock().map_err(|_| Error::from(E_FAIL))?;
        let props = slot.get_or_insert_with(|| self.build_props());
        f(props)
    }

    /// Extract the properties from the file. Never fails loudly — returns whatever it could read.
    fn build_props(&self) -> Vec<(PROPERTYKEY, PROPVARIANT)> {
        let mut out = Vec::new();
        let Some(path) = self.path.try_lock().ok().and_then(|p| p.clone()) else {
            return out;
        };

        // Probe off the host thread under a short wall-clock budget.  This coclass loads
        // IN-PROCESS into Explorer, SearchIndexer, and file-open dialogs, so metadata must never
        // become a full-fidelity image conversion. `read_info_bounded` uses only decoder/container
        // headers plus EXIF; audio tags are retained when their parser completes within the same
        // cheap budget. Only PLAIN data crosses back; the `PROPVARIANT`s are built on this COM
        // thread.
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
        let is_video = matches!(
            crate::formats::category(&ext),
            crate::formats::Category::Video
        );

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
            out.push((
                PKEY_Image_HorizontalResolution,
                PROPVARIANT::from(info.dpi_x),
            ));
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
            out.push((
                PKEY_Media_Duration,
                PROPVARIANT::from(tags.duration_ms.saturating_mul(10_000)),
            ));
        }
        // System.Audio.EncodingBitrate is bits-per-second (VT_UI4); kbps × 1000.
        if tags.bitrate_kbps > 0 {
            out.push((
                PKEY_Audio_EncodingBitrate,
                PROPVARIANT::from(tags.bitrate_kbps.saturating_mul(1000)),
            ));
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
    let wide = crate::wide(s);
    // Overflow-safe byte count, matching command.rs::alloc_pwstr's guard: can't actually
    // overflow for any real string, but keep the allocation provably sound rather than
    // wrapping into an under-sized CoTaskMemAlloc.
    let Some(bytes) = checked_utf16_byte_len(wide.len()) else {
        return PROPVARIANT::default();
    };
    unsafe {
        let p = CoTaskMemAlloc(bytes) as *mut u16;
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

/// Overflow-safe UTF-16 byte length (`len * size_of::<u16>()`, checked). Split out from
/// `pv_lpwstr` so the arithmetic itself is unit-testable without needing a near-`usize::MAX`
/// element `Vec<u16>` just to exercise the overflow branch.
fn checked_utf16_byte_len(len: usize) -> Option<usize> {
    len.checked_mul(2)
}

/// Build a `VT_VECTOR | VT_LPWSTR` PROPVARIANT for the multi-value string keys. System.Music.Artist
/// and System.Music.Genre carry the `PDTF_MULTIPLEVALUES` schema flag, so a scalar string is the
/// wrong canonical type for the index — these must be a string vector (one element here, since our
/// extractors yield a single value). `InitPropVariantFromStringVector` copies the strings.
fn pv_lpwstr_vec(s: &str) -> PROPVARIANT {
    let wide = crate::wide(s);
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

/// Run the header/metadata-only file probe ([`crate::strip::read_info_bounded`] + audio tags) on
/// a detached worker, returning only if it finishes within [`PROBE_BUDGET`], so the calling
/// shell thread blocks for at most 250 ms. This deliberately does not initialize COM: the
/// bounded image probe never invokes WIC, WinRT, ImageMagick, or a pixel decode.
/// `ImageInfo`/`AudioTags` are plain `Send` data; no COM object crosses the channel.
///
/// Built on [`safety::spawn_budgeted`] (shared with `previewhandler.rs`'s decode and `ocr.rs`'s
/// recognizer) — see that function's doc for the ModuleRef-pin / slot-guard / spawn-failure
/// contract this relies on.
fn probe_budgeted(path: String) -> Option<(crate::strip::ImageInfo, crate::strip::AudioTags)> {
    // Acquired here (not inside the worker closure) and moved into `op` below, so
    // `spawn_budgeted`'s spawn-failure path drops it (and so releases the slot) exactly like a
    // normal worker exit would — see that function's doc.
    let lease = PROBE_POOL.acquire()?;

    safety::spawn_budgeted("st2k-property-probe", PROBE_BUDGET, move || {
        let _lease = lease;
        let is_audio = property_path_is_audio(&path);
        let tags = if is_audio {
            crate::strip::read_audio_tags(&path)
        } else {
            crate::strip::AudioTags::default()
        };
        let info = if is_audio {
            crate::strip::ImageInfo::default()
        } else {
            crate::strip::read_info_bounded(&path)
        };
        (info, tags)
    })
}

fn property_path_is_audio(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .is_some_and(|ext| crate::formats::category(&ext) == crate::formats::Category::Audio)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `checked_utf16_byte_len` must catch the overflow instead of silently wrapping into an
    /// under-sized `CoTaskMemAlloc` — the plain `len * 2` this replaced would wrap (release
    /// builds run with `overflow-checks` off) rather than error, handing `pv_lpwstr` a buffer
    /// too small for the UTF-16 copy that follows.
    #[test]
    fn checked_utf16_byte_len_catches_overflow_instead_of_wrapping() {
        assert_eq!(
            checked_utf16_byte_len(usize::MAX),
            None,
            "a byte length that can't fit in usize must be rejected, not wrapped"
        );
        assert_eq!(checked_utf16_byte_len(4), Some(8));
    }

    #[test]
    fn expensive_audio_probe_is_extension_gated() {
        assert!(property_path_is_audio(r"C:\media\track.FLAC"));
        assert!(!property_path_is_audio(r"C:\photos\image.jpg"));
        assert!(!property_path_is_audio(r"C:\photos\extensionless"));
    }

    /// The probe slots must be a LEASE, not a permanent claim (the policy itself is pinned by
    /// `safety::worker_tests::hung_holders_lose_their_slot_when_the_lease_expires`); this
    /// pins that the pool this coclass actually uses is sized and leased as documented.
    #[test]
    fn probe_pool_holds_the_documented_cap() {
        let t0 = 5_000_000u64;
        let held: Vec<safety::Lease> = (0..MAX_ACTIVE_PROBES)
            .map(|_| PROBE_POOL.acquire_at(t0).expect("slot available"))
            .collect();
        assert!(
            PROBE_POOL.acquire_at(t0).is_none(),
            "the concurrency cap must bound live probes"
        );
        assert!(
            PROBE_POOL.acquire_at(t0 + PROBE_LEASE_MS + 1).is_some(),
            "an expired lease must be reclaimable, or the outage is permanent"
        );
        drop(held);
    }

    /// A COM host is free to re-Initialize one `PropertyStore` instance across several files
    /// (a documented, real shell pattern). `with_props`'s `get_or_insert_with` only ever
    /// builds the cache on the FIRST query, so without clearing `props` in `Initialize`, every
    /// query after a re-Initialize would silently keep answering with the PREVIOUS file's
    /// metadata under the new path — no error, just wrong data.
    #[test]
    fn reinitialize_clears_the_previous_files_cached_props() {
        // `#[implement]` moves the real fields onto an inner `PropertyStore`, reachable
        // through `windows::core::ComObject`'s `Deref`/`get()` — the generated `_Impl`
        // wrapper type the trait is written against can't be named or constructed from
        // application code, so this is the supported way to drive the real `Initialize`
        // trait method (through the actual `IInitializeWithFile` vtable, exactly like a COM
        // host would) while still being able to peek at the cache field afterward.
        let com = windows::core::ComObject::new(PropertyStore::default());
        // Seed a stale cache directly, as if a first Initialize + query already ran for a
        // different file — cheaper and more deterministic than driving a real probe through
        // GetCount/GetValue.
        *com.get().props.lock().unwrap() = Some(vec![(PKEY_Title, PROPVARIANT::default())]);

        let init: IInitializeWithFile = com.to_interface();
        let w = crate::wide(r"C:\second\file.jpg");
        let pc = PCWSTR(w.as_ptr());
        unsafe { init.Initialize(pc, 0) }.expect("Initialize should succeed");

        assert!(
            com.get().props.lock().unwrap().is_none(),
            "Initialize must drop the previous file's cached props, or with_props's \
             get_or_insert_with never rebuilds them for the new path"
        );
    }

    /// `PCWSTR::as_wide` (called inside `to_string`) runs an unconditional `wcslen` on the raw
    /// pointer — a null here used to be a hard access violation (not a catchable Rust panic),
    /// in a coclass any local caller can load in-process. Now it must be a clean `Err`.
    #[test]
    fn initialize_with_null_path_returns_an_error_instead_of_crashing() {
        let com = windows::core::ComObject::new(PropertyStore::default());
        let init: IInitializeWithFile = com.to_interface();
        let pc = PCWSTR(core::ptr::null());
        assert!(
            unsafe { init.Initialize(pc, 0) }.is_err(),
            "a null pszFilePath must be rejected before it reaches PCWSTR::to_string()"
        );
    }

    /// `probe_budgeted`'s worker deliberately never touches COM (see its own doc comment): a
    /// WIC-backed fallback was added to this call chain once before and silently violated that,
    /// blanking every Details-pane probe with no error. Run the exact calls the worker makes on
    /// a fresh thread and confirm COM is still uninitialized afterward — `CoInitializeEx`
    /// returning `S_OK` rather than `S_FALSE` is the "this thread never called it before" signal.
    #[test]
    fn probe_worker_path_never_touches_com() {
        use windows::Win32::Foundation::S_FALSE;
        use windows::Win32::System::Com::{
            CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED,
        };

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .spawn(move || {
                let path = r"C:\definitely\does\not\exist\st2k_com_probe_test.jpg".to_string();
                // Mirrors probe_budgeted's worker body exactly, for a non-audio extension.
                let is_audio = property_path_is_audio(&path);
                let _tags = if is_audio {
                    crate::strip::read_audio_tags(&path)
                } else {
                    crate::strip::AudioTags::default()
                };
                let _info = if is_audio {
                    crate::strip::ImageInfo::default()
                } else {
                    crate::strip::read_info_bounded(&path)
                };
                let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
                let already_inited = hr == S_FALSE;
                if hr.is_ok() {
                    unsafe { CoUninitialize() };
                }
                let _ = tx.send(!already_inited);
            })
            .expect("spawn probe-mirroring thread");
        let clean = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap_or(false);
        assert!(
            clean,
            "probe_budgeted's worker path initialized COM somewhere in its call chain — this \
             coclass deliberately keeps that path COM-free (see probe_budgeted's doc comment)"
        );
    }
}
