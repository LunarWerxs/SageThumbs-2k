//! Thumbnail handlers that sat in a slot before ours: remembered at hook time, restored at unhook.

use super::*;

/// Where we remember a thumbnail handler that occupied a `shellex` slot BEFORE we took it,
/// keyed by the exact classes-relative key path we overwrote. Unlike the preview/property
/// handlers (which step aside for an incumbent — Windows' built-ins are richer there), the
/// thumbnail provider IS the product and does take the slot. But taking it must be REVERSIBLE:
/// without this record, `unhook`/uninstall deleted our value and left the slot empty forever, so
/// a user who had Icaros/Adobe/a codec pack thumbnailing a format never got it back —
/// uninstalling SageThumbs did not undo the damage. Machine-wide, mirroring the HKLM registration.
pub(super) const DISPLACED: &str = r"SOFTWARE\SageThumbs2K\DisplacedThumbHandlers";

/// Note the handler currently in `path` under [`DISPLACED`] so unhooking can restore it.
///
/// No-ops when the slot is empty or already ours — which is what makes a re-register
/// idempotent: the SECOND register sees our own CLSID and leaves the original record intact
/// rather than overwriting it with ourselves (which would silently discard the thing we are
/// meant to give back). If a third product takes the slot from us and we re-register later,
/// recording that one is correct: restore returns the slot to whoever held it last.
pub(super) fn remember_displaced(classes: &Key, path: &str) {
    remember_displaced_in(classes, LOCAL_MACHINE, path);
}

/// [`remember_displaced`] against an explicit pair of hives, so the machine-wide path and the
/// PORTABLE per-user path share one implementation. The per-user path must record into HKCU:
/// a zip has no HKLM write access, and it evicts incumbents from `HKCU\Software\Classes` just
/// as destructively as the installer does from HKLM. (`SOFTWARE\...` resolves under either
/// hive — the registry is case-insensitive, and HKCU keeps the record beside the settings.)
pub(super) fn remember_displaced_in(classes: &Key, records: &Key, path: &str) {
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
pub(super) fn restore_displaced(classes: &Key, path: &str) {
    restore_displaced_in(classes, LOCAL_MACHINE, path);
}

/// [`restore_displaced`] against an explicit pair of hives — the twin of
/// [`remember_displaced_in`], shared by the machine-wide and portable per-user paths.
pub(super) fn restore_displaced_in(classes: &Key, records: &Key, path: &str) {
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
