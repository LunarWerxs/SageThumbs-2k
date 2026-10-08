//! Our per-user Scheduled Tasks, registered through the Task Scheduler COM API.
//!
//! Every task used to go through `schtasks.exe`, the cloud-folder one through an XML file
//! written to `%TEMP%` first. VirusTotal's sandbox run of the 3.6.0 installer scored both
//! (sigma: "Scheduled Task Creation Via Schtasks.EXE" and "... From Env Variable Or
//! Potentially Suspicious Path"): a program spawning `schtasks /create`, worse from a temp
//! path, is the shape persistence malware has, and behaviour-based antivirus weighs it the
//! same way. The same task registered in-process leaves no child process and no file.
//!
//! Each call runs on a short-lived thread with its own COM apartment, so no caller's
//! apartment is touched: the Settings window's thread is single-threaded, and the resident
//! helper's thread owns a low-level keyboard hook that must never wait on a service.

use windows::core::{Result, BSTR};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, E_FAIL};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::System::TaskScheduler::{
    ITaskFolder, ITaskService, TaskScheduler, TASK_CREATE_OR_UPDATE, TASK_ENUM_HIDDEN,
    TASK_LOGON_INTERACTIVE_TOKEN,
};
use windows::Win32::System::Variant::VARIANT;

/// Run `f` against the root task folder (`\`), on a fresh thread with its own apartment.
fn with_root_folder<T: Send + 'static>(
    f: impl FnOnce(&ITaskFolder) -> Result<T> + Send + 'static,
) -> Result<T> {
    std::thread::spawn(move || unsafe {
        let init = CoInitializeEx(None, COINIT_MULTITHREADED);
        // A block, so the service and folder are released before the apartment closes.
        let out = (|| {
            let service: ITaskService =
                CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER)?;
            let local = VARIANT::default();
            service.Connect(&local, &local, &local, &local)?;
            f(&service.GetFolder(&BSTR::from("\\"))?)
        })();
        if init.is_ok() {
            CoUninitialize();
        }
        out
    })
    .join()
    .unwrap_or_else(|_| Err(E_FAIL.into()))
}

/// Register `name` from its XML definition, replacing any task of that name. It runs as the
/// calling user, only while they are signed in (an interactive token, so no password is
/// stored), at the run level the XML names.
pub fn register(name: &str, xml: &str) -> Result<()> {
    let (name, xml) = (name.to_owned(), xml.to_owned());
    with_root_folder(move |root| unsafe {
        let none = VARIANT::default();
        root.RegisterTask(
            &BSTR::from(name),
            &BSTR::from(xml),
            TASK_CREATE_OR_UPDATE.0,
            &none,
            &none,
            TASK_LOGON_INTERACTIVE_TOKEN,
            &none,
        )
        .map(drop)
    })
}

/// Delete `name`. A task that is not there is not an error.
pub fn delete(name: &str) -> Result<()> {
    let name = name.to_owned();
    with_root_folder(move |root| unsafe {
        match root.DeleteTask(&BSTR::from(name), 0) {
            Err(e) if e.code() == ERROR_FILE_NOT_FOUND.to_hresult() => Ok(()),
            other => other,
        }
    })
}

/// Delete the machine-wide task `name` an older version registered (before task names
/// carried the user's SID), if it runs as THIS user. Another account's is left alone.
pub fn delete_if_ours(name: &str) {
    if let (Some(sid), Some(xml)) = (current_user_sid(), definition(name)) {
        if xml.contains(&sid) {
            let _ = delete(name);
        }
    }
}

/// Delete every task in the root folder whose name starts with `prefix`, whoever registered
/// it, and return how many went. For the uninstaller, which runs elevated: every user's
/// tasks, signed in or not (a per-hive sweep only reaches the profiles loaded right now).
pub fn delete_all_with_prefix(prefix: &str) -> usize {
    let prefix = prefix.to_owned();
    with_root_folder(move |root| unsafe {
        let tasks = root.GetTasks(TASK_ENUM_HIDDEN.0)?;
        let mut ours = Vec::new();
        // The collection is 1-based.
        for i in 1..=tasks.Count()? {
            let name = tasks.get_Item(&VARIANT::from(i))?.Name()?.to_string();
            if name.starts_with(&prefix) {
                ours.push(name);
            }
        }
        Ok(ours
            .iter()
            .filter(|name| root.DeleteTask(&BSTR::from(name.as_str()), 0).is_ok())
            .count())
    })
    .unwrap_or(0)
}

/// `base_<user SID>`: a task that belongs to one user, so two accounts on one PC never share
/// (or overwrite) one, the way OneDrive names its per-user tasks. `None` when the token gives
/// no SID.
pub fn per_user_name(base: &str) -> Option<String> {
    current_user_sid().map(|sid| format!("{base}_{sid}"))
}

/// Start `name` now, whatever its triggers say.
pub fn run(name: &str) -> Result<()> {
    let name = name.to_owned();
    with_root_folder(move |root| unsafe {
        root.GetTask(&BSTR::from(name))?
            .Run(&VARIANT::default())
            .map(drop)
    })
}

/// The registered definition of `name`, or `None` when there is no such task.
pub fn definition(name: &str) -> Option<String> {
    let name = name.to_owned();
    with_root_folder(move |root| unsafe {
        root.GetTask(&BSTR::from(name))?
            .Xml()
            .map(|xml| xml.to_string())
    })
    .ok()
}

/// Is `name` registered? A file check: Task Scheduler keeps each task's definition at
/// `%SystemRoot%\System32\Tasks\<name>` and gives the task's own user read access to it.
/// No COM and no service round trip, for the resident helper's tray tooltip, whose thread
/// owns the keyboard hook (Windows silently drops a hook whose thread stalls).
pub fn file_present(name: &str) -> bool {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    std::path::Path::new(&root)
        .join("System32")
        .join("Tasks")
        .join(name)
        .is_file()
}

/// The program a task definition starts: its first `<Command>`, unquoted and unescaped.
pub fn command_of(xml: &str) -> Option<String> {
    let start = xml.find("<Command>")? + "<Command>".len();
    let end = start + xml[start..].find("</Command>")?;
    let raw = xml_unescape(xml[start..end].trim());
    Some(raw.trim_matches('"').to_owned())
}

/// `s` with the five XML special characters escaped, for a path or argument placed in a task
/// definition (a folder may legally be called `A&B`).
pub fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// A task that starts `command arguments` with the user's normal (never elevated) token while
/// they are signed in. `triggers` is the inner XML of `<Triggers>`; empty means it only runs
/// when asked ([`run`]). `time_limit` is an ISO 8601 duration, `PT0S` for none.
///
/// It runs on battery too, and at normal priority: Task Scheduler's defaults (stop on
/// battery, below-normal priority) would end the resident helper the moment a laptop is
/// unplugged and slow every process it starts.
pub fn exec_task_xml(
    description: &str,
    triggers: &str,
    command: &str,
    arguments: &str,
    time_limit: &str,
) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\n\
         <Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\n  \
         <RegistrationInfo>\n    <Description>{description}</Description>\n  </RegistrationInfo>\n  \
         <Triggers>\n{triggers}  </Triggers>\n  \
         <Principals>\n    <Principal id=\"Author\">\n      \
         <LogonType>InteractiveToken</LogonType>\n      <RunLevel>LeastPrivilege</RunLevel>\n    \
         </Principal>\n  </Principals>\n  \
         <Settings>\n    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>\n    \
         <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>\n    \
         <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>\n    \
         <ExecutionTimeLimit>{time_limit}</ExecutionTimeLimit>\n    \
         <Priority>5</Priority>\n    <Enabled>true</Enabled>\n  </Settings>\n  \
         <Actions Context=\"Author\">\n    <Exec>\n      <Command>\"{command}\"</Command>\n      \
         <Arguments>{arguments}</Arguments>\n    </Exec>\n  </Actions>\n</Task>\n",
        description = xml_escape(description),
        command = xml_escape(command),
        arguments = xml_escape(arguments),
    )
}

/// A `<Triggers>` entry firing when `user` (a SID or `DOMAIN\name`) signs in, `delay` (ISO
/// 8601) later.
pub fn logon_trigger(user: &str, delay: &str) -> String {
    format!(
        "    <LogonTrigger>\n      <Enabled>true</Enabled>\n      <UserId>{}</UserId>\n      \
         <Delay>{delay}</Delay>\n    </LogonTrigger>\n",
        xml_escape(user)
    )
}

/// The calling user's SID as a string (`S-1-5-21-...`), from the process token.
pub fn current_user_sid() -> Option<String> {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL};
    use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).ok()?;
        let mut len = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut len);
        // u64 cells: TOKEN_USER holds a pointer, so the buffer must be pointer-aligned.
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        let read = GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr().cast()),
            len,
            &mut len,
        );
        let _ = CloseHandle(token);
        read.ok()?;
        let user = &*buf.as_ptr().cast::<TOKEN_USER>();
        let mut text = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut text).ok()?;
        let sid = text.to_string().ok();
        let _ = LocalFree(Some(HLOCAL(text.0.cast())));
        sid
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The contract every caller leans on, against the real Task Scheduler: a definition built
    /// here, with a sign-in trigger for this user's SID, registers (Task Scheduler rejects an
    /// unknown user, so the SID is checked too), reads back with its command intact (a path
    /// with `&` included), shows up to the no-COM presence check, and deletes; deleting it
    /// again is not an error. Nothing else exercises registration, and a wrong flag or a
    /// malformed element fails only here, at run time.
    #[test]
    fn a_task_registers_reads_back_and_deletes() {
        let name = format!("SageThumbs2K_Test_{}", std::process::id());
        let exe = r"C:\Program Files\A&B\SageThumbs2K.exe";
        let sid = current_user_sid().expect("this process's user SID");
        let trigger = logon_trigger(&sid, "PT5S");
        let xml = exec_task_xml("test", &trigger, exe, "--screenshot-daemon", "PT0S");
        let registered = register(&name, &xml);
        let back = definition(&name);
        let present = file_present(&name);
        let deleted = delete(&name);
        registered.expect("register");
        deleted.expect("delete");
        let back = back.expect("a registered task reads back");
        assert_eq!(command_of(&back).as_deref(), Some(exe), "{back}");
        assert!(
            back.contains("<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>"),
            "{back}"
        );
        assert!(present, "the presence check must see a registered task");
        assert!(definition(&name).is_none(), "deleted task still registered");
        assert!(!file_present(&name));
        delete(&name).expect("deleting a missing task is not an error");
    }

    /// The uninstall sweep: every task under the prefix goes, a task outside it stays. This
    /// is what keeps an uninstall from leaving a signed-out user's sign-in task behind,
    /// pointing at a deleted exe.
    #[test]
    fn the_uninstall_sweep_deletes_every_task_under_the_prefix() {
        let prefix = format!("SageThumbs2K_Sweep_{}_", std::process::id());
        let outside = format!("SageThumbs2K_Kept_{}", std::process::id());
        let xml = exec_task_xml(
            "test",
            "",
            r"C:\Windows\System32\cmd.exe",
            "/c exit",
            "PT1M",
        );
        for name in [format!("{prefix}a"), format!("{prefix}b"), outside.clone()] {
            register(&name, &xml).expect("register");
        }
        let swept = delete_all_with_prefix(&prefix);
        let kept = definition(&outside).is_some();
        let _ = delete(&outside);
        assert_eq!(swept, 2);
        assert!(definition(&format!("{prefix}a")).is_none());
        assert!(definition(&format!("{prefix}b")).is_none());
        assert!(kept, "a task outside the prefix must survive the sweep");
    }
}
