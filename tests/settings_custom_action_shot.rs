//! The custom action's Enable switch tells the truth (Discord, 2026-10-21): "The defined quick
//! action shortcut processes even though 'Enable custom action' is disabled."
//!
//! Through 3.7.4 the switch was only set by an import or a sync pull, so on a normal open a
//! bound hotkey sat under a switch that read off, with both combos still live, and the hotkey
//! kept firing. Drives the headless Settings (`--shot --tab nav_quickaction`) over a scratch
//! settings key holding the reporter's binding, Ctrl+Shift+4.

use std::path::PathBuf;
use std::process::Command;

use windows_registry::CURRENT_USER;

/// Ctrl+Shift+4 as Settings stores it: HOTKEYF modifiers (Shift 1 | Control 2) << 8 | VK '4'.
const CTRL_SHIFT_4: u32 = (0x03 << 8) | 0x34;

struct Scratch {
    root: String,
    dir: PathBuf,
}

impl Scratch {
    fn new(case: &str) -> Self {
        let root = format!(
            r"Software\SageThumbs2K\__test_custom_action_{}_{case}",
            std::process::id()
        );
        let dir = std::env::temp_dir().join(format!(
            "st2k_custom_action_{}_{case}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        CURRENT_USER
            .create(&root)
            .and_then(|k| k.set_u32("CustomActionHotkey", CTRL_SHIFT_4))
            .expect("write the scratch hotkey");
        Self { root, dir }
    }

    /// One `--shot` of the Quick action page, with `extra` flags, and its `--report`.
    fn shot(&self, extra: &[&str]) -> String {
        let png = self.dir.join("shot.png");
        let report = self.dir.join("report.txt");
        let _ = std::fs::remove_file(&report);
        let status = Command::new(env!("CARGO_BIN_EXE_SageThumbs2K"))
            .arg("--shot")
            .arg(&png)
            .args(["--tab", "nav_quickaction", "--report"])
            .arg(&report)
            .args(extra)
            .env("ST2K_SETTINGS_ROOT", &self.root)
            .env_remove("ST2K_PORTABLE_INI")
            .status()
            .expect("spawn SageThumbs2K --shot");
        assert!(status.success(), "shot failed: exit {:?}", status.code());
        std::fs::read_to_string(&report).expect("the shot wrote no report")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = CURRENT_USER.remove_tree(&self.root);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn line<'a>(report: &'a str, name: &str) -> &'a str {
    report
        .lines()
        .find(|l| l.starts_with(&format!("{name} ")) || l.starts_with(&format!("{name}=")))
        .unwrap_or_else(|| panic!("no {name} in the report:\n{report}"))
}

#[test]
fn a_bound_hotkey_opens_with_its_switch_on_and_the_switch_turns_it_off() {
    let s = Scratch::new("switch");

    let opened = s.shot(&[]);
    assert_eq!(
        line(&opened, "custom_action_enable"),
        "custom_action_enable checked=1 enabled=1 data=0",
        "a bound hotkey must open under a switch that reads on:\n{opened}"
    );
    assert_eq!(
        line(&opened, "custom_action_hotkey"),
        format!("custom_action_hotkey checked=0 enabled=1 data={CTRL_SHIFT_4}")
    );
    assert_eq!(
        line(&opened, "custom_action_saves"),
        format!("custom_action_saves={CTRL_SHIFT_4}")
    );

    let off = s.shot(&["--click", "custom_action_enable"]);
    assert_eq!(
        line(&off, "custom_action_enable"),
        "custom_action_enable checked=0 enabled=1 data=0"
    );
    assert!(
        line(&off, "custom_action").starts_with("custom_action checked=0 enabled=0 "),
        "switched off, the action picker greys out:\n{off}"
    );
    assert_eq!(
        line(&off, "custom_action_hotkey"),
        "custom_action_hotkey checked=0 enabled=0 data=0",
        "switched off, the hotkey shows (none) and greys out:\n{off}"
    );
    assert_eq!(
        line(&off, "custom_action_saves"),
        "custom_action_saves=0",
        "switched off, Save unbinds the hotkey, so it stops firing:\n{off}"
    );
}
