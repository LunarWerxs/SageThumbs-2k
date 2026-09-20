//! Classic per-extension registration for the thumbnail provider + context menu.
//!
//! A plain in-proc COM server registered via `regsvr32`. Thumbnail providers do
//! NOT need package identity (only the modern `IExplorerCommand` main-flyout does,
//! and that ships as a signed sparse package — see `scripts/packaging/make-msix.ps1`), and the shell
//! runs us out-of-process in its isolated host automatically.
//!
//! Every machine-wide write goes to `HKLM\SOFTWARE\Classes` EXPLICITLY, never through
//! `HKEY_CLASSES_ROOT`: Windows routes an HKCR write to `HKCU\Software\Classes` whenever the
//! key already exists there, so with a portable per-user registration present a
//! `CLASSES_ROOT.create(...)` would silently land the Program Files path and every shellex
//! value in the elevated user's own hive. HKLM would never receive the CLSID, the merged
//! view would still read as registered, other accounts would get nothing, and the uninstall
//! would delete the HKCU copy instead. [`register`] also clears such a per-user registration
//! first, for the same reason `cli.rs` refuses the reverse order (portable-on-installed).
//!
//! KNOWN LIMITATION: Windows resolves a thumbnail handler in priority order —
//! per-user UserChoice ProgID, then the extension's default ProgID's `shellex`,
//! then `SystemFileAssociations`, then the bare-extension key. We register the
//! last two (non-invasively). For formats whose default ProgID already carries a
//! thumbnail handler (e.g. .jpg/.png via the Photos app), that handler still
//! wins. That's acceptable: SageThumbs' value is the formats Windows can't
//! thumbnail at all, where the bare/association key wins. The sparse-package
//! `fileTypeAssociation/ThumbnailHandler` path would sidestep this precedence
//! entirely if it's ever needed.

use windows::core::{Error, Result, HRESULT};
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::UI::Shell::{SHChangeNotify, SHCNE_ASSOCCHANGED, SHCNF_IDLIST};
use windows_registry::{Key, CURRENT_USER, LOCAL_MACHINE};
mod propstore;
use propstore::*;
mod displaced;
use displaced::*;
mod user;
pub(crate) use displaced::{displaced_handlers, displaced_key_ext};
pub(crate) use propstore::perceived_type_is_ours;
use user::*;
pub use user::{
    dll_beside_exe, register_user, unregister_user, user_registration_is_here,
    user_registration_path,
};

use crate::formats::{Category, FORMATS, REMOVED_EXTENSIONS};
use crate::guids::{
    CLSID_CONTEXT_MENU_STR, CLSID_PREVIEW_HANDLER_STR, CLSID_PROPERTY_STORE_STR,
    CLSID_THUMBNAIL_PROVIDER_STR, PREVHOST_APPID, PREVIEW_HANDLER_CATEGORY, THUMB_HANDLER_CATEGORY,
};
use crate::safety::{log, log_error};
use crate::settings::{self, FormatEnabledSnapshot};

const NAME: &str = "SageThumbs 2K Thumbnail Provider";
const CM_NAME: &str = "SageThumbs 2K Context Menu";
const PV_NAME: &str = "SageThumbs 2K Preview Handler";
const PS_NAME: &str = "SageThumbs 2K Property Handler";
/// The machine-wide half of `HKEY_CLASSES_ROOT` (see the module doc for why it is named
/// explicitly). The per-user half is [`user_classes`].
const MACHINE_CLASSES: &str = r"SOFTWARE\Classes";
/// The classic `IContextMenu` handler's `shellex` slot, under `*` (all files); the handler
/// filters to images inside `QueryContextMenu`.
const CONTEXT_MENU_KEY: &str = "*\\shellex\\ContextMenuHandlers\\SageThumbs2K";
/// The machine-wide list mapping an extension to its IPropertyStore handler CLSID.
const PROPERTY_HANDLERS: &str =
    r"SOFTWARE\Microsoft\Windows\CurrentVersion\PropertySystem\PropertyHandlers";
/// The Properties▸Details *tab* layout. Comprehensive — every property the store can emit:
/// Dimensions/BitDepth/DPI/DateTaken/GPS for images, Artist/Genre/Year/Duration/Bitrate for
/// audio, frame size for video. Includes `System.DateCreated` (the pane list already had it —
/// the two were inconsistent before).
const PROP_FULLDETAILS: &str = "prop:System.Image.Dimensions;System.Image.HorizontalSize;System.Image.VerticalSize;System.Image.BitDepth;System.Image.HorizontalResolution;System.Image.VerticalResolution;System.Photo.CameraManufacturer;System.Photo.CameraModel;System.Photo.DateTaken;System.GPS.LatitudeDecimal;System.GPS.LongitudeDecimal;System.Video.FrameWidth;System.Video.FrameHeight;System.Media.Duration;System.Audio.EncodingBitrate;System.Music.Artist;System.Music.AlbumTitle;System.Title;System.Music.TrackNumber;System.Music.Genre;System.Media.Year;System.Size;System.DateCreated;System.DateModified";
/// `System.PropList.AdditionalProperties` — the per-type column set Explorer offers in the
/// "Choose columns…" / right-click-header picker for these formats. Without it our properties
/// are reachable only via "All properties", so a folder of PSDs/RAWs never *offers* Dimensions/
/// DateTaken as a sortable column. This makes the docs' "sortable/groupable columns" claim real.
const PROP_ADDITIONAL: &str = "prop:System.Image.Dimensions;System.Image.BitDepth;System.Photo.DateTaken;System.Photo.CameraModel;System.Media.Duration;System.Audio.EncodingBitrate;System.Music.Artist;System.Music.AlbumTitle;System.Title;System.Music.TrackNumber;System.Music.Genre;System.Media.Year";
/// Marker value written next to a `PerceivedType` WE set, so [`unhook_perceived_type`] can remove
/// ours without clobbering a value Windows or another app owns.
const PERCEIVED_TYPE_MARK: &str = "SageThumbs2K.PerceivedTypeOwner";
/// The machine-wide list the preview pane consults for registered handlers.
const PREVIEW_HANDLERS: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\PreviewHandlers";
const APPROVED: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Shell Extensions\Approved";
/// Image formats whose `PerceivedType=image` is safe to stamp: WIC (and so Photos) opens them,
/// so the verbs Windows attaches to that type (Rotate, Print, Set as background, Edit with
/// Photos) work. The rest of the Image category (PSD, KRA, XCF, …) would get the same verbs
/// and have them fail on a file WIC cannot encode, so those get no `PerceivedType` from us.
/// Camera RAW keeps `image`: the in-box RAW codec opens it.
pub(crate) const WIC_IMAGE_EXTS: &[&str] = &[
    "jpg", "jpeg", "jpe", "jfif", "png", "gif", "bmp", "dib", "tif", "tiff", "heic", "heif", "hif",
    "avif", "webp", "jxr", "wdp", "hdp", "ico", "dds",
];

/// `HKLM\SOFTWARE\Classes`, opened for writing. See the module doc for why this and never
/// `CLASSES_ROOT`.
fn machine_classes() -> Result<Key> {
    LOCAL_MACHINE.create(MACHINE_CLASSES)
}

/// Outcome tally of one best-effort per-extension pass, so a pass that silently wrote
/// nothing (an ACL-locked `SystemFileAssociations`, say) is reported instead of passing as a
/// clean install with no thumbnails and no log line.
#[derive(Default)]
struct Pass {
    written: usize,
    failed: usize,
    /// The first failure's key path and HRESULT, for the log line.
    first_failure: Option<(String, HRESULT)>,
}

impl Pass {
    fn note(&mut self, path: &str, r: Result<()>) {
        match r {
            Ok(()) => self.written += 1,
            Err(e) => {
                self.failed += 1;
                if self.first_failure.is_none() {
                    self.first_failure = Some((path.to_string(), e.code()));
                }
            }
        }
    }

    /// Log the pass when anything failed. True when something was attempted and NOTHING was
    /// written, i.e. the pass as a whole did not happen.
    fn report(&self, what: &str) -> bool {
        if let Some((path, hr)) = &self.first_failure {
            log_error(&format!(
                "register: {what}: {} of {} keys failed; first {path}: hr={:#010x}",
                self.failed,
                self.written + self.failed,
                hr.0
            ));
        }
        self.written == 0 && self.failed > 0
    }
}

/// (Re-)register the shell extension machine-wide under `HKLM\SOFTWARE\Classes` + HKLM.
/// NOTE: the per-extension on/off flags this reads live in the elevated user's HKCU, but the
/// registration they gate is MACHINE-WIDE and so applies to ALL users — there is no per-user
/// thumbnail gate, by design. (See the matching note on [`settings::format_enabled`].)
///
/// The per-user pieces (the folder verb, the type-overlay suppression) are NOT written here:
/// this runs elevated, in whichever account `regsvr32` was launched as, so it cannot write
/// the installing user's HKCU. The installer runs [`sync_user_shell`] as the original user
/// afterwards.
///
/// Returns `Err` when the thumbnail pass wrote no key at all although formats were enabled,
/// or when the preview or property registration failed, so `regsvr32`/the installer see a
/// failure instead of a clean install that draws nothing. Every pass still runs to the end
/// first: a partial registration is better than none, and the log names what failed.
pub fn register(dll_path: &str) -> Result<()> {
    // A per-user (portable) registration in this account's hive would shadow the machine-wide
    // one in the merged HKCR view and outlive the uninstall, pointing at a DLL that is gone.
    if let Some(prev) = user_registration_path() {
        log(&format!(
            "register: clearing the per-user registration at {prev}, which would shadow the \
             machine-wide one"
        ));
        if let Err(e) = unregister_user_classes() {
            log_error(&format!(
                "register: could not clear the per-user registration: hr={:#010x}",
                e.code().0
            ));
        }
    }

    let classes = machine_classes()?;
    // "Approved Shell Extensions" is mandatory on locked-down systems.
    let approved = LOCAL_MACHINE.create(APPROVED)?;
    // One settings snapshot for all four per-extension sweeps below.
    let fmt = settings::format_enabled_snapshot();

    // The thumbnail provider's COM server.
    register_inproc_server(
        &classes,
        CLSID_THUMBNAIL_PROVIDER_STR,
        NAME,
        dll_path,
        &approved,
    )?;

    // Hook each enabled extension; explicitly unhook disabled ones so a
    // re-register reflects the Settings format list (matches the legacy
    // RegisterExtensions-on-OK behavior). Best-effort per extension: a single
    // failing key (transient lock, locked-down subtree) must NOT abort the whole
    // register and skip the context-menu setup + shell-notify below, but it IS
    // counted, and a pass that wrote nothing fails the call at the end.
    let mut thumbs = Pass::default();
    for (ext, _) in FORMATS {
        if fmt.enabled(ext) {
            hook_ext(&classes, ext, &mut thumbs);
        } else {
            unhook_ext(&classes, ext);
        }
    }
    let thumbs_failed = thumbs.report("thumbnail shellex pass");

    // Sweep away stale hooks from extensions OLDER builds registered but we've since dropped
    // (they're no longer in FORMATS, so the loop above never touches their keys → an upgrade
    // would leave orphan shellex entries pointing at our CLSID). Disjoint from FORMATS (tested),
    // so this never unhooks a live format. Best-effort, one pass per (re-)register.
    for ext in REMOVED_EXTENSIONS {
        unhook_ext_and_prune(&classes, ext);
        unhook_ext_preview_and_prune(&classes, ext);
        unhook_ext_propstore(&classes, ext);
    }

    // The classic IContextMenu handler's COM server (for classic-menu machines:
    // StartAllBack, ExplorerPatcher, or the {86ca1aa0…} tweak). Registered under
    // "*" (all files) and filtered to images inside QueryContextMenu.
    register_inproc_server(
        &classes,
        CLSID_CONTEXT_MENU_STR,
        CM_NAME,
        dll_path,
        &approved,
    )?;
    // Best-effort like the preview/property registration below: the format loop above has
    // already displaced third-party thumbnail handlers, so a policy-locked "*" subtree here
    // must not abort before we ever reach preview/property, leaving thumbs hooked but
    // nothing else set up.
    if let Err(e) = set_shellex_key(&classes, CONTEXT_MENU_KEY, CLSID_CONTEXT_MENU_STR) {
        log_error(&format!(
            "register: context menu key {CONTEXT_MENU_KEY}: hr={:#010x}",
            e.code().0
        ));
    }

    // The preview-pane handler and the property handler (Details pane / info-tip / columns).
    // Neither aborts the other or the shell-notify below; both are reported at the end.
    let preview = register_preview_handler(&classes, dll_path, &approved, &fmt);
    if let Err(e) = &preview {
        log_error(&format!(
            "register: preview handler registration failed: hr={:#010x}",
            e.code().0
        ));
    }
    let property = register_property_handler(&classes, dll_path, &approved, &fmt);
    if let Err(e) = &property {
        log_error(&format!(
            "register: property handler registration failed: hr={:#010x}",
            e.code().0
        ));
    }

    notify_shell();
    if thumbs_failed || preview.is_err() || property.is_err() {
        return Err(Error::from(E_FAIL));
    }
    Ok(())
}

/// Bring the CURRENT user's per-user shell pieces in line with their settings: the folder
/// right-click entry ([`crate::foldermenu`]) and the suppression of Explorer's own type-icon
/// overlay ([`crate::typeoverlay`]). Both live in HKCU, which the elevated machine-wide
/// [`register`] cannot write for the user who ran the installer, so the installer runs
/// `SageThumbs2K.exe --sync-user-shell` as the original user after `regsvr32`. Written
/// here and not only from Settings because a normal install never opens Settings; the folder
/// verb records an absolute path to the companion EXE, so re-running this after a move is also
/// what repoints it.
///
/// Doing nothing is the default answer, not a no-op: `hide_type_overlay()` is false for
/// `CornerMark::SystemIcon`, and `typeoverlay::sync(false)` REMOVES any suppression previously
/// written rather than skipping. That is what makes switching back to Windows' icon work.
pub fn sync_user_shell() -> Result<()> {
    crate::foldermenu::sync(settings::folder_prebuild_verb());
    crate::typeoverlay::sync(settings::hide_type_overlay());
    notify_shell();
    Ok(())
}

/// Undo everything [`sync_user_shell`] wrote for the CURRENT user, regardless of the settings.
/// The uninstaller runs `SageThumbs2K.exe --remove-user-shell` as the original user before
/// `regsvr32 /u`; nothing else would ever clean these up, and a leftover verb would point at an
/// EXE that is no longer installed while a leftover empty `TypeOverlay` would keep suppressing
/// another program's icon.
pub fn remove_user_shell() {
    crate::typeoverlay::remove_all();
    crate::foldermenu::remove_all();
    notify_shell();
}

/// Register the IPreviewHandler coclass: its COM server, the surrogate `AppID`
/// (so it runs in `prevhost.exe`, out of process), the global `PreviewHandlers`
/// list entry, and the per-extension `shellex` slot for each enabled format.
fn register_preview_handler(
    classes: &Key,
    dll_path: &str,
    approved: &Key,
    fmt: &FormatEnabledSnapshot,
) -> Result<()> {
    register_inproc_server(
        classes,
        CLSID_PREVIEW_HANDLER_STR,
        PV_NAME,
        dll_path,
        approved,
    )?;
    // "Both" (the shared helper defaults to Apartment): the preview host loads us into its
    // own STA but our render worker self-inits an MTA apartment (`previewhandler.rs`), so the
    // accurate declaration is Both — matching the property handler. (Apartment worked only
    // because prevhost.exe tolerated the mismatch.)
    classes
        .create(format!(
            "CLSID\\{CLSID_PREVIEW_HANDLER_STR}\\InprocServer32"
        ))?
        .set_string("ThreadingModel", "Both")?;
    // The AppID on our CLSID points the shell at the out-of-process preview host.
    classes
        .create(format!("CLSID\\{CLSID_PREVIEW_HANDLER_STR}"))?
        .set_string("AppID", PREVHOST_APPID)?;
    // The machine-wide registered-handlers list (value name = CLSID, data = name).
    LOCAL_MACHINE
        .create(PREVIEW_HANDLERS)?
        .set_string(CLSID_PREVIEW_HANDLER_STR, PV_NAME)?;
    // Hook each enabled extension's preview slot; unhook disabled ones (mirrors the
    // thumbnail per-extension loop, gated by the same Settings format list). A slot another
    // product owns is skipped, not counted, so this pass legitimately writes nothing on a
    // machine where every format already has a richer preview handler.
    let mut pass = Pass::default();
    for (ext, _) in FORMATS {
        if fmt.enabled(ext) {
            hook_ext_preview(classes, ext, &mut pass);
        } else {
            unhook_ext_preview(classes, ext);
        }
    }
    let _ = pass.report("preview shellex pass");
    Ok(())
}

/// Write the `CLSID\{clsid}` (friendly name) and `InprocServer32` (dll path, Apartment
/// threading) keys for one in-proc COM server under `classes`, without an Approved entry.
/// Shared by the machine-wide and the per-user registration paths, which configure alike.
fn write_inproc_server(classes: &Key, clsid: &str, name: &str, dll_path: &str) -> Result<()> {
    let base = format!("CLSID\\{clsid}");
    classes.create(&base)?.set_string("", name)?;
    let inproc = classes.create(format!("{base}\\InprocServer32"))?;
    inproc.set_string("", dll_path)?;
    inproc.set_string("ThreadingModel", "Apartment")
}

/// Register one in-proc COM server: `CLSID\{guid}` (friendly name) +
/// `InprocServer32` (dll path, Apartment threading) + the Approved entry.
/// All of our coclasses configure identically through here.
fn register_inproc_server(
    classes: &Key,
    clsid_str: &str,
    name: &str,
    dll_path: &str,
    approved: &Key,
) -> Result<()> {
    write_inproc_server(classes, clsid_str, name, dll_path)?;
    approved.set_string(clsid_str, name)?;
    Ok(())
}

/// The two `shellex` thumbnail-handler key paths for one extension: the
/// bare-extension key (lowest-priority lookup) and the association-independent
/// `SystemFileAssociations` key (consulted first, without clobbering any app's
/// ProgID-level handler). One source of truth for the key layout.
fn thumb_keys(ext: &str) -> [String; 2] {
    [
        format!(".{ext}\\shellex\\{THUMB_HANDLER_CATEGORY}"),
        format!("SystemFileAssociations\\.{ext}\\shellex\\{THUMB_HANDLER_CATEGORY}"),
    ]
}

/// Point one extension's thumbnail `shellex` keys at our CLSID, first recording any foreign
/// handler we are displacing so [`remove_if_ours`] can put it back.
///
/// Each key is attempted independently (see [`set_shellex_key`]): the module doc above says
/// `SystemFileAssociations` is checked BEFORE the bare-extension key, so a failure on the
/// lower-priority bare key must never skip the higher-priority one. Each outcome is counted
/// in `pass`.
fn hook_ext(classes: &Key, ext: &str, pass: &mut Pass) {
    for path in thumb_keys(ext) {
        remember_displaced(classes, &path);
        pass.note(
            &path,
            set_shellex_key(classes, &path, CLSID_THUMBNAIL_PROVIDER_STR),
        );
    }
}

/// Write `clsid` as `path`'s default value under `root`, one key at a time, handing the
/// outcome back rather than swallowing it: a failure on one key (e.g. the bare-extension key)
/// must never stop a caller from still attempting its sibling key (e.g.
/// `SystemFileAssociations`), but it must be counted and its HRESULT logged. Shared by
/// [`hook_ext`], [`hook_ext_preview`], and [`register_user`]'s per-user loop.
fn set_shellex_key(root: &Key, path: &str, clsid: &str) -> Result<()> {
    root.create(path)?.set_string("", clsid)
}

/// Remove one extension's thumbnail `shellex` keys — but only the ones that
/// actually point at OUR CLSID, so we never clobber a handler another product
/// (or Windows) registered in that slot.
fn unhook_ext(classes: &Key, ext: &str) {
    for path in thumb_keys(ext) {
        remove_if_ours(classes, &path);
    }
}

/// Like [`unhook_ext`], but after removing our handler leaf it also sweeps the
/// now-orphaned parent chain (`…\shellex`, then `.<ext>` /
/// `SystemFileAssociations\.<ext>`). This is the FULL UNINSTALL behavior and
/// must only run on the unregister path — a normal settings-apply re-register
/// disables individual formats with [`unhook_ext`] and must NOT prune parents
/// (the user may re-enable, and a foreign sibling may share the chain).
fn unhook_ext_and_prune(classes: &Key, ext: &str) {
    for path in thumb_keys(ext) {
        remove_if_ours(classes, &path);
        prune_empty_parents(classes, &path);
    }
}

/// True if the key at `path` exists and has zero subkeys AND zero values — i.e.
/// it's a genuinely empty husk safe to delete. A missing key, or any I/O error
/// while probing, returns `false` (conservative: never delete what we can't
/// confirm is empty).
fn is_empty_key(classes: &Key, path: &str) -> bool {
    let Ok(key) = classes.open(path) else {
        return false;
    };
    let no_subkeys = key
        .keys()
        .map(|mut it| it.next().is_none())
        .unwrap_or(false);
    let no_values = key
        .values()
        .map(|mut it| it.next().is_none())
        .unwrap_or(false);
    no_subkeys && no_values
}

/// After our handler leaf at `path` is removed, walk BACK UP the chain deleting
/// each parent that is now genuinely empty: the `…\shellex` container, then the
/// `.<ext>` (or `SystemFileAssociations\.<ext>`) key. Stops at the first
/// non-empty (or missing) parent, so a populated foreign key — or the shared
/// `SystemFileAssociations` root itself — is never touched. `path` is one of
/// the `thumb_keys` entries: `<assoc>\shellex\{THUMB_HANDLER_CATEGORY}`, whose two
/// ancestors we care about are `<assoc>\shellex` and `<assoc>`.
fn prune_empty_parents(classes: &Key, path: &str) {
    // Drop the `\{THUMB_HANDLER_CATEGORY}` leaf component -> `<assoc>\shellex`.
    let Some(shellex) = path.rsplit_once('\\').map(|(parent, _)| parent) else {
        return;
    };
    if !is_empty_key(classes, shellex) {
        return;
    }
    let _ = classes.remove_tree(shellex);

    // Drop the `\shellex` component -> `<assoc>` (`.ext` or
    // `SystemFileAssociations\.ext`). Only prune if it too is now empty.
    let Some(assoc) = shellex.rsplit_once('\\').map(|(parent, _)| parent) else {
        return;
    };
    if is_empty_key(classes, assoc) {
        let _ = classes.remove_tree(assoc);
    }
}

/// Delete a thumbnail-handler `shellex` key only if its default value is our
/// CLSID, then hand the slot back to whoever we took it from. A foreign handler
/// in that slot is left untouched.
fn remove_if_ours(classes: &Key, path: &str) {
    if let Ok(key) = classes.open(path) {
        if key.get_string("").ok().as_deref() == Some(CLSID_THUMBNAIL_PROVIDER_STR) {
            let _ = classes.remove_tree(path);
            restore_displaced(classes, path);
        }
    }
}

/// Undo [`register`] machine-wide. The per-user pieces of THIS (elevated) account are also
/// swept (marker-gated, so only what we wrote); the installing user's own are removed by the
/// uninstaller running [`remove_user_shell`] as that user first.
pub fn unregister() -> Result<()> {
    let classes = machine_classes()?;
    crate::typeoverlay::remove_all();
    crate::foldermenu::remove_all();
    // Order matters: remove the property-store VALUES on `SystemFileAssociations\.<ext>` FIRST,
    // so the subsequent thumbnail/preview `*_and_prune` calls find that key empty and prune it —
    // otherwise the lingering InfoTip/FullDetails/… values keep the key alive as orphan litter.
    for (ext, _) in FORMATS {
        unhook_ext_propstore(&classes, ext);
        unhook_ext_and_prune(&classes, ext);
        unhook_ext_preview_and_prune(&classes, ext);
    }
    // Also sweep historically-dropped extensions (orphans from older builds — see register()).
    for ext in REMOVED_EXTENSIONS {
        unhook_ext_propstore(&classes, ext);
        unhook_ext_and_prune(&classes, ext);
        unhook_ext_preview_and_prune(&classes, ext);
    }
    let _ = classes.remove_tree(format!("CLSID\\{CLSID_THUMBNAIL_PROVIDER_STR}"));
    let _ = classes.remove_tree(CONTEXT_MENU_KEY);
    let _ = classes.remove_tree(format!("CLSID\\{CLSID_CONTEXT_MENU_STR}"));
    let _ = classes.remove_tree(format!("CLSID\\{CLSID_PREVIEW_HANDLER_STR}"));
    // Takes `DisableProcessIsolation` and the "Both" threading model with it.
    let _ = classes.remove_tree(format!("CLSID\\{CLSID_PROPERTY_STORE_STR}"));
    // `create`, not `open`: `open` is read-only in this crate and `remove_value` on a
    // read-only handle silently does nothing, so these two lists kept our CLSIDs after an
    // uninstall. Both keys are in-box Windows keys that always exist, so `create` only ever
    // opens them. (Same trap as [`restore_displaced`] — see the note there.)
    if let Ok(list) = LOCAL_MACHINE.create(PREVIEW_HANDLERS) {
        let _ = list.remove_value(CLSID_PREVIEW_HANDLER_STR);
    }
    if let Ok(approved) = LOCAL_MACHINE.create(APPROVED) {
        let _ = approved.remove_value(CLSID_THUMBNAIL_PROVIDER_STR);
        let _ = approved.remove_value(CLSID_CONTEXT_MENU_STR);
        let _ = approved.remove_value(CLSID_PREVIEW_HANDLER_STR);
        let _ = approved.remove_value(CLSID_PROPERTY_STORE_STR);
    }
    // The loops above restored (and cleared) every slot we still owned. Anything left in the
    // list is a slot some OTHER product has since taken from us — putting those back would
    // clobber the current owner, so the record dies with the uninstall rather than being
    // replayed. Dropping the whole tree also keeps uninstall from leaving our key behind.
    let _ = LOCAL_MACHINE.remove_tree(DISPLACED);
    notify_shell();
    Ok(())
}

// ── preview-handler per-extension hooking (mirrors the thumbnail helpers) ──────

/// The two `shellex` preview-handler key paths for one extension.
fn preview_keys(ext: &str) -> [String; 2] {
    [
        format!(".{ext}\\shellex\\{PREVIEW_HANDLER_CATEGORY}"),
        format!("SystemFileAssociations\\.{ext}\\shellex\\{PREVIEW_HANDLER_CATEGORY}"),
    ]
}

/// Point one extension's preview `shellex` keys at our preview CLSID — but ONLY where the slot
/// is empty or already ours. Never displace another product's preview handler (mirrors
/// [`hook_ext_propstore`]'s guard): a foreign CLSID in the slot means a real handler owns the
/// format, and clobbering it would replace a richer preview with our static frame.
fn hook_ext_preview(classes: &Key, ext: &str, pass: &mut Pass) {
    for path in preview_keys(ext) {
        let existing = classes.open(&path).ok().and_then(|k| k.get_string("").ok());
        if !matches!(
            existing.as_deref(),
            None | Some("") | Some(CLSID_PREVIEW_HANDLER_STR)
        ) {
            continue; // a real handler already owns this slot — leave it alone
        }
        // Best-effort per key, same reasoning as `hook_ext`: a failure on one key must not
        // skip its sibling (the loop used to hard-`?` here and abort on the first failure).
        pass.note(
            &path,
            set_shellex_key(classes, &path, CLSID_PREVIEW_HANDLER_STR),
        );
    }
}

/// Remove one extension's preview `shellex` keys, but only where they point at OUR
/// preview CLSID (never clobber another product's handler).
fn unhook_ext_preview(classes: &Key, ext: &str) {
    for path in preview_keys(ext) {
        remove_if_ours_preview(classes, &path);
    }
}

/// Full-uninstall variant: remove our preview leaf and sweep now-empty parents
/// (reuses the thumbnail path's [`prune_empty_parents`]).
fn unhook_ext_preview_and_prune(classes: &Key, ext: &str) {
    for path in preview_keys(ext) {
        remove_if_ours_preview(classes, &path);
        prune_empty_parents(classes, &path);
    }
}

/// Delete a preview `shellex` key only if its default value is our preview CLSID.
fn remove_if_ours_preview(classes: &Key, path: &str) {
    if let Ok(key) = classes.open(path) {
        if key.get_string("").ok().as_deref() == Some(CLSID_PREVIEW_HANDLER_STR) {
            let _ = classes.remove_tree(path);
        }
    }
}

fn notify_shell() {
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None) };
}

/// Is the thumbnail provider registered machine-wide *right now*?
///
/// Reading back the CLSID from `HKLM\SOFTWARE\Classes` (not the merged HKCR view, which a
/// per-user registration would satisfy just as well) is the cheapest true test that
/// `DllRegisterServer` actually ran: our own registration writes this key and nothing else
/// does. Used by the Settings "Repair file associations" button to check whether the elevated
/// `regsvr32` it just launched really succeeded — launching a process tells you nothing about
/// the outcome, and reporting "repaired" after a silent failure is worse than reporting nothing.
pub fn is_registered() -> bool {
    LOCAL_MACHINE
        .open(format!(
            "{MACHINE_CLASSES}\\CLSID\\{CLSID_THUMBNAIL_PROVIDER_STR}\\InprocServer32"
        ))
        .ok()
        .and_then(|k| k.get_string("").ok())
        .is_some_and(|p| !p.is_empty())
}

// ── per-user registration (the portable build) ────────────────────────────────
//
// Everything above writes HKCR/HKLM and therefore needs elevation. This section is the
// same idea rooted at `HKCU\Software\Classes`, which the shell merges into HKCR ahead of
// the machine-wide view, so a user who cannot install anything still gets thumbnails from
// a DLL sitting in a folder they unzipped.
//
// PROVEN, not assumed (2026-08-06): with only these keys written, a DLL outside Program
// Files, and no elevation, `IShellItemImageFactory::GetImage` with SIIGBF_THUMBNAILONLY
// returned a real 256x192 32bpp bitmap for a hooked extension, and failed with
// 0x8004B200 for an unhooked control extension.
//
// WHAT IS DELIBERATELY ABSENT, because HKCU cannot express it:
//   * the Approved list (HKLM). Only enforced under the EnforceShellExtensionSecurity
//     policy, which is off by default; on a machine that enforces it, per-user handlers
//     are meant to be refused, and quietly failing there is the correct behaviour.
//   * the preview handler: `PreviewHandlers` is an HKLM list, so the Explorer PREVIEW PANE
//     stays installer-only.
//   * the property handler: `PropertySystem\PropertyHandlers` is likewise HKLM, so the
//     Details pane stays installer-only.
//   * the modern Win11 flyout, which needs the signed package in a machine store.
// Thumbnails and the classic right-click menu are what a zip can actually deliver.

#[cfg(test)]
mod displaced_tests;
#[cfg(test)]
mod registry_write_tests;
