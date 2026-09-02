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
/// Hover info-tip layout. ONE combined list serves every category: the shell only shows
/// properties the store actually returns a value for, so an image surfaces Dimensions/Camera,
/// audio surfaces Artist/Title, video its duration — all from the same list. (InfoTip omits
/// empty properties automatically, so no `*` prefix is needed here.)
const PROP_INFOTIP: &str =
    "prop:System.ItemTypeText;System.Image.Dimensions;System.Photo.CameraModel;System.Media.Duration;System.Music.Artist;System.Title;System.Size";
/// The Properties▸Details *tab* layout. Comprehensive — every property the store can emit:
/// Dimensions/BitDepth/DPI/DateTaken/GPS for images, Artist/Genre/Year/Duration/Bitrate for
/// audio, frame size for video. Includes `System.DateCreated` (the pane list already had it —
/// the two were inconsistent before).
const PROP_FULLDETAILS: &str = "prop:System.Image.Dimensions;System.Image.HorizontalSize;System.Image.VerticalSize;System.Image.BitDepth;System.Image.HorizontalResolution;System.Image.VerticalResolution;System.Photo.CameraManufacturer;System.Photo.CameraModel;System.Photo.DateTaken;System.GPS.LatitudeDecimal;System.GPS.LongitudeDecimal;System.Video.FrameWidth;System.Video.FrameHeight;System.Media.Duration;System.Audio.EncodingBitrate;System.Music.Artist;System.Music.AlbumTitle;System.Title;System.Music.TrackNumber;System.Music.Genre;System.Media.Year;System.Size;System.DateCreated;System.DateModified";
/// The BOTTOM details pane layout (`System.PropList.PreviewDetails`). DISTINCT from `FullDetails`
/// (the Properties▸Details *tab*) and `InfoTip` (the hover tooltip): the pane Explorer shows
/// under a selected file reads THIS list, and a format with no PreviewDetails (psd/raw/epub/…)
/// falls back to the bare date/size default — so our handler's dimensions never surfaced there
/// even though `GetValue` returned them. Metadata fields are `*`-prefixed (shown only when the
/// store returns a value), so a PSD shows Dimensions/DateTaken while an audio file shows
/// Artist/Duration/Genre from the same combined list; Size + dates are unprefixed (always present).
const PROP_PREVIEWDETAILS: &str = "prop:*System.Image.Dimensions;*System.Image.BitDepth;*System.Image.HorizontalResolution;*System.Image.VerticalResolution;*System.Photo.CameraManufacturer;*System.Photo.CameraModel;*System.Photo.DateTaken;*System.GPS.LatitudeDecimal;*System.GPS.LongitudeDecimal;*System.Video.FrameWidth;*System.Video.FrameHeight;*System.Media.Duration;*System.Audio.EncodingBitrate;*System.Music.Artist;*System.Music.AlbumTitle;*System.Title;*System.Music.TrackNumber;*System.Music.Genre;*System.Media.Year;System.Size;System.DateCreated;System.DateModified";
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
/// Where we remember a thumbnail handler that occupied a `shellex` slot BEFORE we took it,
/// keyed by the exact classes-relative key path we overwrote. Unlike the preview/property
/// handlers (which step aside for an incumbent — Windows' built-ins are richer there), the
/// thumbnail provider IS the product and does take the slot. But taking it must be REVERSIBLE:
/// without this record, `unhook`/uninstall deleted our value and left the slot empty forever, so
/// a user who had Icaros/Adobe/a codec pack thumbnailing a format never got it back —
/// uninstalling SageThumbs did not undo the damage. Machine-wide, mirroring the HKLM registration.
const DISPLACED: &str = r"SOFTWARE\SageThumbs2K\DisplacedThumbHandlers";
/// Image formats whose `PerceivedType=image` is safe to stamp: WIC (and so Photos) opens them,
/// so the verbs Windows attaches to that type (Rotate, Print, Set as background, Edit with
/// Photos) work. The rest of the Image category (PSD, KRA, XCF, …) would get the same verbs
/// and have them fail on a file WIC cannot encode, so those get no `PerceivedType` from us.
/// Camera RAW keeps `image`: the in-box RAW codec opens it.
const WIC_IMAGE_EXTS: &[&str] = &[
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

/// Register the IPropertyStore coclass: its COM server (threaded "Both" — it also loads in the
/// MTA SearchIndexer), the per-extension `PropertyHandlers\.<ext>` binding, and a combined
/// info-tip / full-details property list so the values actually surface in Explorer.
fn register_property_handler(
    classes: &Key,
    dll_path: &str,
    approved: &Key,
    fmt: &FormatEnabledSnapshot,
) -> Result<()> {
    register_inproc_server(
        classes,
        CLSID_PROPERTY_STORE_STR,
        PS_NAME,
        dll_path,
        approved,
    )?;
    let clsid_key = classes.create(format!("CLSID\\{CLSID_PROPERTY_STORE_STR}"))?;
    // Property handlers prefer "Both" (the shared helper defaults to Apartment).
    clsid_key
        .create("InprocServer32")?
        .set_string("ThreadingModel", "Both")?;
    // The handler initialises with `IInitializeWithFile` (its extractors need the real path).
    // Windows loads property handlers in its isolated property host by default, and a
    // file-initialised handler is not loaded there unless it declares this value, so without
    // it the indexer never asked us for anything. Removed with the CLSID tree in `unregister`.
    clsid_key.set_u32("DisableProcessIsolation", 1)?;
    for (ext, _) in FORMATS {
        if fmt.enabled(ext) {
            let _ = hook_ext_propstore(classes, ext);
        } else {
            unhook_ext_propstore(classes, ext);
        }
    }
    Ok(())
}

/// `(HKLM PropertyHandlers\.<ext>, classes-relative SystemFileAssociations\.<ext>)` for one
/// extension.
fn propstore_keys(ext: &str) -> (String, String) {
    (
        format!("{PROPERTY_HANDLERS}\\.{ext}"),
        format!("SystemFileAssociations\\.{ext}"),
    )
}

/// Bind one extension to our property handler + write its property lists — but ONLY where the
/// slot is empty or already ours. We must NEVER replace Windows' (or another product's) richer
/// property handler: jpg/png/heic/mp3/mp4/mkv/flac/… all have a built-in handler that knows far
/// more than we do, so they keep it. Our value is the formats with NO property handler at all
/// (PSD/RAW/EPUB/comics/CAD/Krita/SVG/…), where dimensions in the Details pane is a pure win.
fn hook_ext_propstore(classes: &Key, ext: &str) -> Result<()> {
    let (handler, assoc) = propstore_keys(ext);
    let existing = LOCAL_MACHINE
        .open(&handler)
        .ok()
        .and_then(|k| k.get_string("").ok());
    if !matches!(
        existing.as_deref(),
        None | Some("") | Some(CLSID_PROPERTY_STORE_STR)
    ) {
        return Ok(()); // a real handler already owns this extension — leave it alone
    }
    LOCAL_MACHINE
        .create(&handler)?
        .set_string("", CLSID_PROPERTY_STORE_STR)?;
    let a = classes.create(&assoc)?;
    // A third-party app can write these SystemFileAssociations values directly, without ever
    // registering a property handler — so the `handler` guard above (which only looked at
    // PropertyHandlers\.<ext>) can't see it. Fill each value only where it's genuinely empty,
    // so such a value is never clobbered.
    set_assoc_value_if_empty(&a, "InfoTip", PROP_INFOTIP);
    set_assoc_value_if_empty(&a, "FullDetails", PROP_FULLDETAILS);
    set_assoc_value_if_empty(&a, "PreviewDetails", PROP_PREVIEWDETAILS);
    set_assoc_value_if_empty(&a, "AdditionalProperties", PROP_ADDITIONAL);
    set_perceived_type(classes, ext);
    Ok(())
}

/// Write `name` on `key` only when it's currently absent/empty — mirrors [`set_perceived_type`]'s
/// "fill an empty slot, never overwrite" rule for the property-list values written above.
fn set_assoc_value_if_empty(key: &Key, name: &str, value: &str) {
    let already = key.get_string(name).ok();
    if matches!(already.as_deref(), Some(s) if !s.is_empty()) {
        return; // a value is already present (Windows or another app) — leave it
    }
    let _ = key.set_string(name, value);
}

/// The `PerceivedType` we stamp for `ext`, or `None` for a format we leave unclassified.
/// Image formats WIC cannot open get `None` (see [`WIC_IMAGE_EXTS`]).
fn perceived_type_for(ext: &str) -> Option<&'static str> {
    Some(match crate::formats::category(ext) {
        Category::Audio => "audio",
        Category::Video => "video",
        Category::Ebook | Category::Document => "document",
        Category::Raw => "image",
        Category::Image if WIC_IMAGE_EXTS.contains(&ext) => "image",
        Category::Image => return None,
        // In practice Windows itself already stamps .zip/.rar/.7z as "compressed",
        // so the already-present guard in `set_perceived_type` usually skips these anyway.
        Category::Archive => "compressed",
    })
}

/// Set `.<ext>`'s `PerceivedType` so `kind:` search + library grouping can classify the
/// formats Windows otherwise doesn't know (kra/ora/blend/epub/djvu/svg/xcf/…). Written ONLY when
/// absent — we never overwrite a value Windows or another app already set — and marked with
/// [`PERCEIVED_TYPE_MARK`] so [`unhook_perceived_type`] removes exactly the values we wrote
/// (on every disable and uninstall) and nothing another app set later.
fn set_perceived_type(classes: &Key, ext: &str) {
    let key = format!(".{ext}");
    let already = classes
        .open(&key)
        .ok()
        .and_then(|k| k.get_string("PerceivedType").ok());
    if matches!(already.as_deref(), Some(s) if !s.is_empty()) {
        return; // a value is already present (Windows or another app) — leave it
    }
    let Some(pt) = perceived_type_for(ext) else {
        return;
    };
    if let Ok(k) = classes.create(&key) {
        if k.set_string("PerceivedType", pt).is_ok() {
            // Marker so unhook can remove OUR PerceivedType without clobbering one another app
            // sets later (we only ever fill an empty slot, but can't otherwise prove ownership).
            let _ = k.set_string(PERCEIVED_TYPE_MARK, "1");
        }
    }
}

/// Remove the `PerceivedType` we set — but ONLY where our [`PERCEIVED_TYPE_MARK`] marker proves it
/// was ours, so a value Windows or another app owns is never clobbered.
fn unhook_perceived_type(classes: &Key, ext: &str) {
    let key = format!(".{ext}");
    // `create`, not `open`: `open` hands back a read-only handle in this crate and
    // `remove_value` on it silently no-ops (see the note on `restore_displaced`), which left
    // PerceivedType/the marker behind on every uninstall/disable.
    if let Ok(k) = classes.create(&key) {
        if k.get_string(PERCEIVED_TYPE_MARK).is_ok() {
            let _ = k.remove_value("PerceivedType");
            let _ = k.remove_value(PERCEIVED_TYPE_MARK);
        }
    }
}

/// Remove our property-handler binding + the prop lists, but ONLY where they're still ours
/// (never clobber a handler / info-tip another product set).
fn unhook_ext_propstore(classes: &Key, ext: &str) {
    let (handler, assoc) = propstore_keys(ext);
    let was_ours = LOCAL_MACHINE
        .open(&handler)
        .ok()
        .and_then(|k| k.get_string("").ok())
        .as_deref()
        == Some(CLSID_PROPERTY_STORE_STR);
    if was_ours {
        let _ = LOCAL_MACHINE.remove_tree(&handler);
        // Remove OUR property lists UNCONDITIONALLY (not by matching the CURRENT const strings):
        // an older install wrote DIFFERENT strings, so an equality check would orphan them across
        // an upgrade-then-uninstall. We are the only writer of these value names for a format we
        // own. Gated on `was_ours` so we never touch lists under a foreign handler.
        // `create`, not `open`: same read-only-handle trap as `unhook_perceived_type` above —
        // `open`'s handle makes `remove_value` a silent no-op, so these four values survived
        // every uninstall/disable.
        if let Ok(k) = classes.create(&assoc) {
            for v in [
                "InfoTip",
                "FullDetails",
                "PreviewDetails",
                "AdditionalProperties",
            ] {
                let _ = k.remove_value(v);
            }
        }
    }
    unhook_perceived_type(classes, ext);
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
    let base = format!("CLSID\\{clsid_str}");
    classes.create(&base)?.set_string("", name)?;
    let inproc = classes.create(format!("{base}\\InprocServer32"))?;
    inproc.set_string("", dll_path)?;
    inproc.set_string("ThreadingModel", "Apartment")?;
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

/// Note the handler currently in `path` under [`DISPLACED`] so unhooking can restore it.
///
/// No-ops when the slot is empty or already ours — which is what makes a re-register
/// idempotent: the SECOND register sees our own CLSID and leaves the original record intact
/// rather than overwriting it with ourselves (which would silently discard the thing we are
/// meant to give back). If a third product takes the slot from us and we re-register later,
/// recording that one is correct: restore returns the slot to whoever held it last.
fn remember_displaced(classes: &Key, path: &str) {
    remember_displaced_in(classes, LOCAL_MACHINE, path);
}

/// [`remember_displaced`] against an explicit pair of hives, so the machine-wide path and the
/// PORTABLE per-user path share one implementation. The per-user path must record into HKCU:
/// a zip has no HKLM write access, and it evicts incumbents from `HKCU\Software\Classes` just
/// as destructively as the installer does from HKLM. (`SOFTWARE\...` resolves under either
/// hive — the registry is case-insensitive, and HKCU keeps the record beside the settings.)
fn remember_displaced_in(classes: &Key, records: &Key, path: &str) {
    let Some(existing) = classes.open(path).ok().and_then(|k| k.get_string("").ok()) else {
        return; // no key, or no default value — nothing was there to displace
    };
    if existing.is_empty() || existing.eq_ignore_ascii_case(CLSID_THUMBNAIL_PROVIDER_STR) {
        return;
    }
    if let Ok(k) = records.create(DISPLACED) {
        let _ = k.set_string(path, &existing);
    }
}

/// Put back the handler we displaced when we took `path`, then forget the record.
///
/// Only ever called once OUR value has already been removed, so the slot is empty and this
/// cannot clobber a live third-party registration. Restoring also leaves the key non-empty,
/// which is what stops [`prune_empty_parents`] from deleting the chain out from under it.
/// TRAP, verified live rather than assumed: `Key::open` hands back a READ-ONLY handle, so
/// `remove_value` on it silently no-ops. Reading the record through `open` is fine, but
/// clearing it needs the writable handle `create` returns (`create` opens an existing key).
/// With the read-only handle the slot WAS restored and the record survived anyway, so
/// `st2k doctor` kept reporting a handler we no longer displaced.
fn restore_displaced(classes: &Key, path: &str) {
    restore_displaced_in(classes, LOCAL_MACHINE, path);
}

/// [`restore_displaced`] against an explicit pair of hives — the twin of
/// [`remember_displaced_in`], shared by the machine-wide and portable per-user paths.
fn restore_displaced_in(classes: &Key, records: &Key, path: &str) {
    let Ok(prev) = records.open(DISPLACED).and_then(|k| k.get_string(path)) else {
        return; // no list, or nothing recorded for this slot
    };
    // The record is the ONLY copy of the displaced product's CLSID. Dropping it when the
    // write-back failed would leave the slot empty AND destroy the means to ever put it right,
    // which is the exact harm this whole mechanism exists to prevent. So the delete is
    // conditional on the restore actually landing; a failed one keeps the record, and the next
    // uninstall, repair or `doctor` run can still recover from it.
    let restored = if prev.is_empty() {
        true // nothing was in the slot to begin with, so the record has served its purpose
    } else {
        classes
            .create(path)
            .and_then(|k| k.set_string("", &prev))
            .is_ok()
    };
    if restored {
        if let Ok(writable) = records.create(DISPLACED) {
            let _ = writable.remove_value(path);
        }
    }
}

/// The `.<ext>` component of a recorded [`DISPLACED`] key path, for callers that want to
/// report by format rather than by registry path. Lives here, beside [`thumb_keys`] which
/// produces those paths, so the two layouts cannot drift apart — `displaced_key_ext_matches`
/// pins them together for every registered format.
pub(crate) fn displaced_key_ext(path: &str) -> Option<&str> {
    path.split('\\').find(|c| c.starts_with('.'))
}

/// Every extension whose thumbnail slot we took from someone else, as
/// `(key path, displaced CLSID)`. Read-only; `st2k doctor` reports these so a user whose
/// thumbnails changed after install can see exactly what we replaced.
pub(crate) fn displaced_handlers() -> Vec<(String, String)> {
    // Both hives: an installed copy records under HKLM, a portable one under HKCU, and the
    // doctor has no business caring which kind of install the person running it has.
    let mut out = Vec::new();
    for root in [&LOCAL_MACHINE, &CURRENT_USER] {
        let Ok(list) = root.open(DISPLACED) else {
            continue;
        };
        let Ok(values) = list.values() else {
            continue;
        };
        // Re-read each name with `get_string` rather than matching on the iterator's value
        // enum — one less API shape to stay pinned to, and a non-string leftover is skipped
        // either way.
        for (name, _) in values {
            if let Ok(clsid) = list.get_string(&name) {
                if !clsid.is_empty() {
                    out.push((name, clsid));
                }
            }
        }
    }
    out
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

/// `HKCU\Software\Classes` — the per-user half of `HKEY_CLASSES_ROOT`.
fn user_classes() -> Result<Key> {
    CURRENT_USER.create(r"Software\Classes")
}

/// Register the thumbnail provider + classic context menu for THIS USER ONLY, pointing at
/// `dll_path`. No elevation, and it never touches the machine-wide hive, so it cannot
/// disturb an installed copy.
pub fn register_user(dll_path: &str) -> Result<()> {
    let classes = user_classes()?;
    for (clsid, name) in [
        (CLSID_THUMBNAIL_PROVIDER_STR, NAME),
        (CLSID_CONTEXT_MENU_STR, CM_NAME),
    ] {
        let base = format!("CLSID\\{clsid}");
        classes.create(&base)?.set_string("", name)?;
        let inproc = classes.create(format!("{base}\\InprocServer32"))?;
        inproc.set_string("", dll_path)?;
        inproc.set_string("ThreadingModel", "Apartment")?;
    }

    // Same per-extension layout as the machine-wide path, so precedence behaves identically.
    // Best-effort per extension: one locked-down key must not abort the rest, but every
    // outcome is counted and a pass that wrote nothing fails the call (see `register`).
    let fmt = settings::format_enabled_snapshot();
    let mut thumbs = Pass::default();
    for (ext, _) in FORMATS {
        if fmt.enabled(ext) {
            for path in thumb_keys(ext) {
                // Same non-destructive claim as the machine-wide path: note whoever held this
                // slot in the user's own hive so `remove_user_if_ours` can hand it straight
                // back. Portable mode is still a real install from the shell's point of view.
                remember_displaced_in(&classes, CURRENT_USER, &path);
                thumbs.note(
                    &path,
                    set_shellex_key(&classes, &path, CLSID_THUMBNAIL_PROVIDER_STR),
                );
            }
        } else {
            remove_user_if_ours(&classes, ext);
        }
    }
    let thumbs_failed = thumbs.report("per-user thumbnail shellex pass");
    // Sweep stale hooks from extensions older builds registered but we've since dropped —
    // mirrors register()/unregister()/unregister_user(), all three of which already do this.
    // Without it, a portable copy upgraded past a dropped extension keeps a stale HKCU
    // shellex entry for it forever (the FORMATS loop above never touches it again).
    for ext in REMOVED_EXTENSIONS {
        remove_user_if_ours(&classes, ext);
    }

    if let Err(e) = set_shellex_key(&classes, CONTEXT_MENU_KEY, CLSID_CONTEXT_MENU_STR) {
        log_error(&format!(
            "register_user: context menu key {CONTEXT_MENU_KEY}: hr={:#010x}",
            e.code().0
        ));
    }

    notify_shell();
    if thumbs_failed {
        return Err(Error::from(E_FAIL));
    }
    Ok(())
}

/// Undo [`register_user`]. Removes only keys whose value is OUR CLSID, so a handler another
/// product owns is never collateral damage, and leaves the machine-wide hive alone. The
/// per-user shell pieces go first, mirroring [`unregister`]: after the documented "run
/// `--off`, then delete the folder", a leftover folder verb would point at an EXE that is
/// gone and a leftover `TypeOverlay` would keep suppressing another program's icon.
pub fn unregister_user() -> Result<()> {
    remove_user_shell();
    unregister_user_classes()
}

/// The class-key half of [`unregister_user`]: the per-user CLSIDs and `shellex` hooks, and
/// nothing under the user's settings or shell pieces. Also what the machine-wide [`register`]
/// runs to clear a portable registration that would shadow it, where taking the folder verb
/// and overlay suppression away too would be wrong.
fn unregister_user_classes() -> Result<()> {
    let classes = user_classes()?;
    for (ext, _) in FORMATS {
        remove_user_if_ours(&classes, ext);
    }
    for ext in REMOVED_EXTENSIONS {
        remove_user_if_ours(&classes, ext);
    }
    if let Ok(k) = classes.open(CONTEXT_MENU_KEY) {
        if k.get_string("").ok().as_deref() == Some(CLSID_CONTEXT_MENU_STR) {
            let _ = classes.remove_tree(CONTEXT_MENU_KEY);
        }
    }
    let _ = classes.remove_tree(format!("CLSID\\{CLSID_THUMBNAIL_PROVIDER_STR}"));
    let _ = classes.remove_tree(format!("CLSID\\{CLSID_CONTEXT_MENU_STR}"));
    // The same final sweep the machine-wide `unregister` does, and for the same reason: a slot
    // a THIRD product has since taken over from us is no longer "ours", so `remove_user_if_ours`
    // skips it and its record is never restored or removed. Without this the portable path
    // leaves records behind that nothing would ever clean up again.
    let _ = CURRENT_USER.remove_tree(DISPLACED);
    notify_shell();
    Ok(())
}

/// Drop one extension's per-user thumbnail hooks, ours only, and prune the containers we
/// created on the way in. Without the prune, turning the feature off leaves an empty
/// `.<ext>\shellex` behind for every one of the 300+ formats — litter in the user's own hive
/// that looks like a half-removed handler to anyone who goes looking.
fn remove_user_if_ours(classes: &Key, ext: &str) {
    for path in thumb_keys(ext) {
        let ours = classes
            .open(&path)
            .ok()
            .and_then(|k| k.get_string("").ok())
            .as_deref()
            == Some(CLSID_THUMBNAIL_PROVIDER_STR);
        if !ours {
            continue;
        }
        let _ = classes.remove_tree(&path);
        // Hand the slot back to whoever we took it from. This also leaves the key non-empty,
        // which is what stops the prune below from deleting the chain out from under it.
        restore_displaced_in(classes, CURRENT_USER, &path);
        // Walk back up: `<assoc>\shellex`, then `<assoc>`. Stop at the first parent that
        // still holds something, so a foreign handler or a populated key is never collateral.
        let Some(shellex) = path.rsplit_once('\\').map(|(parent, _)| parent) else {
            continue;
        };
        if !user_key_is_empty(classes, shellex) {
            continue;
        }
        let _ = classes.remove_tree(shellex);
        if let Some(assoc) = shellex.rsplit_once('\\').map(|(parent, _)| parent) {
            if user_key_is_empty(classes, assoc) {
                let _ = classes.remove_tree(assoc);
            }
        }
    }
}

/// No subkeys and no values. Missing counts as NOT empty so a failed open never licenses a
/// delete (mirrors `is_empty_key`, which guards the machine-wide path the same way).
fn user_key_is_empty(classes: &Key, path: &str) -> bool {
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

/// The DLL path currently registered for THIS USER, if any.
///
/// Returns the path rather than a bool because the portable build has to answer a question a
/// bool cannot: whether the registration points at *this* copy. A user who unzips a second
/// copy, or moves the folder, leaves keys aimed at a DLL that is no longer there, and the
/// symptom is thumbnails silently not appearing.
pub fn user_registration_path() -> Option<String> {
    user_classes()
        .ok()?
        .open(format!(
            "CLSID\\{CLSID_THUMBNAIL_PROVIDER_STR}\\InprocServer32"
        ))
        .ok()?
        .get_string("")
        .ok()
        .filter(|p| !p.is_empty())
}

/// The shell-extension DLL a portable copy registers: the one sitting beside the running exe.
///
/// The caller still has to check it EXISTS. A partially-unpacked (or pruned) zip is exactly the
/// case worth naming in an error message rather than reporting as a generic failure.
pub fn dll_beside_exe() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join("sagethumbs2k.dll"))
}

/// Is the per-user registration pointing at THIS copy's DLL?
///
/// Compares the PATH, not mere presence: a registration left behind by a copy that has since
/// been moved or deleted reads as "on" while drawing no thumbnails at all, which is the single
/// most confusing state a portable user can land in.
pub fn user_registration_is_here() -> bool {
    let (Some(registered), Some(here)) = (user_registration_path(), dll_beside_exe()) else {
        return false;
    };
    // The keys can outlive the file they name — antivirus quarantine, a half-deleted unzip, a
    // manual cleanup that left the exes. The path still matches in that case, so a pure string
    // compare would report "on" for a handler Windows cannot load and no thumbnail will ever
    // come from. Requiring the DLL to actually BE there makes the answer mean what it says.
    if !here.is_file() {
        return false;
    }
    // Case-insensitive: the registry keeps whatever case was written and Windows paths are not
    // case-sensitive, so a pure case difference is the same file.
    registered.eq_ignore_ascii_case(&here.to_string_lossy())
}

#[cfg(test)]
mod displaced_tests {
    use super::*;

    /// The doctor reports displaced handlers BY FORMAT, which means parsing the extension back
    /// out of the key path `hook_ext` recorded. Both halves live in this file precisely so they
    /// can be pinned together: if `thumb_keys` ever changes shape, this fails instead of the
    /// report silently going blank (a `find` that matches nothing returns `None`, which the
    /// doctor skips — a failure mode with no symptom at all).
    #[test]
    fn displaced_key_ext_matches_thumb_keys() {
        for (ext, _) in crate::formats::FORMATS {
            for path in thumb_keys(ext) {
                assert_eq!(
                    displaced_key_ext(&path),
                    Some(format!(".{ext}").as_str()),
                    "could not recover .{ext} from {path}"
                );
            }
        }
    }

    /// The `SystemFileAssociations` twin must not collide with the bare-extension key: they are
    /// stored as two separate value names under `DISPLACED`, so a collision would mean one of
    /// the two displaced handlers is silently forgotten and never restored.
    #[test]
    fn thumb_keys_are_distinct_per_extension() {
        let mut seen = std::collections::BTreeSet::new();
        for (ext, _) in crate::formats::FORMATS {
            for path in thumb_keys(ext) {
                assert!(seen.insert(path.clone()), "duplicate displaced key {path}");
            }
        }
    }
}

#[cfg(test)]
mod registry_write_tests {
    use super::*;

    /// A scratch stand-in for the classes root these helpers write into, removed when the guard
    /// drops, so these tests can exercise real registry writes without touching the machine's
    /// own associations (mirrors the `Scratch` pattern in `typeoverlay.rs`).
    struct Scratch(String);

    impl Scratch {
        fn new(name: &str) -> (Self, Key) {
            let path = format!(r"Software\SageThumbs2K-test\register-{name}");
            let key = CURRENT_USER.create(&path).expect("scratch key");
            (Scratch(path), key)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = CURRENT_USER.remove_tree(&self.0);
        }
    }

    /// `hook_ext`/`hook_ext_preview` used to `?` inside their per-key loop (opus-SG-06 / A054):
    /// a failure on the FIRST key returned `Err` before the second (higher-priority, per the
    /// module doc) key was ever attempted. `set_shellex_key` is the shared best-effort
    /// replacement all three call sites now use; prove it keeps going past a key that can't be
    /// created — a 300-char key-name segment exceeds the registry's documented 255-character
    /// key-name limit, so `create` genuinely fails for it, without needing elevation or
    /// touching a real hive.
    #[test]
    fn set_shellex_key_keeps_writing_later_keys_after_an_earlier_one_fails_to_create() {
        let (_guard, root) = Scratch::new("short-circuit");
        let unwritable = "x".repeat(300);
        let good = "good-sibling";

        let outcomes: Vec<bool> = [unwritable.as_str(), good]
            .iter()
            .map(|name| set_shellex_key(&root, name, "TEST-CLSID").is_ok())
            .collect();
        assert_eq!(
            outcomes,
            [false, true],
            "the failure must be handed back (so the pass can count and log it), not swallowed"
        );

        assert!(
            root.open(&unwritable).is_err(),
            "sanity: the 300-char segment must genuinely have failed to create, or this test \
             proves nothing about the old short-circuit"
        );
        let k = root
            .open(good)
            .expect("the good key after an invalid earlier sibling must still be written");
        assert_eq!(k.get_string("").as_deref(), Ok("TEST-CLSID"));
    }

    /// The ordinary case: a writable path round-trips through `set_shellex_key`.
    #[test]
    fn set_shellex_key_round_trips_on_a_writable_path() {
        let (_guard, root) = Scratch::new("roundtrip");
        set_shellex_key(&root, "child\\grandchild", "TEST-CLSID").expect("write");
        let k = root.open("child\\grandchild").expect("key created");
        assert_eq!(k.get_string("").as_deref(), Ok("TEST-CLSID"));
    }

    /// A pass that attempted keys and wrote none is the "clean install, no thumbnails, no
    /// log line" failure `register` used to report as S_OK; a pass with a partial failure is
    /// logged but is not a failure of the pass as a whole.
    #[test]
    fn a_pass_that_wrote_nothing_is_reported_as_failed() {
        let (_guard, root) = Scratch::new("pass-tally");
        let unwritable = "y".repeat(300);

        let mut all_failed = Pass::default();
        all_failed.note(&unwritable, set_shellex_key(&root, &unwritable, "X"));
        assert!(all_failed.report("test pass: all failed"));
        assert_eq!((all_failed.written, all_failed.failed), (0, 1));
        assert!(
            all_failed.first_failure.is_some(),
            "the first HRESULT is kept for the log"
        );

        let mut partial = Pass::default();
        partial.note(&unwritable, set_shellex_key(&root, &unwritable, "X"));
        partial.note("ok", set_shellex_key(&root, "ok", "X"));
        assert!(!partial.report("test pass: partial"));
        assert_eq!((partial.written, partial.failed), (1, 1));

        let empty = Pass::default();
        assert!(!empty.report("test pass: nothing attempted"));
    }

    /// `PerceivedType=image` pulls Windows' image verbs (Rotate, Print, Set as background)
    /// onto a type; those fail on formats WIC cannot open, so the Image category is stamped
    /// only for the WIC-openable extensions. Camera RAW and the other categories keep their
    /// classification.
    #[test]
    fn perceived_type_is_image_only_where_wic_can_open_it() {
        assert_eq!(perceived_type_for("png"), Some("image"));
        assert_eq!(perceived_type_for("heic"), Some("image"));
        assert_eq!(
            perceived_type_for("cr2"),
            Some("image"),
            "camera RAW keeps image"
        );
        assert_eq!(perceived_type_for("psd"), None, "WIC cannot open a PSD");
        assert_eq!(perceived_type_for("xcf"), None);
        assert_eq!(perceived_type_for("flac"), Some("audio"));
        for ext in WIC_IMAGE_EXTS {
            assert!(
                matches!(
                    crate::formats::category(ext),
                    crate::formats::Category::Image | crate::formats::Category::Raw
                ) || !crate::formats::is_known(ext),
                ".{ext} is listed as WIC-openable but is not an image format"
            );
        }
    }

    /// A054's companion in the property-store path (A055): `set_assoc_value_if_empty` must fill
    /// a genuinely blank value while leaving one a third-party app already set alone, even
    /// though our caller's only ownership signal (the PropertyHandlers\.<ext> guard) can't see
    /// that value at all.
    #[test]
    fn set_assoc_value_if_empty_fills_blanks_but_never_clobbers_a_foreign_value() {
        let (_guard, classes) = Scratch::new("propstore-guard");
        let k = classes.create("Assoc").expect("create");
        k.set_string("InfoTip", "set by some other app")
            .expect("set");
        drop(k);

        let k = classes.create("Assoc").expect("writable handle");
        set_assoc_value_if_empty(&k, "InfoTip", "our default infotip");
        set_assoc_value_if_empty(&k, "FullDetails", "our default fulldetails");

        assert_eq!(
            k.get_string("InfoTip").as_deref(),
            Ok("set by some other app"),
            "a pre-existing foreign value must survive"
        );
        assert_eq!(
            k.get_string("FullDetails").as_deref(),
            Ok("our default fulldetails"),
            "a genuinely empty slot must still get filled"
        );
    }

    /// A056's defect class, reproduced against a safe scratch key instead of the real
    /// `CLASSES_ROOT` paths `unhook_perceived_type`/`unhook_ext_propstore` actually touch:
    /// `Key::open` in this crate hands back a read-only handle, so `remove_value` through it
    /// silently no-ops, while `Key::create` re-opens the SAME existing key with write access
    /// and `remove_value` through THAT handle actually removes it. This is exactly the swap
    /// those two functions needed.
    #[test]
    fn a_read_only_open_handle_cannot_remove_a_value_but_a_create_handle_can() {
        let (_guard, classes) = Scratch::new("open-vs-create");
        let k = classes.create("Marked").expect("create");
        k.set_string("Marker", "1").expect("set");
        drop(k);

        let ro = classes.open("Marked").expect("open (read-only)");
        let _ = ro.remove_value("Marker");
        drop(ro);
        assert_eq!(
            classes
                .open("Marked")
                .unwrap()
                .get_string("Marker")
                .as_deref(),
            Ok("1"),
            "a read-only handle's remove_value must not have taken effect"
        );

        let rw = classes.create("Marked").expect("create (writable)");
        rw.remove_value("Marker")
            .expect("remove_value via a writable handle must succeed");
        assert!(
            classes
                .open("Marked")
                .unwrap()
                .get_string("Marker")
                .is_err(),
            "the value must actually be gone now"
        );
    }

    /// A160: `register_user`'s per-extension loop only ever walks `FORMATS`, so a stale hook
    /// left by a dropped extension needed its own sweep, mirroring `register`/`unregister`/
    /// `unregister_user`. This proves the underlying removal call the new sweep relies on —
    /// `remove_user_if_ours` — actually clears a hook it owns and leaves a foreign one alone
    /// (register_user() itself is not called here: it walks the real, live `FORMATS` list
    /// against the real `HKCU\Software\Classes`, which would mutate this machine's actual
    /// thumbnail associations for 300+ extensions as a side effect of running the test suite).
    #[test]
    fn remove_user_if_ours_clears_our_stale_hook_but_leaves_a_foreign_one() {
        let (_guard, classes) = Scratch::new("removed-ext-sweep");
        // `remove_user_if_ours` calls `thumb_keys`, which is a fixed real-extension-shaped
        // path — reuse it verbatim against the scratch root instead of duplicating its shape.
        for path in thumb_keys("zzzstaletestext") {
            classes
                .create(&path)
                .and_then(|k| k.set_string("", CLSID_THUMBNAIL_PROVIDER_STR))
                .expect("seed our stale hook");
        }
        for path in thumb_keys("zzzforeigntestext") {
            classes
                .create(&path)
                .and_then(|k| k.set_string("", "{some-other-vendor-clsid}"))
                .expect("seed a foreign hook");
        }

        remove_user_if_ours(&classes, "zzzstaletestext");
        remove_user_if_ours(&classes, "zzzforeigntestext");

        for path in thumb_keys("zzzstaletestext") {
            assert!(
                classes.open(&path).is_err(),
                "our own stale hook at {path} must be gone"
            );
        }
        for path in thumb_keys("zzzforeigntestext") {
            let k = classes.open(&path).expect("foreign hook key must survive");
            assert_eq!(k.get_string("").as_deref(), Ok("{some-other-vendor-clsid}"));
        }
    }
}
