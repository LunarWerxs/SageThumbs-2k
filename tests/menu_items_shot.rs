//! The "Edit menu items" editor, driven the way its user drives it (Discord, 2026-10-21, on
//! 3.7.4): "dragging on the list items or dividers ... doesn't do anything", "clicking the
//! 'Done' button ... would apply the changes to the context menu directly", and "add checkboxes
//! next to the separators ... to let the user decide which ones to use".
//!
//! `--shot --window menuitems` opens the editor over a headless Settings. `--drag` starts the
//! drag with the notification comctl32 really sends, at the window it really sends it to;
//! `--space` presses Space on a row; `--done` clicks Done. Each case runs over its own scratch
//! settings key, so the factory order is its starting point.
//!
//! The second report (feedback, 2026-10-22, on 3.9.0): every drag sent the scrollbar to the
//! top; a divider dragged next to another or to either end vanished on the spot; there was no
//! way to add a divider; the title bar's X saved and Escape did nothing; and Reset left the
//! ticks alone. `--scroll`, `--divider`, `--reset`, `--cancel` and `--escape` drive those.

use std::path::PathBuf;
use std::process::Command;

use st2k_actions::verbs::{default_menu_tokens, MENU_SEP_OFF_TOKEN, MENU_SEP_TOKEN};
use windows_registry::CURRENT_USER;

/// A divider row's lParam.
const SEP: i64 = -1;

struct Scratch {
    root: String,
    dir: PathBuf,
}

/// What one editor shot reported.
#[derive(Default)]
struct Shot {
    /// The list's first visible row right after `--scroll`.
    scrolled: i64,
    /// The list's first visible row before the close.
    top: i64,
    /// `(lParam, checked)` per row before the close.
    rows: Vec<(i64, bool)>,
    /// The rows Settings got back after a close.
    after: Vec<(i64, bool)>,
    /// The saved `MenuOrder` after a close.
    saved: Option<String>,
    /// Whether Settings, disabled under the editor, was enabled again by the close.
    owner_enabled: Option<bool>,
    /// The list's one column width and its client width, in px.
    column: (i64, i64),
}

fn parse_row(l: &str, param: &str, checked: &str) -> (i64, bool) {
    let checked = match checked {
        "1" => true,
        "0" => false,
        c => panic!("bad check state {c:?} in {l:?}"),
    };
    (param.parse().unwrap(), checked)
}

impl Scratch {
    fn new(case: &str) -> Self {
        let root = format!(
            r"Software\SageThumbs2K\__test_menu_items_{}_{case}",
            std::process::id()
        );
        let dir =
            std::env::temp_dir().join(format!("st2k_menu_items_{}_{case}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        Self { root, dir }
    }

    /// One editor shot with `extra` flags: its rows `(lParam, checked)` and, after `--done`,
    /// the saved `MenuOrder`.
    fn shot(&self, extra: &[&str]) -> (Vec<(i64, bool)>, Option<String>) {
        let s = self.report(extra);
        (s.rows, s.saved)
    }

    /// One editor shot with `extra` flags, everything it reported.
    fn report(&self, extra: &[&str]) -> Shot {
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
        let mut s = Shot::default();
        for l in text.lines() {
            let f: Vec<&str> = l.split(' ').collect();
            match f.as_slice() {
                ["row", _, param, checked] => s.rows.push(parse_row(l, param, checked)),
                ["final", _, param, checked] => s.after.push(parse_row(l, param, checked)),
                ["top", top] => s.top = top.parse().unwrap(),
                ["scrolled", top] => s.scrolled = top.parse().unwrap(),
                ["column", w, client] => s.column = (w.parse().unwrap(), client.parse().unwrap()),
                ["saved", order] => s.saved = Some(order.to_string()),
                ["saved"] => s.saved = Some(String::new()),
                ["owner_enabled", on] => s.owner_enabled = Some(*on == "1"),
                ["notify", _] => {}
                _ => panic!("unexpected report line {l:?}"),
            }
        }
        s
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
    let shot = s.report(&[]);
    let (rows, saved) = (shot.rows, shot.saved);
    assert_eq!(saved, None, "nothing is saved without Done");
    // The page made the column 300 px; in the editor it fills the list, so a highlight runs
    // up to the scrollbar.
    let (colw, client) = shot.column;
    assert!(
        client > 300,
        "the editor's list is wider than the page's: {client}"
    );
    assert_eq!(colw, client, "the column must fill the list");
    let tokens = default_menu_tokens();
    assert_eq!(
        rows.len(),
        tokens.len(),
        "one row per factory token: {rows:?}"
    );
    for (row, tok) in rows.iter().zip(&tokens) {
        assert_eq!(row.0 == -1, *tok == MENU_SEP_TOKEN, "{tok} vs row {row:?}");
        assert!(
            row.1,
            "every factory row starts ticked, dividers too: {row:?}"
        );
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
    let div = before
        .iter()
        .position(|r| r.0 == -1)
        .expect("a divider row");

    let (after, saved) = s.shot(&["--space", &div.to_string(), "--done"]);
    let mut want = before.clone();
    want[div].1 = false;
    assert_eq!(
        after, want,
        "Space on a divider clears its box and nothing else"
    );

    let mut tokens = default_menu_tokens();
    tokens[div] = MENU_SEP_OFF_TOKEN;
    assert_eq!(
        saved.as_deref(),
        Some(format!("custom,{}", tokens.join(",")).as_str()),
        "the switched-off divider keeps its place in the saved order"
    );
}

/// The factory order with row `from` moved to `to`, as `moved_rows` does it.
fn moved(rows: &[(i64, bool)], from: usize, to: usize) -> Vec<(i64, bool)> {
    let mut want = rows.to_vec();
    let r = want.remove(from);
    want.insert(if from < to { to - 1 } else { to }, r);
    want
}

/// "The menu list/window fully refreshes after dragging an item and thus the scrollbar jumps
/// to the top every time."
#[test]
fn a_drag_keeps_the_list_scrolled_where_it_was() {
    let s = Scratch::new("scroll");
    let before = factory_rows(&s);
    // Scrolled as far down as the list goes, then a drag between two rows near the bottom.
    let (from, to) = (before.len() - 4, before.len() - 1);
    let shot = s.report(&["--scroll", "99", "--drag", &format!("{from},{to}")]);
    assert!(shot.scrolled > 0, "the list must have rows below the fold");
    assert_eq!(
        shot.rows,
        moved(&before, from, to),
        "the drag must move the row"
    );
    assert_eq!(
        shot.top, shot.scrolled,
        "the list must stay scrolled where it was"
    );
}

/// "The divider items disappear or get deleted when dragged next to another divider or the
/// top/bottom of the list ... It should only delete those dividers after clicking on Done."
#[test]
fn a_divider_dragged_beside_another_or_to_the_top_stays_until_done() {
    let s = Scratch::new("divider_drag");
    let before = factory_rows(&s);
    let divs: Vec<usize> = (0..before.len()).filter(|&i| before[i].0 == SEP).collect();
    let (d1, d2) = (divs[0], divs[1]);
    assert!(d2 > d1 + 1, "the factory dividers are apart: {divs:?}");

    // The second divider dropped right under the first: both still there.
    let shot = s.report(&["--drag", &format!("{d2},{}", d1 + 1)]);
    assert_eq!(shot.rows, moved(&before, d2, d1 + 1));
    assert_eq!((shot.rows[d1].0, shot.rows[d1 + 1].0), (SEP, SEP));

    // The first divider dropped above the first row: it stays on top until Done tidies it.
    let shot = s.report(&["--drag", &format!("{d1},0"), "--done"]);
    assert_eq!(shot.rows, moved(&before, d1, 0));
    assert_eq!(shot.rows[0].0, SEP, "kept at the top while editing");
    assert_eq!(
        shot.after.len(),
        before.len() - 1,
        "Done drops the leading divider"
    );
    assert_ne!(shot.after[0].0, SEP);
    let saved = shot.saved.expect("Done saves");
    assert!(
        !saved.starts_with(&format!("custom,{MENU_SEP_TOKEN}")),
        "no leading divider is saved: {saved}"
    );
}

/// "There's no option to add new divider items. Could be added to the context menu."
#[test]
fn dividers_are_added_and_removed_from_the_right_click_menu() {
    let s = Scratch::new("divider_edit");
    let before = factory_rows(&s);
    let tokens = default_menu_tokens();
    assert_ne!(tokens[1], MENU_SEP_TOKEN, "rows 0 and 1 are both items");

    let shot = s.report(&["--divider", "0,below", "--done"]);
    let mut want = before.clone();
    want.insert(1, (SEP, true));
    assert_eq!(shot.rows, want, "a ticked divider under row 0");
    let mut want_tokens = tokens.clone();
    want_tokens.insert(1, MENU_SEP_TOKEN);
    assert_eq!(
        shot.saved.as_deref(),
        Some(format!("custom,{}", want_tokens.join(",")).as_str())
    );

    // A fresh key, so the editor opens on the factory menu again. Remove on an item row takes
    // nothing out.
    let s = Scratch::new("divider_remove");
    let shot = s.report(&["--divider", "0,remove"]);
    assert_eq!(shot.rows, before);

    let div = before.iter().position(|r| r.0 == SEP).unwrap();
    let shot = s.report(&["--divider", &format!("{div},remove"), "--done"]);
    let mut want = before.clone();
    want.remove(div);
    assert_eq!(shot.rows, want, "the divider is gone");
    let mut want_tokens = tokens.clone();
    want_tokens.remove(div);
    assert_eq!(
        shot.saved.as_deref(),
        Some(format!("custom,{}", want_tokens.join(",")).as_str())
    );
}

/// "Closing the Menu items window via the X button saves all changes. It should only save when
/// pressing on Done ... Also closing via Escape button doesn't work." And: "the main Settings
/// window should be focused again after closing the Menu items window": every close enables
/// Settings again before the editor goes, so Windows hands the activation back to it.
#[test]
fn the_x_and_escape_throw_the_edit_away() {
    let s = Scratch::new("cancel");
    let before = factory_rows(&s);
    for close in ["--cancel", "--escape"] {
        let shot = s.report(&["--drag", "0,3", "--space", "5", close]);
        assert_ne!(shot.rows, before, "{close}: the edit happened");
        assert_eq!(
            shot.after, before,
            "{close}: Settings gets the list back unedited"
        );
        assert_eq!(shot.saved.as_deref(), Some(""), "{close}: nothing is saved");
        assert_eq!(shot.owner_enabled, Some(true), "{close}: Settings enabled");
    }
    let shot = s.report(&["--done"]);
    assert_eq!(shot.owner_enabled, Some(true), "Done: Settings enabled");
}

/// "There's only a 'Reset order' button, but no option to reset the checked state of the
/// items ... one button called 'Reset' that reverts all custom changes."
#[test]
fn reset_brings_back_the_factory_menu_ticks_included() {
    let s = Scratch::new("reset");
    let before = factory_rows(&s);
    let shot = s.report(&[
        "--drag",
        "0,3",
        "--divider",
        "0,above",
        "--space",
        "4",
        "--reset",
    ]);
    assert_eq!(
        shot.rows, before,
        "Reset is the factory menu, every row ticked"
    );
}
