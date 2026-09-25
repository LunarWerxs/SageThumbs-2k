//! Windows 11 context-menu verbs: a top-level "SageThumbs" IExplorerCommand
//! with a sub-command flyout (IEnumExplorerCommand) of the convert verbs.
//!
//! The shell instantiates the root command (CLSID_EXPLORER_COMMAND) via the
//! class factory; the leaf verbs are created internally by EnumSubCommands.

use core::cell::Cell;
use core::ffi::c_void;

use windows::core::{Error, Interface, Ref, Result, BOOL, GUID, HRESULT, PWSTR};
use windows::Win32::Foundation::{E_NOTIMPL, E_POINTER, S_FALSE, S_OK};
use windows::Win32::System::Com::{CoTaskMemFree, IBindCtx};
use windows::Win32::UI::Shell::{
    IEnumExplorerCommand, IEnumExplorerCommand_Impl, IExplorerCommand, IExplorerCommand_Impl,
    IShellItemArray, ECF_DEFAULT, ECF_HASSUBCOMMANDS, ECS_ENABLED, ECS_HIDDEN, SIGDN_FILESYSPATH,
};
use windows_implement::implement;

use st2k_actions::verbs;
use st2k_base::host::alloc_pwstr;
use st2k_base::{safety, settings};

/// COM's documented `IExplorerCommand::GetState` signal for "this would be slow and
/// `fOkToBeSlow` said not to block — ask again once it's OK to be slow" (`Urlmon`'s
/// `E_PENDING`, `0x8000000A`). Spelled out as a literal `HRESULT` rather than pulling
/// in `Win32_System_Com_Urlmon` (this crate's only reason to touch that feature) for
/// one constant.
const E_PENDING: HRESULT = HRESULT(0x8000_000A_u32 as i32);

/// The companion EXE's app icon as the modern menu's `"<module>,-<resid>"` reference
/// (resource 1). Installed next to the DLL — if it isn't there, no icon (`E_NOTIMPL`),
/// never an error.
fn app_icon_ref() -> Result<PWSTR> {
    safety::guard_val(|| {
        let exe = st2k_base::host::sibling_of_dll(st2k_base::host::APP_EXE)
            .ok_or_else(|| Error::from(E_NOTIMPL))?;
        alloc_pwstr(&format!("{},-1", exe.display()))
    })
}

/// The `E_NOTIMPL` failure shared by the COM members that have no value to report
/// (`GetToolTip`, `GetCanonicalName`), for both result types they declare.
fn not_implemented<T>() -> Result<T> {
    Err(Error::from(E_NOTIMPL))
}

/// Extract filesystem paths from a shell selection (the IShellItemArray the
/// shell passes to Invoke). Null/empty selection yields an empty Vec.
unsafe fn items_to_paths(items: Ref<'_, IShellItemArray>) -> Vec<String> {
    match items.ok() {
        Ok(arr) => array_to_paths(arr),
        Err(_) => Vec::new(),
    }
}

/// Park the selection in the Global Interface Table so the verb's worker can fetch it
/// in its own apartment and walk it there ([`paths_from_global`]). `None` when the table
/// refuses (the caller then walks inline, as before). The table holds its own reference,
/// so the array outlives the shell's `Invoke` call; the worker revokes it.
unsafe fn park_selection(items: &Ref<'_, IShellItemArray>) -> Option<u32> {
    let arr = items.ok().ok()?;
    let git = st2k_codecs::video::global_interface_table()?;
    git.RegisterInterfaceInGlobal(arr, &IShellItemArray::IID)
        .ok()
}

/// RAII owner of a [`park_selection`] cookie. It revokes the table entry on drop unless
/// [`ParkedCookie::revoke`] already did, so a cookie whose worker never starts (the
/// `spawn` in `run_action_detached_with` failed, dropping the closure) is still revoked
/// instead of leaking the array's reference for explorer.exe's lifetime.
struct ParkedCookie(Option<u32>);

impl ParkedCookie {
    /// Revoke the entry now (whatever happens next) and disarm the drop.
    fn revoke(&mut self) {
        if let Some(cookie) = self.0.take() {
            unsafe {
                if let Some(git) = st2k_codecs::video::global_interface_table() {
                    let _ = git.RevokeInterfaceFromGlobal(cookie);
                }
            }
        }
    }
}

impl Drop for ParkedCookie {
    fn drop(&mut self) {
        self.revoke();
    }
}

/// The worker half of [`park_selection`]: fetch the array back out of the table, revoke
/// the entry (whatever happens next), and walk it here. An empty Vec if the fetch fails.
unsafe fn paths_from_global(mut cookie: ParkedCookie) -> Vec<String> {
    let Some(entry) = cookie.0 else {
        return Vec::new();
    };
    let Some(git) = st2k_codecs::video::global_interface_table() else {
        return Vec::new();
    };
    let mut raw: *mut c_void = std::ptr::null_mut();
    let fetched = git
        .GetInterfaceFromGlobal(entry, &IShellItemArray::IID, &mut raw)
        .is_ok();
    cookie.revoke();
    if !fetched || raw.is_null() {
        safety::log("command: selection could not be fetched from the interface table");
        return Vec::new();
    }
    // SAFETY: a live, AddRef'd IShellItemArray the table just handed this apartment.
    let arr = IShellItemArray::from_raw(raw);
    array_to_paths(&arr)
}

/// Walk an `IShellItemArray` into filesystem paths; items without one are skipped.
unsafe fn array_to_paths(arr: &IShellItemArray) -> Vec<String> {
    let mut out = Vec::new();
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
/// can also feed `selection_is_audio_only` in `MenuCommand::state`.
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

/// True if the selection is non-empty AND every file is video. Mirrors the classic
/// `video_only` gate (`contextmenu/com.rs::selection_kinds`) so the modern flyout narrows to
/// the video verbs instead of offering the full image tree on a selection none of it can read.
/// Audio wins where both could match: `selection_is_audio_only` is consulted first below, and
/// a file cannot be both.
unsafe fn selection_is_video_only(items: &Ref<'_, IShellItemArray>) -> bool {
    every_item_matches(items, verbs::is_video)
}

/// The walk behind `selection_is_audio_only` and `selection_is_video_only`: true when the
/// selection is NON-EMPTY and `pred` holds for every item. Each display name is freed as it
/// goes, and an unreadable item answers false rather than being skipped, because "every item
/// is X" must not become true by ignoring the ones we could not read.
unsafe fn every_item_matches(items: &Ref<'_, IShellItemArray>, pred: fn(&str) -> bool) -> bool {
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
        let hit = pw.to_string().map(|s| pred(&s)).unwrap_or(false);
        CoTaskMemFree(Some(pw.0 as *const c_void));
        if !hit {
            return false;
        }
    }
    true
}

/// True if the selection is non-empty AND every file is audio (music). Mirrors the
/// classic `audio_only` gate (`contextmenu.rs`): an audio-only selection hides the
/// image-only top-level verbs in the modern flyout. Unlike `selection_has_image` this
/// must visit EVERY item (one non-audio file flips the answer false), freeing each
/// display name as it goes. An empty / unreadable selection is not audio-only.
unsafe fn selection_is_audio_only(items: &Ref<'_, IShellItemArray>) -> bool {
    every_item_matches(items, verbs::is_audio)
}

/// Enabled only when the selection contains a supported image — mirrors the
/// classic `IContextMenu` gate (`contextmenu.rs`) so the modern Win11 menu
/// behaves the same. `ECS_HIDDEN` removes the verb from the flyout entirely.
/// The enabled/hidden verdict for the flyout verbs: the verb shows only when the
/// menu is enabled AND the selection holds a supported image.
/// `gate` is a snapshot (see [`settings::MenuGate`]), not a fresh registry read here —
/// the caller re-fetches it each `GetState` call, so a settings change is still
/// honored without a new command object, but without a separate open per flag.
fn state_for(gate: settings::MenuGate, has_image: bool) -> u32 {
    if gate.enabled && has_image {
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
fn root_state(gate: settings::MenuGate, has_image: bool) -> u32 {
    if gate.enabled && (has_image || gate.all_file_types) {
        ECS_ENABLED.0 as u32
    } else {
        ECS_HIDDEN.0 as u32
    }
}

/// Whether a top-level modern-menu QUICK verb should be visible at all, ahead of the
/// shared image/audio gate: both the "Quick verbs on the main menu" master toggle
/// AND this item's own per-item "Menu items" visibility must be on. Named/split out
/// so the combining logic has an independent test — the quick-verb CLSIDs are
/// standalone top-level `IExplorerCommand` objects with their own CLSID, never routed
/// through `EnumSubCommands`' `vis.shown()` filter, so without this check hiding a
/// quick verb in Settings' "Menu items" list did nothing for it.
fn quick_root_visible(quick_verbs_on: bool, item_shown: bool) -> bool {
    quick_verbs_on && item_shown
}

// ---- Modern-menu quick verbs --------------------------------------------
//
// Each quick verb (Convert into ▸ / Convert… / Resize ▸ / Rotate ▸) is its OWN top-level
// IExplorerCommand coclass (own CLSID + `desktop5:Verb` in the package manifest), so Windows 11
// surfaces it DIRECTLY on the modern context menu instead of two levels deep inside the root
// flyout — the modern twin of the classic "quick verbs on main menu" Option (the limitation the
// root `EnumSubCommands` note describes). They reuse the `MenuCommand` flyout machinery: a quick
// verb is a `MenuCommand` flagged `quick_root`, gated by the `gate.quick_verbs` snapshot ON
// TOP of the shared image+audio gate (`MenuCommand::state` with `top_level: true`), so it's
// hidden by default, hidden when the toggle is off, and hidden on an audio-only selection —
// exactly like the classic copy.

/// Binds each quick-verb CLSID to the `MENU` item it surfaces and its manifest `desktop5:Verb`
/// id. The keys MUST equal [`verbs::QUICK_KEYS`] in order (a test pins this) so the modern quick
/// verbs and the classic `quick_items()` stay the same set; the verb ids must match the `Id="…"`
/// attributes in `scripts/packaging/AppxManifest.xml`.
const QUICK_VERBS: &[(GUID, &str, &str)] = &[
    (
        st2k_base::guids::CLSID_QUICK_CONVERT_INTO,
        "menu_convert_into",
        "SageThumbs2KConvertInto",
    ),
    (
        st2k_base::guids::CLSID_QUICK_CONVERT_DIALOG,
        "menu_convert_dialog",
        "SageThumbs2KConvertDialog",
    ),
    (
        st2k_base::guids::CLSID_QUICK_RESIZE,
        "menu_resize",
        "SageThumbs2KResize",
    ),
    (
        st2k_base::guids::CLSID_QUICK_ROTATE,
        "menu_rotate",
        "SageThumbs2KRotate",
    ),
];

/// Whether `clsid` is one of the modern-menu quick-verb coclasses (so the DLL hands it out).
pub fn is_quick_clsid(clsid: GUID) -> bool {
    QUICK_VERBS.iter().any(|(c, _, _)| *c == clsid)
}

/// The `MENU` item a quick-verb `clsid` surfaces, or `None` if `clsid` isn't a quick verb.
/// Looks the CLSID's key up in [`QUICK_VERBS`], then finds that top-level item in `MENU`.
pub fn quick_root_item(clsid: GUID) -> Option<&'static verbs::MenuItem> {
    let key = QUICK_VERBS
        .iter()
        .find(|(c, _, _)| *c == clsid)
        .map(|(_, k, _)| *k)?;
    verbs::MENU.iter().find(|it| it.title() == key)
}

// ---- Root command -------------------------------------------------------

#[implement(IExplorerCommand)]
pub struct ExplorerCommand {
    _ref: st2k_base::host::ModuleRef,
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
            _ref: st2k_base::host::ModuleRef::default(),
            has_image: Cell::new(None),
        }
    }
}

/// Map a raw top-level verb list ([`verbs::ordered_top_level`] /
/// [`verbs::condensed_top_level`]) to modern-menu commands: drop the separators, honor the
/// per-item visibility snapshot, and build one [`MenuCommand`] per item. All are top-level
/// (`top_level: true`) so `GetState` can hide the image-only ones on an audio-only
/// selection; `condensed` picks the always-enabled file-agnostic gate flag for the items
/// shown on an unsupported selection.
fn top_level_commands(
    entries: Vec<(&'static verbs::MenuItem, u32)>,
    vis: &settings::MenuVisibility,
    condensed: bool,
    gate: settings::MenuGate,
) -> Vec<IExplorerCommand> {
    entries
        .into_iter()
        .map(|(it, _)| it)
        .filter(|it| !matches!(it, verbs::MenuItem::Separator))
        .filter(|it| vis.shown(it.title()))
        .map(|it| MenuCommand::new(it, true, condensed, gate).into())
        .collect()
}

impl IExplorerCommand_Impl for ExplorerCommand_Impl {
    fn GetTitle(&self, _items: Ref<'_, IShellItemArray>) -> Result<PWSTR> {
        safety::guard_val(|| alloc_pwstr("SageThumbs 2K"))
    }
    fn GetIcon(&self, _items: Ref<'_, IShellItemArray>) -> Result<PWSTR> {
        app_icon_ref()
    }
    fn GetToolTip(&self, _items: Ref<'_, IShellItemArray>) -> Result<PWSTR> {
        not_implemented()
    }
    fn GetCanonicalName(&self) -> Result<GUID> {
        // No stable canonical verb name (we'd return GUID_NULL); report
        // not-implemented to match the rest of the surface instead of an
        // S_OK + null GUID the shell would treat as meaningful.
        not_implemented()
    }
    fn GetState(&self, items: Ref<'_, IShellItemArray>, slow: BOOL) -> Result<u32> {
        safety::guard_val(|| {
            // Cache the (selection-fixed) image verdict per instance; re-fetch the
            // (now one-open) menu gate each call so a settings change is honored
            // without a new command object.
            let has = match self.has_image.get() {
                Some(v) => v,
                // A full array walk is the "slow" work `fOkToBeSlow` exists to
                // defer; with nothing cached yet, ask the shell to try again once
                // it's OK to be slow instead of walking on its say-so.
                None if !slow.as_bool() => return Err(Error::from(E_PENDING)),
                None => {
                    let v = unsafe { selection_has_image(&items) };
                    self.has_image.set(Some(v));
                    v
                }
            };
            Ok(root_state(settings::menu_gate(), has))
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
            // One registry open for the whole enumeration, shared with every `MenuCommand`
            // this call creates — each child's own `GetState` reuses this snapshot instead
            // of re-opening the same three flags itself.
            let gate = settings::menu_gate();
            // CONDENSED mode: an UNSUPPORTED selection with "show on all file types" enabled gets
            // the file-agnostic utility set (Files-to-folder / Sort / Rename / Pick color / Settings),
            // mirroring the classic handler. GetState ran first and cached has_image; `Some(false)`
            // means the selection had no supported image.
            // `!= Some(true)` (not `== Some(false)`): if GetState somehow didn't run first
            // (has_image == None) AND the toggle is on, default to the condensed set — the safe
            // choice for "show on all file types", since the full image menu would otherwise show
            // (and no-op) on an unsupported file.
            let condensed = self.has_image.get() != Some(true) && gate.all_file_types;
            let items: Vec<IExplorerCommand> = if condensed {
                // Condensed items are file-agnostic → always enabled (the `true` condensed flag).
                top_level_commands(verbs::condensed_top_level(), &vis, true, gate)
            } else {
                // `ordered_top_level()` (not raw `MENU`) so the user's drag-reorder in Settings
                // also applies to the modern flyout, matching the classic handler. Leaf indices
                // aren't used here (IExplorerCommand dispatches the action directly), so the `_`
                // start-index is discarded.
                top_level_commands(verbs::ordered_top_level(), &vis, false, gate)
            };
            Ok(SubCommandEnum::new(items).into())
        })
    }
}

// ---- Menu node command (a submenu group OR a leaf verb) -----------------

#[implement(IExplorerCommand)]
pub struct MenuCommand {
    _ref: st2k_base::host::ModuleRef,
    item: &'static verbs::MenuItem,
    /// True when this command is a TOP-LEVEL flyout entry (created by the root's
    /// `EnumSubCommands`), false when it's a child created by a group's own
    /// `EnumSubCommands`. Only top-level image-only verbs are hidden on an audio-only
    /// selection (see [`Self::state`]); children inherit the base gate.
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
    /// Snapshot of the enabled/all-file-types/quick-verbs flags, taken once — by the root's
    /// `EnumSubCommands` for every item it creates in one call, propagated to a group's own
    /// children, or (for a top-level quick verb, which `factory.rs` creates with no
    /// enumeration context to share) taken fresh at construction — rather than re-opened by
    /// every one of the ~15 top-level commands' own `GetState`.
    gate: settings::MenuGate,
    /// Cached "selection has an image" verdict for THIS instance, mirroring
    /// `ExplorerCommand::has_image`: the shell may call `GetState` more than once on the
    /// same command for a fixed selection, so the array is walked at most once per instance
    /// rather than once per call.
    has_image: Cell<Option<bool>>,
    /// Cached "selection is audio-only" verdict, computed lazily — only a top-level item
    /// that isn't already audio-ok ever needs it (see [`Self::state`]) — and at most once
    /// per instance.
    audio_only: Cell<Option<bool>>,
    /// Same shape as [`Self::audio_only`], for a video-only selection: the flyout keeps only
    /// the verbs a video actually supports. Cached per command for the same reason — the shell
    /// calls `GetState` once per top-level item and the walk is O(selection).
    video_only: Cell<Option<bool>>,
}

impl MenuCommand {
    // ModuleRef::default()'s side effect (live-object add-ref) must run; keep the Default call.
    #[allow(clippy::default_constructed_unit_structs)]
    fn new(
        item: &'static verbs::MenuItem,
        top_level: bool,
        condensed: bool,
        gate: settings::MenuGate,
    ) -> Self {
        Self {
            _ref: st2k_base::host::ModuleRef::default(),
            item,
            top_level,
            condensed,
            quick_root: false,
            gate,
            has_image: Cell::new(None),
            audio_only: Cell::new(None),
            video_only: Cell::new(None),
        }
    }

    /// A top-level modern-menu quick verb wrapping a `MENU` group/leaf (see [`QUICK_VERBS`]).
    /// Top-level (so the audio-only gate applies) and never condensed. `factory.rs` creates
    /// this directly from a CLSID with no enumeration context to share a gate snapshot from,
    /// so it takes its own here — still once per instance rather than once per `GetState` call.
    #[allow(clippy::default_constructed_unit_structs)]
    pub fn quick_root(item: &'static verbs::MenuItem) -> Self {
        Self {
            _ref: st2k_base::host::ModuleRef::default(),
            item,
            top_level: true,
            condensed: false,
            quick_root: true,
            gate: settings::menu_gate(),
            has_image: Cell::new(None),
            audio_only: Cell::new(None),
            video_only: Cell::new(None),
        }
    }

    /// Cached, `fSlowOk`-honoring image/audio verdict for this instance (see the
    /// `has_image`/`audio_only` field docs). A full array walk is exactly the "slow"
    /// work `fOkToBeSlow` exists to defer: when nothing is cached yet and the shell
    /// says not to block, this returns `E_PENDING` (the documented
    /// `IExplorerCommand::GetState` contract) instead of walking, so Explorer asks
    /// again once it's OK to be slow rather than stalling its own thread once per
    /// top-level item on a large selection.
    unsafe fn state(&self, items: &Ref<'_, IShellItemArray>, slow_ok: bool) -> Result<u32> {
        let has_image = cached_verdict(&self.has_image, slow_ok, items, selection_has_image)?;
        let base = state_for(self.gate, has_image);
        if base != ECS_ENABLED.0 as u32 {
            return Ok(base); // menu off or unsupported selection — already hidden
        }
        if self.top_level && !verbs::top_level_audio_ok(self.item.title()) {
            let audio_only =
                cached_verdict(&self.audio_only, slow_ok, items, selection_is_audio_only)?;
            if audio_only {
                return Ok(ECS_HIDDEN.0 as u32);
            }
        }
        if self.top_level && !verbs::top_level_video_ok(self.item.title()) {
            let video_only =
                cached_verdict(&self.video_only, slow_ok, items, selection_is_video_only)?;
            if video_only {
                return Ok(ECS_HIDDEN.0 as u32);
            }
        }
        Ok(base)
    }
}

/// Cached per-selection verdict: returns `cell`'s value, or runs `walk` and caches its
/// result on a miss — unless `!slow_ok`, when it defers with `E_PENDING` instead.
fn cached_verdict(
    cell: &Cell<Option<bool>>,
    slow_ok: bool,
    items: &Ref<'_, IShellItemArray>,
    walk: unsafe fn(&Ref<'_, IShellItemArray>) -> bool,
) -> Result<bool> {
    match cell.get() {
        Some(v) => Ok(v),
        None if !slow_ok => Err(Error::from(E_PENDING)),
        None => {
            let v = unsafe { walk(items) };
            cell.set(Some(v));
            Ok(v)
        }
    }
}

impl IExplorerCommand_Impl for MenuCommand_Impl {
    fn GetTitle(&self, _items: Ref<'_, IShellItemArray>) -> Result<PWSTR> {
        safety::guard_val(|| alloc_pwstr(st2k_base::i18n::t(self.item.title())))
    }
    fn GetIcon(&self, _items: Ref<'_, IShellItemArray>) -> Result<PWSTR> {
        // A top-level quick verb carries the app icon (like the root command) so it's
        // recognizable as ours on the bare modern menu; flyout children stay icon-less.
        if !self.quick_root {
            return Err(Error::from(E_NOTIMPL));
        }
        app_icon_ref()
    }
    fn GetToolTip(&self, _items: Ref<'_, IShellItemArray>) -> Result<PWSTR> {
        not_implemented()
    }
    fn GetCanonicalName(&self) -> Result<GUID> {
        // No stable canonical verb name; not-implemented (was S_OK + GUID_NULL),
        // matching the root command and the rest of the COM surface.
        not_implemented()
    }
    fn GetState(&self, items: Ref<'_, IShellItemArray>, slow: BOOL) -> Result<u32> {
        safety::guard_val(|| {
            let slow_ok = slow.as_bool();
            // A top-level quick verb also requires the quick-verbs Option (so it's hidden by
            // default), then the shared image+audio gate (`top_level: true` → hidden on an
            // audio-only selection). `menu_visibility()` is a separate per-item setting, not
            // part of the `gate` snapshot, and stays a live read like before.
            if self.quick_root {
                let visible = quick_root_visible(
                    self.gate.quick_verbs,
                    settings::menu_visibility().shown(self.item.title()),
                );
                if !visible {
                    return Ok(ECS_HIDDEN.0 as u32);
                }
                return unsafe { self.state(&items, slow_ok) };
            }
            // Condensed (file-agnostic) items skip the image/audio gate: enabled while the menu is on.
            if self.condensed {
                return Ok(if self.gate.enabled {
                    ECS_ENABLED.0 as u32
                } else {
                    ECS_HIDDEN.0 as u32
                });
            }
            unsafe { self.state(&items, slow_ok) }
        })
    }
    fn Invoke(&self, items: Ref<'_, IShellItemArray>, _ctx: Ref<'_, IBindCtx>) -> Result<()> {
        safety::guard(|| {
            if let verbs::MenuItem::Verb(_, action) = self.item {
                // The selection walk (one `GetDisplayName` per item) used to run HERE, on
                // the shell thread, before the worker started; a large selection paid it
                // in explorer.exe. The array now goes into the Global Interface Table and
                // the worker fetches and walks it in its own apartment, so this call
                // costs one `GetCount` and the spawn. If the table refuses, walk inline
                // as before rather than drop the click.
                // Detached worker (see contextmenu.rs): return from Invoke immediately so
                // the shell thread isn't blocked for the batch. No parent HWND handy here,
                // so the error MessageBox (if any) is a top-level dialog.
                match unsafe { park_selection(&items) } {
                    Some(cookie) => {
                        let cookie = ParkedCookie(Some(cookie));
                        let hint = unsafe { items.ok().and_then(|a| a.GetCount()) }.unwrap_or(0);
                        verbs::run_action_detached_with(
                            *action,
                            hint as usize,
                            move || unsafe { paths_from_global(cookie) },
                            None,
                        );
                    }
                    None => {
                        let paths = unsafe { items_to_paths(items) };
                        verbs::run_action_detached(*action, paths, None);
                    }
                }
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
                    // `condensed` and the same `gate` snapshot so a condensed group's leaves
                    // stay enabled too and don't re-open the registry either.
                    .map(|c| MenuCommand::new(c, false, self.condensed, self.gate).into())
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
    _ref: st2k_base::host::ModuleRef,
    items: Vec<IExplorerCommand>,
    pos: Cell<usize>,
}

impl SubCommandEnum {
    // ModuleRef::default()'s side effect (live-object add-ref) must run; keep the Default call.
    #[allow(clippy::default_constructed_unit_structs)]
    fn new(items: Vec<IExplorerCommand>) -> Self {
        Self {
            _ref: st2k_base::host::ModuleRef::default(),
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
mod tests;
