# SageThumbs 2K: storage-mode and multi-user qualification matrix

Audit item E04. This is the repeatable scenario suite for storage mode (installed vs
portable), process elevation (standard vs admin), install/upgrade/uninstall identity (same
user vs an alternate administrator), and concurrent sessions (console plus RDP, or two
users), concentrated on settings persistence and modern-menu (Win11 packaged context menu)
registration. It exists so F05, F06, F07, F08, F09 and F18 (2026-09-05 audit) stay caught
without a human re-deriving the scenario list from scratch every release.

## Format contract

`tests/qualification_matrix.rs` parses the table below by machine. Keep to this shape or
that test breaks:

- The table header row is exactly
  `| # | Scenario | Storage | Process | User | Sessions | Coverage | Evidence |`.
- No blank cells anywhere in the table.
- `Coverage` is exactly one of `AUTOMATED`, `MANUAL`, `UNSUPPORTED`, no other text, no
  markdown emphasis around the word.
- `Evidence` for `AUTOMATED` names one or more real functions, each in one of these exact
  forms: `` `tests/file.rs::fn_name` ``, `` `src/path.rs::module::fn_name` ``, or
  `` `scripts/file.ps1::Function-Name` ``. Every one of them must resolve to a function that
  actually exists in the tree right now (the test reads the named file and looks for
  `fn <name>` / `function <Name>`).
- `Evidence` for `MANUAL` contains the exact phrase `Manual procedure N`, where `N` is the
  number of a `### Procedure N:` heading in the "Manual procedures" section below.
- `Evidence` for `UNSUPPORTED` names the exact file (and function, where one exists) that
  documents the unsupported behaviour, plus a short description of what the user actually
  sees.
- Every finding this table exists to catch (F05, F06, F07, F08, F09, F18) must appear
  (as that literal token) in the text of at least one `AUTOMATED` row.

## Why the cross product is not filled in mechanically

The four columns (Storage / Process / User / Sessions) are not combined exhaustively. Two
collapses are deliberate:

- **`User` only varies for installer flows.** A portable copy has no installer step and
  nothing to elevate, so "alternate admin" never appears against `Storage = portable`: there
  is no install/upgrade/uninstall identity to vary. Running the portable EXE itself is
  always "the interactive user", whoever that is.
- **`Sessions = two` only appears where the underlying mechanism is genuinely
  session-scoped.** Most settings and package behaviour is per-user by construction (a
  second console session exercises the exact same code path against a different HKCU, which
  the existing per-user tests already cover); duplicating every row once per session count
  would inflate the table without adding a distinct failure mode. The rows that do carry
  `Sessions = two` are the ones the audit found breaking specifically because two sessions
  share one resource (F18's licence-history file) or because a tester needs to see the
  isolation hold, not just the mechanism run once.

## Scenarios

| # | Scenario | Storage | Process | User | Sessions | Coverage | Evidence |
|---|---|---|---|---|---|---|---|
| 1 | Portable copy's ini never touches the registry (F07) | portable | standard | same | one | AUTOMATED | `tests/portable_settings.rs::portable_mode_uses_the_ini_and_never_touches_the_registry` |
| 2 | Portable settings export omits the sign-in (credential, identity, licence certificate) and import preserves it; the export also lands cleanly in an installed copy's registry (F04, F05) | portable | standard | same | one | AUTOMATED | `tests/settings_io_portable.rs::portable_export_omits_credentials_and_import_preserves_them` |
| 3 | A fresh portable copy's first-run welcome window adds the thumbnails opt-in row | portable | standard | same | one | AUTOMATED | `tests/first_run_shot.rs::portable_welcome_adds_the_thumbnails_row` |
| 4 | A portable copy launched elevated still resolves its ini beside the EXE, never HKLM or the administrator's HKCU | portable | admin | same | one | MANUAL | Manual procedure 1 |
| 5 | A portable copy stored on a removable or network drive that goes offline mid-session: a setting write fails silently | portable | standard | same | one | UNSUPPORTED | `src/settings.rs`, `portable::update` doc comment: "every public setter here is best-effort"; a failed write is only logged via `crate::safety::log_debug`, with no user-facing error and no detection that the backing drive went offline |
| 6 | The sync-pending marker actually clears after a successful push, so Settings does not silently re-push forever (F06) | installed | standard | same | one | AUTOMATED | `src/bin/app/sync_client.rs::tests::the_pending_marker_clears_after_a_successful_push` |
| 7 | Every registry setting is classified as syncing or never-syncing, so a newly added setting cannot silently stop syncing | installed | standard | same | one | AUTOMATED | `src/bin/app/sync_client.rs::tests::every_setting_is_classified` |
| 8 | The diagnostics report warns that an elevated process's HKCU checks read the administrator's hive, not the interactive user's | installed | admin | same | one | MANUAL | Manual procedure 2 |
| 9 | Two live sessions (console plus RDP, or two users) each writing the shared licence history preserve their own change instead of the second silently discarding the first (F18) | installed | standard | alternate admin | two | AUTOMATED | `src/bin/app/license.rs::tests::two_concurrent_sessions_through_the_lock_both_preserve_their_change` |
| 10 | A licence-history lock that times out writes nothing, rather than clobbering the newer history the other session wrote (F18) | installed | standard | alternate admin | two | AUTOMATED | `src/bin/app/license.rs::tests::a_lock_that_times_out_writes_nothing_rather_than_clobbering_newer_history` |
| 11 | A second interactive user (RDP session) sees their own independent settings, unaffected by the console user's changes | installed | standard | alternate admin | two | MANUAL | Manual procedure 3 |
| 12 | The modern (Win11) context-menu package registers as the original signed-in user during an elevated install, never SYSTEM or the administrator (F08) | installed | admin | same | one | AUTOMATED | `scripts/test-installer-lint.ps1::Test-ModernMenuRegistersAsOriginalUser` |
| 13 | Uninstall/upgrade removes only the exact certificate thumbprint that was installed, never a wildcard-subject sweep of `TrustedPeople` (F09) | installed | admin | same | one | AUTOMATED | `scripts/test-installer-lint.ps1::Test-ExactThumbprintCertRemoval` |
| 14 | Upgrade migrates a pre-fix certificate thumbprint and removes a rotated one cleanly, leaving no orphaned certificate behind (F09) | installed | admin | same | one | AUTOMATED | `scripts/test-installer-lint.ps1::Test-ModernMenuMigratesPreFixThumbprint` and `scripts/test-installer-lint.ps1::Test-ModernMenuRemovesRotatedThumbprint` |
| 15 | Uninstall removes the all-users sparse package synchronously; nothing lingers after the uninstaller process exits (F09) | installed | admin | same | one | AUTOMATED | `scripts/test-installer-lint.ps1::Test-PackageRemovalIsSynchronousAllUsers` |
| 16 | A different administrator (not the user who will run the app) performs the install, upgrade or uninstall; the modern menu still ends up registered for the right interactive user, never orphaned under the installing administrator | installed | admin | alternate admin | one | MANUAL | Manual procedure 4 |
| 17 | The release installer's architecture contract: x64 and ARM64 share one application directory, the correct `ArchitecturesAllowed` matcher is set, and the ImageMagick engine payload is never architecture-gated | installed | admin | same | one | AUTOMATED | `scripts/test-installer-lint.ps1::Assert-ReleaseArchitectureContract` |
| 18 | The full settings/package integration test suite passes natively on ARM64 hardware, cross-compiled for `aarch64-pc-windows-msvc` | installed or portable | standard | same | one | AUTOMATED | CI job `arm64-native` in `.github/workflows/ci.yml` runs `cargo test --locked --tests --target aarch64-pc-windows-msvc`, which executes every `tests/*.rs` case above on real ARM64 hardware, including `tests/portable_settings.rs::portable_mode_uses_the_ini_and_never_touches_the_registry`. Not runnable on an x64 development host; `scripts/qualify.ps1` skips this row loudly rather than pretending to run it. |
| 19 | Modern-menu package registration and clean removal on a native ARM64 machine, since the CI ARM64 job deliberately skips installer and package staging | installed | admin | same | one | MANUAL | Manual procedure 5 |

## Manual procedures

Each procedure assumes a fresh Windows VM snapshot (or one reverted to a clean state) and
the current release installer built by `scripts/build-release.ps1`. Revert the snapshot
after each run so the next run starts clean.

### Procedure 1: portable copy launched elevated stays file-backed (row 4)

Guards: an elevated portable session silently falling back to HKLM or the administrator's
HKCU instead of the ini beside the EXE.

1. Copy the portable build (the folder containing `SageThumbs2K.exe` and no installer) to
   `C:\PortableTest\`.
2. Right-click `SageThumbs2K.exe` and choose "Run as administrator".
3. In Settings, change any one value (for example the thumbnail cache size) and close the
   app.
4. **Passes if:** `C:\PortableTest\SageThumbs2K.ini` contains the changed value, and
   `regedit` under both `HKEY_CURRENT_USER\Software\SageThumbs2K` (for the Administrator
   account) and `HKEY_LOCAL_MACHINE\Software\SageThumbs2K` show no such key was created.

### Procedure 2: doctor report names the elevated-hive caveat (row 8)

Guards: a user running diagnostics elevated being told their per-user registration state
without being warned it is the wrong hive.

1. Install SageThumbs 2K normally (non-elevated day-to-day account, standard install).
2. Open a PowerShell window "as administrator" and run
   `st2k.exe --doctor` (or trigger the in-app "Run diagnostics" action from an elevated
   shortcut).
3. **Passes if:** the report's Environment section contains a line naming "Elevated" and
   stating that HKCU checks below inspect the administrator's hive, which may not match the
   interactive user's session, and recommending a re-run un-elevated.

### Procedure 3: two interactive sessions keep independent settings (row 11)

Guards: a per-user setting leaking across sessions on a shared machine (RDS host, or a
console user plus an RDP user).

1. On a machine with SageThumbs 2K installed, sign in at the console as User A and set a
   distinctive preference (for example, disable thumbnails for one file type).
2. Without signing User A out, open an RDP session to the same machine as User B (a
   different account) and open Settings there.
3. **Passes if:** User B's settings show the defaults (or User B's own prior choices), not
   User A's change, and after User B changes a different preference and disconnects, User
   A's console session still shows User A's own value unchanged.

### Procedure 4: an alternate administrator installs/upgrades/uninstalls (row 16)

Guards: the modern menu ending up registered to the wrong account, or orphaned, when the
person running the installer differs from the person who will use the app.

1. Sign in as User A (standard account) and, using an administrator credential belonging to
   a **different** account (Administrator B), install SageThumbs 2K via the UAC elevation
   prompt (enter Administrator B's credentials at the prompt while User A stays the
   interactive user).
2. Sign out, sign back in as User A, and confirm the classic (right-click) and modern
   (Windows 11 "Show more options" tier) context menus both show SageThumbs 2K entries for
   User A.
3. Repeat step 1 for an upgrade (install a newer build the same way) and then for an
   uninstall (`Add or Remove Programs`, elevated with Administrator B's credentials).
4. **Passes if:** after each step the menu is present for User A when it should be and
   absent after the uninstall, with no leftover entry visible only to Administrator B's own
   profile.

### Procedure 5: modern-menu registration on native ARM64 (row 19)

Guards: package registration or removal working on x64 (the only architecture CI's
`arm64-native` job exercises code on, and even then without installer staging) but not on
real ARM64 hardware, since nothing in CI installs the packaged app there.

1. On a Windows 11 ARM64 machine (or ARM64 VM), run the ARM64 release installer
   (`SageThumbs2K-Setup-<ver>-arm64.exe`) elevated as the interactive user.
2. Confirm the modern (Windows 11) context-menu entry appears for image files.
3. Uninstall via `Add or Remove Programs`.
4. **Passes if:** the modern-menu entry appears after install and is gone after uninstall,
   with no error dialog from `Add-AppxPackage`/`Remove-AppxPackage` at either step.
