//! Is another **Space-bar previewer** (QuickLook, Seer) already on this machine?
//!
//! Asked once, by [`crate::first_run::seed_fresh_defaults`], to choose Quick preview's
//! default on a brand-new install: ON, unless one of these is here. Each grabs Space in
//! Explorer exactly the way ours does, so two at once fight over every press.
//!
//! Four places, because each misses some installs: Add/Remove Programs (the installers), the
//! Run keys (autostart), `%LOCALAPPDATA%\Packages` (the Microsoft Store builds, which never
//! appear in Add/Remove Programs), and the running processes (an unzipped portable copy,
//! which appears nowhere else). A false positive only means Quick preview starts off, the
//! state every install had before this; the matchers stay whole-word so it stays rare.

use std::path::PathBuf;

use windows::core::PWSTR;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::ProcessStatus::K32EnumProcesses;
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_registry::{CURRENT_USER, LOCAL_MACHINE};

/// The previewers, by the name each ships under: its Add/Remove Programs name, its EXE stem
/// and its Store package name.
const RIVALS: &[&str] = &["QuickLook", "Seer", "WinQuickLook"];

/// Is one of [`RIVALS`] installed for this user, or running?
pub(crate) fn installed() -> bool {
    uninstall_names().iter().any(|n| display_name_is_rival(n))
        || run_commands().iter().any(|c| command_is_rival(c))
        || package_dirs().iter().any(|d| package_dir_is_rival(d))
        || running_images().iter().any(|p| command_is_rival(p))
}

fn is_rival_word(word: &str) -> bool {
    RIVALS.iter().any(|r| r.eq_ignore_ascii_case(word))
}

/// "QuickLook 3.7.3" or "Seer 4.0.2": the FIRST word names the program. A whole word, so
/// "Overseer" and "QuickLookAlike" do not count.
fn display_name_is_rival(name: &str) -> bool {
    name.split(|c: char| c.is_whitespace() || c == '(' || c == '-')
        .find(|w| !w.is_empty())
        .is_some_and(is_rival_word)
}

/// A Run-key command line (`"C:\...\QuickLook.exe" /autorun`) or a process image path: the
/// rival is the program it starts, `<rival>.exe`. The arguments after it are ignored.
fn command_is_rival(cmd: &str) -> bool {
    let cmd = cmd.trim_start().trim_start_matches('"');
    // ASCII lowercasing keeps every byte offset, so `end` indexes `cmd` too.
    let Some(end) = cmd.to_ascii_lowercase().find(".exe") else {
        return false;
    };
    let path = &cmd[..end];
    is_rival_word(path.rsplit(['\\', '/']).next().unwrap_or(path))
}

/// A Store package's folder, `<publisher id>.<name>_<publisher hash>`
/// (`21090PaddyXu.QuickLook_egxr34yet59cg`): the rival is `<name>`.
fn package_dir_is_rival(dir: &str) -> bool {
    let Some((_, rest)) = dir.split_once('.') else {
        return false;
    };
    is_rival_word(rest.split('_').next().unwrap_or(rest))
}

/// Every Add/Remove Programs display name: this user's, the machine's, and the machine's
/// 32-bit ones (a 64-bit process sees those only under `WOW6432Node`).
fn uninstall_names() -> Vec<String> {
    const PATHS: [&str; 2] = [
        r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall",
        r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall",
    ];
    let mut names = Vec::new();
    for hive in [CURRENT_USER, LOCAL_MACHINE] {
        for path in PATHS {
            let Ok(root) = hive.open(path) else {
                continue;
            };
            let Ok(entries) = root.keys() else {
                continue;
            };
            names
                .extend(entries.filter_map(|e| root.open(&e).ok()?.get_string("DisplayName").ok()));
        }
    }
    names
}

/// Every autostart command line, this user's and the machine's.
fn run_commands() -> Vec<String> {
    const RUN: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Run";
    let mut commands = Vec::new();
    for hive in [CURRENT_USER, LOCAL_MACHINE] {
        let Ok(run) = hive.open(RUN) else {
            continue;
        };
        let Ok(values) = run.values() else {
            continue;
        };
        commands.extend(values.filter_map(|(name, _)| run.get_string(&name).ok()));
    }
    commands
}

/// The folder names under `%LOCALAPPDATA%\Packages`, one per installed Store app.
fn package_dirs() -> Vec<String> {
    let Some(local) = std::env::var_os("LOCALAPPDATA") else {
        return Vec::new();
    };
    let Ok(dir) = std::fs::read_dir(PathBuf::from(local).join("Packages")) else {
        return Vec::new();
    };
    dir.flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .collect()
}

/// The image path of every process this user may query (the rest are skipped).
fn running_images() -> Vec<String> {
    let mut pids = vec![0u32; 4096];
    let mut bytes = 0u32;
    let cb = (pids.len() * size_of::<u32>()) as u32;
    if !unsafe { K32EnumProcesses(pids.as_mut_ptr(), cb, &mut bytes) }.as_bool() {
        return Vec::new();
    }
    pids.truncate(bytes as usize / size_of::<u32>());
    pids.into_iter().filter_map(image_path).collect()
}

fn image_path(pid: u32) -> Option<String> {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    let named = unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
    }
    .is_ok();
    let _ = unsafe { CloseHandle(process) };
    named.then(|| String::from_utf16_lossy(&buf[..len as usize]))
}

#[cfg(test)]
mod tests {
    use super::{command_is_rival, display_name_is_rival, package_dir_is_rival};

    /// The detection rules, one table per source. Each `false` row is a near miss a looser
    /// matcher (a substring test) would get wrong, and a wrong `true` turns Quick preview off
    /// for a user who has no other previewer.
    #[test]
    fn rival_previewers_are_recognised_by_whole_name_only() {
        for (name, want) in [
            ("QuickLook 3.7.3", true),
            ("Seer 4.0.2", true),
            ("seer", true),
            ("QuickLook (64-bit)", true),
            ("Overseer", false),
            ("QuickLookAlike 1.0", false),
            ("PowerToys (Preview)", false),
            ("", false),
        ] {
            assert_eq!(display_name_is_rival(name), want, "display name {name:?}");
        }
        for (cmd, want) in [
            (
                r#""C:\Program Files\QuickLook\QuickLook.exe" /autorun"#,
                true,
            ),
            (r"C:\Users\x\AppData\Local\Programs\Seer\Seer.EXE", true),
            (r"D:\tools\WinQuickLook.exe", true),
            (r#""C:\Program Files\Overseer\Overseer.exe""#, false),
            (r"C:\Seer\helper.exe --seer", false),
            (r"C:\Program Files\Seer", false),
        ] {
            assert_eq!(command_is_rival(cmd), want, "command {cmd:?}");
        }
        for (dir, want) in [
            ("21090PaddyXu.QuickLook_egxr34yet59cg", true),
            ("Microsoft.PowerToys_8wekyb3d8bbwe", false),
            ("QuickLook", false),
        ] {
            assert_eq!(package_dir_is_rival(dir), want, "package {dir:?}");
        }
    }
}
