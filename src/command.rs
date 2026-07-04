//! Windows 11 context-menu verbs: a top-level "SageThumbs" IExplorerCommand
//! with a sub-command flyout (IEnumExplorerCommand) of the convert verbs.
//!
//! The shell instantiates the root command (CLSID_EXPLORER_COMMAND) via the
//! class factory; the leaf verbs are created internally by EnumSubCommands.

use core::cell::Cell;
use core::ffi::c_void;

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
    let wide = crate::wide(s);
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
/// and no String is kept. Mirrors the classic `is_image` gate. Takes `items` by
/// reference (windows-rs `Ref` is neither `Copy` nor `Clone`) so the same handle
/// can also feed `selection_is_audio_only` in `menu_item_state`.
unsafe fn selection_has_image(items: &Ref<'_, IShellItemArray>) -> bool {
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

/// True if the selection is non-empty AND every file is audio (music). Mirrors the
/// classic `audio_only` gate (`contextmenu.rs`): an audio-only selection hides the
/// image-only top-level verbs in the modern flyout. Unlike `selection_has_image` this
/// must visit EVERY item (one non-audio file flips the answer false), freeing each
/// display name as it goes. An empty / unreadable selection is not audio-only.
unsafe fn selection_is_audio_only(items: &Ref<'_, IShellItemArray>) -> bool {
    let Ok(arr) = items.ok() else {
        return false;
    };
    let Ok(count) = arr.GetCount() else {
        return false;
    };
    if count == 0 {
        return false;
    }
    for i in 0..count {
        let Ok(item) = arr.GetItemAt(i) else {
            return false;
        };
        let Ok(pw) = item.GetDisplayName(SIGDN_FILESYSPATH) else {
            return false;
        };
        let audio = pw.to_string().map(|s| verbs::is_audio(&s)).unwrap_or(false);
        CoTaskMemFree(Some(pw.0 as *const c_void));
        if !audio {
            return false;
        }
    }
    true
}

/// Enabled only when the selection contains a supported image — mirrors the
/// classic `IContextMenu` gate (`contextmenu.rs`) so the modern Win11 menu
/// behaves the same. `ECS_HIDDEN` removes the verb from the flyout entirely.
/// The enabled/hidden verdict shared by both `IExplorerCommand::GetState` impls: the
/// verb shows only when the menu is enabled AND the selection holds a supported image.
/// The menu_enabled() gate is re-checked every call so a settings change is honored
/// without a new command object.
fn state_for(has_image: bool) -> u32 {
    if settings::menu_enabled() && has_image {
        ECS_ENABLED.0 as u32
    } else {
        ECS_HIDDEN.0 as u32
    }
}

/// Visibility of the ROOT "SageThumbs 2K" flyout. Like [`state_for`] for a supported selection,
/// but ALSO shown — in CONDENSED mode — on an UNSUPPORTED selection when the user enabled "show
/// the menu on all file types", mirroring the classic handler (`contextmenu.rs`). Before this the
/// modern Win11 flyout hid itself on any non-image selection regardless of the setting, so the
/// toggle was a silent no-op for stock Win11 users. The condensed item set is chosen in
/// `EnumSubCommands` from the same cached `has_image` verdict GetState computes here.
fn root_state(has_image: bool) -> u32 {
    if settings::menu_enabled() && (has_image || settings::menu_all_file_types()) {
        ECS_ENABLED.0 as u32
    } else {
        ECS_HIDDEN.0 as u32
    }
}

unsafe fn image_state(items: &Ref<'_, IShellItemArray>) -> u32 {
    state_for(selection_has_image(items))
}

/// Per-item visibility for a top-level flyout command. The modern flyout's
/// `EnumSubCommands` lists every top-level verb with NO selection context (it can't
/// filter the list like the classic `QueryContextMenu` does), so the audio gate lands
/// here instead: start from the shared image gate (menu enabled + supported selection),
/// then HIDE an image-only TOP-LEVEL verb when the selection is audio-only — those verbs
/// no-op or produce garbage on a sound file. Audio-ok top-level items
/// (files_to_folder/rename/sort/settings, per [`verbs::top_level_audio_ok`]) and every
/// nested child stay on the base gate (so e.g. the Rename ▸ flyout keeps its audio
/// patterns). `top_level` is false for items created by a group's own `EnumSubCommands`,
/// so the gate only ever hides whole top-level groups, never their leaves.
unsafe fn menu_item_state(
    item: &verbs::MenuItem,
    top_level: bool,
    items: &Ref<'_, IShellItemArray>,
) -> u32 {
    let base = image_state(items);
    if base != ECS_ENABLED.0 as u32 {
        return base; // menu off or unsupported selection — already hidden
    }
    if top_level && !verbs::top_level_audio_ok(item.title()) && selection_is_audio_only(items) {
        return ECS_HIDDEN.0 as u32;
    }
    base
}

// ---- Modern-menu quick verbs --------------------------------------------
//
// Each quick verb (Convert into ▸ / Convert… / Resize ▸ / Rotate ▸) is its OWN top-level
// IExplorerCommand coclass (own CLSID + `desktop5:Verb` in the package manifest), so Windows 11
// surfaces it DIRECTLY on the modern context menu instead of two levels deep inside the root
// flyout — the modern twin of the classic "quick verbs on main menu" Option (the limitation the
// root `EnumSubCommands` note describes). They reuse the `MenuCommand` flyout machinery: a quick
// verb is a `MenuCommand` flagged `quick_root`, gated by `menu_quick_verbs()` ON TOP of the shared
// image+audio gate (`menu_item_state` with `top_level: true`), so it's hidden by default, hidden
// when the toggle is off, and hidden on an audio-only selection — exactly like the classic copy.

/// Binds each quick-verb CLSID to the `MENU` item it surfaces and its manifest `desktop5:Verb`
/// id. The keys MUST equal [`verbs::QUICK_KEYS`] in order (a test pins this) so the modern quick
/// verbs and the classic `quick_items()` stay the same set; the verb ids must match the `Id="…"`
/// attributes in `packaging/AppxManifest.xml`.
const QUICK_VERBS: &[(GUID, &str, &str)] = &[
    (crate::guids::CLSID_QUICK_CONVERT_INTO, "menu_convert_into", "SageThumbs2KConvertInto"),
    (crate::guids::CLSID_QUICK_CONVERT_DIALOG, "menu_convert_dialog", "SageThumbs2KConvertDialog"),
    (crate::guids::CLSID_QUICK_RESIZE, "menu_resize", "SageThumbs2KResize"),
    (crate::guids::CLSID_QUICK_ROTATE, "menu_rotate", "SageThumbs2KRotate"),
];

/// Whether `clsid` is one of the modern-menu quick-verb coclasses (so the DLL hands it out).
pub fn is_quick_clsid(clsid: GUID) -> bool {
    QUICK_VERBS.iter().any(|(c, _, _)| *c == clsid)
}

/// The `MENU` item a quick-verb `clsid` surfaces, or `None` if `clsid` isn't a quick verb.
/// Looks the CLSID's key up in [`QUICK_VERBS`], then finds that top-level item in `MENU`.
pub fn quick_root_item(clsid: GUID) -> Option<&'static verbs::MenuItem> {
    let key = QUICK_VERBS.iter().find(|(c, _, _)| *c == clsid).map(|(_, k, _)| *k)?;
    verbs::MENU.iter().find(|it| it.title() == key)
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
                    let v = unsafe { selection_has_image(&items) };
                    self.has_image.set(Some(v));
                    v
                }
            };
            Ok(root_state(has))
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
            // NOTE: the "Quick verbs on the main menu" Setting (`MenuQuickVerbs`) is NOT honored
            // here — and can't be. IExplorerCommand owns only its own single flyout; it has no way
            // to add sibling items to Explorer's main context menu the way the classic
            // QueryContextMenu handler does (`contextmenu.rs` §2). Surfacing them would mean
            // declaring extra top-level verbs in the AppxManifest (separate coclasses), a larger
            // change. The classic menu honors the toggle; on stock Win11 these verbs live one level
            // in, inside this flyout.
            //
            // One snapshot of the visibility subkey for this enumeration (instead of
            // a key-open per item).
            let vis = settings::menu_visibility();
            // CONDENSED mode: an UNSUPPORTED selection with "show on all file types" enabled gets
            // the file-agnostic utility set (Files-to-folder / Sort / Rename / Pick color / Settings),
            // mirroring the classic handler. GetState ran first and cached has_image; `Some(false)`
            // means the selection had no supported image. (Default to the full menu if GetState
            // somehow didn't run — `None` → not condensed.)
            // `!= Some(true)` (not `== Some(false)`): if GetState somehow didn't run first
            // (has_image == None) AND the toggle is on, default to the condensed set — the safe
            // choice for "show on all file types", since the full image menu would otherwise show
            // (and no-op) on an unsupported file.
            let condensed = self.has_image.get() != Some(true) && settings::menu_all_file_types();
            let items: Vec<IExplorerCommand> = if condensed {
                verbs::condensed_top_level()
                    .into_iter()
                    .map(|(it, _)| it)
                    .filter(|it| !matches!(it, verbs::MenuItem::Separator))
                    .filter(|it| vis.shown(it.title()))
                    // Condensed items are file-agnostic → always enabled (the `true` condensed flag).
                    .map(|it| MenuCommand::new(it, true, true).into())
                    .collect()
            } else {
                // `ordered_top_level()` (not raw `MENU`) so the user's drag-reorder in Settings
                // also applies to the modern flyout, matching the classic handler. Leaf indices
                // aren't used here (IExplorerCommand dispatches the action directly), so the `_`
                // start-index is discarded.
                verbs::ordered_top_level()
                    .into_iter()
                    .map(|(it, _)| it)
                    .filter(|it| !matches!(it, verbs::MenuItem::Separator))
                    // Per-item visibility: hide top-level entries the user unticked in Settings.
                    .filter(|it| vis.shown(it.title()))
                    // These ARE the top-level items — `top_level: true` so `GetState` can hide
                    // the image-only ones on an audio-only selection.
                    .map(|it| MenuCommand::new(it, true, false).into())
                    .collect()
            };
            Ok(SubCommandEnum::new(items).into())
        })
    }
}

// ---- Menu node command (a submenu group OR a leaf verb) -----------------

#[implement(IExplorerCommand)]
pub struct MenuCommand {
    _ref: crate::ModuleRef,
    item: &'static verbs::MenuItem,
    /// True when this command is a TOP-LEVEL flyout entry (created by the root's
    /// `EnumSubCommands`), false when it's a child created by a group's own
    /// `EnumSubCommands`. Only top-level image-only verbs are hidden on an audio-only
    /// selection (see [`menu_item_state`]); children inherit the base gate.
    top_level: bool,
    /// True for the CONDENSED items shown on an unsupported selection ("show on all file
    /// types"). These are file-agnostic, so they're enabled whenever the menu is on — they
    /// BYPASS the image/audio gate that would otherwise hide them (the selection is, by
    /// definition, not a supported image here). Propagated to a group's children so a
    /// condensed group's leaves (e.g. Sort ▸ …) aren't hidden by the gate either.
    condensed: bool,
    /// True when this command is a TOP-LEVEL modern-menu QUICK verb (its own coclass +
    /// `desktop5:Verb`, see [`QUICK_VERBS`]) rather than an item inside the root flyout. A
    /// quick root additionally requires `menu_quick_verbs()` in `GetState` and carries the app
    /// icon in `GetIcon` so it reads as ours on the bare modern menu. Always built with
    /// `top_level: true`, `condensed: false`; never propagated to children (a group's leaves
    /// are plain `MenuCommand::new` items).
    quick_root: bool,
}

impl MenuCommand {
    // ModuleRef::default()'s side effect (live-object add-ref) must run; keep the Default call.
    #[allow(clippy::default_constructed_unit_structs)]
    fn new(item: &'static verbs::MenuItem, top_level: bool, condensed: bool) -> Self {
        Self { _ref: crate::ModuleRef::default(), item, top_level, condensed, quick_root: false }
    }

    /// A top-level modern-menu quick verb wrapping a `MENU` group/leaf (see [`QUICK_VERBS`]).
    /// Top-level (so the audio-only gate applies) and never condensed.
    #[allow(clippy::default_constructed_unit_structs)]
    pub fn quick_root(item: &'static verbs::MenuItem) -> Self {
        Self {
            _ref: crate::ModuleRef::default(),
            item,
            top_level: true,
            condensed: false,
            quick_root: true,
        }
    }
}

impl IExplorerCommand_Impl for MenuCommand_Impl {
    fn GetTitle(&self, _items: Ref<'_, IShellItemArray>) -> Result<PWSTR> {
        safety::guard_val(|| alloc_pwstr(crate::i18n::t(self.item.title())))
    }
    fn GetIcon(&self, _items: Ref<'_, IShellItemArray>) -> Result<PWSTR> {
        // A top-level quick verb carries the app icon (like the root command) so it's
        // recognizable as ours on the bare modern menu; flyout children stay icon-less.
        if !self.quick_root {
            return Err(Error::from(E_NOTIMPL));
        }
        safety::guard_val(|| {
            let exe = crate::sibling_of_dll(crate::APP_EXE).ok_or_else(|| Error::from(E_NOTIMPL))?;
            alloc_pwstr(&format!("{},-1", exe.display()))
        })
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
        safety::guard_val(|| {
            // A top-level quick verb also requires the quick-verbs Option (so it's hidden by
            // default), then the shared image+audio gate (`top_level: true` → hidden on an
            // audio-only selection). Re-read each call so a settings change is honored live.
            if self.quick_root {
                if !settings::menu_quick_verbs() {
                    return Ok(ECS_HIDDEN.0 as u32);
                }
                return Ok(unsafe { menu_item_state(self.item, true, &items) });
            }
            // Condensed (file-agnostic) items skip the image/audio gate: enabled while the menu is on.
            if self.condensed {
                return Ok(if settings::menu_enabled() {
                    ECS_ENABLED.0 as u32
                } else {
                    ECS_HIDDEN.0 as u32
                });
            }
            Ok(unsafe { menu_item_state(self.item, self.top_level, &items) })
        })
    }
    fn Invoke(&self, items: Ref<'_, IShellItemArray>, _ctx: Ref<'_, IBindCtx>) -> Result<()> {
        safety::guard(|| {
            if let verbs::MenuItem::Verb(_, action) = self.item {
                let paths = unsafe { items_to_paths(items) };
                // Detached worker (see contextmenu.rs): return from Invoke immediately so
                // the shell thread isn't blocked for the batch. No parent HWND handy here,
                // so the error MessageBox (if any) is a top-level dialog.
                verbs::run_action_detached(*action, paths, None);
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
                    // Children of a group — `top_level: false` so the audio gate never
                    // hides individual leaves (e.g. the Rename ▸ audio patterns). Propagate
                    // `condensed` so a condensed group's leaves stay enabled too.
                    .map(|c| MenuCommand::new(c, false, self.condensed).into())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The modern quick verbs MUST be the same set, in the same order, as the classic
    /// `quick_items()` — i.e. [`QUICK_VERBS`] keys == [`verbs::QUICK_KEYS`]. If `QUICK_KEYS`
    /// changes (a quick verb added/removed/reordered) without updating the CLSID table + the
    /// manifest verbs, the two menus would silently diverge — this turns that into a CI failure.
    #[test]
    fn quick_verbs_match_quick_keys() {
        let keys: Vec<&str> = QUICK_VERBS.iter().map(|(_, k, _)| *k).collect();
        assert_eq!(keys, verbs::QUICK_KEYS, "QUICK_VERBS keys must equal verbs::QUICK_KEYS");
    }

    /// Every quick-verb CLSID resolves to a real top-level `MENU` item, and `is_quick_clsid`
    /// recognizes it; a non-quick CLSID does not.
    #[test]
    fn quick_clsids_resolve_to_menu_items() {
        for (clsid, key, _) in QUICK_VERBS {
            assert!(is_quick_clsid(*clsid), "is_quick_clsid missed {key}");
            let item = quick_root_item(*clsid).unwrap_or_else(|| panic!("no MENU item for {key}"));
            assert_eq!(item.title(), *key, "quick_root_item returned the wrong MENU node for {key}");
        }
        assert!(!is_quick_clsid(crate::guids::CLSID_EXPLORER_COMMAND));
        assert!(quick_root_item(crate::guids::CLSID_EXPLORER_COMMAND).is_none());
    }

    /// The quick-verb CLSIDs are all distinct (a copy-paste dup would make two verbs activate
    /// the same coclass and silently collapse to one item).
    #[test]
    fn quick_clsids_are_distinct() {
        for (i, (a, _, _)) in QUICK_VERBS.iter().enumerate() {
            for (b, _, _) in &QUICK_VERBS[i + 1..] {
                assert_ne!(a, b, "duplicate quick-verb CLSID");
            }
        }
    }
}
