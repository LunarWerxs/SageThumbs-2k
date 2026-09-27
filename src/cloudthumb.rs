//! The cloud-folder thumbnail provider: IThumbnailProvider + IInitializeWithItem.
//!
//! Explorer asks exactly one provider for a thumbnail of a file inside a cloud sync folder: the
//! one named in the folder's `SyncRootManager` slot (see `register::cloud` for the measurement).
//! This class is what goes into that slot. For each file it:
//!
//!   1. when the file's data is on this PC (downloaded, not an online-only placeholder), asks the
//!      handler that would draw it anywhere else on disk: ours for our formats, run in this
//!      process, or whichever handler Windows has for the type;
//!   2. otherwise, or when that drew nothing, hands the file to the provider it replaced
//!      (OneDrive's own, which serves the cloud's thumbnails for online-only files).
//!
//! It never reads an online-only file: that would download it, which is the one thing a
//! thumbnail must not cause. Hosted out of process by the COM surrogate (`dllhost.exe`), like
//! the ordinary provider, so a crash stays out of Explorer.

use core::cell::{Cell, RefCell};

use windows::core::{Error, Interface, Ref, Result, GUID, HSTRING};
use windows::Win32::Foundation::{E_FAIL, E_POINTER};
use windows::Win32::Graphics::Gdi::HBITMAP;
use windows::Win32::Storage::FileSystem::GetFileAttributesW;
use windows::Win32::System::Com::{
    CLSIDFromString, CoCreateInstance, CoTaskMemFree, CLSCTX, CLSCTX_INPROC_SERVER,
    CLSCTX_LOCAL_SERVER, STGM_READ, STGM_SHARE_DENY_NONE,
};
use windows::Win32::UI::Shell::PropertiesSystem::{IInitializeWithFile, IInitializeWithStream};
use windows::Win32::UI::Shell::{
    AssocQueryStringW, IInitializeWithItem, IInitializeWithItem_Impl, IShellItem,
    IThumbnailProvider, IThumbnailProvider_Impl, SHCreateStreamOnFileEx, ASSOCF_INIT_DEFAULTTOSTAR,
    ASSOCSTR_SHELLEXTENSION, SIGDN_FILESYSPATH, WTSAT_UNKNOWN, WTS_ALPHATYPE,
};
use windows_implement::implement;

use st2k_base::guids::{
    CLSID_CLOUD_THUMB_PROVIDER, CLSID_THUMBNAIL_PROVIDER, THUMB_HANDLER_CATEGORY,
};
use st2k_base::{safety, settings};

use crate::thumbprovider::ThumbnailProvider;

/// `FILE_ATTRIBUTE_OFFLINE`: the data lives elsewhere.
const ATTR_OFFLINE: u32 = 0x0000_1000;
/// `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS`: reading the data would download it (an online-only or
/// partly downloaded cloud placeholder).
const ATTR_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;
const INVALID_FILE_ATTRIBUTES: u32 = u32::MAX;

#[implement(IThumbnailProvider, IInitializeWithItem)]
pub struct CloudThumbnailProvider {
    _ref: st2k_base::host::ModuleRef,
    item: RefCell<Option<IShellItem>>,
    mode: Cell<u32>,
}

impl Default for CloudThumbnailProvider {
    // ModuleRef::default()'s side effect (live-object add-ref) must run; keep the Default call.
    #[allow(clippy::default_constructed_unit_structs)]
    fn default() -> Self {
        Self {
            _ref: st2k_base::host::ModuleRef::default(),
            item: RefCell::new(None),
            mode: Cell::new(0),
        }
    }
}

impl IInitializeWithItem_Impl for CloudThumbnailProvider_Impl {
    fn Initialize(&self, psi: Ref<'_, IShellItem>, grfmode: u32) -> Result<()> {
        safety::guard(|| {
            let item = psi.ok()?;
            let mut slot = self
                .item
                .try_borrow_mut()
                .map_err(|_| Error::from(E_FAIL))?;
            *slot = Some(item.clone());
            self.mode.set(grfmode);
            Ok(())
        })
    }
}

impl IThumbnailProvider_Impl for CloudThumbnailProvider_Impl {
    fn GetThumbnail(
        &self,
        cx: u32,
        phbmp: *mut HBITMAP,
        pdwalpha: *mut WTS_ALPHATYPE,
    ) -> Result<()> {
        safety::guard(|| {
            if phbmp.is_null() || pdwalpha.is_null() {
                return Err(Error::from(E_POINTER));
            }
            unsafe {
                *phbmp = HBITMAP::default();
                *pdwalpha = WTSAT_UNKNOWN;
            }
            let item = self
                .item
                .try_borrow()
                .ok()
                .and_then(|b| b.clone())
                .ok_or_else(|| Error::from(E_FAIL))?;
            let (bmp, alpha) = self.thumbnail_for(&item, cx)?;
            unsafe {
                *phbmp = bmp;
                *pdwalpha = alpha;
            }
            Ok(())
        })
    }
}

/// One way of getting a thumbnail, in the order [`attempts`] lists them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Attempt {
    /// Our own pipeline, in this process.
    Ours,
    /// The handler Windows has for the file type outside cloud folders (this CLSID).
    TypeHandler(GUID),
    /// The provider our chain replaced in this sync folder (this CLSID).
    Replaced(GUID),
}

/// The attempts for one file, in order. `local` = the file's data is on this PC; `type_handler`
/// = the per-extension handler Windows resolves for it; `ours_enabled` = our thumbnails are on;
/// `replaced` = the provider this folder had before us. Pure, so the order is pinned without a
/// sync root: a local file gets the thumbnail it would get anywhere else on disk, and only
/// the replaced provider may ever see an online-only file.
pub(crate) fn attempts(
    local: bool,
    type_handler: Option<GUID>,
    ours_enabled: bool,
    replaced: Option<GUID>,
) -> Vec<Attempt> {
    let mut out = Vec::new();
    if local {
        match type_handler {
            Some(h) if h == CLSID_THUMBNAIL_PROVIDER => {
                if ours_enabled {
                    out.push(Attempt::Ours);
                }
            }
            // Never ourselves again through the type table: that would be a loop.
            Some(h) if h != CLSID_CLOUD_THUMB_PROVIDER => out.push(Attempt::TypeHandler(h)),
            _ => {}
        }
    }
    if let Some(r) = replaced.filter(|r| *r != CLSID_CLOUD_THUMB_PROVIDER) {
        out.push(Attempt::Replaced(r));
    }
    out
}

/// Is the file's data on this PC, so reading it downloads nothing?
pub(crate) fn data_is_local(attrs: u32) -> bool {
    attrs != INVALID_FILE_ATTRIBUTES && attrs & (ATTR_OFFLINE | ATTR_RECALL_ON_DATA_ACCESS) == 0
}

impl CloudThumbnailProvider_Impl {
    fn thumbnail_for(&self, item: &IShellItem, cx: u32) -> Result<(HBITMAP, WTS_ALPHATYPE)> {
        let path = unsafe { item_path(item) };
        let local = path
            .as_deref()
            .is_some_and(|p| data_is_local(unsafe { GetFileAttributesW(&HSTRING::from(p)) }));
        let ext = path.as_deref().and_then(extension);
        let type_handler = ext
            .as_deref()
            .and_then(|e| unsafe { type_thumbnail_handler(e) });
        let replaced = path
            .as_deref()
            .and_then(crate::register::cloud_replaced_provider_for)
            .and_then(|c| parse_clsid(&c));
        let ours_enabled = settings::thumbnails_enabled();
        let plan = attempts(local, type_handler, ours_enabled, replaced);
        safety::log_debugf!(
            "CloudThumbnail: ext={} local={local} plan={plan:?}",
            ext.as_deref().unwrap_or("?")
        );
        for attempt in plan {
            let got = match &attempt {
                Attempt::Ours => path
                    .as_deref()
                    .map_or(Err(Error::from(E_FAIL)), |p| unsafe { ours(p, cx) }),
                Attempt::TypeHandler(clsid) => path
                    .as_deref()
                    .map_or(Err(Error::from(E_FAIL)), |p| unsafe {
                        type_handler_thumb(clsid, p, item, cx)
                    }),
                Attempt::Replaced(clsid) => unsafe {
                    replaced_thumb(clsid, item, self.mode.get(), cx)
                },
            };
            match got {
                Ok(t) => return Ok(t),
                Err(e) => safety::log_debugf!(
                    "CloudThumbnail: {attempt:?} failed hr={:#010x}",
                    e.code().0
                ),
            }
        }
        Err(Error::from(E_FAIL))
    }
}

/// The item's filesystem path, when it has one.
unsafe fn item_path(item: &IShellItem) -> Option<String> {
    let pw = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
    let s = pw.to_string().ok();
    CoTaskMemFree(Some(pw.0 as *const core::ffi::c_void));
    s
}

/// The lowercase extension of `path` without the dot.
fn extension(path: &str) -> Option<String> {
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
}

fn parse_clsid(s: &str) -> Option<GUID> {
    unsafe { CLSIDFromString(&HSTRING::from(s.trim())) }.ok()
}

/// The thumbnail handler Windows resolves for `.ext` through the normal association lookup
/// (ProgID, `SystemFileAssociations`, perceived type) — what it would use outside a sync folder.
pub(crate) unsafe fn type_thumbnail_handler(ext: &str) -> Option<GUID> {
    let mut buf = [0u16; 64];
    let mut len = buf.len() as u32;
    AssocQueryStringW(
        ASSOCF_INIT_DEFAULTTOSTAR,
        ASSOCSTR_SHELLEXTENSION,
        &HSTRING::from(format!(".{ext}")),
        &HSTRING::from(THUMB_HANDLER_CATEGORY),
        Some(windows::core::PWSTR(buf.as_mut_ptr())),
        &mut len,
    )
    .ok()
    .ok()?;
    let s = String::from_utf16_lossy(&buf[..(len as usize).saturating_sub(1).min(buf.len())]);
    parse_clsid(&s)
}

/// A read-only stream over the (local) file.
unsafe fn file_stream(path: &str) -> Result<windows::Win32::System::Com::IStream> {
    SHCreateStreamOnFileEx(
        &HSTRING::from(path),
        (STGM_READ | STGM_SHARE_DENY_NONE).0,
        0,
        false,
        None,
    )
}

/// Our own pipeline over the file, through the ordinary provider object.
unsafe fn ours(path: &str, cx: u32) -> Result<(HBITMAP, WTS_ALPHATYPE)> {
    let provider: IThumbnailProvider = ThumbnailProvider::default().into();
    let init: IInitializeWithStream = provider.cast()?;
    init.Initialize(&file_stream(path)?, STGM_READ.0)?;
    call_provider(&provider, cx)
}

/// The type's own handler, initialised the way it asks to be: a stream, a path or the item.
unsafe fn type_handler_thumb(
    clsid: &GUID,
    path: &str,
    item: &IShellItem,
    cx: u32,
) -> Result<(HBITMAP, WTS_ALPHATYPE)> {
    let provider: IThumbnailProvider = create(clsid, CLSCTX_INPROC_SERVER | CLSCTX_LOCAL_SERVER)?;
    initialize_type_handler(&provider, path, item)?;
    call_provider(&provider, cx)
}

/// Initialise a type handler through the first init interface it offers: a stream, then a
/// path, then the item.
unsafe fn initialize_type_handler(
    provider: &IThumbnailProvider,
    path: &str,
    item: &IShellItem,
) -> Result<()> {
    if let Ok(init) = provider.cast::<IInitializeWithStream>() {
        return init.Initialize(&file_stream(path)?, STGM_READ.0);
    }
    if let Ok(init) = provider.cast::<IInitializeWithFile>() {
        return init.Initialize(&HSTRING::from(path), STGM_READ.0);
    }
    provider
        .cast::<IInitializeWithItem>()?
        .Initialize(item, STGM_READ.0)
}

/// The provider this folder had before us, given the item exactly as the shell gave it to us.
unsafe fn replaced_thumb(
    clsid: &GUID,
    item: &IShellItem,
    mode: u32,
    cx: u32,
) -> Result<(HBITMAP, WTS_ALPHATYPE)> {
    // The shell creates these out of process; do the same, and fall back to in-proc for a
    // provider registered only that way.
    let provider: IThumbnailProvider =
        create(clsid, CLSCTX_LOCAL_SERVER).or_else(|_| create(clsid, CLSCTX_INPROC_SERVER))?;
    provider
        .cast::<IInitializeWithItem>()?
        .Initialize(item, mode)?;
    call_provider(&provider, cx)
}

unsafe fn create(clsid: &GUID, ctx: CLSCTX) -> Result<IThumbnailProvider> {
    CoCreateInstance(clsid, None, ctx)
}

unsafe fn call_provider(
    provider: &IThumbnailProvider,
    cx: u32,
) -> Result<(HBITMAP, WTS_ALPHATYPE)> {
    let mut bmp = HBITMAP::default();
    let mut alpha = WTSAT_UNKNOWN;
    provider.GetThumbnail(cx, &mut bmp, &mut alpha)?;
    if bmp.is_invalid() {
        return Err(Error::from(E_FAIL));
    }
    Ok((bmp, alpha))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONEDRIVE: GUID = GUID::from_u128(0x021E4F06_9DCC_49AD_88CF_ECC2DA314C8A);
    const PHOTOS: GUID = GUID::from_u128(0xC7657C4A_9F68_40FA_A4DF_96BC08EB3551);

    /// The routing contract: a downloaded file gets the handler it would get anywhere else on
    /// disk first (ours for our formats), the folder's own provider after; an online-only file
    /// goes ONLY to the folder's provider, because anything else would have to download it.
    #[test]
    fn local_files_get_their_usual_handler_and_online_only_files_only_the_folders_provider() {
        assert_eq!(
            attempts(true, Some(CLSID_THUMBNAIL_PROVIDER), true, Some(ONEDRIVE)),
            vec![Attempt::Ours, Attempt::Replaced(ONEDRIVE)]
        );
        assert_eq!(
            attempts(true, Some(PHOTOS), true, None),
            vec![Attempt::TypeHandler(PHOTOS)]
        );
        assert_eq!(
            attempts(false, Some(CLSID_THUMBNAIL_PROVIDER), true, Some(ONEDRIVE)),
            vec![Attempt::Replaced(ONEDRIVE)]
        );
        // Thumbnails switched off: ours is skipped, the folder's provider still answers.
        assert_eq!(
            attempts(true, Some(CLSID_THUMBNAIL_PROVIDER), false, Some(ONEDRIVE)),
            vec![Attempt::Replaced(ONEDRIVE)]
        );
        // Never a loop back into this class, whichever table names it.
        assert!(attempts(
            true,
            Some(CLSID_CLOUD_THUMB_PROVIDER),
            true,
            Some(CLSID_CLOUD_THUMB_PROVIDER)
        )
        .is_empty());
    }

    /// Only a file whose data is here may be read: an online-only or partly downloaded
    /// placeholder, or one whose attributes cannot be read, is not.
    #[test]
    fn only_a_downloaded_file_counts_as_local() {
        assert!(data_is_local(0x0000_2420)); // a hydrated placeholder, as measured
        assert!(!data_is_local(0x0040_2420)); // online-only
        assert!(!data_is_local(0x0000_1020)); // offline
        assert!(!data_is_local(u32::MAX));
    }
}
