//! Secure local storage for the Connections OAuth **refresh token** (+ a little
//! identity for the Settings UI). The refresh token is DPAPI-encrypted at the current
//! user's scope (`CryptProtectData`) and stored base64 under
//! `HKCU\Software\SageThumbs2K\OAuth` — so it's only decryptable by this user on this
//! machine, never in plaintext, and each machine does its own sign-in. On a portable copy
//! (issue #227/G117) the same base64 blob and the plaintext identity go into the portable
//! ini next to the running EXE instead of the host's HKCU, so nothing persists on a PC the
//! portable copy is only borrowing. The short-lived **access token stays in memory only**
//! and is never persisted (see `oauth`/`sync`).
//!
//! EXE-only (the Settings app). No `keyring` dependency — DPAPI is already available via
//! the `Win32_Security_Cryptography` feature the app enables for BCrypt, keeping the
//! project's minimal-deps ethos. The refresh token IS a secret and is deliberately kept
//! OUT of the synced settings doc (which is a "settings locker, no secrets" store).

use std::ffi::c_void;

use base64::Engine;
use sagethumbs2k_core::settings;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};
use windows_registry::CURRENT_USER;

const V_REFRESH: &str = "RefreshToken";
const V_SUB: &str = "Sub";
const V_EMAIL: &str = "Email";
const V_NAME: &str = "Name";
const V_PICTURE: &str = "Picture";

/// Prefix for the portable-mode key names (issue #117). Kept distinct from every other
/// root-level setting `sagethumbs2k_core::settings::set_string`/`get_string_opt` might
/// hold, since portable mode routes through those (the root of the portable ini) rather
/// than the `OAuth` registry subkey used on an installed copy.
const PORTABLE_PREFIX: &str = "OAuth_";

/// The signed-in user's identity, for the "Synced as …" UI row. Not a secret. `email` is a
/// per-app privacy-relay address (`<hex>@privaterelay.connections.icu`), never the user's
/// real inbox — `name` is preferred for display. `picture` is a profile-picture URL (needs
/// the `photo` scope); captured for a future avatar but not rendered today.
pub(crate) struct Identity {
    /// Kept for completeness (it's the stable account id) though `signed_in_label` never
    /// surfaces it — a raw `sub` reads as an ugly UUID, so the label prefers name/email.
    #[allow(dead_code)]
    pub sub: String,
    pub email: String,
    pub name: String,
    /// Not rendered yet — no Win32 image fetch/blit wired up (out of scope); persisted so a
    /// future avatar feature doesn't need a re-sign-in to backfill it.
    #[allow(dead_code)]
    pub picture: String,
}

/// `HKCU\Software\SageThumbs2K\OAuth`. Kept separate from the settings root so a
/// "reset all settings" never touches credentials, and `clear()` here never touches
/// settings.
fn oauth_key() -> String {
    format!(r"{}\OAuth", settings::ROOT)
}

// ---- DPAPI ---------------------------------------------------------------

/// DPAPI-encrypt (`protect=true`) or decrypt (`protect=false`) `input` at the current
/// user's scope, UI suppressed. Copies the result out and frees the CNG-allocated buffer.
unsafe fn dpapi(input: &[u8], protect: bool) -> Option<Vec<u8>> {
    let in_blob = CRYPT_INTEGER_BLOB {
        cbData: input.len() as u32,
        pbData: input.as_ptr() as *mut u8,
    };
    let mut out = CRYPT_INTEGER_BLOB::default();
    let ok = if protect {
        CryptProtectData(
            &in_blob,
            PCWSTR::null(),
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
        .is_ok()
    } else {
        CryptUnprotectData(
            &in_blob,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
        .is_ok()
    };
    if !ok || out.pbData.is_null() {
        return None;
    }
    let bytes = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
    let _ = LocalFree(Some(HLOCAL(out.pbData as *mut c_void)));
    Some(bytes)
}

// ---- Public API ----------------------------------------------------------

/// Issue #227/G117: on a portable copy, `HKCU` is the HOST PC's account, not this run's own
/// storage — every other setting already redirects to the portable ini in that case
/// (`settings::portable()`), but this module always wrote the DPAPI-encrypted refresh token
/// and the plaintext identity straight to HKCU, leaving a persistent credential and PII behind
/// on a borrowed PC. Route through the portable ini's root section instead when portable,
/// namespacing with [`PORTABLE_PREFIX`] since that root section is shared with every other
/// portable setting. The DPAPI blob itself is still only decryptable by this Windows user on
/// this machine either way — the change is WHERE the (still-encrypted) blob and the identity
/// text live, not the encryption.
fn portable_key(suffix: &str) -> String {
    format!("{PORTABLE_PREFIX}{suffix}")
}

/// DPAPI-encrypt and persist the refresh token. Best-effort → returns whether it stuck.
pub(crate) fn save_refresh_token(token: &str) -> bool {
    let Some(enc) = (unsafe { dpapi(token.as_bytes(), true) }) else {
        return false;
    };
    let b64 = base64::engine::general_purpose::STANDARD.encode(enc);
    if settings::portable() {
        return settings::set_string(&portable_key(V_REFRESH), &b64).is_ok();
    }
    CURRENT_USER
        .create(oauth_key())
        .and_then(|k| k.set_string(V_REFRESH, &b64))
        .is_ok()
}

/// Load + DPAPI-decrypt the refresh token, or `None` if absent/undecryptable (e.g. the
/// blob was copied from another machine/user — treated as "not signed in").
pub(crate) fn load_refresh_token() -> Option<String> {
    let b64 = if settings::portable() {
        settings::get_string_opt(&portable_key(V_REFRESH))?
    } else {
        CURRENT_USER
            .open(oauth_key())
            .and_then(|k| k.get_string(V_REFRESH))
            .ok()?
    };
    let enc = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .ok()?;
    let plain = unsafe { dpapi(&enc, false) }?;
    String::from_utf8(plain).ok()
}

/// Persist the signed-in identity for the UI (plain, non-secret).
pub(crate) fn save_identity(sub: &str, email: &str, name: &str, picture: &str) {
    if settings::portable() {
        for (key, value) in [
            (V_SUB, sub),
            (V_EMAIL, email),
            (V_NAME, name),
            (V_PICTURE, picture),
        ] {
            let key = portable_key(key);
            if settings::set_string(&key, value).is_err() {
                // The portable store is a hand-editable ini and refuses a value that would
                // corrupt it (a leading `[`, `;` or `#`, a line break); an OAuth display
                // name can be anything. Store it blank rather than keep a stale one.
                let _ = settings::set_string(&key, "");
            }
        }
        return;
    }
    if let Ok(k) = CURRENT_USER.create(oauth_key()) {
        let _ = k.set_string(V_SUB, sub);
        let _ = k.set_string(V_EMAIL, email);
        let _ = k.set_string(V_NAME, name);
        let _ = k.set_string(V_PICTURE, picture);
    }
}

/// The stored identity for the "Synced as …" row, if any. `Name`/`Picture` are missing on
/// identities saved before this app captured them — they simply read back empty, so old
/// stored values keep loading fine.
pub(crate) fn load_identity() -> Option<Identity> {
    if settings::portable() {
        let sub = settings::get_string_opt(&portable_key(V_SUB)).unwrap_or_default();
        let email = settings::get_string_opt(&portable_key(V_EMAIL)).unwrap_or_default();
        let name = settings::get_string_opt(&portable_key(V_NAME)).unwrap_or_default();
        let picture = settings::get_string_opt(&portable_key(V_PICTURE)).unwrap_or_default();
        if sub.is_empty() && email.is_empty() {
            return None;
        }
        return Some(Identity {
            sub,
            email,
            name,
            picture,
        });
    }
    let k = CURRENT_USER.open(oauth_key()).ok()?;
    let sub = k.get_string(V_SUB).unwrap_or_default();
    let email = k.get_string(V_EMAIL).unwrap_or_default();
    let name = k.get_string(V_NAME).unwrap_or_default();
    let picture = k.get_string(V_PICTURE).unwrap_or_default();
    if sub.is_empty() && email.is_empty() {
        return None;
    }
    Some(Identity {
        sub,
        email,
        name,
        picture,
    })
}

/// Whether a refresh token is present (a decryptable one — a foreign blob reads as no).
pub(crate) fn is_signed_in() -> bool {
    load_refresh_token().is_some()
}

/// Forget all local OAuth state (disconnect). Best-effort per value so a partial key
/// still gets cleaned. Never touches the settings root. Portable-aware (issue #227/G117): a
/// portable copy's "clear" must reach the portable ini values it actually signed into, not an
/// HKCU subkey that was never written on that copy.
pub(crate) fn clear() {
    if settings::portable() {
        let _ = settings::set_string(&portable_key(V_REFRESH), "");
        let _ = settings::set_string(&portable_key(V_SUB), "");
        let _ = settings::set_string(&portable_key(V_EMAIL), "");
        let _ = settings::set_string(&portable_key(V_NAME), "");
        let _ = settings::set_string(&portable_key(V_PICTURE), "");
        return;
    }
    if let Ok(k) = CURRENT_USER.open(oauth_key()) {
        let _ = k.remove_value(V_REFRESH);
        let _ = k.remove_value(V_SUB);
        let _ = k.remove_value(V_EMAIL);
        let _ = k.remove_value(V_NAME);
        let _ = k.remove_value(V_PICTURE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Issue #227/G117: the portable-mode keys must be namespaced away from every other
    /// root-level setting the portable ini's root section might hold (the ini has no
    /// subkeys of its own the way HKCU has an `OAuth` subkey), and each of the five values
    /// must land on a distinct name.
    #[test]
    fn portable_keys_are_namespaced_and_distinct() {
        let keys = [
            portable_key(V_REFRESH),
            portable_key(V_SUB),
            portable_key(V_EMAIL),
            portable_key(V_NAME),
            portable_key(V_PICTURE),
        ];
        for k in &keys {
            assert!(
                k.starts_with(PORTABLE_PREFIX),
                "{k} must carry the portable-mode prefix"
            );
        }
        let mut sorted = keys.to_vec();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            keys.len(),
            "every portable key must be distinct"
        );
    }
}
