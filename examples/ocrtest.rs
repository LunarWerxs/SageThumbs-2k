//! Probe: `ocrtest <image>...` runs the OS WinRT OCR on each image and prints the
//! recognized text (verifies the "Copy text" verb end-to-end).
fn main() {
    for path in std::env::args().skip(1) {
        match sagethumbs2k::ocr_probe(&path) {
            Some(text) => println!("OK    {path}\n----\n{text}\n----"),
            None => println!("NONE  {path}  (no text found / no OCR language pack)"),
        }
    }
}
