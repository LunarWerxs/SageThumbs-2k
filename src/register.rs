//! Classic per-extension registration for the thumbnail provider + context menu.
//!
//! A plain in-proc COM server registered via `regsvr32`. Thumbnail providers do
//! NOT need package identity (only the modern `IExplorerCommand` main-flyout does,
//! and that ships as a signed sparse package — see `scripts/packaging/make-msix.ps1`), and the shell
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
use windows_registry::{Key, CLASSES_ROOT, CURRENT_USER, LOCAL_MACHINE};

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
const APPROVED: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Shell Extensions\Approved";
/// Where we remember a thumbnail handler that occupied a `shellex` slot BEFORE we took it,
/// keyed by the exact HKCR key path we overwrote. Unlike the preview/property handlers (which
/// step aside for an incumbent — Windows' built-ins are richer there), the thumbnail provider
/// IS the product and does take the slot. But taking it must be REVERSIBLE: without this
/// record, `unhook`/uninstall deleted our value and left the slot empty forever, so a user who
/// had Icaros/Adobe/a codec pack thumbnailing a format never got it back — uninstalling
/// SageThumbs did not undo the damage. Machine-wide, mirroring the HKCR/HKLM registration.
const DISPLACED: &str = r"SOFTWARE\SageThumbs2K\DisplacedThumbHandlers";

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

    // The folder right-click entry. Written HERE and not only from Settings, because a normal
    // install never opens Settings — without this the verb would exist for nobody until they
    // happened to visit that page. It records an absolute path to the companion EXE, so
    // re-running registration after a move is also what repoints it.
    crate::foldermenu::sync(settings::folder_prebuild_verb());

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
    let already = CLASSES_ROOT
        .open(&key)
        .ok()
        .and_then(|k| k.get_string("PerceivedType").ok());
    if matches!(already.as_deref(), Some(s) if !s.is_empty()) {
        return; // a value is already present (Windows or another app) — leave it
    }
    let pt = match crate::formats::category(ext) {
        crate::formats::Category::Audio => "audio",
        crate::formats::Category::Video => "video",
        crate::formats::Category::Ebook | crate::formats::Category::Document => "document",
        crate::formats::Category::Image | crate::formats::Category::Raw => "image",
        // In practice Windows itself already stamps .zip/.rar/.7z as "compressed",
        // so the already-present guard above usually skips these anyway.
        crate::formats::Category::Archive => "compressed",
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
        .create(format!(
            "CLSID\\{CLSID_PREVIEW_HANDLER_STR}\\InprocServer32"
        ))?
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

/// Point one extension's thumbnail `shellex` keys at our CLSID, first recording any foreign
/// handler we are displacing so [`remove_if_ours`] can put it back.
fn hook_ext(ext: &str) -> Result<()> {
    for path in thumb_keys(ext) {
        remember_displaced(&path);
        CLASSES_ROOT
            .create(&path)?
            .set_string("", CLSID_THUMBNAIL_PROVIDER_STR)?;
    }
    Ok(())
}

/// Note the handler currently in `path` under [`DISPLACED`] so unhooking can restore it.
///
/// No-ops when the slot is empty or already ours — which is what makes a re-register
/// idempotent: the SECOND register sees our own CLSID and leaves the original record intact
/// rather than overwriting it with ourselves (which would silently discard the thing we are
/// meant to give back). If a third product takes the slot from us and we re-register later,
/// recording that one is correct: restore returns the slot to whoever held it last.
fn remember_displaced(path: &str) {
    remember_displaced_in(CLASSES_ROOT, LOCAL_MACHINE, path);
}

/// [`remember_displaced`] against an explicit pair of hives, so the machine-wide path and the
/// PORTABLE per-user path share one implementation. The per-user path must record into HKCU:
/// a zip has no HKLM write access, and it evicts incumbents from `HKCU\Software\Classes` just
/// as destructively as the installer does from HKCR. (`SOFTWARE\...` resolves under either
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
fn restore_displaced(path: &str) {
    restore_displaced_in(CLASSES_ROOT, LOCAL_MACHINE, path);
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
/// CLSID, then hand the slot back to whoever we took it from. A foreign handler
/// in that slot is left untouched.
fn remove_if_ours(path: &str) {
    if let Ok(key) = CLASSES_ROOT.open(path) {
        if key.get_string("").ok().as_deref() == Some(CLSID_THUMBNAIL_PROVIDER_STR) {
            let _ = CLASSES_ROOT.remove_tree(path);
            restore_displaced(path);
        }
    }
}

pub fn unregister() -> Result<()> {
    // Written per ProgID under HKCU by the opt-in "hide Windows' file-type icon" setting.
    // Nothing else would ever clean these up, and a leftover empty `TypeOverlay` would keep
    // suppressing another program's icon long after we are gone.
    crate::typeoverlay::remove_all();
    // The folder right-click entry is ours alone under HKCU, but nothing else would ever take
    // it out, and a leftover verb would point at an EXE that is no longer installed.
    crate::foldermenu::remove_all();
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
        let existing = CLASSES_ROOT
            .open(&path)
            .ok()
            .and_then(|k| k.get_string("").ok());
        if !matches!(
            existing.as_deref(),
            None | Some("") | Some(CLSID_PREVIEW_HANDLER_STR)
        ) {
            continue; // a real handler already owns this slot — leave it alone
        }
        CLASSES_ROOT
            .create(path)?
            .set_string("", CLSID_PREVIEW_HANDLER_STR)?;
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

/// Is the thumbnail provider registered *right now*?
///
/// Reading back the CLSID is the cheapest true test that `DllRegisterServer` actually
/// ran: our own registration writes this key and nothing else does. Used by the Settings
/// "Repair file associations" button to check whether the elevated `regsvr32` it just
/// launched really succeeded — launching a process tells you nothing about the outcome,
/// and reporting "repaired" after a silent failure is worse than reporting nothing.
pub fn is_registered() -> bool {
    CLASSES_ROOT
        .open(format!(
            "CLSID\\{CLSID_THUMBNAIL_PROVIDER_STR}\\InprocServer32"
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
    // Best-effort per extension: one locked-down key must not abort the rest.
    for (ext, _) in crate::formats::FORMATS {
        if settings::format_enabled(ext) {
            for path in thumb_keys(ext) {
                // Same non-destructive claim as the machine-wide path: note whoever held this
                // slot in the user's own hive so `remove_user_if_ours` can hand it straight
                // back. Portable mode is still a real install from the shell's point of view.
                remember_displaced_in(&classes, CURRENT_USER, &path);
                if let Ok(k) = classes.create(&path) {
                    let _ = k.set_string("", CLSID_THUMBNAIL_PROVIDER_STR);
                }
            }
        } else {
            remove_user_if_ours(&classes, ext);
        }
    }

    if let Ok(k) = classes.create("*\\shellex\\ContextMenuHandlers\\SageThumbs2K") {
        let _ = k.set_string("", CLSID_CONTEXT_MENU_STR);
    }

    notify_shell();
    Ok(())
}

/// Undo [`register_user`]. Removes only keys whose value is OUR CLSID, so a handler another
/// product owns is never collateral damage, and leaves the machine-wide hive alone.
pub fn unregister_user() -> Result<()> {
    let classes = user_classes()?;
    for (ext, _) in crate::formats::FORMATS {
        remove_user_if_ours(&classes, ext);
    }
    for ext in crate::formats::REMOVED_EXTENSIONS {
        remove_user_if_ours(&classes, ext);
    }
    if let Ok(k) = classes.open("*\\shellex\\ContextMenuHandlers\\SageThumbs2K") {
        if k.get_string("").ok().as_deref() == Some(CLSID_CONTEXT_MENU_STR) {
            let _ = classes.remove_tree("*\\shellex\\ContextMenuHandlers\\SageThumbs2K");
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
