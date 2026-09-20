#![cfg(test)]

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
    for portable in [
        "EnableThumbs",
        "MenuOrder",
        "Lang",
        "ScreenshotHotkey",
        "JPEG",
    ] {
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
    // Only off-list / wrong-typed entries → nothing applies, and a rejected value is
    // not a failed write: the result is a clean zero, never an error.
    assert_eq!(apply_remote(&doc), Ok(0));
}

/// Audit concern 8 (2026-09-19): a value we ACCEPTED from the remote document and then
/// could not write locally (a read-only ini, a registry key the account cannot write)
/// is a sync error that names the key - `sync_once` used to wrap the count of
/// successful writes in `Ok`, so a store that took nothing reported a clean sync.
#[test]
fn apply_remote_reports_a_failed_local_write_as_an_error() {
    let doc = serde_json::json!({ "JPEG": 85, "ShotSaveDir": "ignored (off-list)" });
    let refused = |_: &str, _: u32| Err("access is denied".to_string());
    let never =
        |_: &str, _: &str| -> Result<(), String> { panic!("no string value is on the doc") };
    let err = apply_remote_with(&doc, refused, never).unwrap_err();
    assert!(err.contains("JPEG"), "names the key: {err}");
    assert!(
        err.contains("access is denied"),
        "carries the store's reason: {err}"
    );
    assert!(err.contains("0 were"), "says how many did land: {err}");

    // The same document against a store that accepts is a plain count.
    let ok = |_: &str, _: u32| Ok(());
    assert_eq!(apply_remote_with(&doc, ok, never), Ok(1));
}

/// A partial failure still names only the keys that failed and counts the rest, and it
/// keeps going past the first refusal rather than stopping - the store failing for one
/// key says nothing about the next.
#[test]
fn apply_remote_keeps_applying_after_one_refused_write_and_names_each_failure() {
    // Two on-list DWORD keys with distinct values; refuse exactly one of them.
    let (a, b) = {
        let mut dwords = ALLOW.iter().filter(|(_, k)| matches!(k, Kind::Dword));
        (dwords.next().unwrap().0, dwords.next().unwrap().0)
    };
    let doc = serde_json::json!({ a: 1, b: 2 });
    let refuse_b = move |name: &str, _: u32| {
        if name == b {
            Err("read-only".to_string())
        } else {
            Ok(())
        }
    };
    let never = |_: &str, _: &str| -> Result<(), String> { unreachable!() };
    let err = apply_remote_with(&doc, refuse_b, never).unwrap_err();
    assert!(err.contains(b) && !err.contains(&format!("{a} (")), "{err}");
    assert!(err.contains("1 were"), "{err}");
}

#[test]
fn apply_remote_rejects_out_of_range_dword_instead_of_truncating() {
    // 4294967296 (2^32) is a valid JSON number and a valid u64, but doesn't fit a u32.
    // The old `as u32` cast wrapped it to 0 and still counted it as applied; `try_from`
    // must reject it instead, so nothing is written and the count stays 0.
    let doc = serde_json::json!({ "JPEG": 4294967296u64 });
    assert_eq!(apply_remote(&doc), Ok(0));
}

#[test]
fn credential_values_are_refused_even_when_nested() {
    let doc = serde_json::json!({
        "future": {
            "token": "ghp_abcdefghijklmnopqrstuvwxyz0123456789"
        }
    });
    let error = validate_sync_snapshot(&doc).unwrap_err();
    assert!(error.contains("GitHub token"), "{error}");
    assert!(validate_sync_snapshot(&serde_json::json!({"Lang": "sk"})).is_ok());
}

#[test]
fn oversized_snapshots_are_rejected_locally() {
    let doc = serde_json::json!({"MenuOrder": "x".repeat(MAX_DOCUMENT_BYTES)});
    let error = validate_sync_snapshot(&doc).unwrap_err();
    assert!(error.contains("over the 65536-byte limit"), "{error}");
    assert!(error.contains("MenuOrder"), "{error}");
}

#[test]
fn etag_and_rate_limit_contract_fields_are_parsed() {
    assert_eq!(parse_etag_version("\"42\""), Some(42));
    assert_eq!(parse_etag_version("W/\"7\""), Some(7));
    assert_eq!(
        retry_after_seconds(br#"{"retry_after_seconds":9}"#),
        Some(9)
    );
}

/// A037: a rotated refresh token the server already accepted must never be silently
/// discarded just because the local DPAPI/HKCU write failed.
#[test]
fn persist_rotation_errors_when_a_rotated_token_fails_to_save() {
    let result = persist_rotation(Some("new-token"), |_| false);
    assert!(
        result.is_err(),
        "a failed save of a rotated token must surface an error, not vanish silently"
    );
}

#[test]
fn persist_rotation_succeeds_when_the_store_accepts_the_rotated_token() {
    assert!(persist_rotation(Some("new-token"), |_| true).is_ok());
}

#[test]
fn persist_rotation_is_a_no_op_when_the_server_did_not_rotate_the_token() {
    // `save` must not even be called when nothing rotated.
    let result = persist_rotation(None, |_| panic!("save must not run without a rotation"));
    assert!(result.is_ok());
}

// ---- the classification guard ------------------------------------------------------

/// The settings module's source, verbatim, embedded at COMPILE time: the hub and its three
/// children (`settings.rs` was split into `settings/{store,thumbs,app_prefs}.rs` on
/// 2026-09-08, and `the_scan_actually_finds_settings` went red the moment the scan still
/// read only the hub, which is exactly the rot that test exists to catch; a new child
/// module must be listed here or its keys are invisible to `every_setting_is_classified`).
///
/// Reading from disk at runtime would make this test depend on the working directory,
/// which differs between `cargo test`, the CI job and a packaged run. `include_str!` resolves
/// relative to THIS file, so each path is checked by the compiler and cannot silently miss.
const SETTINGS_SRC: &[&str] = &[
    include_str!("../../../settings.rs"),
    include_str!("../../../settings/store.rs"),
    include_str!("../../../settings/thumbs.rs"),
    include_str!("../../../settings/app_prefs.rs"),
];

/// Every setting name the settings module reads or writes.
///
/// Deliberately a dumb scan for `…("Name"` after one of the registry accessors, rather than
/// anything clever: a clever matcher that stops matching is indistinguishable from a repo
/// with nothing left to classify, and `the_scan_actually_finds_settings` below is what keeps
/// that from rotting into a no-op.
fn settings_in_source() -> Vec<String> {
    const ACCESSORS: &[&str] = &[
        "get_dword(",
        "get_dword_opt(",
        "set_dword(",
        "remove_dword(",
        "set_dword_tracking_default(",
        "get_string_opt(",
        "set_string(",
    ];
    let mut names = Vec::new();
    for (accessor, src) in ACCESSORS
        .iter()
        .flat_map(|a| SETTINGS_SRC.iter().map(move |s| (a, *s)))
    {
        let mut rest = src;
        while let Some(at) = rest.find(accessor) {
            rest = &rest[at + accessor.len()..];
            let trimmed = rest.trim_start();
            let Some(body) = trimmed.strip_prefix('"') else {
                continue; // a variable, not a literal — nothing to classify from here
            };
            let Some(end) = body.find('"') else { continue };
            let name = &body[..end];
            if !name.is_empty()
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && !names.iter().any(|seen: &String| seen == name)
            {
                names.push(name.to_string());
            }
        }
    }
    names.sort();
    names
}

/// A settings key must be either synced or deliberately not — never merely forgotten.
///
/// This is the check the 2026-08-25 widening was really about. `ALLOW` was written when the
/// app had 33 settings and every one added afterwards defaulted to "does not sync", silently,
/// including the entire Quick preview viewer. Nothing was ever red about it. Now a new
/// setting fails here until someone files it into one list or the other on purpose.
#[test]
fn every_setting_is_classified() {
    let allowed: Vec<&str> = ALLOW.iter().map(|(k, _)| *k).collect();
    let unclassified: Vec<String> = settings_in_source()
        .into_iter()
        .filter(|k| !allowed.contains(&k.as_str()) && !NEVER_SYNCED.contains(&k.as_str()))
        .collect();
    assert!(
        unclassified.is_empty(),
        "these settings sync neither way — add each to ALLOW or NEVER_SYNCED: {unclassified:?}"
    );
}

/// The guard above is worth nothing if the scan silently finds nothing, so pin that it works.
#[test]
fn the_scan_actually_finds_settings() {
    let found = settings_in_source();
    assert!(
        found.len() > 50,
        "the settings scan found only {} names — the accessor list or the file shape changed",
        found.len()
    );
    for expected in ["EnableThumbs", "ShotSaveDir", "PreviewEnabled", "Lang"] {
        assert!(
            found.iter().any(|k| k == expected),
            "the scan missed {expected}, so it is no longer reading the settings sources correctly"
        );
    }
}

/// A key on both lists would make its exclusion meaningless, and nothing else would notice.
#[test]
fn no_setting_is_on_both_lists() {
    for (key, _) in ALLOW {
        assert!(
            !NEVER_SYNCED.contains(key),
            "{key} is in ALLOW and NEVER_SYNCED at the same time"
        );
    }
}

/// F06: the marker must actually CLEAR. `clear` used to go through a read-only
/// `Key::open`, on which `remove_value` fails silently, so a successful push left the
/// marker set for good and every later Settings open re-pushed. Round-trips a scratch
/// name so the developer's real marker is never touched, and leaves nothing behind.
#[test]
fn the_pending_marker_clears_after_a_successful_push() {
    let name = format!("ConnectionsSyncPendingTest{}", std::process::id());
    clear_marker(&name);
    assert!(!marker_set(&name), "a never-set marker reads as clear");
    set_marker(&name);
    assert!(marker_set(&name), "mark must read back");
    clear_marker(&name);
    assert!(
        !marker_set(&name),
        "clear must DELETE the value, not fail silently on a read-only key"
    );
}

/// F06, the actual clearing condition: `finish_push_worker` only clears the pending
/// marker when the push that just finished both succeeded AND was the last outstanding
/// worker. `the_pending_marker_clears_after_a_successful_push` above only round-trips
/// `set_marker`/`clear_marker` directly and never calls `begin_push_worker`/
/// `finish_push_worker` at all, so it would stay green even if the `success &&
/// remaining == 0` guard were removed or inverted. This drives the worker-accounting
/// helpers themselves, against a scratch counter and a scratch marker name so the real
/// `PUSH_WORKERS` static and the real marker are never touched.
#[test]
fn finish_push_worker_only_clears_the_marker_on_a_successful_last_finish() {
    let name = format!("ConnectionsSyncPendingWorkerTest{}", std::process::id());
    let counter = AtomicUsize::new(0);
    clear_marker(&name);

    // A single worker that fails must leave the marker set for retry.
    set_marker(&name);
    begin_push_worker_on(&counter);
    finish_push_worker_on(&counter, &name, false);
    assert!(
        marker_set(&name),
        "a failed push must not clear the pending marker"
    );

    // A fresh single worker finishing successfully must clear it.
    begin_push_worker_on(&counter);
    finish_push_worker_on(&counter, &name, true);
    assert!(
        !marker_set(&name),
        "the last outstanding worker succeeding must clear the pending marker"
    );

    // Two outstanding workers: the first to finish (even successfully) must not clear
    // the marker while the other is still outstanding.
    set_marker(&name);
    begin_push_worker_on(&counter);
    begin_push_worker_on(&counter);
    finish_push_worker_on(&counter, &name, true);
    assert!(
        marker_set(&name),
        "the marker must stay set while another worker is still outstanding"
    );
    finish_push_worker_on(&counter, &name, true);
    assert!(
        !marker_set(&name),
        "the marker must clear once the last outstanding worker finishes successfully"
    );

    clear_marker(&name);
}

/// The marker is state, not a preference: the settings export/import consults this to
/// leave it alone, case-insensitively like every registry value name.
#[test]
fn the_pending_marker_is_classified_as_sync_state() {
    assert!(is_sync_state_value(PENDING_VALUE));
    assert!(is_sync_state_value("connectionssyncpending"));
    assert!(!is_sync_state_value("Theme"));
    assert!(!is_sync_state_value("ConnectionsSyncPendingX"));
}

// ---- F16: initial-sync-pending state (2026-09-05 audit) ----------------------------

/// The new marker round-trips through the same name-parameterised primitives the
/// push-pending marker already proved (`the_pending_marker_clears_after_a_successful_push`
/// above), a scratch name, never the real `INITIAL_SYNC_PENDING_VALUE` key, so the
/// developer's real sync state is never touched.
#[test]
fn the_initial_sync_pending_marker_round_trips() {
    let name = format!("ConnectionsInitialSyncPendingTest{}", std::process::id());
    clear_marker(&name);
    assert!(!marker_set(&name), "a never-set marker reads as clear");
    set_marker(&name);
    assert!(marker_set(&name), "mark must read back");
    clear_marker(&name);
    assert!(!marker_set(&name), "clear must delete the value");
}

#[test]
fn the_initial_sync_pending_marker_is_classified_as_sync_state() {
    assert!(is_sync_state_value(INITIAL_SYNC_PENDING_VALUE));
    assert!(is_sync_state_value("connectionsinitialsyncpending"));
}

#[test]
fn connect_outcome_is_synced_when_the_initial_sync_succeeds() {
    match connect_outcome("Ann".to_string(), Ok(())) {
        ConnectOutcome::Synced { label } => assert_eq!(label, "Ann"),
        ConnectOutcome::InitialSyncPending { .. } => {
            panic!("a successful initial sync must not read as pending")
        }
    }
}

/// This is the exact contradiction the 2026-09-05 audit (F16) found: `connect()` saves
/// the refresh token and identity BEFORE the initial GET/seed, then a failure there
/// used to propagate as a plain `Err`, indistinguishable from a failed login, even
/// though the credential was already durably stored. Against the pre-fix `connect`,
/// there was no `ConnectOutcome` at all (the return type was `Result<String, String>`),
/// so this test could not even compile, let alone pass: it fails against that shape and
/// passes once a failed initial sync is reported as `InitialSyncPending` rather than a
/// bare error that looks like "sign-in failed".
#[test]
fn connect_outcome_is_initial_sync_pending_when_the_initial_sync_fails() {
    let outcome = connect_outcome("Ann".to_string(), Err("network unreachable".to_string()));
    match outcome {
        ConnectOutcome::InitialSyncPending { label, error } => {
            assert_eq!(label, "Ann");
            assert_eq!(error, "network unreachable");
        }
        ConnectOutcome::Synced { .. } => {
            panic!("a failed initial sync must not silently read as fully synced")
        }
    }
}

// ---- E05: the offline classification marker + the named retry bounds ---------------

/// Same shape as `the_initial_sync_pending_marker_round_trips` above: exercises the
/// real `set_marker`/`clear_marker`/`marker_set` primitives `mark_offline`/
/// `clear_offline`/`last_attempt_was_offline` are thin wrappers over, via a scratch
/// name so the developer's real offline flag is never touched.
#[test]
fn the_offline_marker_round_trips() {
    let name = format!("ConnectionsLastAttemptOfflineTest{}", std::process::id());
    clear_marker(&name);
    assert!(!marker_set(&name), "a never-set marker reads as clear");
    set_marker(&name);
    assert!(marker_set(&name), "mark must read back");
    clear_marker(&name);
    assert!(!marker_set(&name), "clear must delete the value");
}

#[test]
fn the_offline_marker_is_classified_as_sync_state() {
    assert!(is_sync_state_value(OFFLINE_VALUE));
    assert!(is_sync_state_value("connectionslastattemptoffline"));
    assert!(!is_sync_state_value(OFFLINE_VALUE.trim_end_matches('e')));
}

/// The retry bounds are supposed to be STATED facts, not accidents of a hardcoded
/// literal buried in a loop, pin the exact numbers so a future edit that quietly
/// changes one shows up as a diff to a named test, not a silent behavior change.
#[test]
fn the_named_retry_bounds_have_the_documented_values() {
    assert_eq!(MAX_PUSH_TRANSIENT_RETRIES, 2);
    assert_eq!(MAX_PUSH_CONFLICT_RETRIES, 3);
    assert_eq!(MAX_PUSH_RATE_LIMIT_RETRIES, 1);
    assert_eq!(MAX_RATE_LIMIT_WAIT, Duration::from_secs(30));
}

/// The exact invariant `finish_push_worker` relies on ("success clears the marker,
/// failure never does"), driven through the REAL [`begin_push_worker_on`]/
/// [`finish_push_worker_on`] primitives (E05 follow-up audit, review item 4c) against a
/// throwaway counter and marker name, rather than mirroring their conditional in the
/// test itself. Against the pre-split shape (a bare `if success && remaining == 0`
/// inline in `finish_push_worker`, addressing `PENDING_VALUE` unconditionally) there was
/// no name-parameterised function to call here at all.
#[test]
fn a_failed_push_never_clears_the_pending_marker_only_a_successful_one_does() {
    let name = format!("ConnectionsSyncPendingMirrorTest{}", std::process::id());
    let counter = AtomicUsize::new(0);
    clear_marker(&name);
    set_marker(&name); // mirrors `mark_push_pending()` after Save

    begin_push_worker_on(&counter);
    finish_push_worker_on(&counter, &name, false);
    assert!(
        marker_set(&name),
        "a failed push must leave the retry marker set for the next Settings open"
    );

    begin_push_worker_on(&counter);
    finish_push_worker_on(&counter, &name, true);
    assert!(
        !marker_set(&name),
        "a successful push with no other worker in flight must clear it"
    );
}

#[test]
fn disconnect_outcome_variants_are_distinguishable() {
    assert_ne!(
        DisconnectOutcome::CloudCopyDeleted,
        DisconnectOutcome::CloudCopyKept
    );
    assert_ne!(
        DisconnectOutcome::WasNotSignedIn,
        DisconnectOutcome::CloudCopyKept
    );
}

/// E05 follow-up audit, review item 2: the exact bug, a signed-in machine whose token
/// refresh failed (offline, or the sign-in server rejected it) used to report
/// `WasNotSignedIn`, the same "nothing to delete" wording as a machine that was never
/// signed in at all, even though the cloud copy was never touched. Against the pre-fix
/// `disconnect` (no `was_signed_in` capture, `Err(_) => WasNotSignedIn` unconditionally)
/// this is exactly the case that read wrong.
#[test]
fn a_signed_in_machine_whose_token_mint_failed_keeps_the_cloud_copy_not_was_not_signed_in() {
    assert_eq!(
        disconnect_outcome(true, None),
        DisconnectOutcome::CloudCopyKept,
        "signed in, but no token to delete with, must not read as never-signed-in"
    );
}

#[test]
fn a_machine_that_was_never_signed_in_reports_was_not_signed_in() {
    assert_eq!(
        disconnect_outcome(false, None),
        DisconnectOutcome::WasNotSignedIn
    );
}

#[test]
fn a_successful_delete_reports_cloud_copy_deleted_regardless_of_prior_state() {
    assert_eq!(
        disconnect_outcome(true, Some(Ok(()))),
        DisconnectOutcome::CloudCopyDeleted
    );
}

/// E05 follow-up audit, review item 1: the exact bug, every sync entry point called
/// `access_token()`, which on a dead network returned `Err(...)` WITHOUT ever calling
/// `mark_offline()`, so a dead network plus Save rendered `SavedLocally`, never
/// `Offline`. Against the pre-fix `access_token` (a bare `oauth::refresh(&rt)?` with no
/// classification at all) there was no such mapping to call here.
#[test]
fn an_unreachable_refresh_classifies_as_offline_with_the_reach_error_text() {
    assert_eq!(
        classify_refresh_error(oauth::RefreshOutcome::Unreachable),
        (true, "couldn't reach the sign-in server".to_string())
    );
}

/// The other half of item 1: ANY response, including a rejection, must clear the
/// offline classification, because a server that answered "no" is not the same failure
/// as a server nobody could reach.
#[test]
fn a_rejected_refresh_classifies_as_not_offline_with_the_servers_own_message() {
    assert_eq!(
        classify_refresh_error(oauth::RefreshOutcome::Rejected(
            "bad refresh token".to_string()
        )),
        (false, "bad refresh token".to_string())
    );
}

#[test]
fn a_failed_delete_attempt_keeps_the_cloud_copy() {
    assert_eq!(
        disconnect_outcome(true, Some(Err("sync failed (HTTP 500)".to_string()))),
        DisconnectOutcome::CloudCopyKept
    );
}

// ---- E05 follow-up audit: fake-server tests driving the REAL push_snapshot loop -----

/// A GET response for `store_get` carrying the given version and empty settings, plus
/// however many scripted responses the caller queues after it, every scenario below
/// starts with `push_snapshot` fetching a base version before its retry loop even begins.
fn script_base_version(version: u64) {
    script_response(Some((
        200,
        format!(r#"{{"version":{version},"settings":{{}}}}"#).into_bytes(),
    )));
}

fn conflict_body(current_version: u64) -> Vec<u8> {
    format!(r#"{{"current":{{"version":{current_version}}}}}"#).into_bytes()
}

/// Item 4a(i): a 409 is retried up to the bound, then the push gives up. Also pins the
/// fix for item 5, `MAX_PUSH_CONFLICT_RETRIES` (3) must mean three RETRIES (four total
/// conflict responses before giving up), not two. Against the pre-fix `conflicts >= 3`
/// checked AFTER incrementing, this test's fourth queued 409 would never be reached ,
/// the push gave up on the THIRD one instead.
#[test]
fn push_retries_a_409_conflict_up_to_the_bound_then_gives_up() {
    reset_test_network_state();
    script_base_version(1);
    for v in 2..=5 {
        script_response(Some((409, conflict_body(v))));
    }
    let err = push_snapshot("tok").unwrap_err();
    assert!(
        err.contains("kept conflicting"),
        "must give up with the conflict message, got {err:?}"
    );
}

/// The other half of item 5: exactly `MAX_PUSH_CONFLICT_RETRIES` conflicts, then a
/// success, must succeed, proving the bound really does grant that many retries rather
/// than one fewer.
#[test]
fn push_succeeds_after_exactly_the_conflict_bound_worth_of_retries() {
    reset_test_network_state();
    script_base_version(1);
    for v in 2..=(1 + u64::from(MAX_PUSH_CONFLICT_RETRIES)) {
        script_response(Some((409, conflict_body(v))));
    }
    script_response(Some((200, br#"{"version":99}"#.to_vec())));
    assert_eq!(push_snapshot("tok"), Ok(99));
}

/// Item 4a(ii): a 429 honours the relay's `retry_after_seconds`, capped by
/// `MAX_RATE_LIMIT_WAIT`, asserted on the computed wait via [`recorded_sleeps`], never
/// by actually sleeping.
#[test]
fn push_honours_retry_after_capped_by_the_rate_limit_ceiling() {
    reset_test_network_state();
    script_base_version(1);
    script_response(Some((429, br#"{"retry_after_seconds":9999}"#.to_vec())));
    script_response(Some((200, br#"{"version":2}"#.to_vec())));
    assert_eq!(push_snapshot("tok"), Ok(2));
    assert_eq!(
        recorded_sleeps(),
        vec![MAX_RATE_LIMIT_WAIT],
        "an outrageous retry_after_seconds must be capped, not honoured verbatim"
    );
}

#[test]
fn push_honours_retry_after_when_it_is_under_the_cap() {
    reset_test_network_state();
    script_base_version(1);
    script_response(Some((429, br#"{"retry_after_seconds":5}"#.to_vec())));
    script_response(Some((200, br#"{"version":2}"#.to_vec())));
    assert_eq!(push_snapshot("tok"), Ok(2));
    assert_eq!(recorded_sleeps(), vec![Duration::from_secs(5)]);
}

/// Item 4a(iii): a 5xx is retried up to `MAX_PUSH_TRANSIENT_RETRIES`, then gives up.
#[test]
fn push_retries_a_5xx_up_to_the_transient_bound_then_gives_up() {
    reset_test_network_state();
    script_base_version(1);
    for _ in 0..=MAX_PUSH_TRANSIENT_RETRIES {
        script_response(Some((503, b"{}".to_vec())));
    }
    let err = push_snapshot("tok").unwrap_err();
    assert!(
        err.contains("sync failed"),
        "must give up with the store's own error mapping, got {err:?}"
    );
}

/// Item 4a(iv): no response at all (`None`) is retried as a transient failure and, once
/// the bound is exhausted, `push_snapshot` reports the SAME "couldn't reach" wording
/// `mark_offline` is paired with everywhere else in this module (see the
/// `store_get`/`store_delete` branches, and `access_token`'s own classification test) -
/// deliberately NOT asserted here via the real `last_attempt_was_offline()` marker, so
/// this test never writes to this machine's actual registry/portable-ini state.
#[test]
fn push_gives_up_with_the_offline_wording_after_exhausting_transient_retries_on_no_response() {
    reset_test_network_state();
    script_base_version(1);
    for _ in 0..=MAX_PUSH_TRANSIENT_RETRIES {
        script_response(None);
    }
    let err = push_snapshot("tok").unwrap_err();
    assert_eq!(err, "couldn't reach the sync server");
}
