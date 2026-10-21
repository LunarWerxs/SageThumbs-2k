#![cfg(test)]

use super::*;

/// A scratch stand-in for `HKCU\Software\Classes`, removed when the guard drops.
struct Scratch(String);

impl Scratch {
    fn new(name: &str) -> (Self, Key) {
        let path = format!(r"Software\SageThumbs2K-test\{name}-{}", std::process::id());
        let _ = CURRENT_USER.remove_tree(&path);
        let key = CURRENT_USER.create(&path).expect("scratch key");
        (Scratch(path), key)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = CURRENT_USER.remove_tree(&self.0);
    }
}

const PHOTOS: &str = "{C7657C4A-9F68-40fa-A4DF-96BC08EB3551}";
const OTHER_APP: &str = "{11111111-2222-3333-4444-555555555555}";

/// The slot value under `progid` in the scratch hive, if any.
fn slot(classes: &Key, progid: &str) -> Option<String> {
    classes
        .open(slot_path(progid))
        .ok()
        .and_then(|k| k.get_string("").ok())
}

/// Only a slot naming Windows' own thumbnailer is taken; another program's, ours and an empty
/// one are left alone. Turning the setting off gives back what was taken, keys and all, so an
/// uninstall never leaves a ProgID pointing at a DLL that is gone.
#[test]
fn sync_takes_only_windows_own_slots_and_gives_them_back_when_off() {
    let (_guard, classes) = Scratch::new("progidslot-sync");
    let progids: Vec<String> = ["St2k.Photo", "St2k.Other", "St2k.Ours", "St2k.Bare"]
        .map(String::from)
        .to_vec();
    let handler_of = |p: &str| match p {
        "St2k.Photo" => Some(PHOTOS.to_string()),
        "St2k.Other" => Some(OTHER_APP.to_string()),
        "St2k.Ours" => Some(CLSID_THUMBNAIL_PROVIDER_STR.to_string()),
        _ => None,
    };
    let windows_own = |c: &str| c.eq_ignore_ascii_case(PHOTOS);

    assert_eq!(sync_into(&classes, true, &progids, handler_of, windows_own), 1);
    assert_eq!(slot(&classes, "St2k.Photo").as_deref(), Some(CLSID_THUMBNAIL_PROVIDER_STR));
    for untouched in ["St2k.Other", "St2k.Ours", "St2k.Bare"] {
        assert!(classes.open(untouched).is_err(), "{untouched} must not be written");
    }

    assert_eq!(sync_into(&classes, false, &progids, handler_of, windows_own), 0);
    assert!(
        classes.open("St2k.Photo").is_err(),
        "a ProgID key that only held our slot must go with it"
    );
}

/// A value already in the user's own hive that we did not write is their override: never
/// replaced, never given back. A ProgID key holding anything else of anyone's survives the
/// give-back.
#[test]
fn a_users_own_slot_and_their_other_values_survive() {
    let (_guard, classes) = Scratch::new("progidslot-own");
    let own = classes.create(slot_path("St2k.Mine")).expect("own slot");
    own.set_string("", OTHER_APP).expect("own value");
    drop(own);
    assert!(!take_slot(&classes, "St2k.Mine"));
    give_back_all(&classes);
    assert_eq!(slot(&classes, "St2k.Mine").as_deref(), Some(OTHER_APP));

    let shared = classes.create("St2k.Shared").expect("progid");
    shared.set_string("TypeOverlay", "").expect("another value");
    drop(shared);
    assert!(take_slot(&classes, "St2k.Shared"));
    give_back_all(&classes);
    assert!(slot(&classes, "St2k.Shared").is_none(), "our slot is given back");
    assert!(
        classes
            .open("St2k.Shared")
            .and_then(|k| k.get_string("TypeOverlay"))
            .is_ok(),
        "the ProgID's other value stays"
    );
}

/// "Windows' own" is a path boundary, not a prefix: a Store app under `C:\WindowsApps` is
/// another program, and overriding it would take a slot the user chose.
#[test]
fn windows_own_means_under_the_windows_directory() {
    let root = r"C:\Windows";
    for (dll, own) in [
        (r"C:\WINDOWS\system32\PhotoMetadataHandler.dll", true),
        (r#""C:\Windows\System32\thumb.dll""#, true),
        (r"%SystemRoot%\system32\thumb.dll", true),
        (r"%windir%\system32\thumb.dll", true),
        (r"C:\WindowsApps\Vendor\thumb.dll", false),
        (r"C:\Program Files\Viewer\thumb.dll", false),
    ] {
        assert_eq!(is_under_windows_dir(dll, root), own, "{dll}");
    }
}
