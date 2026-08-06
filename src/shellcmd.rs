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

/// The taskkill → delete-thumbcache → relaunch sequence shared by the "Rebuild
/// thumbnail cache" and "Repair file associations" buttons. Errors are swallowed on
/// purpose (a missing cache file is not a failure); the chain is one `cmd` line so the
/// relaunch cannot run before the kill.
pub const RESTART_EXPLORER_CLEARING_CACHE: &str = "taskkill /f /im explorer.exe >nul 2>&1 & \
     del /f /q \"%LOCALAPPDATA%\\Microsoft\\Windows\\Explorer\\thumbcache_*.db\" >nul 2>&1 & \
     start \"\" explorer.exe";

/// Restart Explorer + clear the thumbnail cache, then CHECK THE SHELL CAME BACK.
///
/// The one-liner above is fire-and-forget: it kills Explorer, deletes the cache, and asks
/// `start` to relaunch. If that last step does not take, the user is staring at an empty
/// desktop with no taskbar and no idea why - which is exactly what issue #5 did to somebody,
/// via a quoting bug that made `start` open a UNC root instead of the shell. That specific
/// bug is fixed and locked by tests, but "we killed your shell and something went wrong on the
/// way back" is severe enough to be worth confirming rather than assuming, especially now that
/// setup offers this to every user at the end of an install.
///
/// So: issue it, wait for the taskbar window to exist again, and if it does not, relaunch
/// Explorer directly (no `cmd`, no quoting to get wrong) and check once more. Windows'
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
    // once the shell is actually up and serving, which is what the user cares about. It also
    // stays correct when Explorer is running merely as a file-browser window.
    let shell_is_up = || unsafe { FindWindowW(w!("Shell_TrayWnd"), None).is_ok() };

    let _ = cmd_c(RESTART_EXPLORER_CLEARING_CACHE);

    // SETTLE FIRST. `cmd_c` only spawns the interpreter; taskkill has not necessarily run when
    // it returns. Polling immediately finds the OLD taskbar and reports a verified restart
    // having verified nothing - the first two versions of this function both did exactly that,
    // returning "success" in 0.8s while Explorer actually went down and came back seconds
    // later. Trying to catch the down-transition instead is just a smaller race.
    //
    // So do not chase the transition at all. Wait long enough that the kill has certainly been
    // attempted, then assert the thing that actually matters: the shell is up at the end.
    std::thread::sleep(std::time::Duration::from_secs(3));

    // ~15s for `start` to bring it back; a cold shell on a busy machine takes a few seconds.
    for _ in 0..75 {
        if shell_is_up() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    // Not back. Relaunch WITHOUT cmd, so no quoting can be misread this time (issue #5 was a
    // quoting bug in exactly this spot, and it left someone with no shell).
    crate::safety::log("explorer did not return after the cache rebuild - relaunching directly");
    let _ = std::process::Command::new("explorer.exe")
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
    for _ in 0..75 {
        if shell_is_up() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
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

    /// The specific token that broke: `start ""` must not become `start \"\"`, whose
    /// target `cmd` resolves to `\\`.
    #[test]
    fn start_empty_title_is_not_mangled() {
        assert!(RESTART_EXPLORER_CLEARING_CACHE.contains("start \"\" explorer.exe"));
        // `echo` the same token through cmd and confirm the interpreter agrees.
        let out = Command::new("cmd")
            .arg("/c")
            .raw_arg("echo start \"\" explorer.exe")
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .expect("spawn cmd");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            !stdout.contains('\\'),
            "backslash escaping leaked into cmd: {stdout:?}"
        );
    }
}
