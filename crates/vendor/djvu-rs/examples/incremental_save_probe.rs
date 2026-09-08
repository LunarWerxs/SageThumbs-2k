//! Incremental-save probe (#595): bytes written + wall-clock of
//! `DjVuDocumentMut::save_patched` vs a full rewrite, for (a) a same-size
//! in-place edit and (b) a size-changing metadata edit, on a large bundle.
//!
//! ```sh
//! cargo run --release --example incremental_save_probe -- big504.djvu
//! ```

use std::io::Write;
use std::time::Instant;

use djvu_rs::djvu_document::DjVuBookmark;
use djvu_rs::djvu_mut::DjVuDocumentMut;
use djvu_rs::iff::Chunk;

fn info_gamma_edit(doc: &mut DjVuDocumentMut, page_no: usize) {
    let mut seen = 0usize;
    for i in 0..doc.root_child_count() {
        if let Ok(Chunk::Form {
            secondary_id,
            children,
            ..
        }) = doc.chunk_at_path(&[i])
            && secondary_id == b"DJVU"
        {
            if seen == page_no {
                let j = children
                    .iter()
                    .position(|c| matches!(c, Chunk::Leaf { id, .. } if id == b"INFO"))
                    .expect("page has INFO");
                let mut info = doc.chunk_at_path(&[i, j]).unwrap().data().to_vec();
                info[7] ^= 1; // same length, real edit
                doc.replace_leaf(&[i, j], info).unwrap();
                return;
            }
            seen += 1;
        }
    }
    panic!("page {page_no} not found");
}

fn timed_full_write(bytes: &[u8], path: &std::path::Path) -> f64 {
    let t0 = Instant::now();
    let mut f = std::fs::File::create(path).unwrap();
    f.write_all(bytes).unwrap();
    f.sync_all().unwrap();
    t0.elapsed().as_secs_f64()
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: incremental_save_probe <bundle.djvu>");
    let raw = std::fs::read(&path).unwrap();
    // Normalize once through the emitter so the on-disk base is emit-stable
    // (some external files lack the final IFF pad byte).
    let original = {
        let mut doc = DjVuDocumentMut::from_bytes(&raw).unwrap();
        doc.set_bookmarks(&[]).unwrap(); // force a dirty re-emit
        doc.try_into_bytes().unwrap()
    };
    println!(
        "{path}: {:.1} MB bundle (normalized {} B)",
        original.len() as f64 / 1e6,
        original.len()
    );
    let tmp = std::env::temp_dir().join("incr_save_probe.djvu");

    for (label, edit) in [
        (
            "same-size INFO edit (page 252)",
            Box::new(|d: &mut DjVuDocumentMut| info_gamma_edit(d, 252))
                as Box<dyn Fn(&mut DjVuDocumentMut)>,
        ),
        (
            "size-changing bookmark edit",
            Box::new(|d: &mut DjVuDocumentMut| {
                d.set_bookmarks(&[DjVuBookmark {
                    title: "probe".into(),
                    url: "#1".into(),
                    children: Vec::new(),
                }])
                .unwrap()
            }),
        ),
    ] {
        // Full rewrite path.
        let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();
        edit(&mut doc);
        let t0 = Instant::now();
        let full_bytes = doc.try_into_bytes().unwrap();
        let serialize_s = t0.elapsed().as_secs_f64();
        let full_write_s = timed_full_write(&full_bytes, &tmp);

        // Patched path.
        std::fs::write(&tmp, &original).unwrap();
        let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();
        edit(&mut doc);
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&tmp)
            .unwrap();
        let t1 = Instant::now();
        let stats = doc.save_patched(&mut f).unwrap();
        f.sync_all().unwrap();
        let patched_s = t1.elapsed().as_secs_f64();
        drop(f);
        assert_eq!(
            std::fs::read(&tmp).unwrap(),
            full_bytes,
            "patched file must equal the full serialization"
        );

        println!(
            "  {label}:\n    full: serialize {:.1} ms + write {:.1} ms, {} B written\n    patched: {:.1} ms total, {} B written ({:.0}x fewer bytes, {:.1}x faster than serialize+write)",
            serialize_s * 1e3,
            full_write_s * 1e3,
            full_bytes.len(),
            patched_s * 1e3,
            stats.bytes_written,
            full_bytes.len() as f64 / stats.bytes_written.max(1) as f64,
            (serialize_s + full_write_s) / patched_s.max(1e-9),
        );
    }
}
