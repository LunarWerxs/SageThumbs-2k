//! Windows' restart-time rename list (`PendingFileRenameOperations`), as it concerns the
//! installed copy.
//!
//! Setup does not install over a file of ours that the list still names: Inno stops with "the
//! installation of a previous program was not completed", restart first. Under the updater's
//! `/SILENT` launch that refusal is a message box, and CI's second held update sat on it for an
//! hour. So the updater asks first, and says to restart Windows instead of downloading.

/// Inno's uninstall key for this app (installer.iss `AppId`). Its `InstallLocation` is the
/// installed copy's folder, which the next setup installs into again (`UsePreviousAppDir`).
pub(super) const UNINSTALL_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\{B0A1C2D3-E4F5-4607-8899-AABBCCDDEEFF}_is1";

/// The names in `list` that would stop setup in `dir`, the installed copy's folder: any file
/// under it, source or destination (Inno compares both), except a parked image at its top level
/// (`<name>.dll.oldN` or `<name>.exe.oldN` queued for deletion). Setup neither installs nor
/// deletes those, and nearly every update leaves one while Explorer maps the shell extension, so
/// counting them would hold back every second update until a restart.
pub(super) fn blocking_names<'a>(list: &'a [String], dir: &str) -> Vec<&'a str> {
    let mut under = dir.trim_end_matches('\\').to_lowercase();
    under.push('\\');
    list.iter()
        .map(|entry| strip_marks(entry))
        .filter(|path| {
            let lower = path.to_lowercase();
            lower.starts_with(&under) && !is_parked_image(&lower[under.len()..])
        })
        .collect()
}

/// One entry without Windows' marks: `*1` or `*2` (whether the file or its folder existed), `!`
/// (replace an existing file), then the `\??\` path prefix.
fn strip_marks(entry: &str) -> &str {
    let mut s = entry;
    if let Some(rest) = s.strip_prefix('*') {
        s = rest.trim_start_matches(|c: char| c.is_ascii_digit());
    }
    s = s.strip_prefix('!').unwrap_or(s);
    s.strip_prefix(r"\??\").unwrap_or(s)
}

/// Is `rest`, a lowercase path inside the install folder, an image the installer parked at its
/// top level?
fn is_parked_image(rest: &str) -> bool {
    if rest.contains('\\') {
        return false;
    }
    let Some((name, n)) = rest.rsplit_once(".old") else {
        return false;
    };
    !n.is_empty()
        && n.bytes().all(|b| b.is_ascii_digit())
        && (name.ends_with(".dll") || name.ends_with(".exe"))
}

/// The first name in Windows' restart-time list that would stop setup from updating the
/// installed copy. None when SageThumbs is not installed, nothing of ours waits, or the list
/// cannot be read (setup then says so itself).
pub(super) fn restart_blocker() -> Option<String> {
    let machine = windows_registry::LOCAL_MACHINE;
    let dir = machine
        .open(UNINSTALL_KEY)
        .and_then(|k| k.get_string("InstallLocation"))
        .ok()
        .filter(|d| !d.trim().is_empty())?;
    let session = machine
        .open(r"SYSTEM\CurrentControlSet\Control\Session Manager")
        .ok()?;
    // Windows spills a full list into the second value.
    let list: Vec<String> = [
        "PendingFileRenameOperations",
        "PendingFileRenameOperations2",
    ]
    .iter()
    .filter_map(|value| session.get_multi_string(value).ok())
    .flatten()
    .collect();
    blocking_names(&list, &dir)
        .first()
        .map(|name| (*name).to_string())
}

/// Does Windows still have to restart to finish an earlier update before setup will install
/// another one?
pub fn restart_pending() -> bool {
    restart_blocker().is_some()
}
