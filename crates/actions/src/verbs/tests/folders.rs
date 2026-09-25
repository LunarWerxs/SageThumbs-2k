#![cfg(test)]

//! The folder verbs: icons, combine-to-CBZ, files-to-folder, sort and tags-to-folders.

use super::*;

#[test]
fn set_folder_icon_writes_ini_and_ico() {
    let dir = std::env::temp_dir().join(format!("st2k_foldericon_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let png = dir.join("pic.png");
    // A non-square source — the icon should be padded to a square canvas.
    image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
        300,
        120,
        image::Rgb([200, 60, 60]),
    ))
    .save(&png)
    .unwrap();

    set_folder_icon(png.to_str().unwrap()).unwrap();

    let ico = dir.join("SageThumbsFolder.ico");
    let ini = dir.join("desktop.ini");
    assert!(ico.exists(), "icon file should be written");
    assert!(ini.exists(), "desktop.ini should be written");

    let icon = image::open(&ico).unwrap();
    assert_eq!(
        (icon.width(), icon.height()),
        (256, 256),
        "icon is a 256² square"
    );

    let ini_text = std::fs::read_to_string(&ini).unwrap();
    assert!(
        ini_text.contains("[.ShellClassInfo]"),
        "ini has the section"
    );
    assert!(
        ini_text.contains("IconResource=SageThumbsFolder.ico,0"),
        "ini points at the icon"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn combine_to_cbz_zips_pages_in_natural_order() {
    let dir = std::env::temp_dir().join(format!("st2k_cbz_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // Out-of-order names: natural sort must put 2 before 10.
    let names = ["10.png", "2.png", "1.png"];
    let mut paths = Vec::new();
    for n in names {
        let p = dir.join(n);
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(8, 8))
            .save(&p)
            .unwrap();
        paths.push(p.to_str().unwrap().to_string());
    }

    let slot = combined_path(&paths[0], "cbz");
    let out = slot.path().to_path_buf();
    combine_to_cbz(&paths, &out, OnOmit::Report).unwrap();
    assert!(out.exists() && out.extension().unwrap() == "cbz");

    // Reopen the archive: the ComicInfo.xml sidecar first (the CBZ RFC wants it
    // there), then the 3 pages in 1 → 2 → 10 page order.
    let f = std::fs::File::open(&out).unwrap();
    let mut zip = zip::ZipArchive::new(f).unwrap();
    assert_eq!(zip.len(), 4);
    let order: Vec<String> = (0..zip.len())
        .map(|i| zip.by_index(i).unwrap().name().to_string())
        .collect();
    assert_eq!(
        order,
        vec!["ComicInfo.xml", "001_1.png", "002_2.png", "003_10.png"]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn files_to_folder_creates_and_moves() {
    let dir = std::env::temp_dir().join(format!("st2k_f2f_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut paths = Vec::new();
    for n in ["a.txt", "b.txt", "c.bin"] {
        let p = dir.join(n);
        std::fs::write(&p, b"x").unwrap();
        paths.push(p.to_str().unwrap().to_string());
    }

    let (folder, moved, skipped) = files_to_folder(&paths, "My Group").unwrap();
    assert_eq!(folder, dir.join("My Group"));
    assert_eq!((moved, skipped), (3, 0));
    assert!(
        folder.join("a.txt").exists()
            && folder.join("b.txt").exists()
            && folder.join("c.bin").exists()
    );
    // Originals moved out of the parent.
    assert!(!dir.join("a.txt").exists());

    // A second call with the same name makes a *fresh* folder, never merges.
    let p2 = dir.join("d.txt");
    std::fs::write(&p2, b"y").unwrap();
    let (folder2, _, _) = files_to_folder(&[p2.to_str().unwrap().to_string()], "My Group").unwrap();
    assert_eq!(folder2, dir.join("My Group (2)"));
    // Illegal filename chars in the name are sanitized.
    let p3 = dir.join("e.txt");
    std::fs::write(&p3, b"z").unwrap();
    let (folder3, _, _) = files_to_folder(&[p3.to_str().unwrap().to_string()], "a/b:c").unwrap();
    assert_eq!(folder3, dir.join("a-b-c"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sort_by_dimensions_buckets_by_size() {
    let dir = std::env::temp_dir().join(format!("st2k_dims_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut paths = Vec::new();
    for (n, w, h) in [("a.png", 100, 100), ("b.png", 100, 100), ("c.png", 64, 48)] {
        let p = dir.join(n);
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(w, h))
            .save(&p)
            .unwrap();
        paths.push(p.to_str().unwrap().to_string());
    }

    let (moved, skipped) = sort_by_dimensions(&paths);
    assert_eq!((moved, skipped), (3, 0));
    assert!(dir.join("100x100").join("a.png").exists());
    assert!(dir.join("100x100").join("b.png").exists());
    assert!(dir.join("64x48").join("c.png").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn expands_tag_template() {
    use st2k_codecs::strip::AudioTags;
    let t = AudioTags {
        artist: Some("A".into()),
        album: Some("B".into()),
        title: Some("T".into()),
        track: Some(5),
        ..Default::default()
    };
    assert_eq!(expand_template("$artist - $album", &t, "X"), "A - B");
    assert_eq!(expand_template("$track $title", &t, "X"), "05 T"); // track zero-padded
                                                                   // A missing tag is replaced by the fallback text.
    assert_eq!(
        expand_template("$artist", &AudioTags::default(), "Unknown"),
        "Unknown"
    );
}

#[test]
fn tags_to_folders_moves_and_copies_by_template() {
    let dir = std::env::temp_dir().join(format!("st2k_ttf_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let a = dir.join("a.wav");
    let b = dir.join("b.wav");
    tagged_wav(&a, "Alpha");
    tagged_wav(&b, "Beta");
    let dest = dir.join("sorted");

    // Move: two artists → two folders, originals gone.
    let files = vec![
        a.to_str().unwrap().to_string(),
        b.to_str().unwrap().to_string(),
    ];
    let (done, skipped) = tags_to_folders(&files, &dest, "$artist", "Unknown", true);
    assert_eq!((done, skipped), (2, 0));
    assert!(dest.join("Alpha").join("a.wav").exists());
    assert!(dest.join("Beta").join("b.wav").exists());
    assert!(!a.exists(), "move should remove the original");

    // Copy: original stays put.
    let c = dir.join("c.wav");
    tagged_wav(&c, "Gamma");
    let (done2, _) = tags_to_folders(
        &[c.to_str().unwrap().to_string()],
        &dest,
        "$artist",
        "Unknown",
        false,
    );
    assert_eq!(done2, 1);
    assert!(c.exists(), "copy should keep the original");
    assert!(dest.join("Gamma").join("c.wav").exists());
    let _ = std::fs::remove_dir_all(&dir);
}
