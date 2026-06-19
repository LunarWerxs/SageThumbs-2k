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
    // Overflow-safe byte count (len * size_of::<u16>()); can't actually overflow for
    // any real string, but keep the allocation provably sound rather than wrapping.
    let bytes = wide.len().checked_mul(2).ok_or_else(|| Error::from(E_OUTOFMEMORY))?;
    let p = unsafe { CoTaskMemAlloc(bytes) } as *mut u16;
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

/// True if the selection contains at least one supported image. Lazy: iterates
/// the array and stops at the FIRST match instead of materializing every path
/// into a Vec — each display name is freed right after its extension is tested
/// and no String is kept. Mirrors the classic `is_image` gate.
unsafe fn selection_has_image(items: Ref<'_, IShellItemArray>) -> bool {
    let Ok(arr) = items.ok() else {
        return false;
    };
    let Ok(count) = arr.GetCount() else {
        return false;
    };
    for i in 0..count {
        if let Ok(item) = arr.GetItemAt(i) {
            if let Ok(pw) = item.GetDisplayName(SIGDN_FILESYSPATH) {
                let hit = pw.to_string().map(|s| verbs::is_image(&s)).unwrap_or(false);
                CoTaskMemFree(Some(pw.0 as *const c_void));
                if hit {
                    return true;
                }
            }
        }
    }
    false
}

/// Enabled only when the selection contains a supported image — mirrors the
/// classic `IContextMenu` gate (`contextmenu.rs`) so the modern Win11 menu
/// behaves the same. `ECS_HIDDEN` removes the verb from the flyout entirely.
unsafe fn image_state(items: Ref<'_, IShellItemArray>) -> u32 {
    if settings::menu_enabled() && selection_has_image(items) {
        ECS_ENABLED.0 as u32
    } else {
        ECS_HIDDEN.0 as u32
    }
}

// ---- Root command -------------------------------------------------------

#[implement(IExplorerCommand)]
pub struct ExplorerCommand {
    _ref: crate::ModuleRef,
    /// Cached "selection contains an image" verdict. The shell may call
    /// `GetState` repeatedly on one command instance and the selection is fixed
    /// for the object's lifetime, so we iterate the array at most once.
    has_image: Cell<Option<bool>>,
}

impl Default for ExplorerCommand {
    // ModuleRef::default() is load-bearing: it bumps the live-object count via its
    // side-effecting Default impl. The bare-literal rewrite clippy suggests would skip that.
    #[allow(clippy::default_constructed_unit_structs)]
    fn default() -> Self {
        Self {
            _ref: crate::ModuleRef::default(),
            has_image: Cell::new(None),
        }
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
            let exe = crate::sibling_of_dll(crate::APP_EXE).ok_or_else(|| Error::from(E_NOTIMPL))?;
            alloc_pwstr(&format!("{},-1", exe.display()))
        })
    }
    fn GetToolTip(&self, _items: Ref<'_, IShellItemArray>) -> Result<PWSTR> {
        Err(Error::from(E_NOTIMPL))
    }
    fn GetCanonicalName(&self) -> Result<GUID> {
        // No stable canonical verb name (we'd return GUID_NULL); report
        // not-implemented to match the rest of the surface instead of an
        // S_OK + null GUID the shell would treat as meaningful.
        Err(Error::from(E_NOTIMPL))
    }
    fn GetState(&self, items: Ref<'_, IShellItemArray>, _slow: BOOL) -> Result<u32> {
        safety::guard_val(|| {
            // Cache the (selection-fixed) image verdict per instance; re-check
            // the cheap menu_enabled() gate each call so a settings change is
            // honored without a new command object.
            let has = match self.has_image.get() {
                Some(v) => v,
                None => {
                    let v = unsafe { selection_has_image(items) };
                    self.has_image.set(Some(v));
                    v
                }
            };
            let state = if settings::menu_enabled() && has {
                ECS_ENABLED.0 as u32
            } else {
                ECS_HIDDEN.0 as u32
            };
            Ok(state)
        })
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
            // One snapshot of the visibility subkey for this enumeration (instead of
            // a key-open per item).
            let vis = settings::menu_visibility();
            let items: Vec<IExplorerCommand> = verbs::MENU
                .iter()
                .filter(|it| !matches!(it, verbs::MenuItem::Separator))
                // Per-item visibility: hide top-level entries the user unticked in Settings.
                .filter(|it| vis.shown(it.title()))
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
    // ModuleRef::default()'s side effect (live-object add-ref) must run; keep the Default call.
    #[allow(clippy::default_constructed_unit_structs)]
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
        // No stable canonical verb name; not-implemented (was S_OK + GUID_NULL),
        // matching the root command and the rest of the COM surface.
        Err(Error::from(E_NOTIMPL))
    }
    fn GetState(&self, items: Ref<'_, IShellItemArray>, _slow: BOOL) -> Result<u32> {
        safety::guard_val(|| Ok(unsafe { image_state(items) }))
    }
    fn Invoke(&self, items: Ref<'_, IShellItemArray>, _ctx: Ref<'_, IBindCtx>) -> Result<()> {
        safety::guard(|| {
            if let verbs::MenuItem::Verb(_, action) = self.item {
                let paths = unsafe { items_to_paths(items) };
                let report = verbs::run_action(*action, &paths);
                // The modern command has no parent HWND handy — None lets
                // MessageBox create a top-level dialog. Silent on success.
                report.surface(None);
                // On a clean success, reveal the output ONLY if it went to a new
                // folder; in-place outputs (next to a source) don't pop a window.
                report.reveal(&paths);
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
    // ModuleRef::default()'s side effect (live-object add-ref) must run; keep the Default call.
    #[allow(clippy::default_constructed_unit_structs)]
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
