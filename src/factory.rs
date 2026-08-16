//! IClassFactory for the single in-proc server. One factory type serves any
//! of our CLSIDs; it is told which CLSID to construct via `new`.

use windows::core::{Error, IUnknown, Interface, Ref, Result, BOOL, GUID};
use windows::Win32::Foundation::{
    CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_NOINTERFACE, E_POINTER,
};
use windows::Win32::System::Com::{IClassFactory, IClassFactory_Impl};
use windows_implement::implement;

use crate::command::{self, ExplorerCommand};
use crate::contextmenu::ContextMenu;
use crate::guids;
use crate::previewhandler::PreviewHandler;
use crate::propstore::PropertyStore;
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
        Self {
            _ref: crate::ModuleRef::default(),
            clsid,
        }
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
                guids::CLSID_PROPERTY_STORE => PropertyStore::default().into(),
                // A modern-menu quick verb (Convert into / Convert… / Resize / Rotate):
                // construct a top-level MenuCommand over the MENU item that CLSID maps to.
                clsid if command::is_quick_clsid(clsid) => match command::quick_root_item(clsid) {
                    Some(item) => command::MenuCommand::quick_root(item).into(),
                    // The CLSID pattern is recognized but maps to no MENU item: this class
                    // is not available, not merely "doesn't support the requested interface".
                    None => return Err(Error::from(CLASS_E_CLASSNOTAVAILABLE)),
                },
                _ => return Err(Error::from(E_NOINTERFACE)),
            };

            // A null riid reaches windows-core's QueryInterface, which does not null-check
            // it itself — matching the guard `dll_get_class_object` already applies before
            // it ever gets here.
            if riid.is_null() {
                return Err(Error::from(E_POINTER));
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A null riid reaches windows-core's `QueryInterface`, which does not null-check it
    /// itself — a hard access violation under `panic = "abort"`, not a catchable `Result`.
    /// Drives the raw vtable entry directly rather than the safe `CreateInstance<P0, T>`
    /// convenience wrapper, which always fills in a real IID and so can never exercise a
    /// null one.
    #[test]
    fn create_instance_with_null_riid_returns_error_instead_of_crashing() {
        let com = windows::core::ComObject::new(ClassFactory::new(guids::CLSID_CONTEXT_MENU));
        let iface: IClassFactory = com.to_interface();
        let mut ppv: *mut core::ffi::c_void = core::ptr::null_mut();
        let hr = unsafe {
            (Interface::vtable(&iface).CreateInstance)(
                Interface::as_raw(&iface),
                core::ptr::null_mut(),
                core::ptr::null(),
                &mut ppv,
            )
        };
        assert!(
            hr.is_err(),
            "a null riid must be rejected before it reaches query(), or an access violation follows"
        );
    }
}
