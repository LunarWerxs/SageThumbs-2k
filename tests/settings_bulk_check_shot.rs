//! Space over many selected rows of Settings ▸ File types ticks or clears every one of them.
//! Through 3.8.0 the bulk check (Space, and the right-click Check / Uncheck / Toggle selected)
//! skipped every row past the first few dozen formats: it only touched rows whose list index
//! was also a valid "Menu items" entry, a guard meant for the menu list's dividers. Drives the
//! headless Settings (`--shot --tab nav_filetypes --space-all-formats`) over a scratch key.

use std::process::Command;

use windows_registry::CURRENT_USER;

#[test]
fn space_over_every_selected_format_sets_them_all_alike() {
    let root = format!(
        r"Software\SageThumbs2K\__test_bulk_check_{}",
        std::process::id()
    );
    let dir = std::env::temp_dir().join(format!("st2k_bulk_check_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    struct Cleanup(String, std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = CURRENT_USER.remove_tree(&self.0);
            let _ = std::fs::remove_dir_all(&self.1);
        }
    }
    let _cleanup = Cleanup(root.clone(), dir.clone());

    let report = dir.join("report.txt");
    let status = Command::new(env!("CARGO_BIN_EXE_SageThumbs2K"))
        .arg("--shot")
        .arg(dir.join("shot.png"))
        .args(["--tab", "nav_filetypes", "--space-all-formats", "--report"])
        .arg(&report)
        .env("ST2K_SETTINGS_ROOT", &root)
        .env_remove("ST2K_PORTABLE_INI")
        .status()
        .expect("spawn SageThumbs2K --shot");
    assert!(status.success(), "shot failed: exit {:?}", status.code());
    let text = std::fs::read_to_string(&report).expect("the shot wrote no report");
    let line = text
        .lines()
        .find_map(|l| l.strip_prefix("formats_checked="))
        .unwrap_or_else(|| panic!("no formats_checked in the report:\n{text}"));
    let (ticked, count) = line.split_once(" of ").expect("ticked of count");
    let (ticked, count): (usize, usize) = (ticked.parse().unwrap(), count.parse().unwrap());
    assert!(
        count > 100,
        "the File types list should hold every format: {count}"
    );
    let target = text
        .lines()
        .find_map(|l| l.strip_prefix("formats_space_target="))
        .unwrap_or_else(|| panic!("no formats_space_target in the report:\n{text}"));
    let want = match target {
        "1" => count,
        "0" => 0,
        t => panic!("formats_space_target={t:?}"),
    };
    assert_eq!(
        ticked, want,
        "Space over all {count} selected rows must set every one to {target}: {ticked} ticked"
    );
}
