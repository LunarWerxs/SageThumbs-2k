//! Cloud sync folders (OneDrive, Synology Drive, Nextcloud, ...): putting our cloud-folder
//! provider in the one thumbnail slot such a folder has, and handing it back.
//!
//! WHY THIS EXISTS (measured 2026-09-27 against a test sync root on this machine): once a file
//! inside a registered sync root is a Cloud Files placeholder — the normal state of EVERY file
//! OneDrive or Synology Drive syncs, fully downloaded ones included — Explorer never asks the
//! per-extension thumbnail handlers for it. It asks only the provider CLSID named in the sync
//! root's `SyncRootManager\<id>\ThumbnailProvider` value, and with no value there it fails with
//! `WTS_E_NOSTORAGEPROVIDERTHUMBNAILHANDLER` (0x8004B207). So a .pdn or .afphoto in OneDrive
//! showed a stock icon however healthy our registration was (issue #16 was this, blamed on
//! hydration at the time).
//!
//! The fix is a chain: our provider (`crate::cloudthumb`) goes into that value, the value it
//! replaced is recorded beside it under [`CHAIN_VALUE`], and the provider draws what it can and
//! hands everything else back to the recorded one. Unchaining puts the recorded value back.
//! The shell activates the slot out of process, which is why the class is registered with a
//! `DllSurrogate` AppID (the same shape Nextcloud's client uses for its own provider).
//!
//! Each sync root key is owned by the user whose client registered it (CREATOR OWNER has full
//! control), so a user can chain their own roots without elevation; the elevated installer can
//! chain every user's. OneDrive may write its own value back when it (re-)registers, so the chain
//! is re-applied at sign-in (a per-user logon task, [`sync_relink_task`]), on every app launch
//! and by the daily update check ([`relink`]).

use super::*;

use st2k_base::guids::{CLOUD_THUMB_APPID_STR, CLSID_CLOUD_THUMB_PROVIDER_STR};

/// Where every sync root (every user's) is registered with the shell.
pub(crate) const SYNC_ROOT_MANAGER: &str =
    r"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\SyncRootManager";
/// The value the shell reads the sync root's thumbnail provider CLSID from.
const THUMB_VALUE: &str = "ThumbnailProvider";
/// Our record of the provider we replaced: its CLSID, or empty when the slot was empty.
pub(crate) const CHAIN_VALUE: &str = "SageThumbs2K.ChainedThumbnailProvider";
/// A packaged provider (Dropbox, iCloud) names its app here and declares its handlers in its
/// package manifest instead of the registry.
const AUMID_VALUE: &str = "AUMID";
pub(super) const NAME: &str = "SageThumbs 2K Cloud Folder Thumbnail Provider";
/// The per-user task that re-links the chain after sign-in (see the module doc).
const RELINK_TASK: &str = "SageThumbs2K_CloudFolders";

/// One registered sync root, as the doctor and the chain see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncRoot {
    /// The `SyncRootManager` subkey name: `<provider>!<SID>!<account>`.
    pub id: String,
    /// The folders it syncs (`UserSyncRoots`, one per SID).
    pub folders: Vec<String>,
    /// The current `ThumbnailProvider` value, when there is one.
    pub provider: Option<String>,
    /// Our record of what we replaced, when we are chained in.
    pub chained: Option<String>,
    /// The packaged app behind the root, when there is one.
    pub aumid: Option<String>,
}

impl SyncRoot {
    /// The provider name, the part of the id before the first `!` (`OneDrive`, `SynologyDrive`).
    pub fn provider_name(&self) -> &str {
        self.id.split('!').next().unwrap_or(&self.id)
    }

    /// Is our provider the one in the slot right now?
    pub fn is_chained(&self) -> bool {
        self.provider
            .as_deref()
            .is_some_and(|p| p.eq_ignore_ascii_case(CLSID_CLOUD_THUMB_PROVIDER_STR))
    }
}

/// What chaining does to one sync root's slot.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ChainStep {
    /// Ours already: leave it (and its record) alone.
    Keep,
    /// Take the slot, recording `record` (the replaced CLSID, empty for an empty slot).
    Take { record: String },
    /// A packaged provider whose handler lives in its manifest: an empty registry slot there
    /// does not mean "no provider", and taking it would hide that provider's own thumbnails
    /// behind a chain with nothing to hand back to. Left alone.
    Skip,
}

/// The chaining decision for one slot, from its current value and whether a packaged app owns
/// the root. Pure, so the rules are pinned without a registry.
pub(crate) fn chain_step(current: Option<&str>, packaged: bool) -> ChainStep {
    match current.map(str::trim).filter(|s| !s.is_empty()) {
        Some(c) if c.eq_ignore_ascii_case(CLSID_CLOUD_THUMB_PROVIDER_STR) => ChainStep::Keep,
        Some(c) => ChainStep::Take {
            record: c.to_string(),
        },
        None if packaged => ChainStep::Skip,
        None => ChainStep::Take {
            record: String::new(),
        },
    }
}

/// What unchaining does to one sync root's slot.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum UnchainStep {
    /// Ours is in the slot and we replaced this CLSID: put it back.
    Restore(String),
    /// Ours is in the slot and the slot was empty before us: empty it again.
    Clear,
    /// Someone else has the slot now (the client re-registered): leave their value, drop our record.
    DropRecord,
}

/// The unchaining decision for one slot. Pure, like [`chain_step`].
pub(crate) fn unchain_step(current: Option<&str>, recorded: Option<&str>) -> UnchainStep {
    let ours = current.is_some_and(|c| {
        c.trim()
            .eq_ignore_ascii_case(CLSID_CLOUD_THUMB_PROVIDER_STR)
    });
    if !ours {
        return UnchainStep::DropRecord;
    }
    match recorded.map(str::trim).filter(|r| !r.is_empty()) {
        Some(r) => UnchainStep::Restore(r.to_string()),
        None => UnchainStep::Clear,
    }
}

/// Is `path` the folder `root` or inside it? Case-insensitive, on a path-component boundary, so
/// `C:\OneDrive2\x` is not "inside" `C:\OneDrive`.
pub(crate) fn path_is_under(path: &str, root: &str) -> bool {
    let root = root.trim_end_matches(['\\', '/']);
    if root.is_empty() || path.len() < root.len() {
        return false;
    }
    let (head, tail) = path.split_at(root.len());
    head.eq_ignore_ascii_case(root) && (tail.is_empty() || tail.starts_with(['\\', '/']))
}

/// Every sync root registered on this machine. Read-only; unreadable keys are skipped.
pub fn sync_roots() -> Vec<SyncRoot> {
    let Ok(manager) = LOCAL_MACHINE.open(SYNC_ROOT_MANAGER) else {
        return Vec::new();
    };
    let Ok(ids) = manager.keys() else {
        return Vec::new();
    };
    ids.filter_map(|id| read_root(&manager, id)).collect()
}

fn read_root(manager: &Key, id: String) -> Option<SyncRoot> {
    let key = manager.open(&id).ok()?;
    let folders = key
        .open("UserSyncRoots")
        .ok()
        .map(|k| {
            k.values()
                .map(|vals| {
                    vals.filter_map(|(_, v)| String::try_from(v).ok())
                        .filter(|p| !p.is_empty())
                        .collect()
                })
                .unwrap_or_default()
        })
        .unwrap_or_default();
    Some(SyncRoot {
        provider: key.get_string(THUMB_VALUE).ok(),
        chained: key.get_string(CHAIN_VALUE).ok(),
        aumid: key.get_string(AUMID_VALUE).ok().filter(|s| !s.is_empty()),
        folders,
        id,
    })
}

/// How a chain or unchain pass went.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CloudPass {
    /// Slots changed this pass.
    pub changed: usize,
    /// Slots already as wanted.
    pub unchanged: usize,
    /// Slots left alone on purpose (packaged providers).
    pub skipped: usize,
    /// Slots we could not write (another user's root without elevation, say).
    pub failed: usize,
}

/// Put our provider into every sync root's slot we can write, recording what it replaced.
pub fn chain_all() -> CloudPass {
    let mut pass = CloudPass::default();
    for root in sync_roots() {
        match chain_step(root.provider.as_deref(), root.aumid.is_some()) {
            ChainStep::Keep => pass.unchanged += 1,
            ChainStep::Skip => pass.skipped += 1,
            ChainStep::Take { record } => match write_chain(&root.id, &record) {
                Ok(()) => pass.changed += 1,
                Err(_) => pass.failed += 1,
            },
        }
    }
    if pass.changed > 0 {
        log(&format!(
            "cloud: chained {} sync folder thumbnail slot(s) ({} already, {} skipped, {} not writable)",
            pass.changed, pass.unchanged, pass.skipped, pass.failed
        ));
    }
    pass
}

/// Record first, then take the slot: a crash between the two leaves a record and the old value,
/// which the next pass completes, never our value with no record of what it replaced.
fn write_chain(id: &str, record: &str) -> Result<()> {
    let key = LOCAL_MACHINE.create(format!("{SYNC_ROOT_MANAGER}\\{id}"))?;
    key.set_string(CHAIN_VALUE, record)?;
    key.set_string(THUMB_VALUE, CLSID_CLOUD_THUMB_PROVIDER_STR)
}

/// Hand every slot we hold back to what it held before, and drop our records.
pub fn unchain_all() -> CloudPass {
    let mut pass = CloudPass::default();
    for root in sync_roots() {
        if root.chained.is_none() && !root.is_chained() {
            pass.unchanged += 1;
            continue;
        }
        let step = unchain_step(root.provider.as_deref(), root.chained.as_deref());
        match write_unchain(&root.id, &step) {
            Ok(()) => pass.changed += 1,
            Err(_) => pass.failed += 1,
        }
    }
    if pass.changed > 0 || pass.failed > 0 {
        log(&format!(
            "cloud: unchained {} sync folder thumbnail slot(s), {} not writable",
            pass.changed, pass.failed
        ));
    }
    pass
}

fn write_unchain(id: &str, step: &UnchainStep) -> Result<()> {
    let key = LOCAL_MACHINE.create(format!("{SYNC_ROOT_MANAGER}\\{id}"))?;
    match step {
        UnchainStep::Restore(clsid) => key.set_string(THUMB_VALUE, clsid)?,
        UnchainStep::Clear => {
            let _ = key.remove_value(THUMB_VALUE);
        }
        UnchainStep::DropRecord => {}
    }
    let _ = key.remove_value(CHAIN_VALUE);
    Ok(())
}

/// The provider we replaced in the sync root that holds `path`: `Some(clsid)` when there was
/// one, `None` when `path` is in no chained root or the slot we took was empty.
pub(crate) fn replaced_provider_for(path: &str) -> Option<String> {
    sync_roots()
        .into_iter()
        .find(|r| r.folders.iter().any(|f| path_is_under(path, f)))
        .and_then(|r| r.chained)
        .filter(|c| {
            !c.trim().is_empty()
                && !c
                    .trim()
                    .eq_ignore_ascii_case(CLSID_CLOUD_THUMB_PROVIDER_STR)
        })
}

/// Is our cloud-folder class registered where the shell will find it (either hive)?
pub fn provider_registered() -> bool {
    windows_registry::CLASSES_ROOT
        .open(format!(
            "CLSID\\{CLSID_CLOUD_THUMB_PROVIDER_STR}\\InprocServer32"
        ))
        .and_then(|k| k.get_string(""))
        .is_ok_and(|p| !p.is_empty())
}

/// Bring this user's sync folders in line with the `CloudThumbs` setting: chained when it is on
/// and our class is registered, handed back otherwise. What sign-in, app launches and the daily
/// update check run; cheap (a few registry reads) when nothing changed.
pub fn relink() -> CloudPass {
    if settings::cloud_thumbs() && provider_registered() {
        chain_all()
    } else {
        unchain_all()
    }
}

/// Register the cloud-folder class under `classes` (`HKLM\SOFTWARE\Classes` or the user's own):
/// the in-proc server plus an AppID whose empty `DllSurrogate` makes COM host it in `dllhost.exe`,
/// which is how the shell activates a sync root's provider.
pub(super) fn register_class(classes: &Key, dll_path: &str) -> Result<()> {
    write_inproc_server(classes, CLSID_CLOUD_THUMB_PROVIDER_STR, NAME, dll_path)?;
    classes
        .create(format!("CLSID\\{CLSID_CLOUD_THUMB_PROVIDER_STR}"))?
        .set_string("AppID", CLOUD_THUMB_APPID_STR)?;
    let appid = classes.create(format!("AppID\\{CLOUD_THUMB_APPID_STR}"))?;
    appid.set_string("", NAME)?;
    appid.set_string("DllSurrogate", "")
}

/// [`register_class`], the Approved-list entry when there is a machine-wide list to write, and
/// then this machine's sync folders brought in line with the setting. Best-effort and logged
/// under `who`: a failure here costs cloud folders only, never the rest of the registration.
pub(super) fn register_and_link(classes: &Key, dll_path: &str, approved: Option<&Key>, who: &str) {
    let registered = register_class(classes, dll_path).and_then(|()| match approved {
        Some(list) => list.set_string(CLSID_CLOUD_THUMB_PROVIDER_STR, NAME),
        None => Ok(()),
    });
    if let Err(e) = registered {
        log_error(&format!(
            "{who}: cloud-folder provider registration failed: hr={:#010x}",
            e.code().0
        ));
        return;
    }
    let _ = if settings::cloud_thumbs() {
        chain_all()
    } else {
        unchain_all()
    };
}

/// Undo [`register_class`] under `classes`.
pub(super) fn unregister_class(classes: &Key) {
    let _ = classes.remove_tree(format!("CLSID\\{CLSID_CLOUD_THUMB_PROVIDER_STR}"));
    let _ = classes.remove_tree(format!("AppID\\{CLOUD_THUMB_APPID_STR}"));
}

/// The logon task definition: `exe --cloud-relink`, one minute and again five minutes after THIS
/// user signs in (a cloud client re-registering its folder at startup may put its own provider
/// back after the first run). A logon trigger scoped to the creating user needs no elevation.
pub(crate) fn relink_task_xml(exe: &str, user: &str) -> String {
    use st2k_base::tasksched::{exec_task_xml, logon_trigger};
    let triggers = logon_trigger(user, "PT1M") + &logon_trigger(user, "PT5M");
    exec_task_xml(
        "Keeps SageThumbs 2K thumbnails working in cloud sync folders (OneDrive, Synology Drive, ...).",
        &triggers,
        exe,
        "--cloud-relink",
        "PT2M",
    )
}

/// Create or drop the per-user logon task to match the setting. `exe` is the companion EXE the
/// task runs. Registered in-process ([`st2k_base::tasksched`]): no `schtasks.exe`, no XML file
/// in `%TEMP%`. Best-effort: Task Scheduler refusing only costs the sign-in re-link, and the
/// other triggers (app launch, the daily update check) still run.
pub fn sync_relink_task(exe: &std::path::Path, wanted: bool) {
    if !wanted {
        remove_relink_task();
        return;
    }
    let user = match (std::env::var("USERDOMAIN"), std::env::var("USERNAME")) {
        (Ok(d), Ok(u)) if !d.is_empty() && !u.is_empty() => format!("{d}\\{u}"),
        _ => return,
    };
    let xml = relink_task_xml(&exe.to_string_lossy(), &user);
    if let Err(e) = st2k_base::tasksched::register(RELINK_TASK, &xml) {
        log_error(&format!(
            "cloud: could not create the sign-in re-link task: {e}"
        ));
    }
}

/// Drop the per-user logon task, if there is one.
pub fn remove_relink_task() {
    let _ = st2k_base::tasksched::delete(RELINK_TASK);
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONEDRIVE: &str = "{021E4F06-9DCC-49AD-88CF-ECC2DA314C8A}";

    /// The chain's whole contract in one place: take a foreign or empty slot recording what was
    /// there, never re-record our own value, and never take the empty slot of a packaged
    /// provider (its handler lives in its manifest, so there would be nothing to hand back to).
    #[test]
    fn chaining_records_what_it_replaces_and_leaves_packaged_providers_alone() {
        assert_eq!(
            chain_step(Some(ONEDRIVE), true),
            ChainStep::Take {
                record: ONEDRIVE.into()
            }
        );
        assert_eq!(
            chain_step(None, false),
            ChainStep::Take {
                record: String::new()
            }
        );
        assert_eq!(chain_step(None, true), ChainStep::Skip);
        assert_eq!(
            chain_step(Some(&CLSID_CLOUD_THUMB_PROVIDER_STR.to_lowercase()), false),
            ChainStep::Keep
        );
    }

    /// Unchaining puts back exactly what was recorded, and never overwrites a provider the cloud
    /// client has put back itself since.
    #[test]
    fn unchaining_restores_the_record_and_never_clobbers_a_client_that_took_its_slot_back() {
        let ours = Some(CLSID_CLOUD_THUMB_PROVIDER_STR);
        assert_eq!(
            unchain_step(ours, Some(ONEDRIVE)),
            UnchainStep::Restore(ONEDRIVE.into())
        );
        assert_eq!(unchain_step(ours, Some("")), UnchainStep::Clear);
        assert_eq!(unchain_step(ours, None), UnchainStep::Clear);
        assert_eq!(
            unchain_step(Some(ONEDRIVE), Some(ONEDRIVE)),
            UnchainStep::DropRecord
        );
    }

    /// A file is matched to its sync root on a whole path component, case-insensitively.
    #[test]
    fn a_path_belongs_to_a_sync_root_only_on_a_component_boundary() {
        assert!(path_is_under(
            r"C:\Users\a\OneDrive\x.pdn",
            r"C:\Users\a\OneDrive"
        ));
        assert!(path_is_under(
            r"c:\users\A\onedrive\x.pdn",
            r"C:\Users\a\OneDrive\"
        ));
        assert!(!path_is_under(
            r"C:\Users\a\OneDrive2\x.pdn",
            r"C:\Users\a\OneDrive"
        ));
        assert!(!path_is_under(r"C:\x.pdn", ""));
    }
}
