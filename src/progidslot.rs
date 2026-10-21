//! Take a file type's ProgID-level thumbnail slot from Windows' own Photos thumbnailer while
//! the user has asked for a tile only we draw: the format badge or the transparency
//! checkerboard (issue #66).
//!
//! # The precedence that hides us
//!
//! Explorer resolves a thumbnail handler at the file type's **ProgID** before it reads the
//! extension keys `register.rs` writes (see its module doc, KNOWN LIMITATION). Windows ships
//! `jpegfile` with its own Photos thumbnailer in that slot (`PhotoMetadataHandler.dll`), and an
//! association or a Windows build can put one under the ProgID a `.png` resolves through too,
//! so every file of that type is drawn by Windows and never reaches us. For a plain picture
//! that costs nothing anyone can see, which is why the limitation was accepted. With the badge
//! on it is visible at a glance: issue #66's folder showed `JPG` on every JPEG and no mark on
//! the PNGs beside them.
//!
//! # Measured (2026-10-21, Windows 11 build 26200)
//!
//! A scratch `.stjp` whose class default is `jpegfile`, thumbnailed through
//! `IShellItemImageFactory` with the badge on: no badge (Windows drew it). With
//! `HKCU\Software\Classes\jpegfile\ShellEx\{e357fccd-…}` set to our CLSID and nothing else
//! changed, the next file got the badge. `HKCU\Software\Classes` merges ahead of the machine
//! half, so the override needs no elevation and leaves Windows' own key untouched.
//!
//! # What is taken, and when
//!
//! Only a slot that names **Windows' own** handler (its DLL under the Windows directory), only
//! under a ProgID an enabled format resolves through, and only while [`wanted`]. Another
//! program's handler in that slot is a choice the user made by installing it: `st2k doctor`
//! names it and this never overrides it. A ProgID several extensions share (`jpegfile` serves
//! `.jpg`, `.jpeg`, `.jpe` and `.jfif`) is taken when any enabled one resolves through it.
//!
//! Every value carries a marker beside it, and [`sync`] gives back every marked slot (found by
//! enumerating the user's classes hive, the same way `typeoverlay` clears its values) before
//! writing what the settings call for. Turning the badge off, a changed default program, and
//! uninstall therefore all hand the slot back to Windows.

use windows_registry::{Key, CLASSES_ROOT, CURRENT_USER};

use st2k_base::formats;
use st2k_base::guids::{CLSID_THUMBNAIL_PROVIDER_STR, THUMB_HANDLER_CATEGORY};

/// Marker proving a thumbnail slot under a ProgID is one we wrote.
const MARK: &str = "SageThumbs2K.ThumbSlot";

/// `ShellEx\{IThumbnailProvider category}`, relative to a ProgID.
fn slot_path(progid: &str) -> String {
    format!(r"{progid}\ShellEx\{THUMB_HANDLER_CATEGORY}")
}

/// Whether the settings ask for a tile Windows' own thumbnailer cannot draw: our format badge,
/// or the transparency checkerboard.
pub fn wanted() -> bool {
    st2k_base::settings::format_badge() || st2k_base::settings::thumb_checker()
}

/// The handler `progid`'s own thumbnail slot names in the merged classes view, if any.
pub(crate) fn slot_handler(progid: &str) -> Option<String> {
    CLASSES_ROOT
        .open(slot_path(progid))
        .ok()?
        .get_string("")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Whether `clsid` is one of Windows' own handlers: its DLL lives under the Windows directory.
/// Shared with `st2k doctor`, which reports these apart from other programs' handlers.
pub(crate) fn is_windows_own_handler(clsid: &str) -> bool {
    let Some(dll) = CLASSES_ROOT
        .open(format!(r"CLSID\{clsid}\InprocServer32"))
        .ok()
        .and_then(|k| k.get_string("").ok())
    else {
        return false;
    };
    let root = std::env::var("SystemRoot").unwrap_or_default();
    is_under_windows_dir(&dll, &root)
}

/// `dll` (as an `InprocServer32` value spells it) sits under `%SystemRoot%`, `%windir%` or the
/// literal `root`. Pure, so the boundary rule is testable: `C:\WindowsApps` is not `C:\Windows`.
fn is_under_windows_dir(dll: &str, root: &str) -> bool {
    let lower = dll.trim().trim_start_matches('"').to_ascii_lowercase();
    let at_boundary = |pfx: &str| {
        !pfx.is_empty()
            && lower.starts_with(pfx)
            && (lower.len() == pfx.len() || lower.as_bytes()[pfx.len()] == b'\\')
    };
    at_boundary("%systemroot%") || at_boundary("%windir%") || at_boundary(&root.to_ascii_lowercase())
}

/// Whether `clsid` names no COM class at all: no `CLSID` key and no packaged-COM entry.
/// Windows ships `icofile` pointing at one (`{43372ada-945b-4713-82db-d217b338ae6f}`). Explorer
/// cannot create it and goes on to the next key, so `.ico` still reaches us (measured
/// 2026-10-21: the badge appears). `st2k doctor` had been reporting it as a competitor.
pub(crate) fn is_dead_handler(clsid: &str) -> bool {
    CLASSES_ROOT.open(format!(r"CLSID\{clsid}")).is_err()
        && CLASSES_ROOT
            .open(format!(r"PackagedCom\ClassIndex\{clsid}"))
            .is_err()
}

/// Whether a slot naming `handler` is one to take: Windows' own, and not already ours.
fn takes(handler: &str, windows_own: impl Fn(&str) -> bool) -> bool {
    !handler.eq_ignore_ascii_case(CLSID_THUMBNAIL_PROVIDER_STR) && windows_own(handler)
}

/// Point `progid`'s thumbnail slot at us in the user hive `classes`, with our marker beside it.
/// A value already in the USER hive that we did not write is somebody's own override, and stays.
fn take_slot(classes: &Key, progid: &str) -> bool {
    let path = slot_path(progid);
    if let Ok(k) = classes.open(&path) {
        if k.get_string("").is_ok() && k.get_string(MARK).is_err() {
            return false;
        }
    }
    let Ok(k) = classes.create(&path) else {
        return false;
    };
    k.set_string("", CLSID_THUMBNAIL_PROVIDER_STR).is_ok() && k.set_string(MARK, "1").is_ok()
}

/// Give `progid`'s slot back if our marker proves we took it, then prune the `ShellEx` and
/// ProgID keys when they existed only to hold it.
fn give_back(classes: &Key, progid: &str) {
    let path = slot_path(progid);
    let Ok(ro) = classes.open(&path) else {
        return;
    };
    if ro.get_string(MARK).is_err() {
        return;
    }
    drop(ro);
    let _ = classes.remove_tree(&path);
    for key in [format!(r"{progid}\ShellEx"), progid.to_string()] {
        if !is_empty(classes, &key) {
            break;
        }
        let _ = classes.remove_tree(&key);
    }
}

/// `path` exists under `classes` and holds no values and no subkeys.
fn is_empty(classes: &Key, path: &str) -> bool {
    classes.open(path).is_ok_and(|k| {
        k.values().is_ok_and(|mut v| v.next().is_none())
            && k.keys().is_ok_and(|mut s| s.next().is_none())
    })
}

/// Give back every slot we ever took, found by ENUMERATING the user's classes hive rather than
/// re-deriving today's associations: a ProgID nothing points at any more (the user picked a new
/// default program) is still reached. Same reasoning as `typeoverlay::clear_every_mark`.
fn give_back_all(classes: &Key) {
    let Ok(names) = classes.keys() else {
        return;
    };
    // Collect before mutating: the iterator reads the key the removals write to.
    let names: Vec<String> = names.collect();
    for name in names {
        give_back(classes, &name);
    }
}

/// Every ProgID an enabled format resolves through (the `UserChoice` and the class default,
/// `typeoverlay::progids_for`), each once.
fn enabled_progids() -> Vec<String> {
    let fmt = st2k_base::settings::format_enabled_snapshot();
    let mut out: Vec<String> = Vec::new();
    for (ext, _) in formats::FORMATS {
        if !fmt.enabled(ext) {
            continue;
        }
        for progid in crate::typeoverlay::progids_for(ext) {
            if !out.contains(&progid) {
                out.push(progid);
            }
        }
    }
    out
}

/// [`sync`] against an explicit user hive and lookups, so the order (give everything back,
/// THEN read the merged view and take) is testable on a scratch key. Returns how many slots
/// were taken.
fn sync_into(
    classes: &Key,
    take: bool,
    progids: &[String],
    handler_of: impl Fn(&str) -> Option<String>,
    windows_own: impl Fn(&str) -> bool,
) -> usize {
    give_back_all(classes);
    if !take {
        return 0;
    }
    progids
        .iter()
        .filter(|p| handler_of(p).is_some_and(|h| takes(&h, &windows_own)))
        .filter(|p| take_slot(classes, p))
        .count()
}

/// Take (or give back) the ProgID-level slots for every enabled format. Called wherever
/// `typeoverlay::sync` is: after registration, when Settings changes the corner mark or the
/// checkerboard, from setup's badge box and from Repair. The caller tells the shell (a
/// thumbnail-cache rebuild or `SHCNE_ASSOCCHANGED`), as it already does for the overlay.
pub fn sync(take: bool) {
    let Ok(classes) = CURRENT_USER.create(r"Software\Classes") else {
        return;
    };
    // Read the ProgIDs before giving anything back: the list comes from associations, which
    // our slot values do not change.
    let progids = enabled_progids();
    let taken = sync_into(&classes, take, &progids, slot_handler, is_windows_own_handler);
    if taken > 0 {
        st2k_base::safety::log(&format!(
            "progidslot: took {taken} ProgID thumbnail slot(s) from Windows' own thumbnailer"
        ));
    }
}

/// Give back every slot we ever took, regardless of the settings. For uninstall.
pub fn remove_all() {
    if let Ok(classes) = CURRENT_USER.create(r"Software\Classes") {
        give_back_all(&classes);
    }
}

#[cfg(test)]
mod tests;
