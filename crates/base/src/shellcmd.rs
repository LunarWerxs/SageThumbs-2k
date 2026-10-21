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

/// The Explorer that owns the taskbar, as Restart Manager names a process (its id and start
/// time, so a recycled id is never mistaken for it). `None` with no taskbar in this session.
fn shell_process() -> Option<windows::Win32::System::RestartManager::RM_UNIQUE_PROCESS> {
    use windows::core::w;
    use windows::Win32::Foundation::{CloseHandle, FILETIME};
    use windows::Win32::System::RestartManager::RM_UNIQUE_PROCESS;
    use windows::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, GetWindowThreadProcessId};
    unsafe {
        let tray = FindWindowW(w!("Shell_TrayWnd"), None).ok()?;
        let mut pid = 0u32;
        GetWindowThreadProcessId(tray, Some(&mut pid));
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let [mut created, mut exited, mut kernel, mut user] = [FILETIME::default(); 4];
        let timed = GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user);
        let _ = CloseHandle(process);
        timed.ok()?;
        Some(RM_UNIQUE_PROCESS {
            dwProcessId: pid,
            ProcessStartTime: created,
        })
    }
}

/// Close the Explorer processes holding the cache, and the one that owns the taskbar, delete
/// the cache, reopen Explorer, all through Restart Manager: the API installers use to replace
/// a file in use. It asks Explorer to close and starts it again (`explorer.exe
/// /LOADSAVEDWINDOWS`; the folder windows come back through `folders`, not that switch). The
/// taskbar's Explorer is always in the set, so the restart happens even when no Explorer has
/// the cache open (no cache files yet, or none mapped right now): the setup box and the
/// Repair button promise a restart, not only a cache sweep. No `cmd`, no `taskkill`, no
/// `del`: a program force-killing `explorer.exe` and deleting files through `cmd` is how
/// VirusTotal's sandbox described the old one-liner (sigma "File Deletion Via Del"), and what
/// behaviour-based antivirus scores. The files are deleted best-effort either way; one
/// Explorer still holds open simply stays.
///
/// Returns whether Explorer closed when asked. `false` (logged) when it refused or Restart
/// Manager could not take it: Explorer then simply keeps running, nothing restarted, unless
/// it was left half-closed with the taskbar already gone ([`finish_half_closed`]).
fn cycle_explorer(cache: &[std::path::PathBuf]) -> bool {
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::RestartManager::{RmRegisterResources, RmRestart, RmShutdown};
    let mut explorers = explorers_holding(cache);
    if let Some(shell) = shell_process() {
        if !explorers.iter().any(|p| p.dwProcessId == shell.dwProcessId) {
            explorers.push(shell);
        }
    }
    let session = (!explorers.is_empty()).then(RmSession::start).flatten();
    let registered = session.as_ref().is_some_and(|s| unsafe {
        RmRegisterResources(s.0, None, Some(&explorers), None) == ERROR_SUCCESS
    });
    // 0: ask, never force. An Explorer that will not close keeps running untouched.
    let closed = registered
        && session.as_ref().is_some_and(|s| {
            let shut = unsafe { RmShutdown(s.0, 0, None) };
            if shut != ERROR_SUCCESS {
                crate::safety::log(&format!(
                    "explorer restart: Explorer did not close when asked (error {})",
                    shut.0
                ));
            }
            shut == ERROR_SUCCESS
        });
    if registered && !closed {
        if let Some(s) = &session {
            finish_half_closed(s.0, &explorers);
        }
    }
    for file in cache {
        let _ = std::fs::remove_file(file);
    }
    // Even after a refusal: Restart Manager starts again whatever it did close.
    if registered {
        if let Some(s) = &session {
            let _ = unsafe { RmRestart(s.0, None, None) };
        }
    }
    closed
}

/// Whether the taskbar (`Shell_TrayWnd`) exists in this session. The WINDOW rather than an
/// `explorer.exe` process: a process exists the instant it starts, while the window only
/// appears once the shell is actually up and serving, which is what the user cares about.
fn taskbar_is_up() -> bool {
    use windows::core::w;
    use windows::Win32::UI::WindowsAndMessaging::FindWindowW;
    unsafe { FindWindowW(w!("Shell_TrayWnd"), None).is_ok() }
}

/// Whether `p` is still running: the same id AND the same start time, so a recycled id is
/// never mistaken for it.
fn still_running(p: &windows::Win32::System::RestartManager::RM_UNIQUE_PROCESS) -> bool {
    use windows::Win32::Foundation::{CloseHandle, FILETIME, STILL_ACTIVE};
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, p.dwProcessId)
        else {
            return false;
        };
        let [mut created, mut exited, mut kernel, mut user] = [FILETIME::default(); 4];
        let timed = GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user);
        let mut code = 0u32;
        let coded = GetExitCodeProcess(process, &mut code);
        let _ = CloseHandle(process);
        timed.is_ok()
            && coded.is_ok()
            && code == STILL_ACTIVE.0 as u32
            && created.dwLowDateTime == p.ProcessStartTime.dwLowDateTime
            && created.dwHighDateTime == p.ProcessStartTime.dwHighDateTime
    }
}

/// What to do once Restart Manager reports that an Explorer did not close when asked, from
/// what is on screen and how many of the Explorers asked are still running after the grace.
#[derive(Debug, PartialEq, Eq)]
enum AfterRefusal {
    /// The taskbar is up: Explorer refused cleanly and nothing was disturbed. Leave it be (it
    /// may be in the middle of a file copy); asking, never forcing, is the rule.
    LeaveRunning,
    /// The taskbar is gone and everything asked has exited after all: the restart can go on.
    Exited,
    /// The taskbar is gone but an Explorer asked is still running: half-closed (issue #68).
    Force,
}

fn after_refusal(taskbar_up: bool, still_running: usize) -> AfterRefusal {
    if taskbar_up {
        AfterRefusal::LeaveRunning
    } else if still_running == 0 {
        AfterRefusal::Exited
    } else {
        AfterRefusal::Force
    }
}

/// How long a half-closed Explorer gets to finish exiting on its own before it is forced.
const HALF_CLOSED_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

/// Issue #68 (Windows 11 26H2, a File Explorer window open): Restart Manager reported "did not
/// close when asked" (`ERROR_FAIL_SHUTDOWN`, 351) and the taskbar vanished and never came back,
/// because the shell had torn its taskbar down but was still running when Restart Manager
/// stopped waiting. Counted as refused, it is never restarted, and while it lingers a plain
/// `explorer.exe` hands its request to that process and exits, so the direct relaunch in
/// [`restart_explorer_clearing_cache`] could not bring the taskbar back either. Once the taskbar
/// is gone there is no working shell left to protect: give the Explorers asked a grace period
/// to finish exiting, then let Restart Manager force the rest in the same session, so its
/// restart (and our relaunch, if that is still needed) starts a clean shell.
fn finish_half_closed(
    session: u32,
    asked: &[windows::Win32::System::RestartManager::RM_UNIQUE_PROCESS],
) {
    use windows::Win32::System::RestartManager::{RmForceShutdown, RmShutdown};
    let running = || asked.iter().filter(|p| still_running(p)).count();
    let deadline = std::time::Instant::now() + HALF_CLOSED_GRACE;
    while running() > 0 && !taskbar_is_up() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let left = running();
    match after_refusal(taskbar_is_up(), left) {
        AfterRefusal::LeaveRunning | AfterRefusal::Exited => {}
        AfterRefusal::Force => {
            let forced = unsafe { RmShutdown(session, RmForceShutdown.0 as u32, None) };
            crate::safety::log(&format!(
                "explorer restart: taskbar gone but {left} Explorer process(es) still running \
                 after {}s - forced through Restart Manager (result {})",
                HALF_CLOSED_GRACE.as_secs(),
                forced.0
            ));
        }
    }
}

/// Holds the per-user Explorer-restart lock for as long as it lives.
struct RestartLock(#[allow(dead_code, reason = "held only to keep the lock open")] std::fs::File);

/// How long a restart waits for another one already running before going ahead anyway: a
/// restart takes ~30 s at worst (the close, the grace, two waits for the shell), so this only
/// runs out on a lock left by something stuck.
const RESTART_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(120);

/// One Explorer restart at a time for this user. Two at once fight: each Restart Manager pass
/// closes the shell the other has just brought back, which is the desktop "flashing between a
/// black screen and Explorer with an empty desktop during setup" a 3.7.4 uninstall survey
/// reported (setup's "Restart File Explorer" box and the welcome window's badge switch each
/// start one, detached, seconds apart). A file opened without sharing, not a named mutex:
/// setup opens Settings elevated, and the user's normal processes cannot open an elevated
/// process's mutex, while a file in the user's own `LOCALAPPDATA` serves both. The lock is
/// released when the handle closes, so a crashed restart cannot hold it.
fn restart_lock() -> Option<RestartLock> {
    let dir = std::env::var_os("LOCALAPPDATA")?;
    lock_file(
        &std::path::Path::new(&dir).join("SageThumbs2K.restart.lock"),
        RESTART_LOCK_WAIT,
    )
}

/// Open `path` with no sharing, waiting up to `wait` while another holder has it. `None` when
/// the wait runs out or the file cannot be opened at all; the caller then goes ahead unlocked.
fn lock_file(path: &std::path::Path, wait: std::time::Duration) -> Option<RestartLock> {
    use std::os::windows::fs::OpenOptionsExt;
    const ERROR_SHARING_VIOLATION: i32 = 32;
    let deadline = std::time::Instant::now() + wait;
    let mut waited = false;
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .share_mode(0)
            .open(path)
        {
            Ok(file) => return Some(RestartLock(file)),
            Err(e)
                if e.raw_os_error() == Some(ERROR_SHARING_VIOLATION)
                    && std::time::Instant::now() < deadline =>
            {
                if !waited {
                    crate::safety::log("explorer restart: another restart is running - waiting");
                    waited = true;
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
            Err(_) => return None,
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
/// Returns whether Explorer restarted and the shell is confirmed back. Callers treat it as
/// best-effort: there is nothing useful left to do about `false` except not claim success.
pub fn restart_explorer_clearing_cache() -> bool {
    // Serialized per user; see `restart_lock`. Held to the end of this function.
    let _lock = restart_lock();
    let shell_is_up = taskbar_is_up;
    // Read before the restart closes them; reopened once the taskbar is back.
    let open = folders::open_folders();
    let settle = std::time::Duration::from_secs(3);

    let cycled = cycle_explorer(&thumbcache_files());
    if wait_for_shell(&shell_is_up) {
        folders::reopen_missing(open, settle);
        return cycled;
    }
    crate::safety::log("explorer did not return after the cache rebuild - relaunching directly");
    let _ = Command::new("explorer.exe")
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
    if wait_for_shell(&shell_is_up) {
        folders::reopen_missing(open, settle);
        return cycled;
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

    /// Issue #68's rule: a close Explorer refused is forced only once the taskbar is already
    /// gone. With the taskbar up Explorer is healthy and may be mid-copy, so it is never forced.
    #[test]
    fn a_refused_close_is_forced_only_once_the_taskbar_is_gone() {
        assert_eq!(after_refusal(true, 1), AfterRefusal::LeaveRunning);
        assert_eq!(after_refusal(true, 0), AfterRefusal::LeaveRunning);
        assert_eq!(after_refusal(false, 0), AfterRefusal::Exited);
        assert_eq!(after_refusal(false, 2), AfterRefusal::Force);
    }

    /// Two restarts never run at once (the 3.7.4 setup survey's flashing desktop): while one
    /// holds the lock a second cannot take it, and it is free again the moment the first ends.
    #[test]
    fn a_second_restart_waits_for_the_first() {
        let dir = std::env::temp_dir().join(format!("st2k_restart_lock_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("restart.lock");
        let short = std::time::Duration::from_millis(300);
        let first = lock_file(&path, short).expect("the first restart takes the lock");
        assert!(
            lock_file(&path, short).is_none(),
            "a second must not get in"
        );
        drop(first);
        assert!(
            lock_file(&path, short).is_some(),
            "free once the first ends"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The taskbar's Explorer, as `shell_process` names it, is a process Restart Manager
    /// accepts and knows as Explorer. With a wrong start time it drops the process without a
    /// word, and the restart silently does not happen. Registers and lists it; shuts nothing
    /// down. Skipped where there is no taskbar (a CI runner's session).
    #[test]
    fn restart_manager_knows_the_taskbar_process_as_explorer() {
        use windows::Win32::Foundation::ERROR_SUCCESS;
        use windows::Win32::System::RestartManager::{
            RmExplorer, RmGetList, RmRegisterResources, RM_PROCESS_INFO,
        };
        let Some(shell) = shell_process() else {
            eprintln!("no taskbar in this session; skipped");
            return;
        };
        let session = RmSession::start().expect("a Restart Manager session");
        let registered = unsafe { RmRegisterResources(session.0, None, Some(&[shell]), None) };
        assert_eq!(registered, ERROR_SUCCESS);
        let mut list = vec![RM_PROCESS_INFO::default(); 4];
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
        assert_eq!(got, ERROR_SUCCESS, "{needed} processes");
        list.truncate(count as usize);
        assert!(
            list.iter()
                .any(|p| p.Process.dwProcessId == shell.dwProcessId
                    && p.ApplicationType == RmExplorer),
            "Restart Manager did not list pid {} as Explorer",
            shell.dwProcessId
        );
    }
}
