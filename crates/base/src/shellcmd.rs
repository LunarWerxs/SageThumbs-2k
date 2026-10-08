//! Spawning `cmd /c <line>` without letting Rust mangle the line.
//!
//! `std::process::Command::arg` escapes an argument for the **MSVCRT** convention:
//! embedded `"` come out as `\"`. `cmd.exe` does not use that convention — it has no
//! backslash escape at all — so a batch line that contains quotes arrives corrupted.
//!
//! The concrete bug this exists to prevent (GitHub issue #5): the payload
//! `… & start "" explorer.exe` was handed to `cmd` via `.args(["/c", line])`, which
//! put `start \"\" explorer.exe` on the command line. `cmd` reads `\` as a literal
//! character and `""` as an empty quoted string, so `start` received the target `\\`
//! — a UNC root — and the shell popped *"Windows cannot find '\\'"* (localized as
//! *"the network path was not found"*). The preceding `taskkill` had already killed
//! Explorer, so the user was left with no shell.
//!
//! `raw_arg` appends the string verbatim, which is what `cmd` wants.

mod folders;

use std::os::windows::process::CommandExt;
use std::process::{Child, Command};

/// `CREATE_NO_WINDOW` — run the interpreter without flashing a console at the user.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Spawn `cmd /c <line>`, passing `line` to the interpreter **verbatim**.
///
/// Always use this instead of `Command::new("cmd").args(["/c", line])`: see the module
/// docs for what the escaping does to any line containing a quote.
pub fn cmd_c(line: &str) -> std::io::Result<Child> {
    Command::new("cmd")
        .arg("/c")
        .raw_arg(line)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
}

/// Every `thumbcache_*.db` in this user's Explorer cache folder.
fn thumbcache_files() -> Vec<std::path::PathBuf> {
    let Some(local) = std::env::var_os("LOCALAPPDATA") else {
        return Vec::new();
    };
    let dir = std::path::Path::new(&local).join(r"Microsoft\Windows\Explorer");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("thumbcache_") && n.ends_with(".db"))
        })
        .collect()
}

/// A Restart Manager session, ended when dropped.
struct RmSession(u32);

impl RmSession {
    fn start() -> Option<Self> {
        use windows::core::PWSTR;
        use windows::Win32::Foundation::ERROR_SUCCESS;
        use windows::Win32::System::RestartManager::{RmStartSession, CCH_RM_SESSION_KEY};
        let mut handle = 0u32;
        let mut key = [0u16; CCH_RM_SESSION_KEY as usize + 1];
        let started = unsafe { RmStartSession(&mut handle, None, PWSTR(key.as_mut_ptr())) };
        (started == ERROR_SUCCESS).then_some(Self(handle))
    }
}

impl Drop for RmSession {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::System::RestartManager::RmEndSession(self.0);
        }
    }
}

/// The Explorer processes in THIS session that hold any of `files` open, as Restart Manager
/// sees them. Explorer only: another program reading the cache (a file manager using the
/// shell's thumbnails) is never closed, and neither is anyone else's Explorer.
fn explorers_holding(
    files: &[std::path::PathBuf],
) -> Vec<windows::Win32::System::RestartManager::RM_UNIQUE_PROCESS> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{ERROR_MORE_DATA, ERROR_SUCCESS};
    use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows::Win32::System::RestartManager::{
        RmExplorer, RmGetList, RmRegisterResources, RM_PROCESS_INFO,
    };
    use windows::Win32::System::Threading::GetCurrentProcessId;
    let Some(session) = RmSession::start() else {
        return Vec::new();
    };
    let wide: Vec<Vec<u16>> = files
        .iter()
        .map(|f| crate::host::wide(&f.to_string_lossy()))
        .collect();
    let names: Vec<PCWSTR> = wide.iter().map(|w| PCWSTR(w.as_ptr())).collect();
    if unsafe { RmRegisterResources(session.0, Some(&names), None, None) } != ERROR_SUCCESS {
        return Vec::new();
    }
    let mut ours = 0u32;
    let _ = unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut ours) };
    let mut list: Vec<RM_PROCESS_INFO> = Vec::new();
    // Twice at most: the first call sizes the list, the second fills it (a third would mean the
    // set keeps changing under us, and then doing nothing is the safe answer).
    for _ in 0..2 {
        let (mut needed, mut count, mut reasons) = (0u32, list.len() as u32, 0u32);
        let got = unsafe {
            RmGetList(
                session.0,
                &mut needed,
                &mut count,
                Some(list.as_mut_ptr()),
                &mut reasons,
            )
        };
        if got == ERROR_MORE_DATA {
            list.resize(needed as usize, RM_PROCESS_INFO::default());
            continue;
        }
        if got != ERROR_SUCCESS {
            return Vec::new();
        }
        list.truncate(count as usize);
        return list
            .iter()
            .filter(|p| p.ApplicationType == RmExplorer && p.TSSessionId == ours)
            .map(|p| p.Process)
            .collect();
    }
    Vec::new()
}

/// Close the Explorer processes holding the cache, delete the cache, reopen Explorer, all
/// through Restart Manager: the API installers use to replace a file in use. It asks Explorer
/// to close and starts it again (`explorer.exe /LOADSAVEDWINDOWS`; the folder windows come
/// back through `folders`, not that switch). No `cmd`, no `taskkill`, no `del`: a program force-killing `explorer.exe` and deleting files
/// through `cmd` is how VirusTotal's sandbox described the old one-liner (sigma "File
/// Deletion Via Del"), and what behaviour-based antivirus scores. The files are deleted
/// best-effort either way; one Explorer still holds open simply stays.
fn cycle_explorer(cache: &[std::path::PathBuf]) {
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::RestartManager::{RmRegisterResources, RmRestart, RmShutdown};
    let explorers = explorers_holding(cache);
    let session = (!explorers.is_empty()).then(RmSession::start).flatten();
    let registered = session.as_ref().is_some_and(|s| unsafe {
        RmRegisterResources(s.0, None, Some(&explorers), None) == ERROR_SUCCESS
    });
    if registered {
        if let Some(s) = &session {
            // 0: ask, never force. An Explorer that will not close keeps running untouched.
            let _ = unsafe { RmShutdown(s.0, 0, None) };
        }
    }
    for file in cache {
        let _ = std::fs::remove_file(file);
    }
    if registered {
        if let Some(s) = &session {
            let _ = unsafe { RmRestart(s.0, None, None) };
        }
    }
}

/// Poll `is_up` every 200 ms for ~15 s, returning `true` as soon as it reports the shell
/// back and `false` if the window never appears.
///
/// ~15s for Explorer to come back; a cold shell on a busy machine takes a few seconds.
fn wait_for_shell(is_up: &impl Fn() -> bool) -> bool {
    for _ in 0..75 {
        if is_up() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    false
}

/// Restart Explorer + clear the thumbnail cache (the "Rebuild thumbnail cache" and "Repair
/// file associations" buttons, setup's "Restart File Explorer" box, the post-update toast),
/// then CHECK THE SHELL CAME BACK.
///
/// "We closed your shell and something went wrong on the way back" is severe enough to be
/// worth confirming rather than assuming: issue #5 left somebody staring at an empty desktop
/// with no taskbar (a quoting bug in the old `cmd` relaunch). Restart Manager returns once
/// Explorer is down and its restart issued, so the check below sees the NEW taskbar, never the
/// old one; if it does not appear, relaunch Explorer directly and check once more. Windows'
/// `AutoRestartShell` is a third net beneath both, but it is a registry value a machine can
/// have turned off, so it is not something to rely on.
///
/// Returns whether the shell is confirmed back. Callers treat it as best-effort: there is
/// nothing useful left to do about `false` except not claim success.
pub fn restart_explorer_clearing_cache() -> bool {
    use windows::core::w;
    use windows::Win32::UI::WindowsAndMessaging::FindWindowW;

    // `Shell_TrayWnd` is the taskbar. Checking for the WINDOW rather than an `explorer.exe`
    // process matters: a process exists the instant it starts, while the window only appears
    // once the shell is actually up and serving, which is what the user cares about.
    let shell_is_up = || unsafe { FindWindowW(w!("Shell_TrayWnd"), None).is_ok() };
    // Read before the restart closes them; reopened once the taskbar is back.
    let open = folders::open_folders();
    let settle = std::time::Duration::from_secs(3);

    cycle_explorer(&thumbcache_files());
    if wait_for_shell(&shell_is_up) {
        folders::reopen_missing(open, settle);
        return true;
    }
    crate::safety::log("explorer did not return after the cache rebuild - relaunching directly");
    let _ = Command::new("explorer.exe")
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
    if wait_for_shell(&shell_is_up) {
        folders::reopen_missing(open, settle);
        return true;
    }
    crate::safety::log("explorer STILL not back after a direct relaunch");
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression lock for issue #5. `cmd` must receive the quotes we wrote, not
    /// `\"`-escaped ones. `echo` reproduces its argument verbatim, so the output tells
    /// us exactly what the interpreter parsed.
    ///
    /// With the old `.args(["/c", line])` this prints `\"quoted\"` and fails.
    #[test]
    fn quotes_reach_cmd_unescaped() {
        let out = Command::new("cmd")
            .arg("/c")
            .raw_arg("echo \"quoted\"")
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .expect("spawn cmd");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert_eq!(
            stdout.trim(),
            "\"quoted\"",
            "cmd saw a mangled line: {stdout:?}"
        );
    }
}
