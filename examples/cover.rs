// Probe: decode each path argument through the full pipeline (container cover
// extraction included) and print the resulting image size.
fn main() {
    for path in std::env::args().skip(1) {
        match std::fs::read(&path) {
            Ok(bytes) => match sagethumbs2k::probe_cover(&bytes) {
                Some((w, h)) => println!("OK    {w}x{h}\t{path}"),
                None => println!("FAIL  (no thumbnail)\t{path}"),
            },
            Err(e) => println!("ERR   {e}\t{path}"),
        }
    }
}
