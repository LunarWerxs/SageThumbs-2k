//! A private temp folder per magick child, removed once the child is reaped (issue #56).
//!
//! ImageMagick writes `magick-<32 chars>` files to its temp folder: a copy of any stdin input
//! its coder must seek in (every PSD), and its pixel cache once an image outgrows `-limit
//! memory`. It deletes them only when it exits on its own, and we kill every decode child the
//! moment its PNG arrives (and every child that runs over budget), so each PSD thumbnail could
//! leave hundreds of MB behind; one user's `%TEMP%` reached 100 GB. Each child now gets
//! `%TEMP%\SageThumbs2K-magick\<pid>-<n>` as its whole temp area, and the folder goes with it.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

/// The folder, under `%TEMP%`, that holds one subfolder per live magick child.
const ROOT: &str = "SageThumbs2K-magick";

/// A leftover older than this belongs to no live child: the longest any child may run is
/// the Full budget's 600 s wall clock.
const STALE_AFTER: Duration = Duration::from_secs(60 * 60);

/// One child's temp folder. Declare it BEFORE the child, so it drops after the reap.
pub(super) struct MagickScratch(Option<PathBuf>);

impl MagickScratch {
    /// A fresh, empty folder. When none can be made the child keeps the ordinary temp folder,
    /// and the stale sweep collects what it leaves there.
    pub(super) fn new() -> Self {
        static STARTED: std::sync::Once = std::sync::Once::new();
        STARTED.call_once(|| {
            // Its own thread: `%TEMP%` can hold thousands of entries, and a decode is waiting.
            let _ = st2k_base::safety::spawn_pinned("st2k-magick-sweep", sweep_stale_magick_temp);
        });
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(ROOT);
        if std::fs::create_dir_all(&root).is_ok() {
            for _ in 0..8 {
                let n = SEQ.fetch_add(1, Ordering::Relaxed);
                let dir = root.join(format!("{}-{n}", std::process::id()));
                if std::fs::create_dir(&dir).is_ok() {
                    return Self(Some(dir));
                }
            }
        }
        st2k_base::safety::log_debug("magick: could not make a private temp folder");
        Self(None)
    }

    /// Point every temp-file setting ImageMagick reads at this folder.
    pub(super) fn apply(&self, cmd: &mut Command) {
        if let Some(dir) = &self.0 {
            cmd.env("MAGICK_TEMPORARY_PATH", dir)
                .env("TMP", dir)
                .env("TEMP", dir);
        }
    }

    #[cfg(test)]
    pub(super) fn dir(&self) -> Option<&Path> {
        self.0.as_deref()
    }
}

impl Drop for MagickScratch {
    fn drop(&mut self) {
        let Some(dir) = self.0.take() else {
            return;
        };
        // The child is reaped by now, but an antivirus scan can hold a file a moment longer.
        for attempt in 0..4 {
            if std::fs::remove_dir_all(&dir).is_ok() || !dir.exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(25 << attempt));
        }
        st2k_base::safety::log_debugf!("magick: could not remove {}", dir.display());
    }
}

/// Delete what earlier runs left in `%TEMP%`: see [`sweep_stale_in`]. Runs once per process at
/// the first magick child, and once after every install.
pub fn sweep_stale_magick_temp() {
    sweep_stale_in(&std::env::temp_dir());
}

/// Delete the leftovers in `temp` older than [`STALE_AFTER`]: our per-child folders whose owner
/// died before removing them, our staged inputs (`st2k-coder-*`), and the loose
/// `magick-<32 chars>` files earlier versions left. A file still open is skipped, because
/// Windows refuses to delete it, so a live ImageMagick run, ours or anyone's, keeps its files.
fn sweep_stale_in(temp: &Path) {
    let now = SystemTime::now();
    // Read fresh, not from the directory listing, whose copy of the time can lag.
    let stale = |e: &std::fs::DirEntry| {
        std::fs::metadata(e.path())
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > STALE_AFTER)
    };
    if let Ok(children) = std::fs::read_dir(temp.join(ROOT)) {
        for e in children.flatten().filter(stale) {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
    if let Ok(entries) = std::fs::read_dir(temp) {
        for e in entries.flatten() {
            let is_file = e.file_type().is_ok_and(|t| t.is_file());
            if is_file && is_leftover_name(&e.file_name().to_string_lossy()) && stale(&e) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

/// ImageMagick's own temp name, `magick-` and exactly 32 characters of its file-name alphabet,
/// or one of our staged inputs. Nothing looser: this deletes files in a folder every program
/// shares, so `magick-config.xml` or another tool's `magick-cache` must never match.
fn is_leftover_name(name: &str) -> bool {
    if let Some(tail) = name.strip_prefix("magick-") {
        return tail.len() == 32
            && tail
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    }
    name.starts_with("st2k-coder-")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A magick child killed mid-run (the decode path kills every child) leaves nothing: its
    /// temp file lands in the private folder, and the folder goes when the guard drops.
    #[test]
    fn a_killed_magick_child_leaves_no_temp_files() {
        use std::io::Write;
        use std::os::windows::process::CommandExt;
        let Some(exe) = super::super::magick_exe() else {
            return;
        };
        let scratch = MagickScratch::new();
        let dir = scratch.dir().expect("a private temp folder").to_path_buf();
        let mut cmd = Command::new(exe);
        cmd.args(["psd:-", "PNG:-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(st2k_base::host::CREATE_NO_WINDOW);
        super::super::apply_magick_environment(&mut cmd, exe);
        scratch.apply(&mut cmd);
        let mut child = cmd.spawn().expect("magick starts");
        // A PSD needs a seekable input, so magick copies stdin to a temp file as it arrives.
        // Keep stdin open: the copy never finishes, exactly like a child killed mid-decode.
        let mut stdin = child.stdin.take().expect("stdin pipe");
        let _ = stdin.write_all(&vec![0u8; 1 << 20]);
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let spilled = loop {
            let any = std::fs::read_dir(&dir).is_ok_and(|mut d| d.next().is_some());
            if any || std::time::Instant::now() > deadline {
                break any;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let _ = child.kill();
        let _ = child.wait();
        drop(stdin);
        assert!(
            spilled,
            "magick's temp file should land in {}",
            dir.display()
        );
        drop(scratch);
        assert!(!dir.exists(), "{} should be gone", dir.display());
    }

    /// The sweep deletes only old leftovers with OUR names or ImageMagick's exact temp name.
    /// Each kept row is a file that a looser rule (a prefix match, no age check) would delete
    /// from a folder every program shares.
    #[test]
    fn the_stale_sweep_deletes_only_old_imagemagick_leftovers() {
        let temp = std::env::temp_dir().join(format!("st2k-sweep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(temp.join(ROOT).join("1-0")).unwrap();
        std::fs::write(temp.join(ROOT).join("1-0").join("magick-x"), b"x").unwrap();
        let im_name = format!("magick-{}", "aZ09_-".repeat(6).get(..32).unwrap());
        let fresh_name = format!("magick-{}", "Q".repeat(32));
        let cases = [
            (im_name.as_str(), true, false),
            ("st2k-coder-7-3.rla", true, false),
            ("magick-config.xml", true, true),
            ("magick-0123456789012345678901234567890", true, true), // 31 characters
            (fresh_name.as_str(), false, true),
        ];
        let old = SystemTime::now() - 2 * STALE_AFTER;
        let age = |path: &Path| {
            // FILE_WRITE_ATTRIBUTES is all `set_modified` needs; BACKUP_SEMANTICS opens a folder.
            use std::os::windows::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .access_mode(0x100)
                .custom_flags(0x0200_0000)
                .open(path)
                .and_then(|f| f.set_modified(old))
                .unwrap();
        };
        for (name, is_old, _) in cases {
            std::fs::write(temp.join(name), b"x").unwrap();
            if is_old {
                age(&temp.join(name));
            }
        }
        age(&temp.join(ROOT).join("1-0"));
        std::fs::create_dir(temp.join(ROOT).join("1-1")).unwrap();

        sweep_stale_in(&temp);

        for (name, _, kept) in cases {
            assert_eq!(temp.join(name).exists(), kept, "{name}");
        }
        assert!(
            !temp.join(ROOT).join("1-0").exists(),
            "an old child folder goes"
        );
        assert!(
            temp.join(ROOT).join("1-1").exists(),
            "a live child's folder stays"
        );
        let _ = std::fs::remove_dir_all(&temp);
    }
}
