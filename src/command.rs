//! Windows 11 context-menu verbs: a top-level "SageThumbs" IExplorerCommand
//! with a sub-command flyout (IEnumExplorerCommand) of the convert verbs.
//!
//! The shell instantiates the root command (CLSID_EXPLORER_COMMAND) via the
//! class factory; the leaf verbs are created internally by EnumSubCommands.

use core::cell::Cell;
use core::ffi::c_void;
use std::iter::once;

use windows_implement::implement;
use windows::core::{Error, Ref, Result, BOOL, GUID, HRESULT, PWSTR};
use windows::Win32::Foundation::{E_NOTIMPL, E_OUTOFMEMORY, E_POINTER, S_FALSE, S_OK};
use windows::Win32::System::Com::{CoTaskMemAlloc, CoTaskMemFree, IBindCtx};
use windows::Win32::UI::Shell::{
    IEnumExplorerCommand, IEnumExplorerCommand_Impl, IExplorerCommand, IExplorerCommand_Impl,
    IShellItemArray, ECF_DEFAULT, ECF_HASSUBCOMMANDS, ECS_ENABLED, ECS_HIDDEN, SIGDN_FILESYSPATH,
};

use crate::{safety, settings, verbs};

/// Allocate a NUL-terminated wide string with CoTaskMemAlloc; the shell frees it.
fn alloc_pwstr(s: &str) -> Result<PWSTR> {
    let wide: Vec<u16> = s.encode_utf16().chain(once(0)).collect();
    let p = unsafe { CoTaskMemAlloc(wide.len() * 2) } as *mut u16;
    if p.is_null() {
        return Err(Error::from(E_OUTOFMEMORY));
    }
    unsafe { std::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len()) };
    Ok(PWSTR(p))
}

/// Extract filesystem paths from a shell selection (the IShellItemArray the
/// shell passes to Invoke). Null/empty selection yields an empty Vec.
unsafe fn items_to_paths(items: Ref<'_, IShellItemArray>) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(arr) = items.ok() else {
        return out;
    };
    let Ok(count) = arr.GetCount() else {
        return out;
    };
    for i in 0..count {
        if let Ok(item) = arr.GetItemAt(i) {
            if let Ok(pw) = item.GetDisplayName(SIGDN_FILESYSPATH) {
                if let Ok(s) = pw.to_string() {
                    out.push(s);
                }
                CoTaskMemFree(Some(pw.0 as *const c_void));
            }
        }
    }
    out
}

/// Enabled only when the selection contains a supported image — mirrors the
/// classic `IContextMenu` gate (`contextmenu.rs`) so the modern Win11 menu
/// behaves the same. `ECS_HIDDEN` removes the verb from the flyout entirely.
unsafe fn image_state(items: Ref<'_, IShellItemArray>) -> u32 {
    if settings::menu_enabled() && items_to_paths(items).iter().any(|p| verbs::is_image(p)) {
        ECS_ENABLED.0 as u32
    } else {
        ECS_HIDDEN.0 as u32
    }
}

// ---- Root command -------------------------------------------------------

#[implement(IExplorerCommand)]
pub struct ExplorerCommand {
    _ref: crate::ModuleRef,
}

impl Default for ExplorerCommand {
    fn default() -> Self {
        Self { _ref: crate::ModuleRef::default() }
    }
}

impl IExplorerCommand_Impl for ExplorerCommand_Impl {
    fn GetTitle(&self, _items: Ref<'_, IShellItemArray>) -> Result<PWSTR> {
        safety::guard_val(|| alloc_pwstr("SageThumbs 2K"))
    }
    fn GetIcon(&self, _items: Ref<'_, IShellItemArray>) -> Result<PWSTR> {
        safety::guard_val(|| {
            // The companion EXE carries the app icon as resource 1; the modern
            // menu takes "<module>,-<resid>" icon references. Installed next to
            // the DLL — if it isn't there, no icon (E_NOTIMPL), never an error.
            let dll = crate::module_path().map_err(|_| Error::from(E_NOTIMPL))?;
            let exe = std::path::Path::new(&dll)
                .parent()
                .map(|d| d.join("sagethumbs2k-app.exe"))
                .filter(|p| p.exists())
                .ok_or_else(|| Error::from(E_NOTIMPL))?;
            alloc_pwstr(&format!("{},-1", exe.display()))
        })
    }
    fn GetToolTip(&self, _items: Ref<'_, IShellItemArray>) -> Result<PWSTR> {
        Err(Error::from(E_NOTIMPL))
    }
    fn GetCanonicalName(&self) -> Result<GUID> {
        Ok(GUID::from_u128(0))
    }
    fn GetState(&self, items: Ref<'_, IShellItemArray>, _slow: BOOL) -> Result<u32> {
        safety::guard_val(|| Ok(unsafe { image_state(items) }))
    }
    fn Invoke(&self, _items: Ref<'_, IShellItemArray>, _ctx: Ref<'_, IBindCtx>) -> Result<()> {
        // The flyout is shown instead; the root itself has no action.
        Ok(())
    }
    fn GetFlags(&self) -> Result<u32> {
        Ok(ECF_HASSUBCOMMANDS.0 as u32)
    }
    fn EnumSubCommands(&self) -> Result<IEnumExplorerCommand> {
        safety::guard_val(|| {
            let items: Vec<IExplorerCommand> = verbs::MENU
                .iter()
                .filter(|it| !matches!(it, verbs::MenuItem::Separator))
                .map(|it| MenuCommand::new(it).into())
                .collect();
            Ok(SubCommandEnum::new(items).into())
        })
    }
}

// ---- Menu node command (a submenu group OR a leaf verb) -----------------

#[implement(IExplorerCommand)]
pub struct MenuCommand {
    _ref: crate::ModuleRef,
    item: &'static verbs::MenuItem,
}

impl MenuCommand {
    fn new(item: &'static verbs::MenuItem) -> Self {
        Self { _ref: crate::ModuleRef::default(), item }
    }
}

impl IExplorerCommand_Impl for MenuCommand_Impl {
    fn GetTitle(&self, _items: Ref<'_, IShellItemArray>) -> Result<PWSTR> {
        safety::guard_val(|| alloc_pwstr(crate::i18n::t(self.item.title())))
    }
    fn GetIcon(&self, _items: Ref<'_, IShellItemArray>) -> Result<PWSTR> {
        Err(Error::from(E_NOTIMPL))
    }
    fn GetToolTip(&self, _items: Ref<'_, IShellItemArray>) -> Result<PWSTR> {
        Err(Error::from(E_NOTIMPL))
    }
    fn GetCanonicalName(&self) -> Result<GUID> {
        Ok(GUID::from_u128(0))
    }
    fn GetState(&self, items: Ref<'_, IShellItemArray>, _slow: BOOL) -> Result<u32> {
        safety::guard_val(|| Ok(unsafe { image_state(items) }))
    }
    fn Invoke(&self, items: Ref<'_, IShellItemArray>, _ctx: Ref<'_, IBindCtx>) -> Result<()> {
        safety::guard(|| {
            if let verbs::MenuItem::Verb(_, action) = self.item {
                let paths = unsafe { items_to_paths(items) };
                verbs::run_action(*action, &paths);
            }
            Ok(())
        })
    }
    fn GetFlags(&self) -> Result<u32> {
        match self.item {
            verbs::MenuItem::Group(..) => Ok(ECF_HASSUBCOMMANDS.0 as u32),
            // Separators are filtered out before a MenuCommand wraps an item, so
            // this never holds one; treat it as a plain leaf for exhaustiveness.
            verbs::MenuItem::Verb(..) | verbs::MenuItem::Separator => Ok(ECF_DEFAULT.0 as u32),
        }
    }
    fn EnumSubCommands(&self) -> Result<IEnumExplorerCommand> {
        safety::guard_val(|| match self.item {
            verbs::MenuItem::Group(_, children) => {
                let items: Vec<IExplorerCommand> = children
                    .iter()
                    .filter(|c| !matches!(c, verbs::MenuItem::Separator))
                    .map(|c| MenuCommand::new(c).into())
                    .collect();
                Ok(SubCommandEnum::new(items).into())
            }
            verbs::MenuItem::Verb(..) | verbs::MenuItem::Separator => Err(Error::from(E_NOTIMPL)),
        })
    }
}

// ---- Sub-command enumerator ---------------------------------------------

#[implement(IEnumExplorerCommand)]
pub struct SubCommandEnum {
    _ref: crate::ModuleRef,
    items: Vec<IExplorerCommand>,
    pos: Cell<usize>,
}

impl SubCommandEnum {
    fn new(items: Vec<IExplorerCommand>) -> Self {
        Self {
            _ref: crate::ModuleRef::default(),
            items,
            pos: Cell::new(0),
        }
    }
}

impl IEnumExplorerCommand_Impl for SubCommandEnum_Impl {
    fn Next(
        &self,
        celt: u32,
        puicommand: *mut Option<IExplorerCommand>,
        pceltfetched: *mut u32,
    ) -> HRESULT {
        // The only HRESULT-returning COM method; guard it like the rest so a
        // panic can't unwind across the COM ABI (safety.rs invariant).
        safety::guard_hr(|| {
            if puicommand.is_null() && celt != 0 {
                return E_POINTER;
            }
            let mut fetched = 0u32;
            let mut pos = self.pos.get();
            for i in 0..celt as usize {
                if pos >= self.items.len() {
                    break;
                }
                // The out array slots are uninitialized; write (don't assign,
                // which would drop garbage).
                unsafe { std::ptr::write(puicommand.add(i), Some(self.items[pos].clone())) };
                pos += 1;
                fetched += 1;
            }
            self.pos.set(pos);
            if !pceltfetched.is_null() {
                unsafe { *pceltfetched = fetched };
            }
            if fetched == celt {
                S_OK
            } else {
                S_FALSE
            }
        })
    }

    fn Skip(&self, celt: u32) -> Result<()> {
        let pos = (self.pos.get() + celt as usize).min(self.items.len());
        self.pos.set(pos);
        Ok(())
    }

    fn Reset(&self) -> Result<()> {
        self.pos.set(0);
        Ok(())
    }

    fn Clone(&self) -> Result<IEnumExplorerCommand> {
        safety::guard_val(|| {
            let clone = SubCommandEnum::new(self.items.clone());
            clone.pos.set(self.pos.get());
            Ok(clone.into())
        })
    }
}
