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

use windows_implement::implement;
use windows::core::{Error, Result, PCWSTR};
use windows::Win32::Foundation::{E_FAIL, E_INVALIDARG, PROPERTYKEY, STG_E_ACCESSDENIED};
use windows::Win32::Storage::EnhancedStorage::{
    PKEY_Image_Dimensions, PKEY_Image_HorizontalSize, PKEY_Image_VerticalSize, PKEY_Music_AlbumTitle,
    PKEY_Music_Artist, PKEY_Music_TrackNumber, PKEY_Photo_CameraManufacturer, PKEY_Photo_CameraModel,
    PKEY_Title,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::UI::Shell::PropertiesSystem::{
    IInitializeWithFile, IInitializeWithFile_Impl, IPropertyStore, IPropertyStore_Impl,
};

use crate::safety;

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

        // Image dimensions + EXIF camera (same probe "Image info" uses, under the decode guards).
        let info = crate::strip::read_info(&path);
        if info.width > 0 && info.height > 0 {
            out.push((
                PKEY_Image_Dimensions,
                PROPVARIANT::from(format!("{} x {}", info.width, info.height).as_str()),
            ));
            out.push((PKEY_Image_HorizontalSize, PROPVARIANT::from(info.width)));
            out.push((PKEY_Image_VerticalSize, PROPVARIANT::from(info.height)));
        }
        if let Some(make) = info.make.filter(|s| !s.is_empty()) {
            out.push((PKEY_Photo_CameraManufacturer, PROPVARIANT::from(make.as_str())));
        }
        if let Some(model) = info.model.filter(|s| !s.is_empty()) {
            out.push((PKEY_Photo_CameraModel, PROPVARIANT::from(model.as_str())));
        }

        // Audio tags (lofty + our ASF parser). Returns empties for non-audio, so this is cheap.
        let tags = crate::strip::read_audio_tags(&path);
        if let Some(artist) = tags.artist.filter(|s| !s.is_empty()) {
            out.push((PKEY_Music_Artist, PROPVARIANT::from(artist.as_str())));
        }
        if let Some(album) = tags.album.filter(|s| !s.is_empty()) {
            out.push((PKEY_Music_AlbumTitle, PROPVARIANT::from(album.as_str())));
        }
        if let Some(title) = tags.title.filter(|s| !s.is_empty()) {
            out.push((PKEY_Title, PROPVARIANT::from(title.as_str())));
        }
        if let Some(track) = tags.track.filter(|&t| t > 0) {
            out.push((PKEY_Music_TrackNumber, PROPVARIANT::from(track)));
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
