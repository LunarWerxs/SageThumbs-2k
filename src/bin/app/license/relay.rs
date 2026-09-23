//! Talking to the licence relay: the URLs, the machine fingerprint and redeeming a key.

use super::*;

pub(crate) const RELAY_BASE: &str = "https://st2k.lunarwerx.com";

/// Where a licence is bought. A relay redirect rather than the checkout's own address on
/// purpose: the checkout is a Pay offer whose id changes with a repricing or a new product,
/// and this string is compiled into every copy ever shipped. The redirect moves; this does
/// not. Opened by the Licence page's Buy button and named in the business-nag notices.
///
/// ⛔ THIS ONE GOES TO THE PERPETUAL PRODUCT. Since 2026-09-16 the commercial licence is sold
/// two ways - US$49 once (this door) and US$2.99 a month - and they are two catalog products,
/// because `catalog_products.licence_term` is one value per product with a CHECK behind it, so
/// one product cannot be both. The monthly plan points at the SITE
/// (`sagethumbs.lunarwerx.com`), NOT at a relay route: the relay never grew a `/subscribe` twin
/// of this path, and because its catch-all answers an unknown path `200` with the sponsor
/// manifest rather than `404`, a URL published ahead of the route showed buyers raw JSON
/// (2026-09-17). The site's pricing card links BOTH plans straight at their hosted checkout
/// pages, so there is no redirect to keep in step with the catalog. It is deliberately NOT a
/// constant here: nothing in the app OPENS it. It is named as text by `licence_monthly_hint` and `licence_buy_pointer`,
/// because the Licence page's action row is three buttons by design (Michael, 2026-09-15) and
/// a fourth would undo that - the monthly plan gets the full-width prospect line instead. If a
/// button for it is ever wanted, add the constant then, not before.
pub(crate) const BUY_URL: &str = "https://st2k.lunarwerx.com/buy";

/// Where another 12 months of updates is bought (US$29), for a licence that is already
/// held. Like [`BUY_URL`] this is a relay redirect on the relay rather than the checkout's
/// own address (`/renew`, the twin of `/buy`), which forwards the query string, so a
/// repricing or a new checkout page moves the link without a release. The relay's default
/// target is the checkout page for [`crate::licence_cert::PRODUCT_ID`].
pub(super) const RENEW_URL: &str = "https://st2k.lunarwerx.com/renew";

/// The buyer's own self-serve door into the seat portal: paste the licence key, land in the
/// same roster a merchant-minted "Manage seats" link would open. This is Connections'
/// `SeatPortalClaimView.vue`, shared between enterprise.connections.icu and
/// licensing.connections.icu - closes the gap the 2026-09-11 finding named ("a buyer cannot
/// move their own licence to a new computer"): re-redeeming a 1-installation key on a new
/// machine is correctly refused (`redemption_count < max_redemptions`), and until now nothing
/// in this app ever pointed the buyer at the self-service door that already existed for it
/// (`POST /api/public/enterprise/seats/rebind`).
///
/// ⛔ NEVER append `?key=...`. Unlike a portal TOKEN, a licence key is the long-lived,
/// powerful credential, and the claim page's own contract is that it is typed into a form
/// field and POSTed, never carried in a URL/query string (same posture this module already
/// takes with the licence key elsewhere - see `renew_url`'s doc for why a *prefix* is never
/// substituted into a URL either). Opened bare; the buyer pastes their key on the page itself.
///
/// A relay redirect (`/claim`, like `/buy`) since 2026-09-18, when `connections.icu` was
/// suspended by its registry: the portal's host is a [vars] line on the relay now, so it can
/// move again without a release.
pub(crate) const PORTAL_CLAIM_URL: &str = "https://st2k.lunarwerx.com/claim";

/// The renewal link for this machine: the checkout page for our product, with the stored
/// licence key pre-filled when we have one.
///
/// The key comes from [`crate::cred_store`] (per-user, DPAPI-encrypted), NEVER from the
/// breadcrumb, which by contract holds only a display prefix - see that store's
/// `V_LICENCE_KEY` note. With no stored key the bare page is opened and the checkout asks
/// for it, which is a worse experience and a perfectly correct one; a prefix is never
/// substituted, because `?key=esk_A1B2` would look like a key and redeem nothing.
pub(crate) fn renew_url() -> String {
    match crate::cred_store::load_licence_key()
        .as_deref()
        .and_then(normalize_key)
    {
        Some(key) => format!("{RENEW_URL}?key={}", crate::http::form_enc(&key)),
        None => RENEW_URL.to_string(),
    }
}

/// Per-request timeout. The relay is a small Cloudflare Worker; 15 seconds is
/// generous for it and short enough that a dead network doesn't hang the Settings
/// window for a user who is just trying to close it.
pub(super) const RELAY_TIMEOUT_SECS: u64 = 15;

/// Wall-clock cap on one whole relay call. The per-request timeout above resets on every
/// partial read, so a peer trickling bytes could otherwise hold a worker thread open for
/// as long as it liked; past this the call is abandoned and reads as Offline.
pub(super) const RELAY_OVERALL_SECS: u64 = 30;

/// Response size cap. Every relay reply is a few bytes of JSON; 64 KiB is headroom,
/// not an expectation, the same defensive-cap idea as [`HISTORY_MAX_BYTES`].
pub(super) const RELAY_MAX_RESP_BYTES: usize = 64 * 1024;

/// The salt joined onto the machine's `MachineGuid` before hashing, so the relay
/// never sees (or could reverse-engineer) the raw Windows machine identifier, only a
/// value specific to this product. Not a secret - it is compiled into every copy of
/// the app - it exists to make the fingerprint a distinct namespace, not to be hidden.
pub(super) const FINGERPRINT_SALT: &str = "SageThumbs2K-seat-v1";

/// Where Windows keeps the per-machine install identifier. Readable by any user
/// (unlike most of HKLM\SOFTWARE\Microsoft\Cryptography's siblings), which is why the
/// design doc calls it out by name as the fingerprint source.
pub(super) const CRYPTOGRAPHY_KEY: &str = r"SOFTWARE\Microsoft\Cryptography";

/// SHA-256 of `data` via CNG's single-shot helper, raw 32 bytes. `None` on the
/// vanishingly unlikely CNG failure. The one copy: `oauth`'s PKCE challenge and
/// `update::verify`'s asset digest call it as `crate::license::sha256`.
pub(crate) fn sha256(data: &[u8]) -> Option<[u8; 32]> {
    use windows::Win32::Security::Cryptography::{BCryptHash, BCRYPT_SHA256_ALG_HANDLE};
    let mut out = [0u8; 32];
    let status = unsafe { BCryptHash(BCRYPT_SHA256_ALG_HANDLE, None, data, &mut out) };
    status.is_ok().then_some(out)
}

/// A stable-but-anonymous identifier for this machine: lowercase hex SHA-256 of the
/// registry's `MachineGuid` joined with [`FINGERPRINT_SALT`]. `None` only when the
/// value genuinely can't be read (locked-down machine, corrupt hive) - callers treat
/// that the same as any other network failure (see `redeem`/`refresh_entitlement`),
/// never as a reason to reject a key or deny an entitlement.
pub(crate) fn machine_fingerprint() -> Option<String> {
    let guid = windows_registry::LOCAL_MACHINE
        .open(CRYPTOGRAPHY_KEY)
        .and_then(|k| k.get_string("MachineGuid"))
        .ok()?;
    fingerprint_from_guid(&guid)
}

/// The pure half of [`machine_fingerprint`] - hashing, with the registry read
/// already done - so the hex-encoding and salting can be pinned in a test without
/// touching HKLM.
pub(super) fn fingerprint_from_guid(guid: &str) -> Option<String> {
    let digest = sha256(format!("{guid}{FINGERPRINT_SALT}").as_bytes())?;
    Some(st2k_base::hex::encode(&digest))
}

/// Turn whatever a human typed or pasted into the canonical `esk_XXXXX-XXXXX-XXXXX-
/// XXXXX` shape (uppercase groups), or `None` if it isn't a licence key. Trims
/// surrounding whitespace, ignores case and dashes anywhere in the body (so a key
/// copied without its group separators, or typed in lowercase, still normalizes),
/// and otherwise requires exactly the `esk_` prefix plus 20 alphanumeric characters -
/// no more, no fewer. Deliberately strict on the SHAPE: this only decides "does this
/// look like one of our keys", never whether it is valid, which is the relay's job.
pub(crate) fn normalize_key(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let lower = trimmed.to_ascii_lowercase();
    let rest = lower.strip_prefix("esk_")?;
    let body: String = rest.chars().filter(|&c| c != '-').collect();
    if body.len() != 20 || !body.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return None;
    }
    // The all-ASCII-alphanumeric check above means indexing by byte offset here can
    // never land inside a multi-byte character.
    let upper = body.to_ascii_uppercase();
    let groups = [&upper[0..5], &upper[5..10], &upper[10..15], &upper[15..20]];
    Some(format!("esk_{}", groups.join("-")))
}

/// The display prefix for a canonical key: `esk_` plus the first four body
/// characters, e.g. `esk_A1B2`. This is the ONLY form of a key this module ever
/// prints, logs, or shows - never the full key (see the module rules).
pub(crate) fn key_prefix(canonical: &str) -> String {
    let body = canonical.strip_prefix("esk_").unwrap_or(canonical);
    format!("esk_{}", &body[..body.len().min(4)])
}

/// What redeeming a key resulted in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RedeemOutcome {
    /// The relay accepted the key. `key_prefix` is the relay's own echo of it (never
    /// derived locally), so what the UI shows is exactly what the relay validated.
    Redeemed { key_prefix: String },
    /// The relay explicitly said no, with a human-readable reason to show.
    Rejected { message: String },
    /// No definite answer: bad local key shape aside, everything else here (no
    /// network, a 5xx, a response we don't understand) means "try again later," not
    /// "your key is wrong" - telling someone their real key is bad because a Worker
    /// hiccupped would be a worse failure than saying nothing.
    Offline,
}

/// Redeem a licence key against the relay. BLOCKING - run on a worker thread.
pub(crate) fn redeem(raw_key: &str) -> RedeemOutcome {
    let Some(canonical) = normalize_key(raw_key) else {
        // Never even ask the relay about something that isn't shaped like one of our
        // keys - this is a local formatting problem, not a validity question.
        return RedeemOutcome::Rejected {
            message: "That doesn't look like a SageThumbs 2K licence key.".to_string(),
        };
    };
    let Some(fingerprint) = machine_fingerprint() else {
        // Can't identify this machine, so there is no request to make. Failing
        // toward Offline (not Rejected) keeps a local read hiccup from ever reading
        // to the user as "your key is bad."
        return RedeemOutcome::Offline;
    };
    let body = match serde_json::to_vec(&json!({ "key": canonical, "subject": fingerprint })) {
        Ok(b) => b,
        Err(_) => return RedeemOutcome::Offline,
    };
    let url = format!("{RELAY_BASE}/license/redeem");
    let resp = crate::http::request_with_deadline(
        "POST",
        &url,
        "Content-Type: application/json",
        &body,
        RELAY_TIMEOUT_SECS,
        RELAY_OVERALL_SECS,
        RELAY_MAX_RESP_BYTES,
    );
    // The certificate rides along on the SAME response, so it costs no extra call. Read
    // separately from the outcome mapping rather than threaded through `RedeemOutcome`,
    // which would change a shape the Settings page and a dozen tests already match on.
    let (outcome, certificate) = match resp {
        Some(r) => (
            redeem_outcome_from_response(r.status, &r.body, &canonical),
            certificate_from_response(&r.body),
        ),
        None => (RedeemOutcome::Offline, None),
    };
    if let RedeemOutcome::Redeemed { key_prefix } = &outcome {
        // Keep the certificate before anything else: from here on this machine can prove
        // its own licence with no network, which is the whole point of having one.
        // Best-effort - a machine that cannot store it is exactly as licensed as before.
        if let Some(cert) = &certificate {
            let _ = crate::cred_store::save_licence_cert(cert);
        }
        // Keep the key too, in the SAME per-user DPAPI store, so the Renew button can hand
        // it to the checkout instead of making the customer dig out their purchase email.
        // This is the one place a full key is written and `cred_store::V_LICENCE_KEY` says
        // why it is there and not in the breadcrumb.
        let _ = crate::cred_store::save_licence_key(&canonical);
        let now = st2k_base::unixtime::now();
        update_history(|h| {
            h.was_business = true;
            h.last_status = "active".to_string();
            h.last_reason.clear();
            h.last_positive_unix = now;
            h.key_prefix = key_prefix.clone();
            // A fresh key ends any revocation clock; the evaluation clock is left as it
            // was, because `last_positive_unix > 0` retires it for good.
            h.revoked_unix = 0;
        });
        // A portable copy has no HKLM the installer could have written, so this is
        // the ONE store `read_mode` consults for it - see that function's docs.
        if st2k_base::settings::portable() {
            let _ = st2k_base::settings::set_string(MODE_VALUE, "business");
        }
    }
    outcome
}

/// Pull the offline certificate out of a redeem response, if it sent one.
///
/// Pure, and deliberately incurious: anything that is not a non-empty string is simply
/// absent. A relay that has not been taught to forward the certificate yet, an older
/// deployment, a truncated body - all of them mean "no certificate", which costs nothing
/// because the relay breadcrumb licenses the machine exactly as it did before.
pub(super) fn certificate_from_response(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<Value>(body)
        .ok()?
        .get("certificate")
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty())
        .map(String::from)
}

/// Map an HTTP status + raw body from `POST /license/redeem` to a [`RedeemOutcome`].
/// Pulled out of [`redeem`] so it can be driven from hand-written JSON in tests
/// exactly the way the network path drives it - no fingerprint, no WinINet, no clock.
pub(super) fn redeem_outcome_from_response(
    status: u16,
    body: &[u8],
    canonical: &str,
) -> RedeemOutcome {
    let json: Option<Value> = serde_json::from_slice(body).ok();
    if (200..300).contains(&status) {
        return redeem_outcome_ok(&json, canonical);
    }
    if status == 429 {
        // The relay's rate limit, not a verdict on the key: say so, rather than let a
        // paying customer read a busy office as a bad key.
        let message = json
            .as_ref()
            .and_then(|v| v.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("Too many attempts. Try again in a few minutes.")
            .to_string();
        return RedeemOutcome::Rejected { message };
    }
    if (400..500).contains(&status) {
        return match json {
            Some(v) if v.get("ok").and_then(Value::as_bool) == Some(false) => {
                let message = v
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("That key wasn't accepted.")
                    .to_string();
                RedeemOutcome::Rejected { message }
            }
            // An unparsable 4xx body is a relay/format surprise, not a confirmed
            // rejection - don't put words in the relay's mouth it never said.
            _ => RedeemOutcome::Offline,
        };
    }
    // 5xx, redirects we don't follow, anything else: Offline.
    RedeemOutcome::Offline
}

/// Map a 2xx `POST /license/redeem` response body to a [`RedeemOutcome`].
fn redeem_outcome_ok(json: &Option<Value>, canonical: &str) -> RedeemOutcome {
    match json
        .as_ref()
        .and_then(|v| v.get("ok"))
        .and_then(Value::as_bool)
    {
        Some(true) => match json
            .as_ref()
            .and_then(|v| v.get("keyPrefix"))
            .and_then(Value::as_str)
        {
            Some(prefix) if !prefix.is_empty() => RedeemOutcome::Redeemed {
                key_prefix: prefix.to_string(),
            },
            // The relay accepted the key but did not echo a prefix: the local
            // prefix of the key that was sent is the same value.
            _ => RedeemOutcome::Redeemed {
                key_prefix: key_prefix(canonical),
            },
        },
        _ => RedeemOutcome::Offline,
    }
}
