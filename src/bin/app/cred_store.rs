//! Secure local storage for the Connections OAuth **refresh token** (+ a little
//! identity for the Settings UI). The refresh token is DPAPI-encrypted at the current
//! user's scope (`CryptProtectData`) and stored base64 under
//! `HKCU\Software\SageThumbs2K\OAuth` — so it's only decryptable by this user on this
//! machine, never in plaintext, and each machine does its own sign-in. The short-lived
//! **access token stays in memory only** and is never persisted (see `oauth`/`sync`).
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

/// The signed-in user's identity, for the "Synced as …" UI row. Not a secret.
pub(crate) struct Identity {
    pub sub: String,
    pub email: String,
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
        CryptProtectData(&in_blob, PCWSTR::null(), None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out)
            .is_ok()
    } else {
        CryptUnprotectData(&in_blob, None, None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out).is_ok()
    };
    if !ok || out.pbData.is_null() {
        return None;
    }
    let bytes = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
    let _ = LocalFree(Some(HLOCAL(out.pbData as *mut c_void)));
    Some(bytes)
}

// ---- Public API ----------------------------------------------------------

/// DPAPI-encrypt and persist the refresh token. Best-effort → returns whether it stuck.
pub(crate) fn save_refresh_token(token: &str) -> bool {
    let Some(enc) = (unsafe { dpapi(token.as_bytes(), true) }) else {
        return false;
    };
    let b64 = base64::engine::general_purpose::STANDARD.encode(enc);
    CURRENT_USER
        .create(oauth_key())
        .and_then(|k| k.set_string(V_REFRESH, &b64))
        .is_ok()
}

/// Load + DPAPI-decrypt the refresh token, or `None` if absent/undecryptable (e.g. the
/// blob was copied from another machine/user — treated as "not signed in").
pub(crate) fn load_refresh_token() -> Option<String> {
    let b64 = CURRENT_USER.open(oauth_key()).and_then(|k| k.get_string(V_REFRESH)).ok()?;
    let enc = base64::engine::general_purpose::STANDARD.decode(b64.trim()).ok()?;
    let plain = unsafe { dpapi(&enc, false) }?;
    String::from_utf8(plain).ok()
}

/// Persist the signed-in identity for the UI (plain, non-secret).
pub(crate) fn save_identity(sub: &str, email: &str) {
    if let Ok(k) = CURRENT_USER.create(oauth_key()) {
        let _ = k.set_string(V_SUB, sub);
        let _ = k.set_string(V_EMAIL, email);
    }
}

/// The stored identity for the "Synced as …" row, if any.
pub(crate) fn load_identity() -> Option<Identity> {
    let k = CURRENT_USER.open(oauth_key()).ok()?;
    let sub = k.get_string(V_SUB).unwrap_or_default();
    let email = k.get_string(V_EMAIL).unwrap_or_default();
    if sub.is_empty() && email.is_empty() {
        return None;
    }
    Some(Identity { sub, email })
}

/// Whether a refresh token is present (a decryptable one — a foreign blob reads as no).
pub(crate) fn is_signed_in() -> bool {
    load_refresh_token().is_some()
}

/// Forget all local OAuth state (disconnect). Best-effort per value so a partial key
/// still gets cleaned. Never touches the settings root.
pub(crate) fn clear() {
    if let Ok(k) = CURRENT_USER.open(oauth_key()) {
        let _ = k.remove_value(V_REFRESH);
        let _ = k.remove_value(V_SUB);
        let _ = k.remove_value(V_EMAIL);
    }
}
