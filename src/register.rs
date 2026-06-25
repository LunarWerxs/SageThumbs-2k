//! Classic per-extension registration for the thumbnail provider + context menu.
//!
//! A plain in-proc COM server registered via `regsvr32`. Thumbnail providers do
//! NOT need package identity (only the modern `IExplorerCommand` main-flyout does,
//! and that ships as a signed sparse package — see `packaging/make-msix.ps1`), and the shell
//! runs us out-of-process in its isolated host automatically.
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

use windows::core::Result;
use windows::Win32::UI::Shell::{SHChangeNotify, SHCNE_ASSOCCHANGED, SHCNF_IDLIST};
use windows_registry::{CLASSES_ROOT, LOCAL_MACHINE};

use crate::guids::{
    CLSID_CONTEXT_MENU_STR, CLSID_PREVIEW_HANDLER_STR, CLSID_PROPERTY_STORE_STR,
    CLSID_THUMBNAIL_PROVIDER_STR,
};
use crate::settings;

const NAME: &str = "SageThumbs 2K Thumbnail Provider";
const CM_NAME: &str = "SageThumbs 2K Context Menu";
const PV_NAME: &str = "SageThumbs 2K Preview Handler";
const PS_NAME: &str = "SageThumbs 2K Property Handler";
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
/// The IThumbnailProvider shell-extension handler category GUID.
const THUMB_HANDLER: &str = "{E357FCCD-A995-4576-B01F-234630154E96}";
/// The IPreviewHandler category GUID — the `shellex` slot the preview host reads.
const PREVIEW_HANDLER: &str = "{8895b1c6-b41f-4c1c-a562-0d564250836f}";
/// The x64 preview-host surrogate AppID (`system32\prevhost.exe`) — verified
/// against the in-box TXT/RTF/Font preview handlers on this Win11 box. Setting it
/// on our CLSID makes the shell load us OUT of process, never inside explorer.exe.
const PREVHOST_APPID: &str = "{6d2b5079-2f0b-48dd-ab7f-97cec514d30b}";
/// The machine-wide list the preview pane consults for registered handlers.
const PREVIEW_HANDLERS: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\PreviewHandlers";
const APPROVED: &str =
    r"SOFTWARE\Microsoft\Windows\CurrentVersion\Shell Extensions\Approved";

/// (Re-)register the shell extension machine-wide under HKCR/HKLM. NOTE: the
/// per-extension on/off flags this reads via [`settings::format_enabled`] live
/// in the elevated user's HKCU, but the registration they gate is MACHINE-WIDE
/// (HKCR) and so applies to ALL users — there is no per-user thumbnail gate, by
/// design. (See the matching note on [`settings::format_enabled`].)
pub fn register(dll_path: &str) -> Result<()> {
    // "Approved Shell Extensions" is mandatory on locked-down systems.
    let approved = LOCAL_MACHINE.create(APPROVED)?;

    // The thumbnail provider's COM server.
    register_inproc_server(CLSID_THUMBNAIL_PROVIDER_STR, NAME, dll_path, &approved)?;

    // Hook each enabled extension; explicitly unhook disabled ones so a
    // re-register reflects the Options format list (matches the legacy
    // RegisterExtensions-on-OK behavior). Best-effort per extension: a single
    // failing key (transient lock, locked-down subtree) must NOT abort the whole
    // register and skip the context-menu setup + shell-notify below.
    for (ext, _) in crate::formats::FORMATS {
        if settings::format_enabled(ext) {
            let _ = hook_ext(ext);
        } else {
            unhook_ext(ext);
        }
    }

    // Sweep away stale hooks from extensions OLDER builds registered but we've since dropped
    // (they're no longer in FORMATS, so the loop above never touches their keys → an upgrade
    // would leave orphan shellex entries pointing at our CLSID). Disjoint from FORMATS (tested),
    // so this never unhooks a live format. Best-effort, one pass per (re-)register.
    for ext in crate::formats::REMOVED_EXTENSIONS {
        unhook_ext_and_prune(ext);
        unhook_ext_preview_and_prune(ext);
        unhook_ext_propstore(ext);
    }

    // The classic IContextMenu handler's COM server (for classic-menu machines:
    // StartAllBack, ExplorerPatcher, or the {86ca1aa0…} tweak). Registered under
    // "*" (all files) and filtered to images inside QueryContextMenu.
    register_inproc_server(CLSID_CONTEXT_MENU_STR, CM_NAME, dll_path, &approved)?;
    CLASSES_ROOT
        .create("*\\shellex\\ContextMenuHandlers\\SageThumbs2K")?
        .set_string("", CLSID_CONTEXT_MENU_STR)?;

    // The preview-pane handler. Best-effort: a failure here (e.g. a locked-down
    // PreviewHandlers list) must never break the thumbnail/context-menu setup above.
    let _ = register_preview_handler(dll_path, &approved);

    // The property handler (Details pane / info-tip / columns). Best-effort: a locked-down
    // PropertySystem subtree must never break the thumbnail/context-menu setup above.
    let _ = register_property_handler(dll_path, &approved);

    notify_shell();
    Ok(())
}

/// Register the IPropertyStore coclass: its COM server (threaded "Both" — it also loads in the
/// MTA SearchIndexer), the per-extension `PropertyHandlers\.<ext>` binding, and a combined
/// info-tip / full-details property list so the values actually surface in Explorer.
fn register_property_handler(dll_path: &str, approved: &windows_registry::Key) -> Result<()> {
    register_inproc_server(CLSID_PROPERTY_STORE_STR, PS_NAME, dll_path, approved)?;
    // Property handlers prefer "Both" (the shared helper defaults to Apartment).
    CLASSES_ROOT
        .create(format!("CLSID\\{CLSID_PROPERTY_STORE_STR}\\InprocServer32"))?
        .set_string("ThreadingModel", "Both")?;
    for (ext, _) in crate::formats::FORMATS {
        if settings::format_enabled(ext) {
            let _ = hook_ext_propstore(ext);
        } else {
            unhook_ext_propstore(ext);
        }
    }
    Ok(())
}

/// `(HKLM PropertyHandlers\.<ext>, HKCR SystemFileAssociations\.<ext>)` for one extension.
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
fn hook_ext_propstore(ext: &str) -> Result<()> {
    let (handler, assoc) = propstore_keys(ext);
    let existing = LOCAL_MACHINE.open(&handler).ok().and_then(|k| k.get_string("").ok());
    if !matches!(existing.as_deref(), None | Some("") | Some(CLSID_PROPERTY_STORE_STR)) {
        return Ok(()); // a real handler already owns this extension — leave it alone
    }
    LOCAL_MACHINE.create(&handler)?.set_string("", CLSID_PROPERTY_STORE_STR)?;
    let a = CLASSES_ROOT.create(&assoc)?;
    a.set_string("InfoTip", PROP_INFOTIP)?;
    a.set_string("FullDetails", PROP_FULLDETAILS)?;
    a.set_string("PreviewDetails", PROP_PREVIEWDETAILS)?;
    a.set_string("AdditionalProperties", PROP_ADDITIONAL)?;
    set_perceived_type(ext);
    Ok(())
}

/// Set `HKCR\.<ext>`'s `PerceivedType` so `kind:` search + library grouping can classify the
/// formats Windows otherwise doesn't know (kra/ora/blend/epub/djvu/svg/xcf/…). Written ONLY when
/// absent — we never overwrite a value Windows or another app already set. NOT removed on unhook:
/// a correct classification is harmless to leave behind, and since we only ever write into an empty
/// slot we also can't prove on removal that the current value is ours rather than one a freshly
/// installed app added later — so leaving it avoids clobbering that.
fn set_perceived_type(ext: &str) {
    let key = format!(".{ext}");
    let already = CLASSES_ROOT.open(&key).ok().and_then(|k| k.get_string("PerceivedType").ok());
    if matches!(already.as_deref(), Some(s) if !s.is_empty()) {
        return; // a value is already present (Windows or another app) — leave it
    }
    let pt = match crate::formats::category(ext) {
        crate::formats::Category::Audio => "audio",
        crate::formats::Category::Video => "video",
        crate::formats::Category::Ebook | crate::formats::Category::Document => "document",
        crate::formats::Category::Image | crate::formats::Category::Raw => "image",
    };
    if let Ok(k) = CLASSES_ROOT.create(&key) {
        if k.set_string("PerceivedType", pt).is_ok() {
            // Marker so unhook can remove OUR PerceivedType without clobbering one another app
            // sets later (we only ever fill an empty slot, but can't otherwise prove ownership).
            let _ = k.set_string(PERCEIVED_TYPE_MARK, "1");
        }
    }
}

/// Remove the `PerceivedType` we set — but ONLY where our [`PERCEIVED_TYPE_MARK`] marker proves it
/// was ours, so a value Windows or another app owns is never clobbered.
fn unhook_perceived_type(ext: &str) {
    let key = format!(".{ext}");
    if let Ok(k) = CLASSES_ROOT.open(&key) {
        if k.get_string(PERCEIVED_TYPE_MARK).is_ok() {
            let _ = k.remove_value("PerceivedType");
            let _ = k.remove_value(PERCEIVED_TYPE_MARK);
        }
    }
}

/// Remove our property-handler binding + the prop lists, but ONLY where they're still ours
/// (never clobber a handler / info-tip another product set).
fn unhook_ext_propstore(ext: &str) {
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
        if let Ok(k) = CLASSES_ROOT.open(&assoc) {
            for v in ["InfoTip", "FullDetails", "PreviewDetails", "AdditionalProperties"] {
                let _ = k.remove_value(v);
            }
        }
    }
    unhook_perceived_type(ext);
}

/// Register the IPreviewHandler coclass: its COM server, the surrogate `AppID`
/// (so it runs in `prevhost.exe`, out of process), the global `PreviewHandlers`
/// list entry, and the per-extension `shellex` slot for each enabled format.
fn register_preview_handler(dll_path: &str, approved: &windows_registry::Key) -> Result<()> {
    register_inproc_server(CLSID_PREVIEW_HANDLER_STR, PV_NAME, dll_path, approved)?;
    // "Both" (the shared helper defaults to Apartment): the preview host loads us into its
    // own STA but our render worker self-inits an MTA apartment (`previewhandler.rs`), so the
    // accurate declaration is Both — matching the property handler. (Apartment worked only
    // because prevhost.exe tolerated the mismatch.)
    CLASSES_ROOT
        .create(format!("CLSID\\{CLSID_PREVIEW_HANDLER_STR}\\InprocServer32"))?
        .set_string("ThreadingModel", "Both")?;
    // The AppID on our CLSID points the shell at the out-of-process preview host.
    CLASSES_ROOT
        .create(format!("CLSID\\{CLSID_PREVIEW_HANDLER_STR}"))?
        .set_string("AppID", PREVHOST_APPID)?;
    // The machine-wide registered-handlers list (value name = CLSID, data = name).
    LOCAL_MACHINE
        .create(PREVIEW_HANDLERS)?
        .set_string(CLSID_PREVIEW_HANDLER_STR, PV_NAME)?;
    // Hook each enabled extension's preview slot; unhook disabled ones (mirrors the
    // thumbnail per-extension loop, gated by the same Options format list).
    for (ext, _) in crate::formats::FORMATS {
        if settings::format_enabled(ext) {
            let _ = hook_ext_preview(ext);
        } else {
            unhook_ext_preview(ext);
        }
    }
    Ok(())
}

/// Register one in-proc COM server: `CLSID\{guid}` (friendly name) +
/// `InprocServer32` (dll path, Apartment threading) + the Approved entry.
/// Both of our coclasses configure identically through here.
fn register_inproc_server(
    clsid_str: &str,
    name: &str,
    dll_path: &str,
    approved: &windows_registry::Key,
) -> Result<()> {
    let base = format!("CLSID\\{clsid_str}");
    CLASSES_ROOT.create(&base)?.set_string("", name)?;
    let inproc = CLASSES_ROOT.create(format!("{base}\\InprocServer32"))?;
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
        format!(".{ext}\\shellex\\{THUMB_HANDLER}"),
        format!("SystemFileAssociations\\.{ext}\\shellex\\{THUMB_HANDLER}"),
    ]
}

/// Point one extension's thumbnail `shellex` keys at our CLSID.
fn hook_ext(ext: &str) -> Result<()> {
    for path in thumb_keys(ext) {
        CLASSES_ROOT.create(path)?.set_string("", CLSID_THUMBNAIL_PROVIDER_STR)?;
    }
    Ok(())
}

/// Remove one extension's thumbnail `shellex` keys — but only the ones that
/// actually point at OUR CLSID, so we never clobber a handler another product
/// (or Windows) registered in that slot.
fn unhook_ext(ext: &str) {
    for path in thumb_keys(ext) {
        remove_if_ours(&path);
    }
}

/// Like [`unhook_ext`], but after removing our handler leaf it also sweeps the
/// now-orphaned parent chain (`…\shellex`, then `.<ext>` /
/// `SystemFileAssociations\.<ext>`). This is the FULL UNINSTALL behavior and
/// must only run on the unregister path — a normal settings-apply re-register
/// disables individual formats with [`unhook_ext`] and must NOT prune parents
/// (the user may re-enable, and a foreign sibling may share the chain).
fn unhook_ext_and_prune(ext: &str) {
    for path in thumb_keys(ext) {
        remove_if_ours(&path);
        prune_empty_parents(&path);
    }
}

/// True if the key at `path` exists and has zero subkeys AND zero values — i.e.
/// it's a genuinely empty husk safe to delete. A missing key, or any I/O error
/// while probing, returns `false` (conservative: never delete what we can't
/// confirm is empty).
fn is_empty_key(path: &str) -> bool {
    let Ok(key) = CLASSES_ROOT.open(path) else {
        return false;
    };
    let no_subkeys = key.keys().map(|mut it| it.next().is_none()).unwrap_or(false);
    let no_values = key.values().map(|mut it| it.next().is_none()).unwrap_or(false);
    no_subkeys && no_values
}

/// After our handler leaf at `path` is removed, walk BACK UP the chain deleting
/// each parent that is now genuinely empty: the `…\shellex` container, then the
/// `.<ext>` (or `SystemFileAssociations\.<ext>`) key. Stops at the first
/// non-empty (or missing) parent, so a populated foreign key — or the shared
/// `SystemFileAssociations` root itself — is never touched. `path` is one of
/// the `thumb_keys` entries: `<assoc>\shellex\{THUMB_HANDLER}`, whose two
/// ancestors we care about are `<assoc>\shellex` and `<assoc>`.
fn prune_empty_parents(path: &str) {
    // Drop the `\{THUMB_HANDLER}` leaf component -> `<assoc>\shellex`.
    let Some(shellex) = path.rsplit_once('\\').map(|(parent, _)| parent) else {
        return;
    };
    if !is_empty_key(shellex) {
        return;
    }
    let _ = CLASSES_ROOT.remove_tree(shellex);

    // Drop the `\shellex` component -> `<assoc>` (`.ext` or
    // `SystemFileAssociations\.ext`). Only prune if it too is now empty.
    let Some(assoc) = shellex.rsplit_once('\\').map(|(parent, _)| parent) else {
        return;
    };
    if is_empty_key(assoc) {
        let _ = CLASSES_ROOT.remove_tree(assoc);
    }
}

/// Delete a thumbnail-handler `shellex` key only if its default value is our
/// CLSID. A foreign handler in that slot is left untouched.
fn remove_if_ours(path: &str) {
    if let Ok(key) = CLASSES_ROOT.open(path) {
        if key.get_string("").ok().as_deref() == Some(CLSID_THUMBNAIL_PROVIDER_STR) {
            let _ = CLASSES_ROOT.remove_tree(path);
        }
    }
}

pub fn unregister() -> Result<()> {
    // Order matters: remove the property-store VALUES on `SystemFileAssociations\.<ext>` FIRST,
    // so the subsequent thumbnail/preview `*_and_prune` calls find that key empty and prune it —
    // otherwise the lingering InfoTip/FullDetails/… values keep the key alive as orphan litter.
    for (ext, _) in crate::formats::FORMATS {
        unhook_ext_propstore(ext);
        unhook_ext_and_prune(ext);
        unhook_ext_preview_and_prune(ext);
    }
    // Also sweep historically-dropped extensions (orphans from older builds — see register()).
    for ext in crate::formats::REMOVED_EXTENSIONS {
        unhook_ext_propstore(ext);
        unhook_ext_and_prune(ext);
        unhook_ext_preview_and_prune(ext);
    }
    let _ = CLASSES_ROOT.remove_tree(format!("CLSID\\{CLSID_THUMBNAIL_PROVIDER_STR}"));
    let _ = CLASSES_ROOT.remove_tree("*\\shellex\\ContextMenuHandlers\\SageThumbs2K");
    let _ = CLASSES_ROOT.remove_tree(format!("CLSID\\{CLSID_CONTEXT_MENU_STR}"));
    let _ = CLASSES_ROOT.remove_tree(format!("CLSID\\{CLSID_PREVIEW_HANDLER_STR}"));
    let _ = CLASSES_ROOT.remove_tree(format!("CLSID\\{CLSID_PROPERTY_STORE_STR}"));
    if let Ok(list) = LOCAL_MACHINE.open(PREVIEW_HANDLERS) {
        let _ = list.remove_value(CLSID_PREVIEW_HANDLER_STR);
    }
    if let Ok(approved) = LOCAL_MACHINE.open(APPROVED) {
        let _ = approved.remove_value(CLSID_THUMBNAIL_PROVIDER_STR);
        let _ = approved.remove_value(CLSID_CONTEXT_MENU_STR);
        let _ = approved.remove_value(CLSID_PREVIEW_HANDLER_STR);
        let _ = approved.remove_value(CLSID_PROPERTY_STORE_STR);
    }
    notify_shell();
    Ok(())
}

// ── preview-handler per-extension hooking (mirrors the thumbnail helpers) ──────

/// The two `shellex` preview-handler key paths for one extension.
fn preview_keys(ext: &str) -> [String; 2] {
    [
        format!(".{ext}\\shellex\\{PREVIEW_HANDLER}"),
        format!("SystemFileAssociations\\.{ext}\\shellex\\{PREVIEW_HANDLER}"),
    ]
}

/// Point one extension's preview `shellex` keys at our preview CLSID — but ONLY where the slot
/// is empty or already ours. Never displace another product's preview handler (mirrors
/// [`hook_ext_propstore`]'s guard): a foreign CLSID in the slot means a real handler owns the
/// format, and clobbering it would replace a richer preview with our static frame.
fn hook_ext_preview(ext: &str) -> Result<()> {
    for path in preview_keys(ext) {
        let existing = CLASSES_ROOT.open(&path).ok().and_then(|k| k.get_string("").ok());
        if !matches!(existing.as_deref(), None | Some("") | Some(CLSID_PREVIEW_HANDLER_STR)) {
            continue; // a real handler already owns this slot — leave it alone
        }
        CLASSES_ROOT.create(path)?.set_string("", CLSID_PREVIEW_HANDLER_STR)?;
    }
    Ok(())
}

/// Remove one extension's preview `shellex` keys, but only where they point at OUR
/// preview CLSID (never clobber another product's handler).
fn unhook_ext_preview(ext: &str) {
    for path in preview_keys(ext) {
        remove_if_ours_preview(&path);
    }
}

/// Full-uninstall variant: remove our preview leaf and sweep now-empty parents
/// (reuses the thumbnail path's [`prune_empty_parents`]).
fn unhook_ext_preview_and_prune(ext: &str) {
    for path in preview_keys(ext) {
        remove_if_ours_preview(&path);
        prune_empty_parents(&path);
    }
}

/// Delete a preview `shellex` key only if its default value is our preview CLSID.
fn remove_if_ours_preview(path: &str) {
    if let Ok(key) = CLASSES_ROOT.open(path) {
        if key.get_string("").ok().as_deref() == Some(CLSID_PREVIEW_HANDLER_STR) {
            let _ = CLASSES_ROOT.remove_tree(path);
        }
    }
}

fn notify_shell() {
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None) };
}
