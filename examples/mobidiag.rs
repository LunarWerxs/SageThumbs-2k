// One-off MOBI structure dumper for troubleshooting cover extraction.
fn be16(b: &[u8], o: usize) -> Option<u16> { Some(u16::from_be_bytes(b.get(o..o + 2)?.try_into().ok()?)) }
fn be32(b: &[u8], o: usize) -> Option<u32> { Some(u32::from_be_bytes(b.get(o..o + 4)?.try_into().ok()?)) }

fn main() {
    for path in std::env::args().skip(1) {
        let bytes = std::fs::read(&path).unwrap();
        let name = std::path::Path::new(&path).file_name().unwrap().to_string_lossy();
        println!("=== {name} ({} bytes) ===", bytes.len());
        let rc = be16(&bytes, 76).unwrap_or(0) as usize;
        let recoff = |n: usize| be32(&bytes, 78 + n * 8).map(|v| v as usize);
        let rec = |n: usize| -> Option<&[u8]> {
            let s = recoff(n)?;
            let e = if n + 1 < rc { recoff(n + 1)? } else { bytes.len() };
            if e < s { return None; }
            bytes.get(s..e)
        };
        let Some(rec0) = rec(0) else { println!("  no rec0"); continue };
        println!("  rec_count={rc}  enc@12={:?}  MOBI@16={:?}", be16(rec0, 12), std::str::from_utf8(rec0.get(16..20).unwrap_or(b"")));
        let mlen = be32(rec0, 20).unwrap_or(0) as usize;
        println!("  mobi_len={mlen}  firstImage: [0x68=104]={:?} [16+0x68=120]={:?} [16+0x6C=124]={:?}",
            be32(rec0, 104), be32(rec0, 120), be32(rec0, 124));
        println!("  exth_flags@144={:#x}", be32(rec0, 144).unwrap_or(0));
        let exs = 16 + mlen;
        if rec0.get(exs..exs + 4) == Some(b"EXTH") {
            let cnt = be32(rec0, exs + 8).unwrap_or(0) as usize;
            let mut p = exs + 12;
            print!("  EXTH({cnt}):");
            for _ in 0..cnt.min(300) {
                let Some(t) = be32(rec0, p) else { break };
                let l = be32(rec0, p + 4).unwrap_or(0) as usize;
                if l < 8 { break; }
                if matches!(t, 201 | 202) { print!(" [tag{t}=u32:{:?}]", be32(rec0, p + 8)); }
                p += l;
            }
            println!();
        } else { println!("  NO EXTH"); }
        print!("  image records:");
        for n in 0..rc {
            if let Some(d) = rec(n) {
                let mag = if d.starts_with(&[0xFF, 0xD8, 0xFF]) { "JPG" }
                    else if d.starts_with(b"\x89PNG") { "PNG" }
                    else if d.starts_with(b"GIF8") { "GIF" } else { "" };
                if !mag.is_empty() { print!(" {n}:{}b{mag}", d.len()); }
            }
        }
        println!("\n");
    }
}
