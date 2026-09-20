use super::*;

/// 2026-09-19 audit F07, the connect -> pull -> Save round trip. Runs the body in a child
/// copy of this test binary with `ST2K_PORTABLE_INI` pointing at a scratch file, because
/// the settings store's backing is resolved once per process and the parent process
/// must never write to the developer's real HKCU.
#[test]
fn a_pull_delivered_by_connect_reaches_the_controls_before_save_can_overwrite_it() {
    const MARK: &str = "ST2K_TEST_CONNECT_PULL_SAVE";
    if std::env::var_os(MARK).is_none() {
        let dir = std::env::temp_dir().join(format!("st2k_f07_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ini = dir.join("SageThumbs2K.ini");
        std::fs::write(&ini, "[Settings]\r\nJPEG=70\r\n").unwrap();
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "settings_dlg::sync::tests::a_pull_delivered_by_connect_reaches_the_controls_before_save_can_overwrite_it",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(MARK, "1")
            .env("ST2K_PORTABLE_INI", &ini)
            .output()
            .expect("spawn the child test");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success() && stdout.contains("test result: ok. 1 passed"),
            "child failed:\n{stdout}\n{stderr}"
        );
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }

    assert!(settings::portable(), "the child must be on the scratch ini");
    assert_eq!(settings::jpeg_quality(), 70, "the seeded pre-pull value");
    unsafe {
        let hinst: HINSTANCE = windows::Win32::System::LibraryLoader::GetModuleHandleW(None)
            .unwrap()
            .into();
        let hwnd = super::super::shot::build_settings_shot_window(hinst, false)
            .expect("headless settings window");
        let mut ok = windows::core::BOOL::default();
        assert_eq!(GetDlgItemInt(hwnd, ID_JPEG, Some(&mut ok), false), 70);

        // The pull lands in the store while the dialog is open - what `connect` /
        // `retry_initial_sync` do through `apply_remote` - and the control still shows 70.
        settings::set_dword("JPEG", 42).unwrap();
        assert_eq!(GetDlgItemInt(hwnd, ID_JPEG, Some(&mut ok), false), 70);

        // The regression: before the fix a Save here wrote 70 back over the pull.
        on_connected_synced(hwnd);
        assert_eq!(
            GetDlgItemInt(hwnd, ID_JPEG, Some(&mut ok), false),
            42,
            "Connected(Synced) must reload the controls from the pulled store"
        );
        super::super::values::apply_tuning_numbers(hwnd);
        assert_eq!(settings::jpeg_quality(), 42, "Save kept the pulled value");
        let _ = DestroyWindow(hwnd);
    }
}

#[test]
fn signed_out_state_ignores_pending_markers() {
    assert_eq!(sync_row_state(false, true, true), SyncRowState::SignedOut);
}

#[test]
fn initial_sync_pending_takes_priority_over_push_pending() {
    assert_eq!(
        sync_row_state(true, true, true),
        SyncRowState::InitialSyncPending
    );
}

#[test]
fn push_pending_without_initial_pending_is_its_own_state() {
    assert_eq!(sync_row_state(true, false, true), SyncRowState::PushPending);
}

#[test]
fn fully_synced_when_signed_in_with_no_pending_markers() {
    assert_eq!(sync_row_state(true, false, false), SyncRowState::Synced);
}

/// F16 (2026-09-05 audit): before this fix, the ONLY signal available to the button, the
/// status line, and the click handler was `is_signed_in()`, so an authenticated-but-not-
/// yet-synced account was indistinguishable from a fully synced one: the status line said
/// "Synced" and the button said "Stop syncing" while a message box, from the very same
/// event, said "sign-in failed". Against that old shape there was no `InitialSyncPending`
/// state to return at all (the equivalent logic collapsed straight to `Synced` whenever
/// `is_signed_in()` was true), so this test fails there and passes now.
#[test]
fn an_authenticated_but_unsynced_account_never_reads_as_fully_synced() {
    let state = sync_row_state(true, true, false);
    assert_ne!(
        state,
        SyncRowState::Synced,
        "must not silently claim to be caught up"
    );
    assert_eq!(state, SyncRowState::InitialSyncPending);
}

// ---- E05: SyncState, the status-line source of truth --------------------------------

fn signals(
    signed_in: bool,
    initial_sync_pending: bool,
    push_pending: bool,
    offline: bool,
) -> SyncSignals {
    SyncSignals {
        signed_in,
        initial_sync_pending,
        push_pending,
        offline,
        error: None,
        who: None,
        updated_from_other_device: false,
    }
}

#[test]
fn signed_out_derives_off_regardless_of_stale_markers() {
    assert_eq!(
        derive_sync_state(&signals(false, true, true, true)),
        SyncState::Off
    );
}

/// Against pre-E05 code (`SyncRowState`, no `Offline` variant at all) this is simply
/// `InitialSyncPending`, the whole point of this test is that a transport failure and
/// a server rejection, both surfacing as "the initial sync hasn't finished", must now
/// render two DIFFERENT lines.
#[test]
fn initial_sync_pending_splits_into_offline_or_pending_by_reachability() {
    assert_eq!(
        derive_sync_state(&signals(true, true, false, true)),
        SyncState::Offline
    );
    assert_eq!(
        derive_sync_state(&signals(true, true, false, false)),
        SyncState::InitialSyncPending { error: None }
    );
}

#[test]
fn push_pending_splits_into_offline_or_saved_locally_by_reachability() {
    assert_eq!(
        derive_sync_state(&signals(true, false, true, true)),
        SyncState::Offline
    );
    assert_eq!(
        derive_sync_state(&signals(true, false, true, false)),
        SyncState::SavedLocally { error: None }
    );
}

/// E05 follow-up audit: `offline` must win even when NEITHER pending marker is set, a
/// pull that failed to reach the server sets no marker of its own (only a push/initial-
/// sync failure does). Against the old nested shape this fell straight through to
/// `Synced` below, rendering "up to date" right after a sync attempt that never left
/// this machine.
#[test]
fn offline_wins_even_with_no_pending_markers_set() {
    assert_eq!(
        derive_sync_state(&signals(true, false, false, true)),
        SyncState::Offline
    );
}

#[test]
fn no_pending_markers_derives_synced_with_the_given_who() {
    let mut s = signals(true, false, false, false);
    s.who = Some("ann@example.com".to_string());
    assert_eq!(
        derive_sync_state(&s),
        SyncState::Synced {
            who: Some("ann@example.com".to_string()),
            updated_from_other_device: false,
        }
    );
}

/// `Connecting` is never derived, it's constructed directly by `begin_connect`/
/// `begin_retry_initial_sync` for the transient overlay. Reachable all the same: this
/// is what "every variant reachable from hand-built inputs" means for a variant with
/// no signals of its own.
#[test]
fn connecting_is_constructed_directly_and_renders_as_its_own_line() {
    assert_eq!(
        render_sync_state(&SyncState::Connecting),
        (t("sync_state_connecting").to_string(), false)
    );
}

/// The invariant the whole audit item is about: none of the three "not caught up yet"
/// states may render the green "Synced" text, whatever their payload.
#[test]
fn offline_initial_pending_and_saved_locally_never_render_as_synced() {
    let synced_text = t("sync_state_synced").to_string();
    let updated_text = t("sync_state_updated").to_string();
    let synced_as_example = t("sync_state_synced_as").replace("{who}", "ann@example.com");
    for state in [
        SyncState::Offline,
        SyncState::InitialSyncPending { error: None },
        SyncState::InitialSyncPending {
            error: Some("boom".to_string()),
        },
        SyncState::SavedLocally { error: None },
        SyncState::SavedLocally {
            error: Some("boom".to_string()),
        },
    ] {
        let (text, green) = render_sync_state(&state);
        assert_ne!(
            text, synced_text,
            "{state:?} must not render the plain synced line"
        );
        assert_ne!(
            text, updated_text,
            "{state:?} must not render the updated line"
        );
        assert_ne!(
            text, synced_as_example,
            "{state:?} must not render the synced-as line"
        );
        assert!(!green, "{state:?} must not tint green");
    }
}

#[test]
fn offline_renders_its_own_locale_key() {
    assert_eq!(
        render_sync_state(&SyncState::Offline),
        (t("sync_state_offline").to_string(), false)
    );
}

#[test]
fn saved_locally_with_error_renders_the_error_text() {
    assert_eq!(
        render_sync_state(&SyncState::SavedLocally {
            error: Some("syncing too often, retry after 5 seconds".to_string())
        }),
        (
            t("sync_state_pending_err")
                .replace("{error}", "syncing too often, retry after 5 seconds"),
            false
        )
    );
}

/// Matches the pre-E05 behavior exactly (the Pulled(Ok(true)) branch always showed the
/// plain "updated" line, ignoring the account name), `who` must not leak in here.
#[test]
fn updated_from_other_device_wins_over_who() {
    assert_eq!(
        render_sync_state(&SyncState::Synced {
            who: Some("ann@example.com".to_string()),
            updated_from_other_device: true,
        }),
        (t("sync_state_updated").to_string(), true)
    );
}

#[test]
fn synced_renders_green_with_or_without_a_who() {
    assert_eq!(
        render_sync_state(&SyncState::Synced {
            who: None,
            updated_from_other_device: false,
        }),
        (t("sync_state_synced").to_string(), true)
    );
    assert_eq!(
        render_sync_state(&SyncState::Synced {
            who: Some("ann@example.com".to_string()),
            updated_from_other_device: false,
        }),
        (
            t("sync_state_synced_as").replace("{who}", "ann@example.com"),
            true
        )
    );
}
