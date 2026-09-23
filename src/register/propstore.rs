//! Property-handler registration: the Details-pane property lists, PerceivedType, and their removal.

use super::*;

/// Hover info-tip layout. ONE combined list serves every category: the shell only shows
/// properties the store actually returns a value for, so an image surfaces Dimensions/Camera,
/// audio surfaces Artist/Title, video its duration — all from the same list. (InfoTip omits
/// empty properties automatically, so no `*` prefix is needed here.)
pub(super) const PROP_INFOTIP: &str =
    "prop:System.ItemTypeText;System.Image.Dimensions;System.Photo.CameraModel;System.Media.Duration;System.Music.Artist;System.Title;System.Size";

/// The BOTTOM details pane layout (`System.PropList.PreviewDetails`). DISTINCT from `FullDetails`
/// (the Properties▸Details *tab*) and `InfoTip` (the hover tooltip): the pane Explorer shows
/// under a selected file reads THIS list, and a format with no PreviewDetails (psd/raw/epub/…)
/// falls back to the bare date/size default — so our handler's dimensions never surfaced there
/// even though `GetValue` returned them. Metadata fields are `*`-prefixed (shown only when the
/// store returns a value), so a PSD shows Dimensions/DateTaken while an audio file shows
/// Artist/Duration/Genre from the same combined list; Size + dates are unprefixed (always present).
pub(super) const PROP_PREVIEWDETAILS: &str = "prop:*System.Image.Dimensions;*System.Image.BitDepth;*System.Image.HorizontalResolution;*System.Image.VerticalResolution;*System.Photo.CameraManufacturer;*System.Photo.CameraModel;*System.Photo.DateTaken;*System.GPS.LatitudeDecimal;*System.GPS.LongitudeDecimal;*System.Video.FrameWidth;*System.Video.FrameHeight;*System.Media.Duration;*System.Audio.EncodingBitrate;*System.Music.Artist;*System.Music.AlbumTitle;*System.Title;*System.Music.TrackNumber;*System.Music.Genre;*System.Media.Year;System.Size;System.DateCreated;System.DateModified";

/// The four `SystemFileAssociations\.<ext>` property-list values and the string this build
/// writes into each. ONE table feeds both [`hook_ext_propstore`] (what gets written) and
/// [`remove_owned_prop_lists`] (what may be removed), so the two can never disagree about which
/// value names are ours.
pub(super) const PROP_LISTS: [(&str, &str); 4] = [
    ("InfoTip", PROP_INFOTIP),
    ("FullDetails", PROP_FULLDETAILS),
    ("PreviewDetails", PROP_PREVIEWDETAILS),
    ("AdditionalProperties", PROP_ADDITIONAL),
];

/// Property-list strings EARLIER builds wrote that no `PROP_*` constant above matches any more.
/// Unhook used to delete the four list values unconditionally whenever the handler binding was
/// ours, on the assumption that nobody else writes those value names for a format we own
/// (2026-09-05 audit, F35). That assumption fails the moment a user or another product edits one
/// of the lists while we stay the property handler: disabling the format or uninstalling threw
/// their customisation away. A value is now removed only when its content is one THIS code
/// wrote, which needs every string it has ever written and not just today's: an install that
/// upgraded from 0.6.0 without ever re-hooking still carries these two, and matching only the
/// current constants would orphan them on uninstall.
/// MAINTENANCE RULE: when a `PROP_*` constant above changes, append the string it replaced here,
/// or the upgrade-then-uninstall path leaks the old value.
pub(super) const LEGACY_PROP_LISTS: &[&str] = &[
    // 0.6.0 InfoTip (the first release with a property handler); 0.7.0 added CameraModel and
    // Duration and the string has not changed since.
    "prop:System.ItemTypeText;System.Image.Dimensions;System.Music.Artist;System.Title;System.Size",
    // 0.6.0 FullDetails; 0.7.0 grew it to today's list. 0.6.0 wrote no PreviewDetails or
    // AdditionalProperties at all, so those two names have only ever carried today's strings.
    "prop:System.Image.Dimensions;System.Image.HorizontalSize;System.Image.VerticalSize;System.Photo.CameraManufacturer;System.Photo.CameraModel;System.Music.Artist;System.Music.AlbumTitle;System.Title;System.Music.TrackNumber;System.Size;System.DateModified",
];

/// Register the IPropertyStore coclass: its COM server (threaded "Both" — it also loads in the
/// MTA SearchIndexer), the per-extension `PropertyHandlers\.<ext>` binding, and a combined
/// info-tip / full-details property list so the values actually surface in Explorer.
pub(super) fn register_property_handler(
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
pub(super) fn propstore_keys(ext: &str) -> (String, String) {
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
pub(super) fn hook_ext_propstore(classes: &Key, ext: &str) -> Result<()> {
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
    for (name, value) in PROP_LISTS {
        set_assoc_value_if_empty(&a, name, value);
    }
    set_perceived_type(classes, ext);
    Ok(())
}

/// Write `name` on `key` only when it's currently absent/empty — mirrors [`set_perceived_type`]'s
/// "fill an empty slot, never overwrite" rule for the property-list values written above.
pub(super) fn set_assoc_value_if_empty(key: &Key, name: &str, value: &str) {
    let filled = key.get_string(name).map_or_else(
        |_| key.values().is_ok_and(|mut v| v.any(|(n, _)| n == name)),
        |s| !s.is_empty(),
    ); // a present non-string value counts as filled
    if filled {
        return; // a value is already present (Windows or another app) — leave it
    }
    let _ = key.set_string(name, value);
}

/// The `PerceivedType` we stamp for `ext`, or `None` for a format we leave unclassified.
/// Image formats WIC cannot open get `None` (see [`WIC_IMAGE_EXTS`]).
pub(super) fn perceived_type_for(ext: &str) -> Option<&'static str> {
    Some(match st2k_base::formats::category(ext) {
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
/// formats Windows otherwise doesn't know (epub/djvu documents, audio/video, camera RAW, and
/// the WIC-openable images). Written ONLY when absent — we never overwrite a value Windows or
/// another app already set — and marked with [`PERCEIVED_TYPE_MARK`] so [`unhook_perceived_type`]
/// removes exactly the values we wrote (on every disable and uninstall) and nothing another app set later.
pub(super) fn set_perceived_type(classes: &Key, ext: &str) {
    let key = format!(".{ext}");
    let filled = classes.open(&key).ok().is_some_and(|k| {
        k.get_string("PerceivedType").map_or_else(
            |_| {
                k.values()
                    .is_ok_and(|mut v| v.any(|(n, _)| n == "PerceivedType"))
            },
            |s| !s.is_empty(),
        )
    });
    if filled {
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

/// Whether the `PerceivedType` on `.<ext>` is one WE wrote (see [`set_perceived_type`], which
/// only ever fills an empty slot and marks what it filled).
///
/// [`crate::typeoverlay`] needs this: Explorer draws no corner icon on a type it perceives as
/// an image, so where WE are the reason it perceives one, we are the reason the icon the user
/// used to see went away — and putting it back is a correction, not an addition. Where WINDOWS
/// perceives it (every format it decodes itself), there was never an icon and adding one would
/// be a change nobody asked for.
pub(crate) fn perceived_type_is_ours(ext: &str) -> bool {
    windows_registry::CLASSES_ROOT
        .open(format!(".{ext}"))
        .ok()
        .and_then(|k| k.get_string(PERCEIVED_TYPE_MARK).ok())
        .is_some()
}

/// Remove the `PerceivedType` we set — but ONLY where our [`PERCEIVED_TYPE_MARK`] marker proves it
/// was ours, so a value Windows or another app owns is never clobbered.
pub(super) fn unhook_perceived_type(classes: &Key, ext: &str) {
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
pub(super) fn unhook_ext_propstore(classes: &Key, ext: &str) {
    let (handler, assoc) = propstore_keys(ext);
    let was_ours = LOCAL_MACHINE
        .open(&handler)
        .ok()
        .and_then(|k| k.get_string("").ok())
        .as_deref()
        == Some(CLSID_PROPERTY_STORE_STR);
    if was_ours {
        let _ = LOCAL_MACHINE.remove_tree(&handler);
        // Gated on `was_ours` so we never touch lists under a foreign handler; which of the four
        // values then go is decided per value by their content (see `remove_owned_prop_lists`).
        // `create`, not `open`: same read-only-handle trap as `unhook_perceived_type` above,
        // `open`'s handle makes `remove_value` a silent no-op, so these four values survived
        // every uninstall/disable.
        if let Ok(k) = classes.create(&assoc) {
            remove_owned_prop_lists(&k);
        }
    }
    unhook_perceived_type(classes, ext);
}

/// True when `value` is a property list THIS code wrote, in this or any earlier build.
pub(super) fn is_owned_prop_list(value: &str) -> bool {
    PROP_LISTS.iter().any(|(_, ours)| *ours == value) || LEGACY_PROP_LISTS.contains(&value)
}

/// Remove, from a writable association key, each of the [`PROP_LISTS`] values whose content is
/// still one we wrote (any build, see [`LEGACY_PROP_LISTS`]). A value a user or another product
/// has since changed, added, or stored as a non-string type is left in place: it is their
/// customisation, not our litter (2026-09-05 audit, F35). `hook_ext_propstore` only ever fills an
/// EMPTY slot, so there is nothing displaced to restore here; leaving the foreign value is the
/// whole of the undo. Any other value on the key is never touched.
pub(super) fn remove_owned_prop_lists(assoc: &Key) {
    for (name, _) in PROP_LISTS {
        let Ok(current) = assoc.get_string(name) else {
            continue; // absent, or not a string we could have written
        };
        if is_owned_prop_list(&current) {
            let _ = assoc.remove_value(name);
        }
    }
}
