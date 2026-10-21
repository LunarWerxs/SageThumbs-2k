//! The "Edit menu items" editor, driven the way its user drives it (Discord, 2026-10-21, on
//! 3.7.4): "dragging on the list items or dividers ... doesn't do anything", "clicking the
//! 'Done' button ... would apply the changes to the context menu directly", and "add checkboxes
//! next to the separators ... to let the user decide which ones to use".
//!
//! `--shot --window menuitems` opens the editor over a headless Settings. `--drag` starts the
//! drag with the notification comctl32 really sends, at the window it really sends it to;
//! `--space` presses Space on a row; `--done` clicks Done. Each case runs over its own scratch
//! settings key, so the factory order is its starting point.

use std::path::PathBuf;
use std::process::Command;

use st2k_actions::verbs::{default_menu_tokens, MENU_SEP_OFF_TOKEN, MENU_SEP_TOKEN};
use windows_registry::CURRENT_USER;

struct Scratch {
    root: String,
    dir: PathBuf,
}

impl Scratch {
    fn new(case: &str) -> Self {
        let root = format!(
            r"Software\SageThumbs2K\__test_menu_items_{}_{case}",
            std::process::id()
        );
        let dir = std::env::temp_dir().join(format!("st2k_menu_items_{}_{case}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        Self { root, dir }
    }

    /// One editor shot with `extra` flags: its rows `(lParam, checked)` and, after `--done`,
    /// the saved `MenuOrder`.
    fn shot(&self, extra: &[&str]) -> (Vec<(i64, bool)>, Option<String>) {
        let png = self.dir.join("shot.png");
        let report = self.dir.join("report.txt");
        let _ = std::fs::remove_file(&report);
        let status = Command::new(env!("CARGO_BIN_EXE_SageThumbs2K"))
            .arg("--shot")
            .arg(&png)
            .args(["--window", "menuitems", "--report"])
            .arg(&report)
            .args(extra)
            .env("ST2K_SETTINGS_ROOT", &self.root)
            .env_remove("ST2K_PORTABLE_INI")
            .status()
            .expect("spawn SageThumbs2K --shot");
        assert!(status.success(), "shot failed: exit {:?}", status.code());
        assert!(png.is_file(), "the shot wrote no PNG");
        let text = std::fs::read_to_string(&report).expect("the shot wrote no report");
        let mut rows = Vec::new();
        let mut saved = None;
        for l in text.lines() {
            let f: Vec<&str> = l.split(' ').collect();
            match f.as_slice() {
                ["row", _, param, checked] => {
                    rows.push((param.parse().unwrap(), *checked == "1"));
                }
                ["saved", order] => saved = Some(order.to_string()),
                ["saved"] => saved = Some(String::new()),
                ["notify", _] => {}
                _ => panic!("unexpected report line {l:?}"),
            }
        }
        (rows, saved)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = CURRENT_USER.remove_tree(&self.root);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The factory rows are the factory tokens one for one, every divider ticked.
fn factory_rows(s: &Scratch) -> Vec<(i64, bool)> {
    let (rows, saved) = s.shot(&[]);
    assert_eq!(saved, None, "nothing is saved without Done");
    let tokens = default_menu_tokens();
    assert_eq!(rows.len(), tokens.len(), "one row per factory token: {rows:?}");
    for (row, tok) in rows.iter().zip(&tokens) {
        assert_eq!(row.0 == -1, *tok == MENU_SEP_TOKEN, "{tok} vs row {row:?}");
        assert!(row.1, "every factory row starts ticked, dividers too: {row:?}");
    }
    rows
}

#[test]
fn a_dragged_row_moves_and_done_saves_it() {
    let s = Scratch::new("drag");
    let before = factory_rows(&s);

    // Row 0 dropped just above row 3 lands at index 2.
    let (after, saved) = s.shot(&["--drag", "0,3", "--done"]);
    let mut want = before.clone();
    let moved = want.remove(0);
    want.insert(2, moved);
    assert_eq!(after, want, "the drag must move the row");

    let mut tokens = default_menu_tokens();
    let t = tokens.remove(0);
    tokens.insert(2, t);
    assert_eq!(
        saved.as_deref(),
        Some(format!("custom,{}", tokens.join(",")).as_str()),
        "Done must save the new order without Settings' own Save"
    );
}

#[test]
fn a_divider_can_be_switched_off_and_done_saves_it_off() {
    let s = Scratch::new("divider");
    let before = factory_rows(&s);
    let div = before.iter().position(|r| r.0 == -1).expect("a divider row");

    let (after, saved) = s.shot(&["--space", &div.to_string(), "--done"]);
    let mut want = before.clone();
    want[div].1 = false;
    assert_eq!(after, want, "Space on a divider clears its box and nothing else");

    let mut tokens = default_menu_tokens();
    tokens[div] = MENU_SEP_OFF_TOKEN;
    assert_eq!(
        saved.as_deref(),
        Some(format!("custom,{}", tokens.join(",")).as_str()),
        "the switched-off divider keeps its place in the saved order"
    );
}
