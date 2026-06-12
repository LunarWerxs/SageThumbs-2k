//! IClassFactory for the single in-proc server. One factory type serves any
//! of our CLSIDs; it is told which CLSID to construct via `new`.

use windows_implement::implement;
use windows::core::{Error, Interface, Ref, Result, BOOL, GUID, IUnknown};
use windows::Win32::Foundation::{CLASS_E_NOAGGREGATION, E_NOINTERFACE};
use windows::Win32::System::Com::{IClassFactory, IClassFactory_Impl};

use crate::command::ExplorerCommand;
use crate::contextmenu::ContextMenu;
use crate::guids;
use crate::safety;
use crate::thumbprovider::ThumbnailProvider;

#[implement(IClassFactory)]
pub struct ClassFactory {
    _ref: crate::ModuleRef,
    clsid: GUID,
}

impl ClassFactory {
    pub fn new(clsid: GUID) -> Self {
        Self { _ref: crate::ModuleRef::default(), clsid }
    }
}

impl IClassFactory_Impl for ClassFactory_Impl {
    fn CreateInstance(
        &self,
        punkouter: Ref<'_, IUnknown>,
        riid: *const GUID,
        ppvobject: *mut *mut core::ffi::c_void,
    ) -> Result<()> {
        safety::guard(|| {
            unsafe {
                if !ppvobject.is_null() {
                    *ppvobject = core::ptr::null_mut();
                }
            }
            if !punkouter.is_null() {
                return Err(Error::from(CLASS_E_NOAGGREGATION));
            }

            let unknown: IUnknown = match self.clsid {
                guids::CLSID_THUMBNAIL_PROVIDER => ThumbnailProvider::default().into(),
                guids::CLSID_EXPLORER_COMMAND => ExplorerCommand::default().into(),
                guids::CLSID_CONTEXT_MENU => ContextMenu::default().into(),
                _ => return Err(Error::from(E_NOINTERFACE)),
            };

            unsafe { unknown.query(riid, ppvobject).ok() }
        })
    }

    fn LockServer(&self, flock: BOOL) -> Result<()> {
        safety::guard(|| {
            if flock.as_bool() {
                crate::dll_add_ref();
            } else {
                crate::dll_release();
            }
            Ok(())
        })
    }
}
