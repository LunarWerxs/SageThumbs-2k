//! The selection in voidtools Everything: finding its result list and reading the focused row across the process boundary.

use super::*;

/// Everything's hidden "what is the result list focused on" child window. Its window TEXT is the
/// FULL path of that result — always the full path, whatever the result list's own column
/// settings show — and Everything keeps it current as the focus moves.
pub(super) const EVERYTHING_FOCUS_CLASS: PCWSTR = w!("EVERYTHING_RESULT_LIST_FOCUS");

/// Whether a window class belongs to an Everything search window.
///
/// Everything names its window class after the RUNNING INSTANCE: `EVERYTHING` for the default
/// one (1.4, and 1.5 from beta on), `EVERYTHING_(1.5a)` while the 1.5 alpha's `alpha_instance`
/// setting was on, `EVERYTHING_(<name>)` for `-instance <name>` — which portable copies use to
/// run beside an installed one. So the STEM is the only stable part of the name; match that, as
/// QuickLook does, and every instance counts without the user editing an Everything setting.
///
/// This deliberately also matches Everything's hidden `EVERYTHING_TASKBAR_NOTIFICATION` window.
/// That one is never foreground and has no focus child, so both callers reject it anyway, and a
/// narrower rule would just be a second thing to keep in step with voidtools' naming.
pub(crate) fn is_everything_class(cls: &str) -> bool {
    cls.starts_with("EVERYTHING")
}

/// The [`EVERYTHING_FOCUS_CLASS`] child of an Everything window, if it publishes one.
///
/// Everything 1.5 added this window so an external previewer could read the focused result (it
/// is how QuickLook and Seer do it). It is the PREFERRED source by a distance: it hands over a
/// full path directly, with no dependence on which columns the user happens to show. Only when
/// it is absent — i.e. on 1.4 — does [`everything_listview_path`] take over.
pub(crate) unsafe fn everything_focus_window(fg: HWND) -> Option<HWND> {
    let h = FindWindowExW(Some(fg), None, EVERYTHING_FOCUS_CLASS, PCWSTR::null()).ok()?;
    (!h.0.is_null()).then_some(h)
}

/// Everything **1.4**'s result list — the fallback source for builds with no focus window.
///
/// 1.4 predates [`EVERYTHING_FOCUS_CLASS`] entirely, but its result list is an ordinary
/// `SysListView32` (verified against 1.4.1.1032), so the focused row can be read out of the
/// control itself. Finding the child sends NO message, which is what makes this safe to call
/// from the Space hook's gate; the reads that DO send messages all happen later, off the hook.
pub(crate) unsafe fn everything_result_list(fg: HWND) -> Option<HWND> {
    let h = FindWindowExW(Some(fg), None, w!("SysListView32"), PCWSTR::null()).ok()?;
    (!h.0.is_null()).then_some(h)
}

/// The file Everything currently has focused in its result list, or `None` when the foreground
/// isn't an Everything window, when neither source can answer, or when the user is TYPING in the
/// search box.
///
/// The typing check is load-bearing, not belt-and-braces, and it guards BOTH sources: the focus
/// window keeps its last value after the result list loses focus, and the list keeps its focused
/// row for the same reason — so without it a stale path would be handed out while the caret sits
/// in the search box (and the hotkey path has no other guard). Everything's search box is a real
/// `Edit`, so a focused one reports a caret through `GetGUIThreadInfo`; the result list is a
/// `SysListView32` and reports none — measured against 1.5.0.1420b and 1.4.1.1032 alike.
pub(super) unsafe fn everything_selection() -> Option<String> {
    let fg = GetForegroundWindow();
    if fg.0.is_null() || !is_everything_class(&class_name(fg)) {
        return None;
    }
    // Issue #209/P15: the class-name check above matches any local process that
    // registers a window whose class merely STARTS WITH "EVERYTHING" — nothing
    // stops another process on the same desktop from doing that and becoming
    // foreground. Without this, a spoofed window's self-reported text/cells would
    // be trusted as the selection and fed straight into a hotkey-bound action
    // (Upload, Convert, StripMetadata, …) that mutates files or talks to the
    // network. Verify the window's OWNING PROCESS is really some build of
    // Everything before trusting anything it reports.
    if !owning_process_is_everything(fg) {
        return None;
    }
    if caret_active(fg) {
        return None;
    }
    everything_focus_path(fg).or_else(|| everything_listview_path(fg))
}

/// Best-effort check that `hwnd` is owned by a process whose EXE is (some build
/// of) Everything, by name — `voidtools` ships `Everything.exe`, `Everything64.exe`,
/// alpha builds like `Everything-1.5a.exe`, and portable copies renamed with a
/// leading "Everything" (matching how [`is_everything_class`] treats the window
/// class's stem as the only stable part of the name). Any failure to resolve the
/// owning process's image path is treated as "not Everything" — the safe default
/// for a check that exists to keep an untrusted window from being trusted.
pub(super) unsafe fn owning_process_is_everything(hwnd: HWND) -> bool {
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == 0 {
        return false;
    }
    let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
        return false;
    };
    let mut buf = [0u16; 260];
    let mut len = buf.len() as u32;
    let ok = QueryFullProcessImageNameW(
        process,
        PROCESS_NAME_WIN32,
        PWSTR(buf.as_mut_ptr()),
        &mut len,
    )
    .is_ok();
    let _ = CloseHandle(process);
    if !ok {
        return false;
    }
    std::path::Path::new(&String::from_utf16_lossy(&buf[..len as usize]))
        .file_name()
        .and_then(|f| f.to_str())
        .is_some_and(is_everything_exe_name)
}

/// Is `file_name` (just the EXE's own name, no directory) one voidtools would ship —
/// pure and separately testable, unlike the process-handle plumbing around it.
pub(crate) fn is_everything_exe_name(file_name: &str) -> bool {
    let f = file_name.to_ascii_lowercase();
    f.starts_with("everything") && f.ends_with(".exe")
}

/// The focused result as Everything 1.5+ publishes it: the window TEXT of its hidden focus child.
pub(super) unsafe fn everything_focus_path(fg: HWND) -> Option<String> {
    let hidden = everything_focus_window(fg)?;
    // Cross-process, `GetWindowTextLengthW` may over-report, so bound it before allocating
    // (the longest path Windows can name) and REFUSE past the bound rather than truncate: a
    // cut-off path would hand the action a different file. The copy's return is the exact count.
    const MAX_PATH_CCH: i32 = 32_767;
    let n = GetWindowTextLengthW(hidden);
    if n <= 0 || n > MAX_PATH_CCH {
        return None;
    }
    let mut buf = vec![0u16; n as usize + 1];
    let got = GetWindowTextW(hidden, &mut buf);
    if got <= 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..got as usize]))
}

/// Every cross-process list-view query is a blocking `SendMessage` into EVERYTHING's message
/// loop, so all of them are bounded. `SMTO_ABORTIFHUNG` plus this timeout keep an Everything
/// that is mid-rebuild from parking the preview on a wedged window.
pub(super) const LV_TIMEOUT_MS: u32 = 200;

/// Ceiling on how many cells of the focused row we read. Everything 1.4 ships four columns
/// (Name / Path / Size / Date Modified); this only stops a very wide custom layout from turning
/// one keypress into dozens of round trips.
pub(super) const LV_MAX_CELLS: i32 = 12;

/// Wide chars of the scratch text buffer allocated inside Everything's address space.
pub(super) const LV_TEXT_CCH: usize = 1024;

/// A scratch buffer allocated inside ANOTHER process, released with its process handle on drop.
///
/// `LVM_GETITEMTEXTW` is handed a `LVITEMW` whose `pszText` must be valid in the LIST VIEW's
/// address space, not ours — so both the struct and the buffer it points at have to live over
/// there. This is the standard way to read another process's list view; nothing is injected and
/// nothing is left behind.
pub(super) struct RemoteScratch {
    pub(super) process: HANDLE,
    pub(super) base: *mut c_void,
}

impl RemoteScratch {
    /// Open `pid` for the three VM rights this needs and reserve `size` bytes in it. `None` if
    /// the process is out of reach — an ELEVATED Everything is the case that hits this, and
    /// failing closed there is right: Windows would withhold its keystrokes from us anyway.
    pub(super) unsafe fn open(pid: u32, size: usize) -> Option<Self> {
        let process = OpenProcess(
            PROCESS_VM_OPERATION | PROCESS_VM_READ | PROCESS_VM_WRITE,
            false,
            pid,
        )
        .ok()?;
        let base = VirtualAllocEx(
            process,
            None,
            size,
            MEM_COMMIT | MEM_RESERVE,
            PAGE_READWRITE,
        );
        if base.is_null() {
            let _ = CloseHandle(process);
            return None;
        }
        Some(Self { process, base })
    }
}

impl Drop for RemoteScratch {
    fn drop(&mut self) {
        unsafe {
            let _ = VirtualFreeEx(self.process, self.base, 0, MEM_RELEASE);
            let _ = CloseHandle(self.process);
        }
    }
}

/// Send one bounded message to a window in another process, returning its reply. `None` means
/// the target never answered inside [`LV_TIMEOUT_MS`] — treated everywhere here as "no answer",
/// never as a value.
pub(super) unsafe fn ask(hwnd: HWND, msg: u32, wparam: usize, lparam: isize) -> Option<usize> {
    let mut out = 0usize;
    let ok = SendMessageTimeoutW(
        hwnd,
        msg,
        WPARAM(wparam),
        LPARAM(lparam),
        SMTO_ABORTIFHUNG,
        LV_TIMEOUT_MS,
        Some(&mut out),
    );
    (ok.0 != 0).then_some(out)
}

/// The row the result list has FOCUSED (the one carrying the caret box), falling back to the
/// first SELECTED row. Both replies are plain integers, so neither needs the remote scratch.
pub(super) unsafe fn lv_focused_row(list: HWND) -> Option<i32> {
    for flags in [LVNI_FOCUSED, LVNI_SELECTED] {
        // wParam is the row to search AFTER; -1 means "from the start".
        let row = ask(list, LVM_GETNEXTITEM, usize::MAX, flags as isize)? as i32;
        if row >= 0 {
            return Some(row);
        }
    }
    None
}

/// How many columns the result list shows, read off its header. Anything unexpected (a hidden
/// header, a wedged window) falls back to the cap, which only costs a few extra round trips.
pub(super) unsafe fn lv_cell_count(list: HWND) -> i32 {
    let Some(header) = ask(list, LVM_GETHEADER, 0, 0).filter(|h| *h != 0) else {
        return LV_MAX_CELLS;
    };
    let header = HWND(header as *mut c_void);
    match ask(header, HDM_GETITEMCOUNT, 0, 0) {
        Some(n) => (n as i32).clamp(1, LV_MAX_CELLS),
        None => LV_MAX_CELLS,
    }
}

/// One cell of one row, read out of a list view owned by another process.
pub(super) unsafe fn lv_cell(
    list: HWND,
    scratch: &RemoteScratch,
    row: i32,
    cell: i32,
) -> Option<String> {
    let text_at = scratch.base.byte_add(core::mem::size_of::<LVITEMW>());
    let item = LVITEMW {
        mask: LVIF_TEXT,
        iItem: row,
        iSubItem: cell,
        pszText: PWSTR(text_at as *mut u16),
        cchTextMax: LV_TEXT_CCH as i32,
        ..Default::default()
    };
    WriteProcessMemory(
        scratch.process,
        scratch.base,
        (&raw const item).cast(),
        core::mem::size_of::<LVITEMW>(),
        None,
    )
    .ok()?;
    // The reply is the character count actually written into the REMOTE buffer.
    let got = ask(list, LVM_GETITEMTEXTW, row as usize, scratch.base as isize)?;
    let n = got.min(LV_TEXT_CCH);
    if n == 0 {
        return Some(String::new());
    }
    let mut buf = vec![0u16; n];
    ReadProcessMemory(
        scratch.process,
        text_at,
        buf.as_mut_ptr().cast(),
        n * core::mem::size_of::<u16>(),
        None,
    )
    .ok()?;
    Some(String::from_utf16_lossy(&buf))
}

/// The file focused in an Everything **1.4** result list, reconstructed from the row's cells.
pub(super) unsafe fn everything_listview_path(fg: HWND) -> Option<String> {
    let list = everything_result_list(fg)?;
    let row = lv_focused_row(list)?;
    let mut pid = 0u32;
    GetWindowThreadProcessId(list, Some(&mut pid));
    if pid == 0 {
        return None;
    }
    let scratch = RemoteScratch::open(
        pid,
        core::mem::size_of::<LVITEMW>() + LV_TEXT_CCH * core::mem::size_of::<u16>(),
    )?;
    let count = lv_cell_count(list);
    let mut cells = Vec::with_capacity(count as usize);
    for cell in 0..count {
        cells.push(lv_cell(list, &scratch, row, cell)?);
    }
    resolve_result_row(&cells, |p| std::path::Path::new(p).exists())
}

/// Turn the cells of one focused Everything result row into a full path.
///
/// Everything's columns are user-configurable AND localized, so identifying the path column by
/// its TITLE would break on 1.4's language packs and on any custom layout. This rule needs
/// neither: cell 0 is the item's name, so the first later cell that JOINS with it into an
/// existing path is the directory column.
///
/// **The order is load-bearing.** A Path cell is itself an existing directory, so testing bare
/// cells first resolves every row to its PARENT folder — which reads as working right up until
/// you notice Space previewed the containing folder instead of the file you picked.
///
/// `exists` is injected so the rule is testable without touching a disk.
pub(super) fn resolve_result_row(
    cells: &[String],
    exists: impl Fn(&str) -> bool,
) -> Option<String> {
    let name = cells.first().map(String::as_str).unwrap_or_default();
    if !name.is_empty() {
        for dir in cells.iter().skip(1).filter(|c| !c.is_empty()) {
            let joined = join_under(dir, name);
            if exists(&joined) {
                return Some(joined);
            }
        }
    }
    // A "Full Path & Name" column carries the whole thing in ONE cell, so nothing joins.
    cells
        .iter()
        .find(|c| is_rooted_path(c) && exists(c))
        .cloned()
}

/// `dir` + `name`, tolerating a trailing separator and a bare drive letter. `C:` + `x` must
/// become `C:\x`; plain concatenation would give `C:x`, which means "x relative to C:'s current
/// directory" — a different file, and usually a nonexistent one.
pub(super) fn join_under(dir: &str, name: &str) -> String {
    format!("{}\\{}", dir.trim_end_matches(['\\', '/']), name)
}

/// Whether a cell is a rooted path — `X:\…` or a `\\server\share` UNC. Everything can also list
/// results from an ETP/FTP server, whose "path" no file API can open; those never match, so such
/// a row simply yields nothing rather than a path that fails later.
pub(super) fn is_rooted_path(p: &str) -> bool {
    let b = p.as_bytes();
    (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/'))
        || p.starts_with("\\\\")
}
