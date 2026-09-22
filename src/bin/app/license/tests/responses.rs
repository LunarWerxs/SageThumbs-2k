#![cfg(test)]

//! Parsing what the relay says: keys, redeem and check responses, ISO instants, certificates.

use super::*;

#[test]
fn normalize_key_accepts_the_canonical_shape_case_and_dash_insensitively() {
    let canonical = "esk_A1B2C-3D4E5-F6G7H-8I9J0";
    assert_eq!(normalize_key(canonical).as_deref(), Some(canonical));
    // Lowercase, no dashes at all.
    assert_eq!(
        normalize_key("esk_a1b2c3d4e5f6g7h8i9j0").as_deref(),
        Some(canonical)
    );
    // Uppercase prefix, mixed case body, dashes in different places.
    assert_eq!(
        normalize_key("ESK_a1B2c3D4e5-f6G7h8I9j0").as_deref(),
        Some(canonical)
    );
    // Leading/trailing whitespace, the way a copy-paste often arrives.
    assert_eq!(
        normalize_key("  esk_A1B2C-3D4E5-F6G7H-8I9J0  ").as_deref(),
        Some(canonical)
    );
}

#[test]
fn normalize_key_rejects_the_wrong_length_and_the_wrong_prefix() {
    assert_eq!(normalize_key(""), None, "empty");
    assert_eq!(
        normalize_key("esk_A1B2C-3D4E5-F6G7H-8I9J"),
        None,
        "one character short of 20"
    );
    assert_eq!(
        normalize_key("esk_A1B2C-3D4E5-F6G7H-8I9J00"),
        None,
        "one character over 20"
    );
    assert_eq!(
        normalize_key("xyz_A1B2C3D4E5F6G7H8I9J0"),
        None,
        "wrong prefix word"
    );
    assert_eq!(
        normalize_key("A1B2C3D4E5F6G7H8I9J0"),
        None,
        "no prefix at all"
    );
    assert_eq!(
        normalize_key("esk_A1B2C-3D4E5-F6G7H-8I9J!"),
        None,
        "a non-alphanumeric character in the body"
    );
}

#[test]
fn key_prefix_is_the_marker_plus_the_first_four_body_characters() {
    assert_eq!(key_prefix("esk_A1B2C-3D4E5-F6G7H-8I9J0"), "esk_A1B2");
}

#[test]
fn redeem_response_2xx_ok_true_with_a_key_prefix_is_redeemed() {
    assert_eq!(
        map_redeem(
            200,
            br#"{"ok":true,"status":"active","keyPrefix":"esk_A1B2"}"#
        ),
        RedeemOutcome::Redeemed {
            key_prefix: "esk_A1B2".to_string()
        }
    );
}

#[test]
fn redeem_response_4xx_ok_false_is_rejected_with_the_relays_message() {
    assert_eq!(
        map_redeem(
            400,
            br#"{"ok":false,"error":"invalid_key","message":"That key isn't recognized."}"#
        ),
        RedeemOutcome::Rejected {
            message: "That key isn't recognized.".to_string()
        }
    );
    assert_eq!(
        map_redeem(409, br#"{"ok":false,"error":"used"}"#),
        RedeemOutcome::Rejected {
            message: "That key wasn't accepted.".to_string()
        },
        "missing message falls back to a generic one, still Rejected"
    );
}

#[test]
fn redeem_response_everything_else_is_offline() {
    assert_eq!(
        map_redeem(500, b"internal error"),
        RedeemOutcome::Offline,
        "5xx"
    );
    assert_eq!(
        map_redeem(200, b"not json"),
        RedeemOutcome::Offline,
        "unparsable 2xx"
    );
    assert_eq!(
        map_redeem(400, b"not json"),
        RedeemOutcome::Offline,
        "unparsable 4xx - never invented as a rejection"
    );
    assert_eq!(
        map_redeem(200, br#"{"ok":true}"#),
        RedeemOutcome::Redeemed {
            key_prefix: "esk_A1B2".to_string()
        },
        "2xx with no keyPrefix falls back to the local prefix of the key sent"
    );
    assert_eq!(
        map_redeem(302, b""),
        RedeemOutcome::Offline,
        "a redirect status this module doesn't chase"
    );
}

#[test]
fn check_response_200_entitled_true_parses() {
    assert_eq!(
        parse_check_response(200, br#"{"ok":true,"entitled":true,"status":"active"}"#),
        Some(CheckResult {
            entitled: true,
            status: "active".to_string(),
            reason: None,
            maintenance_active: None,
            maint_unix: None,
        })
    );
}

#[test]
fn check_response_200_entitled_false_revoked_parses() {
    assert_eq!(
        parse_check_response(200, br#"{"ok":true,"entitled":false,"status":"revoked"}"#),
        Some(CheckResult {
            entitled: false,
            status: "revoked".to_string(),
            reason: None,
            maintenance_active: None,
            maint_unix: None,
        })
    );
}

/// The relay's `reason` rides along only in the shape the relay defines for it; anything
/// else is dropped, never stored, never shown.
#[test]
fn check_response_keeps_a_well_formed_reason_and_drops_the_rest() {
    let with = |body: &[u8]| parse_check_response(200, body).and_then(|r| r.reason);
    assert_eq!(
        with(br#"{"ok":true,"entitled":false,"status":"revoked","reason":"seat_revoked"}"#),
        Some("seat_revoked".to_string())
    );
    assert_eq!(
        with(br#"{"ok":true,"entitled":false,"status":"revoked","reason":"contract_ended"}"#),
        Some("contract_ended".to_string())
    );
    assert_eq!(
        with(br#"{"ok":true,"entitled":false,"status":"revoked","reason":"Seat Revoked!"}"#),
        None,
        "not a token"
    );
    assert_eq!(
        with(br#"{"ok":true,"entitled":false,"status":"revoked","reason":""}"#),
        None,
        "empty"
    );
    assert_eq!(
        with(br#"{"ok":true,"entitled":false,"status":"revoked","reason":7}"#),
        None,
        "wrong type"
    );
}

/// The 2026-09-10 maintenance fields, present and absent. ABSENT IS THE LOAD-BEARING
/// CASE: every relay deployment older than that date, and every Pay that does not answer
/// them, sends neither - and both must read as "no window on record" (`None`), which
/// leaves every build offered, rather than as a closed window, which would stop offering
/// updates to the entire installed base at once.
#[test]
fn check_response_reads_the_maintenance_window_and_tolerates_its_absence() {
    let with = |body: &[u8]| parse_check_response(200, body).expect("a 200 answer");

    let full = with(
        br#"{"ok":true,"entitled":true,"status":"active","maintenanceActive":true,"maintenanceEndsAt":"2027-09-03T00:00:00Z"}"#,
    );
    assert_eq!(full.maintenance_active, Some(true));
    assert_eq!(full.maint_unix, Some(1_819_929_600));

    let bare = with(br#"{"ok":true,"entitled":true,"status":"active"}"#);
    assert_eq!(bare.maintenance_active, None);
    assert_eq!(bare.maint_unix, None, "absent is unknown, never expired");

    // An explicit null, and a date we cannot parse, both mean the same thing.
    let nulled = with(
        br#"{"ok":true,"entitled":true,"status":"active","maintenanceActive":null,"maintenanceEndsAt":null}"#,
    );
    assert_eq!(nulled.maint_unix, None);
    let junk =
        with(br#"{"ok":true,"entitled":true,"status":"active","maintenanceEndsAt":"whenever"}"#);
    assert_eq!(junk.maint_unix, None);

    // A window that has closed is still a perfectly good answer, and says nothing about
    // whether the machine is entitled.
    let closed = with(
        br#"{"ok":true,"entitled":true,"status":"active","maintenanceActive":false,"maintenanceEndsAt":"2025-01-01"}"#,
    );
    assert!(closed.entitled, "a lapsed window never unlicenses");
    assert_eq!(closed.maintenance_active, Some(false));
    assert_eq!(closed.maint_unix, Some(1_735_689_600));
}

/// [`parse_iso_unix`] at the shapes the relay actually sends, and refusing the ones it
/// must not guess at. The reference values are the well-known Unix epochs for those
/// instants; a wrong civil-days implementation misses them by whole days.
#[test]
fn iso_instants_parse_and_ambiguous_ones_are_refused() {
    assert_eq!(parse_iso_unix("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(
        parse_iso_unix("2000-03-01"),
        Some(951_868_800),
        "leap-year rule"
    );
    assert_eq!(parse_iso_unix("2026-09-10T12:34:56Z"), Some(1_789_043_696));
    assert_eq!(
        parse_iso_unix("2026-09-10T12:34:56.789Z"),
        Some(1_789_043_696)
    );
    assert_eq!(parse_iso_unix(" 2026-09-10 12:34 "), Some(1_789_043_640));
    assert_eq!(parse_iso_unix("2026-09-10T12:34"), Some(1_789_043_640));

    // An offset other than Z is REFUSED rather than silently read as UTC: this value
    // decides whether a customer is offered a build, and guessing hours is worse than
    // answering "no window on record".
    assert_eq!(parse_iso_unix("2026-09-10T12:34:56+02:00"), None);
    assert_eq!(parse_iso_unix("2026-09-10T12:34:56-05:00"), None);
    // ...including after a fraction, which used to be cut off WITH the offset behind it.
    assert_eq!(parse_iso_unix("2026-09-10T12:34:56.789+02:00"), None);
    assert_eq!(parse_iso_unix("2026-09-10T12:34:56.5-05:00"), None);

    assert_eq!(parse_iso_unix(""), None);
    assert_eq!(parse_iso_unix("whenever"), None);
    assert_eq!(parse_iso_unix("2026-13-01"), None, "month out of range");
    assert_eq!(
        parse_iso_unix("2026-09-10T25:00:00Z"),
        None,
        "hour out of range"
    );
    assert_eq!(
        parse_iso_unix("1969-12-31"),
        None,
        "before the epoch has no u64"
    );
}

/// The window is recorded on EVERY understood answer and is a separate fact from the
/// entitlement: a relay that stops saying `entitled` must not also wipe the date the
/// Licence page shows, and an answer carrying no window must leave what is on record
/// alone rather than clearing it.
#[test]
fn a_check_persists_the_maintenance_window_without_touching_the_licence() {
    let dir = temp_dir("maint");
    let path = dir.join("history.json");
    let _ = std::fs::remove_file(&path);
    let now = 1_760_000_000u64;

    apply_check_response(
        &path,
        now,
        200,
        br#"{"ok":true,"entitled":true,"status":"active","maintenanceEndsAt":"2027-09-03T00:00:00Z"}"#,
    )
    .expect("an entitled answer");
    let after = read_history(&path).expect("history");
    assert_eq!(after.maint_unix, 1_819_929_600);
    assert_eq!(after.last_positive_unix, now);

    // A later answer with no window at all leaves the recorded one standing.
    apply_check_response(
        &path,
        now + 60,
        200,
        br#"{"ok":true,"entitled":true,"status":"active"}"#,
    )
    .expect("a second answer");
    assert_eq!(
        read_history(&path).expect("history").maint_unix,
        1_819_929_600,
        "silence about the window is not news about the window"
    );

    // And a CLOSED window still leaves the machine licensed - the one property this
    // whole feature must never break.
    let ent = apply_check_response(
        &path,
        now + 120,
        200,
        br#"{"ok":true,"entitled":true,"status":"active","maintenanceActive":false,"maintenanceEndsAt":"2025-01-01"}"#,
    )
    .expect("a third answer");
    assert_eq!(
        ent,
        Entitlement::Licensed,
        "a lapsed window never unlicenses"
    );
    assert_eq!(
        read_history(&path).expect("history").maint_unix,
        1_735_689_600
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn check_response_failures_are_none() {
    assert_eq!(parse_check_response(500, b""), None, "5xx");
    assert_eq!(parse_check_response(200, b"not json"), None, "unparsable");
    assert_eq!(
        parse_check_response(200, br#"{"ok":false}"#),
        None,
        "ok:false"
    );
    assert_eq!(
        parse_check_response(200, br#"{"ok":true,"status":"active"}"#),
        None,
        "missing entitled"
    );
}

#[test]
fn fingerprint_from_guid_is_deterministic_lowercase_hex() {
    let a = fingerprint_from_guid("11111111-2222-3333-4444-555555555555").unwrap();
    let b = fingerprint_from_guid("11111111-2222-3333-4444-555555555555").unwrap();
    let c = fingerprint_from_guid("00000000-0000-0000-0000-000000000000").unwrap();
    assert_eq!(a, b, "same GUID hashes the same every time");
    assert_ne!(a, c, "different GUIDs must not collide");
    assert_eq!(a.len(), 64, "SHA-256 as lowercase hex is 64 characters");
    assert!(a
        .bytes()
        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
}

#[test]
fn a_redeem_response_carrying_a_certificate_yields_it() {
    let body = br#"{"ok":true,"keyPrefix":"esk_A1B2","certificate":"aGVhZGVy.c2ln"}"#;
    assert_eq!(
        certificate_from_response(body).as_deref(),
        Some("aGVhZGVy.c2ln")
    );
}

#[test]
fn a_redeem_response_without_one_is_simply_absent() {
    // A relay that has not been taught to forward it yet, an empty string, a body
    // that is not JSON at all: every one of these means "no certificate", never an
    // error, because the breadcrumb licenses the machine exactly as it did before.
    for body in [
        br#"{"ok":true,"keyPrefix":"esk_A1B2"}"#.as_slice(),
        br#"{"ok":true,"certificate":""}"#.as_slice(),
        br#"{"ok":true,"certificate":null}"#.as_slice(),
        br#"{"ok":true,"certificate":123}"#.as_slice(),
        b"not json at all".as_slice(),
        b"".as_slice(),
    ] {
        assert_eq!(certificate_from_response(body), None);
    }
}
