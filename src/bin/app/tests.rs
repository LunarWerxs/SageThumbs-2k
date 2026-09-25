#![cfg(test)]

use super::{update_piggyback_wanted, wanted_tab};

fn argv(rest: &[&str]) -> Vec<String> {
    std::iter::once("SageThumbs2K.exe".to_string())
        .chain(rest.iter().map(|s| (*s).to_string()))
        .collect()
}

#[test]
fn piggyback_covers_ordinary_launches_and_spares_the_headless_ones() {
    // The whole point of the piggyback: an install where the resident helper was never
    // enabled still gets update checks, from whatever the user actually opens.
    for ordinary in [
        vec![],
        vec!["--convert", "list.txt"],
        vec!["--preview", "a.png"],
        vec!["--eyedropper"],
        vec!["--files-to-folder", "list.txt"],
        vec!["--rename-with-pattern", "list.txt"],
    ] {
        assert!(
            update_piggyback_wanted(&argv(&ordinary)),
            "{ordinary:?} should fire the update check"
        );
    }

    // Headless captures must stay deterministic and side-effect free, the automation
    // route has a synthetic-pixels-only contract, the daemon owns its own timer, and the
    // one-shot must never spawn itself.
    for excluded in [
        vec!["--shot", "out.png"],
        vec!["--shot", "out.png", "--window", "preview"],
        vec!["--shot-gif", "out.gif"],
        vec!["--screenshot-automation"],
        vec!["--screenshot-daemon"],
        vec!["--update-check"],
        vec!["--update-task"],
        vec!["--update-task", "remove"],
        vec!["--update-selftest", "setup.exe"],
        vec!["--updated", "1.7.0"],
        vec!["--heal-hotkeys"],
        vec!["--rebuild-thumbnail-cache"],
        vec!["--rebuild-thumbnail-cache-now"],
        vec!["--bench-preview", "C:\\pics"],
        vec!["--bench-nav", "C:\\pics", "20"],
        vec!["--bench-mash", "C:\\pics", "20"],
        vec!["--explorer-selection"],
        vec!["--explorer-selection", "--after-ms", "300"],
        vec!["--sync-user-shell"],
        vec!["--remove-user-shell"],
        vec!["--remove-user-state"],
        vec!["--export-settings", "out.json"],
        vec!["--import-settings", "in.json"],
    ] {
        assert!(
            !update_piggyback_wanted(&argv(&excluded)),
            "{excluded:?} must NOT fire the update check"
        );
    }
}

/// `--tab N` backs the Quick preview caption's Settings gear, so a launch that carries it
/// has to land on that page. It was parsed only inside `--shot` until 2026-08-24, which
/// meant a normal launch silently opened page 0 and a live probe reported a clean pass
/// against a control that did not exist.
#[test]
fn tab_flag_selects_a_real_settings_page() {
    let quick = crate::settings_dlg::page_named("nav_quickpreview").expect("a Quick preview page");
    assert_eq!(
        wanted_tab(&argv(&["--tab", &quick.to_string()])),
        Some(quick)
    );
    // By name, as the viewer's caption gear and the licence reminders ask for it.
    assert_eq!(
        wanted_tab(&argv(&["--tab", "nav_quickpreview"])),
        Some(quick)
    );
    assert_eq!(wanted_tab(&argv(&["--tab", "nav_no_such_page"])), None);
    assert_eq!(wanted_tab(&argv(&["--tab", "0"])), Some(0));
    // Absent, malformed, or with nothing after it: open normally, never panic.
    assert_eq!(wanted_tab(&argv(&[])), None);
    assert_eq!(wanted_tab(&argv(&["--tab"])), None);
    assert_eq!(wanted_tab(&argv(&["--tab", "not-a-number"])), None);
    assert_eq!(wanted_tab(&argv(&["--tab", "-1"])), None);
    // Past the end is REFUSED rather than clamped: it means the caller's page list and
    // this build's disagree, and quietly opening the last page would hide that.
    let past_end = crate::settings_dlg::NAV_CATEGORY_COUNT;
    assert_eq!(wanted_tab(&argv(&["--tab", &past_end.to_string()])), None);
}

/// The build script stamps the app binary with the crate version (the same stamp the
/// installer's stub gets from `installer.iss`), and the updater's stamp reader reads it back:
/// this test binary is the app binary's own build, so it carries the resource.
#[test]
fn this_binary_is_stamped_with_the_crate_version() {
    let me = std::env::current_exe().unwrap();
    let want: Vec<u32> = env!("CARGO_PKG_VERSION")
        .split('.')
        .map(|p| p.parse().unwrap())
        .collect();
    assert_eq!(
        st2k_appkit::update::pe_stamped_version(&me),
        Some((want[0], want[1], want[2])),
        "this binary's stamp is the crate version"
    );
}
