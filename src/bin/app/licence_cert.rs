//! Offline licence certificates: a signed statement from Pay that this installation
//! holds a licence, verified locally with no network at all.
//!
//! WHY THIS EXISTS, and what it is deliberately NOT. Until now the only proof of a
//! business licence was `GET /license/check` against the relay, which meant a licensed
//! customer's standing depended on a Cloudflare Worker being correctly configured and
//! reachable. On 2026-09-03 it was neither: a revoked API key sitting in the Worker made
//! every check answer `502`, and every paying machine read as offline for hours. Nothing
//! in the app was wrong; the answer simply could not arrive.
//!
//! A certificate removes that failure mode by construction. `POST /license/redeem`
//! returns one, signed Ed25519 over the licence's claims, and this module verifies it
//! against a public key compiled into the binary. No key of ours ships, no request is
//! made, and no outage anywhere can turn a licensed machine into an unlicensed one.
//!
//! ⛔ IT DOES NOT REPLACE THE RELAY CHECK, and must not be made to. A signed statement
//! cannot be withdrawn, so the only revocation reach an offline artifact has is its own
//! expiry. The relay check stays exactly where it is and keeps doing what a certificate
//! cannot: noticing a revocation within hours rather than within the certificate's life,
//! and carrying the REASON (`seat_revoked` / `contract_ended`) that the deauthorised
//! notice names. The two compose:
//!
//! * The certificate is the FLOOR. A machine holding a valid one is licensed, full stop,
//!   and no network condition can take that away.
//! * The relay is the REFINEMENT. When it answers, it answers sooner and says more.
//!
//! That composition is the actual fix for what went wrong: the relay stops being
//! load-bearing. A relay that is down, misconfigured, or pointed at the wrong company
//! now costs slower revocation, not a customer's access.
//!
//! FAIL DIRECTION. Every error here means "no certificate", never "not licensed" - the
//! caller falls through to the relay breadcrumb exactly as before. That is the same rule
//! the rest of `license.rs` follows ("everything here fails toward Personal/free"), read
//! the other way round: a certificate can only ever ADD standing, never remove it.
//!
//! REFRESH. A certificate carries an `exp` (30 days by default). Re-presenting the same
//! redemption code to `/license/redeem` mints a fresh one - a replay is expected and
//! answers `replayed: true` - which is why [`crate::cred_store`] keeps the code.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde_json::Value;

/// Pay's Ed25519 public key, as the raw 32 bytes (the tail of the SPKI DER served at
/// `GET https://licensing.connections.icu/api/licences/verification-key`).
///
/// Baked in at build time on purpose: an offline check cannot fetch a key, which is the
/// entire point. Verified 2026-09-04 against BOTH encodings that endpoint publishes - the
/// PEM's DER tail and the JWK's `x` - which agree.
///
/// ⚠️ THERE IS NO KEY ID. The certificate carries no `kid` and the endpoint serves one key
/// rather than a set, so a client cannot tell "signed by a key I do not have" from
/// "forged" - both are simply a bad signature. If Pay ever rotates this key, every copy
/// already in the field stops verifying new certificates and cannot learn the new key.
/// That is survivable only because of the fail direction above: a rotation degrades this
/// module to silence and the relay check carries licensing on its own.
const VERIFY_KEY: [u8; 32] = [
    0xed, 0x07, 0xd3, 0x99, 0xe3, 0xd6, 0xe9, 0x26, 0xd1, 0x0d, 0x7b, 0xdd, 0x8e, 0x5d, 0x6d, 0x1d,
    0x13, 0x19, 0x62, 0x91, 0xd9, 0xfd, 0xd3, 0x61, 0xbb, 0x65, 0xc8, 0xc0, 0x98, 0x6e, 0x67, 0xac,
];

/// The audience every licence certificate carries. Checked on every verify, because the
/// same signing key is used for other short-lived token kinds: skip this and one of those
/// verifies as a perpetual licence.
const AUDIENCE: &str = "connections-licence";

/// Our Pay catalog product. A certificate for some other product of ours would verify
/// cryptographically and mean nothing here.
pub(crate) const PRODUCT_ID: &str = "24544461-9530-4edb-84e5-4f3471876d98";

/// The claims this app acts on, read out of the signed payload.
///
/// Pulled field by field off a [`Value`] rather than through a derive, the same way
/// [`crate::license::History`] reads its breadcrumb: this project carries `serde_json`
/// but not `serde`'s derive machinery, and a certificate is far too small to justify
/// dragging proc-macros into the tree for it. Unknown fields are ignored by
/// construction, so a future Pay release can add some without breaking installed copies.
struct Claims {
    aud: String,
    product: String,
    sub: String,
    exp: i64,
    /// ⛔ GENUINELY NULLABLE, and it must stay an `Option`. `maint` is absent-or-null on a
    /// licence whose updates never lapse, and reading that as `0` would refuse every build
    /// ever made. `as_i64` answers `None` for both absent and JSON `null`, which is
    /// exactly the meaning wanted here, so the two cases deliberately are not told apart.
    maint: Option<i64>,
}

impl Claims {
    /// `None` when the payload is not an object, or a field this app needs is missing or
    /// the wrong type. Never a partial read: a certificate we cannot fully understand is
    /// not a certificate.
    fn from_json(v: &Value) -> Option<Self> {
        let obj = v.as_object()?;
        let text = |k: &str| obj.get(k).and_then(Value::as_str).map(String::from);
        Some(Self {
            aud: text("aud")?,
            product: text("product")?,
            sub: text("sub")?,
            exp: obj.get("exp").and_then(Value::as_i64)?,
            maint: obj.get("maint").and_then(Value::as_i64),
        })
    }
}

/// What a good certificate says, split into the two questions it answers.
///
/// ⛔ TWO FIELDS, NEVER ONE. `licensed` gates whether the software runs; `updates_allowed`
/// gates whether it may take a NEW BUILD. They have different lifetimes - a perpetual
/// licence stays licensed forever while its update window closes after twelve months -
/// and collapsing them into one boolean is how software somebody bought outright gets
/// switched off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Verified {
    /// This machine holds a licence. Gate the LAUNCH on this.
    pub licensed: bool,
    /// This build is inside the maintenance window. Gate the UPDATER on this, and
    /// nothing else.
    pub updates_allowed: bool,
}

/// Why a certificate did not verify. Every variant means "no certificate" to the caller,
/// never "not licensed" - they are separated only so the doctor report can say which.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CertError {
    /// Not `<payload>.<signature>`.
    Malformed,
    /// A segment was not unpadded base64url, or the signature was not 64 bytes.
    BadEncoding,
    /// Ed25519 said no. Also what a key rotation looks like from in here.
    BadSignature,
    /// Verified, but the payload is not JSON we understand.
    BadPayload,
    /// Verified, but signed for something other than a licence.
    WrongAudience,
    /// Verified, but for another product or another machine.
    NotThisMachine,
    /// Verified and correct, but past its `exp`. Re-redeem the stored code.
    Expired,
}

/// Verify a certificate and say what it grants.
///
/// `now_unix` and `build_date_unix` are parameters rather than reads so the whole thing
/// stays a pure function of its inputs - the same reason the rest of this module's
/// neighbours take a clock. `build_date_unix` is THIS build's own release date, stamped in
/// at compile time; the maintenance rule is `build_date <= maint`, compared by DATE rather
/// than by parsing version strings, which is exactly how Pay decides it server-side.
pub(crate) fn verify(
    cert: &str,
    expected_sub: &str,
    now_unix: i64,
    build_date_unix: i64,
) -> Result<Verified, CertError> {
    let (payload_b64, sig_b64) = cert.split_once('.').ok_or(CertError::Malformed)?;
    if payload_b64.is_empty() || sig_b64.is_empty() || sig_b64.contains('.') {
        return Err(CertError::Malformed);
    }

    let sig_raw = URL_SAFE_NO_PAD
        .decode(sig_b64)
        .map_err(|_| CertError::BadEncoding)?;
    let sig_bytes: [u8; 64] = sig_raw
        .as_slice()
        .try_into()
        .map_err(|_| CertError::BadEncoding)?;

    let key = VerifyingKey::from_bytes(&VERIFY_KEY).map_err(|_| CertError::BadSignature)?;

    // ⛔ SIGNED OVER THE BASE64URL TEXT, NOT THE DECODED JSON. Verify the bytes of the
    // payload segment exactly as they arrived, then decode. Decoding first and verifying
    // the JSON fails every time, and confirmed against a live certificate on 2026-09-04.
    key.verify(payload_b64.as_bytes(), &Signature::from_bytes(&sig_bytes))
        .map_err(|_| CertError::BadSignature)?;

    let json = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|_| CertError::BadEncoding)?;
    let value: Value = serde_json::from_slice(&json).map_err(|_| CertError::BadPayload)?;
    let claims = Claims::from_json(&value).ok_or(CertError::BadPayload)?;

    if claims.aud != AUDIENCE {
        return Err(CertError::WrongAudience);
    }
    if claims.product != PRODUCT_ID || claims.sub != expected_sub {
        return Err(CertError::NotThisMachine);
    }
    if claims.exp <= now_unix {
        return Err(CertError::Expired);
    }

    Ok(Verified {
        licensed: true,
        // No `maint` means updates never lapse. Reading it as 0 would refuse everything.
        updates_allowed: claims.maint.is_none_or(|m| build_date_unix <= m),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A REAL certificate, minted 2026-09-04 by the live public redeem door against the
    /// internal test licence `1d1f3d29-…` and signed by the production key. Kept verbatim
    /// so the parser, the base64url handling and the signature check are pinned against
    /// what Pay actually emits rather than against something hand-rolled here.
    ///
    /// Its claims: sub `ce12…cafe`, product ST2K, `maint` 1819977955 (2027-09-03),
    /// `ceil` null (the window is still open), `exp` 1791130974 (2026-10-04).
    const REAL_CERT: &str = concat!(
        "eyJhdWQiOiJjb25uZWN0aW9ucy1saWNlbmNlIiwibGljIjoiMWQxZjNkMjktODM5OS00YTY5LTk4ZTgt",
        "ZmI1Y2ViNWI1M2E2IiwicHJvZHVjdCI6IjI0NTQ0NDYxLTk1MzAtNGVkYi04NGU1LTRmMzQ3MTg3NmQ5",
        "OCIsInN1YiI6ImNlMTIwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAw",
        "MDAwMDAwMDAwMGNhZmUiLCJ1bml0IjoiaW5zdGFsbGF0aW9uIiwidGVybSI6InBlcnBldHVhbF9wbHVz",
        "X21haW50ZW5hbmNlIiwidW5pdHMiOjEsInNlYXQiOiJjMTE2YTZiMy04M2QzLTRjYzQtOTE3MC1kMmE4",
        "ZDUwNzg2NGEiLCJtYWludCI6MTgxOTk3Nzk1NSwiY2VpbCI6bnVsbCwiaWF0IjoxNzg4NTM4OTc0LCJl",
        "eHAiOjE3OTExMzA5NzR9.",
        "N15ul_HgXnzpmUohjFFKD5yIW5UwlNdR4t8E_bYjChPnsncvHCxoiB3ldyMDfwF28CHHq-ddB2NymBlx",
        "EH6jAg"
    );

    const REAL_SUB: &str = "ce1200000000000000000000000000000000000000000000000000000000cafe";
    /// Inside the fixture's window. Pinned, never `now()`: a test that passes until a
    /// date and then fails on its own is not a test.
    const INSIDE: i64 = 1_788_600_000;
    const MAINT: i64 = 1_819_977_955;

    #[test]
    fn a_real_certificate_verifies_and_licenses_this_machine() {
        let v = verify(REAL_CERT, REAL_SUB, INSIDE, INSIDE).expect("should verify");
        assert!(v.licensed);
        assert!(v.updates_allowed, "build inside the maintenance window");
    }

    #[test]
    fn a_build_past_the_maintenance_window_keeps_running_but_stops_updating() {
        let v = verify(REAL_CERT, REAL_SUB, INSIDE, MAINT + 1).expect("should verify");
        assert!(
            v.licensed,
            "perpetual: the licence survives the update window closing"
        );
        assert!(!v.updates_allowed, "past `maint`, no new builds");
    }

    #[test]
    fn a_build_exactly_on_the_maintenance_boundary_is_allowed() {
        let v = verify(REAL_CERT, REAL_SUB, INSIDE, MAINT).expect("should verify");
        assert!(v.updates_allowed, "`build_date <= maint`, inclusive");
    }

    #[test]
    fn another_machines_certificate_is_not_this_machines_licence() {
        assert_eq!(
            verify(REAL_CERT, "deadbeef", INSIDE, INSIDE),
            Err(CertError::NotThisMachine)
        );
    }

    #[test]
    fn an_expired_certificate_is_refused_and_asks_to_be_re_minted() {
        assert_eq!(
            verify(REAL_CERT, REAL_SUB, 1_791_130_975, INSIDE),
            Err(CertError::Expired)
        );
    }

    #[test]
    fn a_tampered_payload_fails_the_signature() {
        // Flip one character of the payload segment; the signature covers this text.
        let (p, s) = REAL_CERT.split_once('.').unwrap();
        let mut bad = p.to_string();
        bad.replace_range(0..1, if p.starts_with('e') { "f" } else { "e" });
        assert_eq!(
            verify(&format!("{bad}.{s}"), REAL_SUB, INSIDE, INSIDE),
            Err(CertError::BadSignature)
        );
    }

    #[test]
    fn a_tampered_signature_fails() {
        let (p, s) = REAL_CERT.split_once('.').unwrap();
        let mut bad = s.to_string();
        bad.replace_range(0..1, if s.starts_with('N') { "M" } else { "N" });
        assert_eq!(
            verify(&format!("{p}.{bad}"), REAL_SUB, INSIDE, INSIDE),
            Err(CertError::BadSignature)
        );
    }

    #[test]
    fn garbage_is_refused_without_panicking() {
        for junk in [
            "",
            ".",
            "no-dot-at-all",
            "a.b",
            "....",
            "eyJhIjoxfQ.short",
            "!!!.!!!",
        ] {
            let got = verify(junk, REAL_SUB, INSIDE, INSIDE);
            assert!(got.is_err(), "{junk:?} should not verify");
        }
    }

    #[test]
    fn a_padded_or_standard_base64_certificate_is_refused_rather_than_guessed() {
        // Pay emits UNPADDED base64url. Anything else is not a certificate we issued.
        let (p, s) = REAL_CERT.split_once('.').unwrap();
        assert!(verify(&format!("{p}=.{s}"), REAL_SUB, INSIDE, INSIDE).is_err());
    }
}
