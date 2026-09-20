#![cfg(test)]

use super::*;

/// A scratch stand-in for the classes root these helpers write into, removed when the guard
/// drops, so these tests can exercise real registry writes without touching the machine's
/// own associations (mirrors the `Scratch` pattern in `typeoverlay.rs`).
struct Scratch(String);

impl Scratch {
    fn new(name: &str) -> (Self, Key) {
        let path = format!(r"Software\SageThumbs2K-test\register-{name}");
        let key = CURRENT_USER.create(&path).expect("scratch key");
        (Scratch(path), key)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = CURRENT_USER.remove_tree(&self.0);
    }
}

/// `hook_ext`/`hook_ext_preview` used to `?` inside their per-key loop (opus-SG-06 / A054):
/// a failure on the FIRST key returned `Err` before the second (higher-priority, per the
/// module doc) key was ever attempted. `set_shellex_key` is the shared best-effort
/// replacement all three call sites now use; prove it keeps going past a key that can't be
/// created — a 300-char key-name segment exceeds the registry's documented 255-character
/// key-name limit, so `create` genuinely fails for it, without needing elevation or
/// touching a real hive.
#[test]
fn set_shellex_key_keeps_writing_later_keys_after_an_earlier_one_fails_to_create() {
    let (_guard, root) = Scratch::new("short-circuit");
    let unwritable = "x".repeat(300);
    let good = "good-sibling";

    let outcomes: Vec<bool> = [unwritable.as_str(), good]
        .iter()
        .map(|name| set_shellex_key(&root, name, "TEST-CLSID").is_ok())
        .collect();
    assert_eq!(
        outcomes,
        [false, true],
        "the failure must be handed back (so the pass can count and log it), not swallowed"
    );

    assert!(
        root.open(&unwritable).is_err(),
        "sanity: the 300-char segment must genuinely have failed to create, or this test \
         proves nothing about the old short-circuit"
    );
    let k = root
        .open(good)
        .expect("the good key after an invalid earlier sibling must still be written");
    assert_eq!(k.get_string("").as_deref(), Ok("TEST-CLSID"));
}

/// The ordinary case: a writable path round-trips through `set_shellex_key`.
#[test]
fn set_shellex_key_round_trips_on_a_writable_path() {
    let (_guard, root) = Scratch::new("roundtrip");
    set_shellex_key(&root, "child\\grandchild", "TEST-CLSID").expect("write");
    let k = root.open("child\\grandchild").expect("key created");
    assert_eq!(k.get_string("").as_deref(), Ok("TEST-CLSID"));
}

/// A pass that attempted keys and wrote none is the "clean install, no thumbnails, no
/// log line" failure `register` used to report as S_OK; a pass with a partial failure is
/// logged but is not a failure of the pass as a whole.
#[test]
fn a_pass_that_wrote_nothing_is_reported_as_failed() {
    let (_guard, root) = Scratch::new("pass-tally");
    let unwritable = "y".repeat(300);

    let mut all_failed = Pass::default();
    all_failed.note(&unwritable, set_shellex_key(&root, &unwritable, "X"));
    assert!(all_failed.report("test pass: all failed"));
    assert_eq!((all_failed.written, all_failed.failed), (0, 1));
    assert!(
        all_failed.first_failure.is_some(),
        "the first HRESULT is kept for the log"
    );

    let mut partial = Pass::default();
    partial.note(&unwritable, set_shellex_key(&root, &unwritable, "X"));
    partial.note("ok", set_shellex_key(&root, "ok", "X"));
    assert!(!partial.report("test pass: partial"));
    assert_eq!((partial.written, partial.failed), (1, 1));

    let empty = Pass::default();
    assert!(!empty.report("test pass: nothing attempted"));
}

/// `PerceivedType=image` pulls Windows' image verbs (Rotate, Print, Set as background)
/// onto a type; those fail on formats WIC cannot open, so the Image category is stamped
/// only for the WIC-openable extensions. Camera RAW and the other categories keep their
/// classification.
#[test]
fn perceived_type_is_image_only_where_wic_can_open_it() {
    assert_eq!(perceived_type_for("png"), Some("image"));
    assert_eq!(perceived_type_for("heic"), Some("image"));
    assert_eq!(
        perceived_type_for("cr2"),
        Some("image"),
        "camera RAW keeps image"
    );
    assert_eq!(perceived_type_for("psd"), None, "WIC cannot open a PSD");
    assert_eq!(perceived_type_for("xcf"), None);
    assert_eq!(perceived_type_for("flac"), Some("audio"));
    for ext in WIC_IMAGE_EXTS {
        assert!(
            matches!(
                crate::formats::category(ext),
                crate::formats::Category::Image | crate::formats::Category::Raw
            ) || !crate::formats::is_known(ext),
            ".{ext} is listed as WIC-openable but is not an image format"
        );
    }
}

/// A054's companion in the property-store path (A055): `set_assoc_value_if_empty` must fill
/// a genuinely blank value while leaving one a third-party app already set alone, even
/// though our caller's only ownership signal (the PropertyHandlers\.<ext> guard) can't see
/// that value at all.
#[test]
fn set_assoc_value_if_empty_fills_blanks_but_never_clobbers_a_foreign_value() {
    let (_guard, classes) = Scratch::new("propstore-guard");
    let k = classes.create("Assoc").expect("create");
    k.set_string("InfoTip", "set by some other app")
        .expect("set");
    drop(k);

    let k = classes.create("Assoc").expect("writable handle");
    set_assoc_value_if_empty(&k, "InfoTip", "our default infotip");
    set_assoc_value_if_empty(&k, "FullDetails", "our default fulldetails");

    assert_eq!(
        k.get_string("InfoTip").as_deref(),
        Ok("set by some other app"),
        "a pre-existing foreign value must survive"
    );
    assert_eq!(
        k.get_string("FullDetails").as_deref(),
        Ok("our default fulldetails"),
        "a genuinely empty slot must still get filled"
    );
}

/// A056's defect class, reproduced against a safe scratch key instead of the real
/// `CLASSES_ROOT` paths `unhook_perceived_type`/`unhook_ext_propstore` actually touch:
/// `Key::open` in this crate hands back a read-only handle, so `remove_value` through it
/// silently no-ops, while `Key::create` re-opens the SAME existing key with write access
/// and `remove_value` through THAT handle actually removes it. This is exactly the swap
/// those two functions needed.
#[test]
fn a_read_only_open_handle_cannot_remove_a_value_but_a_create_handle_can() {
    let (_guard, classes) = Scratch::new("open-vs-create");
    let k = classes.create("Marked").expect("create");
    k.set_string("Marker", "1").expect("set");
    drop(k);

    let ro = classes.open("Marked").expect("open (read-only)");
    let _ = ro.remove_value("Marker");
    drop(ro);
    assert_eq!(
        classes
            .open("Marked")
            .unwrap()
            .get_string("Marker")
            .as_deref(),
        Ok("1"),
        "a read-only handle's remove_value must not have taken effect"
    );

    let rw = classes.create("Marked").expect("create (writable)");
    rw.remove_value("Marker")
        .expect("remove_value via a writable handle must succeed");
    assert!(
        classes
            .open("Marked")
            .unwrap()
            .get_string("Marker")
            .is_err(),
        "the value must actually be gone now"
    );
}

/// A160: `register_user`'s per-extension loop only ever walks `FORMATS`, so a stale hook
/// left by a dropped extension needed its own sweep, mirroring `register`/`unregister`/
/// `unregister_user`. This proves the underlying removal call the new sweep relies on —
/// `remove_user_if_ours` — actually clears a hook it owns and leaves a foreign one alone
/// (register_user() itself is not called here: it walks the real, live `FORMATS` list
/// against the real `HKCU\Software\Classes`, which would mutate this machine's actual
/// thumbnail associations for 300+ extensions as a side effect of running the test suite).
#[test]
fn remove_user_if_ours_clears_our_stale_hook_but_leaves_a_foreign_one() {
    let (_guard, classes) = Scratch::new("removed-ext-sweep");
    // `remove_user_if_ours` calls `thumb_keys`, which is a fixed real-extension-shaped
    // path — reuse it verbatim against the scratch root instead of duplicating its shape.
    for path in thumb_keys("zzzstaletestext") {
        classes
            .create(&path)
            .and_then(|k| k.set_string("", CLSID_THUMBNAIL_PROVIDER_STR))
            .expect("seed our stale hook");
    }
    for path in thumb_keys("zzzforeigntestext") {
        classes
            .create(&path)
            .and_then(|k| k.set_string("", "{some-other-vendor-clsid}"))
            .expect("seed a foreign hook");
    }

    remove_user_if_ours(&classes, "zzzstaletestext");
    remove_user_if_ours(&classes, "zzzforeigntestext");

    for path in thumb_keys("zzzstaletestext") {
        assert!(
            classes.open(&path).is_err(),
            "our own stale hook at {path} must be gone"
        );
    }
    for path in thumb_keys("zzzforeigntestext") {
        let k = classes.open(&path).expect("foreign hook key must survive");
        assert_eq!(k.get_string("").as_deref(), Ok("{some-other-vendor-clsid}"));
    }
}

/// Seed an association key exactly as a completed registration by THIS build leaves it.
fn seed_current_prop_lists(k: &Key) {
    for (name, value) in PROP_LISTS {
        k.set_string(name, value).expect("seed our list");
    }
}

/// 2026-09-05 audit, F35 (a): `unhook_ext_propstore` deleted all four property-list values
/// whenever the handler binding was ours, so a list a user or another product had changed
/// AFTER we registered died with our uninstall or with a plain format disable. Register,
/// then let the user re-point the hover tip, retype another list as a DWORD, and add an
/// unrelated value on the same key: all three must survive while our two untouched lists go.
/// Against the pre-fix code the edited InfoTip and the DWORD are both removed.
#[test]
fn remove_owned_prop_lists_keeps_lists_changed_after_we_wrote_them() {
    let (_guard, classes) = Scratch::new("proplist-changed");
    let k = classes.create("Assoc").expect("create");
    seed_current_prop_lists(&k);
    k.set_string("InfoTip", "prop:System.Size;System.DateModified")
        .expect("user edit");
    k.set_u32("AdditionalProperties", 1).expect("retyped value");
    k.set_string("ContentViewModeForBrowse", "prop:~System.ItemNameDisplay")
        .expect("unrelated value");

    remove_owned_prop_lists(&k);

    assert_eq!(
        k.get_string("InfoTip").as_deref(),
        Ok("prop:System.Size;System.DateModified"),
        "a list the user changed after registration must survive"
    );
    assert_eq!(
        k.get_u32("AdditionalProperties"),
        Ok(1),
        "a value we could not have written (wrong type) must survive"
    );
    assert_eq!(
        k.get_string("ContentViewModeForBrowse").as_deref(),
        Ok("prop:~System.ItemNameDisplay"),
        "an unrelated value on the same key must never be touched"
    );
    for name in ["FullDetails", "PreviewDetails"] {
        assert!(
            k.get_string(name).is_err(),
            "our unchanged {name} must still be removed"
        );
    }
}

/// F35 (b), the "we wrote it, nobody changed it" case the fix must preserve exactly: every
/// list as this build writes it is removed, and a value we never write is not.
#[test]
fn remove_owned_prop_lists_removes_the_unchanged_lists_this_build_wrote() {
    let (_guard, classes) = Scratch::new("proplist-ours");
    let k = classes.create("Assoc").expect("create");
    seed_current_prop_lists(&k);
    k.set_string("ContentViewModeForBrowse", "prop:~System.ItemNameDisplay")
        .expect("unrelated value");

    remove_owned_prop_lists(&k);

    for (name, _) in PROP_LISTS {
        assert!(k.get_string(name).is_err(), "our {name} must be gone");
    }
    assert_eq!(
        k.get_string("ContentViewModeForBrowse").as_deref(),
        Ok("prop:~System.ItemNameDisplay"),
        "the unrelated value must survive a clean unhook too"
    );
}

/// F35 (c): an install that upgraded from 0.6.0 still carries that release's InfoTip and
/// FullDetails strings, because the later hook fills only EMPTY slots and never rewrote
/// them. They are ours and must go, while a PreviewDetails the user added in between (0.6.0
/// wrote none, so the upgrade hook skipped it) is theirs and must stay. The literals are
/// deliberately NOT read from `LEGACY_PROP_LISTS`: dropping an entry from that table must
/// fail this test. A fix that matched only today's constants would orphan both legacy values.
#[test]
fn remove_owned_prop_lists_recognises_the_lists_an_older_build_wrote() {
    let (_guard, classes) = Scratch::new("proplist-legacy");
    let k = classes.create("Assoc").expect("create");
    k.set_string(
        "InfoTip",
        "prop:System.ItemTypeText;System.Image.Dimensions;System.Music.Artist;System.Title;System.Size",
    )
    .expect("0.6.0 InfoTip");
    k.set_string(
        "FullDetails",
        "prop:System.Image.Dimensions;System.Image.HorizontalSize;System.Image.VerticalSize;System.Photo.CameraManufacturer;System.Photo.CameraModel;System.Music.Artist;System.Music.AlbumTitle;System.Title;System.Music.TrackNumber;System.Size;System.DateModified",
    )
    .expect("0.6.0 FullDetails");
    k.set_string(
        "PreviewDetails",
        "prop:*System.Image.Dimensions;System.Size",
    )
    .expect("user-added list");

    remove_owned_prop_lists(&k);

    assert!(
        k.get_string("InfoTip").is_err(),
        "the 0.6.0 InfoTip is ours and must be removed"
    );
    assert!(
        k.get_string("FullDetails").is_err(),
        "the 0.6.0 FullDetails is ours and must be removed"
    );
    assert_eq!(
        k.get_string("PreviewDetails").as_deref(),
        Ok("prop:*System.Image.Dimensions;System.Size"),
        "a list the user added between releases must survive"
    );
}

/// The ownership predicate behind F35: every string this build writes and every string an
/// older build wrote is ours; the empty string, a foreign list and a one-token edit of our
/// own list are not. Also pins the maintenance rule on `LEGACY_PROP_LISTS`: it holds
/// REPLACED strings only, so an entry equal to a current constant means someone appended the
/// new value instead of the one it displaced.
#[test]
fn owned_prop_list_predicate_covers_every_build_and_nothing_else() {
    for (name, ours) in PROP_LISTS {
        assert!(is_owned_prop_list(ours), "today's {name} must be ours");
    }
    for legacy in LEGACY_PROP_LISTS {
        assert!(
            is_owned_prop_list(legacy),
            "legacy list must be ours: {legacy}"
        );
        assert!(
            !PROP_LISTS.iter().any(|(_, cur)| cur == legacy),
            "LEGACY_PROP_LISTS holds replaced strings only, not a current one: {legacy}"
        );
    }
    assert!(!is_owned_prop_list(""));
    assert!(!is_owned_prop_list("prop:System.Size"));
    let edited = format!("{PROP_INFOTIP};System.Rating");
    assert!(
        !is_owned_prop_list(&edited),
        "one appended property makes the list the user's"
    );
}
