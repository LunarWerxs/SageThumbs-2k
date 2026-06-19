//! IClassFactory for the single in-proc server. One factory type serves any
//! of our CLSIDs; it is told which CLSID to construct via `new`.

use windows_implement::implement;
use windows::core::{Error, Interface, Ref, Result, BOOL, GUID, IUnknown};
use windows::Win32::Foundation::{CLASS_E_NOAGGREGATION, E_NOINTERFACE, E_POINTER};
use windows::Win32::System::Com::{IClassFactory, IClassFactory_Impl};

use crate::command::ExplorerCommand;
use crate::contextmenu::ContextMenu;
use crate::guids;
use crate::previewhandler::PreviewHandler;
use crate::safety;
use crate::thumbprovider::ThumbnailProvider;

#[implement(IClassFactory)]
pub struct ClassFactory {
    _ref: crate::ModuleRef,
    clsid: GUID,
}

impl ClassFactory {
    // ModuleRef::default()'s side effect (live-object add-ref) must run; keep the Default call.
    #[allow(clippy::default_constructed_unit_structs)]
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
            // A null out-pointer can't be written — including by `query()` below,
            // which would deref it. Reject up front (E_POINTER) like the other COM
            // entry points (lib.rs `DllGetClassObject`), instead of half-guarding it
            // here and forwarding the null on.
            if ppvobject.is_null() {
                return Err(Error::from(E_POINTER));
            }
            unsafe {
                *ppvobject = core::ptr::null_mut();
            }
            if !punkouter.is_null() {
                return Err(Error::from(CLASS_E_NOAGGREGATION));
            }

            let unknown: IUnknown = match self.clsid {
                guids::CLSID_THUMBNAIL_PROVIDER => ThumbnailProvider::default().into(),
                guids::CLSID_EXPLORER_COMMAND => ExplorerCommand::default().into(),
                guids::CLSID_CONTEXT_MENU => ContextMenu::default().into(),
                guids::CLSID_PREVIEW_HANDLER => PreviewHandler::default().into(),
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
