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
    Command::new("cmd").arg("/c").raw_arg(line).creation_flags(CREATE_NO_WINDOW).spawn()
}

/// The taskkill → delete-thumbcache → relaunch sequence shared by the "Rebuild
/// thumbnail cache" and "Repair file associations" buttons. Errors are swallowed on
/// purpose (a missing cache file is not a failure); the chain is one `cmd` line so the
/// relaunch cannot run before the kill.
pub const RESTART_EXPLORER_CLEARING_CACHE: &str = "taskkill /f /im explorer.exe >nul 2>&1 & \
     del /f /q \"%LOCALAPPDATA%\\Microsoft\\Windows\\Explorer\\thumbcache_*.db\" >nul 2>&1 & \
     start \"\" explorer.exe";

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
        assert_eq!(stdout.trim(), "\"quoted\"", "cmd saw a mangled line: {stdout:?}");
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
        assert!(!stdout.contains('\\'), "backslash escaping leaked into cmd: {stdout:?}");
    }
}
