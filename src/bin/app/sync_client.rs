//! Connections settings-sync: the allowlisted push/pull between the local HKCU settings
//! and the per-user cloud document at `studio.connections.icu/v1/app-data/{appId}`.
//!
//! Only an explicit **allowlist** of portable preferences is synced — never machine-local
//! values (absolute paths, the upload-host config), local-only flags, or secrets. The
//! store is the "settings locker": one JSON object, ≤64 KB, optimistic-concurrency writes
//! (RFC 7386 deep-merge). EXE-only; the DLL never links this.
//!
//! Public API (all blocking → the Settings UI calls these on a worker thread):
//!   - [`is_signed_in`] / [`signed_in_label`] — UI state.
//!   - [`connect`] — interactive browser sign-in, store creds, initial pull/seed.
//!   - [`pull_on_open`] — pull remote → local (seed if the remote is empty).
//!   - [`push`] — push the current local allowlisted settings.
//!   - [`disconnect`] — delete the remote doc + forget local creds.

use serde_json::{Map, Value};

use sagethumbs2k_core::settings;
use windows_registry::CURRENT_USER;

use crate::{cred_store, http, oauth};

const STORE_BASE: &str = "https://studio.connections.icu/v1/app-data";
const TIMEOUT_SECS: u64 = 20;
const MAX_RESP: usize = 128 * 1024;

#[derive(Clone, Copy)]
enum Kind {
    Dword,
    Str,
}

/// The syncable-key allowlist — portable preferences ONLY. Deliberately excludes
/// `ShotSaveDir` (absolute path), `Debug` (local diagnostics), the install-state
/// flags, `ModernMenuActive` (HKLM installer state), and everything under the `OAuth`
/// subkey (secrets). The `MenuItems\*` and `<ext>\Enabled` subkeys are deferred to v1.1
/// (they need subkey enumeration + the elevated re-register path, respectively).
const ALLOW: &[(&str, Kind)] = &[
    ("EnableThumbs", Kind::Dword),
    ("MaxSize", Kind::Dword),
    ("Width", Kind::Dword),
    ("Height", Kind::Dword),
    ("UseEmbedded", Kind::Dword),
    ("JPEG", Kind::Dword),
    ("PNG", Kind::Dword),
    ("EnableMenu", Kind::Dword),
    ("MenuAllFileTypes", Kind::Dword),
    ("MenuPreview", Kind::Dword),
    ("MenuQuickVerbs", Kind::Dword),
    ("PreviewChecker", Kind::Dword),
    ("PreserveFileDate", Kind::Dword),
    ("ContainerSort", Kind::Dword),
    ("ContainerPreferCover", Kind::Dword),
    ("ContainerSkipScanlation", Kind::Dword),
    ("CvJpegQuality", Kind::Dword),
    ("CvWebpQuality", Kind::Dword),
    ("CvWebpLossless", Kind::Dword),
    ("CvPngLevel", Kind::Dword),
    ("CvMagickQuality", Kind::Dword),
    ("ScreenshotHotkey", Kind::Dword),
    ("ScreenshotQuickHotkey", Kind::Dword),
    ("CustomAction", Kind::Dword),
    ("CustomActionHotkey", Kind::Dword),
    ("ScreenshotHideTray", Kind::Dword),
    ("ShotUseSaveDir", Kind::Dword),
    ("UpdateAutoCheck", Kind::Dword),
    ("Lang", Kind::Str),
    ("MenuOrder", Kind::Str),
];

// ---- local <-> JSON ------------------------------------------------------

/// Snapshot the currently-stored allowlisted settings into a JSON object. Only values
/// that are actually PRESENT in the registry are included, so a machine that never
/// touched a setting won't push its default and clobber another machine's choice.
fn read_local() -> Map<String, Value> {
    let mut map = Map::new();
    if let Ok(k) = CURRENT_USER.open(settings::ROOT) {
        for (name, kind) in ALLOW {
            match kind {
                Kind::Dword => {
                    if let Ok(v) = k.get_u32(name) {
                        map.insert((*name).to_string(), Value::from(v));
                    }
                }
                Kind::Str => {
                    if let Ok(s) = k.get_string(name) {
                        map.insert((*name).to_string(), Value::from(s));
                    }
                }
            }
        }
    }
    map
}

/// Apply a remote `settings` object to local HKCU — but ONLY allowlisted keys with the
/// expected type. Unknown keys are ignored (forward-compat + a hostile/expanded doc can't
/// write arbitrary registry values). Returns how many values were applied.
fn apply_remote(settings_obj: &Value) -> u32 {
    let Some(obj) = settings_obj.as_object() else {
        return 0;
    };
    let Ok(k) = CURRENT_USER.create(settings::ROOT) else {
        return 0;
    };
    let mut applied = 0;
    for (name, kind) in ALLOW {
        let Some(val) = obj.get(*name) else { continue };
        let ok = match kind {
            Kind::Dword => val.as_u64().is_some_and(|n| k.set_u32(name, n as u32).is_ok()),
            Kind::Str => val.as_str().is_some_and(|s| k.set_string(name, s).is_ok()),
        };
        if ok {
            applied += 1;
        }
    }
    applied
}

// ---- store transport -----------------------------------------------------

fn store_url() -> String {
    format!("{STORE_BASE}/{}", oauth::CLIENT_ID)
}

fn auth_headers(token: &str) -> String {
    format!("Authorization: Bearer {token}\r\nContent-Type: application/json")
}

/// GET the current doc → `(version, settings)`. A never-written user is `(0, {})`.
fn store_get(token: &str) -> Result<(u64, Value), String> {
    let resp = http::request("GET", &store_url(), &auth_headers(token), &[], TIMEOUT_SECS, MAX_RESP)
        .ok_or_else(|| "couldn't reach the sync server".to_string())?;
    if resp.status != 200 {
        return Err(store_error(resp.status, &resp.body));
    }
    let json: Value =
        serde_json::from_slice(&resp.body).map_err(|_| "the sync server sent an unreadable reply".to_string())?;
    let version = json.get("version").and_then(Value::as_u64).unwrap_or(0);
    let settings = json.get("settings").cloned().unwrap_or_else(|| Value::Object(Map::new()));
    Ok((version, settings))
}

/// POST the local snapshot as an RFC 7386 deep-merge write, retrying on a version
/// conflict (bounded). Returns the new version.
fn push_snapshot(token: &str) -> Result<u64, String> {
    let snapshot = Value::Object(read_local());
    let mut base = store_get(token)?.0;
    for _ in 0..3 {
        let body = serde_json::json!({ "settings": snapshot, "baseVersion": base, "merge": true });
        let bytes = serde_json::to_vec(&body).map_err(|e| e.to_string())?;
        let resp = http::request("POST", &store_url(), &auth_headers(token), &bytes, TIMEOUT_SECS, MAX_RESP)
            .ok_or_else(|| "couldn't reach the sync server".to_string())?;
        match resp.status {
            200 => {
                let json: Value = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
                return Ok(json.get("version").and_then(Value::as_u64).unwrap_or(base + 1));
            }
            409 => {
                // Stale baseVersion — take the server's current version and retry.
                let json: Value = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
                base = json
                    .get("current")
                    .and_then(|c| c.get("version"))
                    .and_then(Value::as_u64)
                    .or_else(|| store_get(token).ok().map(|(v, _)| v))
                    .unwrap_or(base);
            }
            _ => return Err(store_error(resp.status, &resp.body)),
        }
    }
    Err("sync kept conflicting with another device — please try again".to_string())
}

fn store_delete(token: &str) -> Result<(), String> {
    let resp = http::request("DELETE", &store_url(), &auth_headers(token), &[], TIMEOUT_SECS, MAX_RESP)
        .ok_or_else(|| "couldn't reach the sync server".to_string())?;
    // 204 = deleted, 404 = already gone — both fine for "disconnect".
    if matches!(resp.status, 200 | 204 | 404) {
        Ok(())
    } else {
        Err(store_error(resp.status, &resp.body))
    }
}

/// Map the documented store status codes to short, friendly messages.
fn store_error(status: u16, body: &[u8]) -> String {
    match status {
        401 => return "your sign-in expired — please sign in again".to_string(),
        403 => return "this app isn't authorized for that account".to_string(),
        413 => return "your settings are too large to sync".to_string(),
        429 => return "syncing too often — please wait a moment".to_string(),
        _ => {}
    }
    if let Ok(json) = serde_json::from_slice::<Value>(body) {
        if let Some(e) = json.get("error").and_then(Value::as_str) {
            return format!("sync failed: {e}");
        }
    }
    format!("sync failed (HTTP {status})")
}

/// Mint a fresh access token from the stored refresh token (rotating + re-persisting it).
fn access_token() -> Result<String, String> {
    let rt = cred_store::load_refresh_token().ok_or_else(|| "not signed in".to_string())?;
    let tokens = oauth::refresh(&rt)?;
    if let Some(new_rt) = &tokens.refresh_token {
        cred_store::save_refresh_token(new_rt);
    }
    Ok(tokens.access_token)
}

// ---- public orchestration (UI-facing) ------------------------------------

/// Whether a (decryptable) refresh token is stored on this machine.
pub(crate) fn is_signed_in() -> bool {
    cred_store::is_signed_in()
}

/// The name (or, failing that, the relay email) to show in the "Synced as …" row, if
/// signed in. The `email` claim is an opaque per-app privacy-relay address
/// (`<hex>@privaterelay.connections.icu`), never the user's real inbox, so `name` is
/// preferred whenever we have one. A bare `sub` is never surfaced — `None` if all we have
/// is an id with no name and no email.
pub(crate) fn signed_in_label() -> Option<String> {
    let id = cred_store::load_identity()?;
    if !id.name.is_empty() {
        Some(id.name)
    } else if !id.email.is_empty() {
        Some(id.email)
    } else {
        None
    }
}

/// Interactive sign-in: browser round-trip, securely store the refresh token + identity,
/// then do the initial pull (or seed the cloud from local if it's empty). Returns the
/// display label (name/email/sub) for the UI. Blocking — run on a worker thread.
pub(crate) fn connect() -> Result<String, String> {
    let tokens = oauth::login()?;
    let rt = tokens
        .refresh_token
        .clone()
        .ok_or_else(|| "the sign-in server didn't return a refresh token".to_string())?;
    if !cred_store::save_refresh_token(&rt) {
        return Err("couldn't securely store your sign-in".to_string());
    }
    let (sub, email, name, picture) = oauth::identity_from_tokens(&tokens).unwrap_or_default();
    cred_store::save_identity(&sub, &email, &name, &picture);

    // Initial pull/seed using the access token we already hold (no refresh needed).
    let (version, settings) = store_get(&tokens.access_token)?;
    if version > 0 {
        apply_remote(&settings);
    } else {
        push_snapshot(&tokens.access_token)?;
    }
    Ok(if !name.is_empty() {
        name
    } else if !email.is_empty() {
        email
    } else {
        sub
    })
}

/// Pull remote settings and apply them locally; seed the cloud if it's empty. Returns
/// `Ok(true)` if any values were applied (so the UI should refresh its controls).
/// Blocking — run on a worker thread.
pub(crate) fn pull_on_open() -> Result<bool, String> {
    let token = access_token()?;
    let (version, settings) = store_get(&token)?;
    if version > 0 {
        Ok(apply_remote(&settings) > 0)
    } else {
        push_snapshot(&token)?;
        Ok(false)
    }
}

/// Push the current local allowlisted settings to the cloud. Blocking — run on a worker
/// thread (called after the user applies settings changes).
pub(crate) fn push() -> Result<(), String> {
    let token = access_token()?;
    push_snapshot(&token).map(|_| ())
}

/// Disconnect: best-effort delete the remote doc, then forget local credentials.
pub(crate) fn disconnect() {
    if let Ok(token) = access_token() {
        let _ = store_delete(&token);
    }
    cred_store::clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_excludes_machine_local_and_secrets() {
        let names: Vec<&str> = ALLOW.iter().map(|(n, _)| *n).collect();
        for banned in [
            "ShotSaveDir",
            "Debug",
            "InstallReported",
            "DevMachine",
            "Tombstone",
            "ModernMenuActive",
            "RefreshToken",
        ] {
            assert!(!names.contains(&banned), "{banned} must NEVER be synced");
        }
        for portable in ["EnableThumbs", "MenuOrder", "Lang", "ScreenshotHotkey", "JPEG"] {
            assert!(names.contains(&portable), "{portable} should be syncable");
        }
    }

    #[test]
    fn allowlist_has_no_duplicates() {
        let mut names: Vec<&str> = ALLOW.iter().map(|(n, _)| *n).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "duplicate key in the sync allowlist");
    }

    #[test]
    fn apply_remote_ignores_non_allowlisted_and_wrong_types() {
        // A hostile/expanded doc: an off-list key + a wrong-typed on-list key. `apply_remote`
        // must not count either (and never writes the off-list key). We assert on the count
        // rather than touching the real registry for the on-list value.
        let doc = serde_json::json!({
            "ShotSaveDir": "C:\\evil\\path",   // off-list → ignored
            "SomeRandomKey": 1,                 // off-list → ignored
            "JPEG": "not-a-number"              // on-list but wrong type → not applied
        });
        // Only off-list / wrong-typed entries → nothing applies.
        assert_eq!(apply_remote(&doc), 0);
    }
}
