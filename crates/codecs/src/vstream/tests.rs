use super::*;
use windows::Win32::UI::Shell::SHCreateMemStream;

/// A source three 4 KiB blocks and a bit long, each byte different from its neighbours, behind
/// a [`BlockCacheStream`] reading it 4 KiB at a time.
fn stream(len: usize) -> (Vec<u8>, IStream) {
    let src: Vec<u8> = (0..len).map(|i| (i * 7 % 251) as u8).collect();
    let inner = unsafe { SHCreateMemStream(Some(&src)) }.expect("memory stream");
    let deadline = Instant::now() + std::time::Duration::from_secs(60);
    let cache = BlockCacheStream::with_block(inner, len as u64, deadline, MIN_BLOCK);
    (src, cache.into())
}

/// What a decoder sees: the source's own bytes across block edges, a read running into the end
/// short with S_FALSE as a real stream's is, and nothing past the end. Windows' PDF engine reads
/// through this at 64 KiB blocks since issue #59, and a byte off at an edge is a corrupt PDF.
#[test]
fn reads_across_blocks_are_the_sources_bytes_and_end_like_a_stream() {
    let (src, s) = stream(3 * 4096 + 123);
    let read_at = |off: usize, n: usize| {
        unsafe { s.Seek(off as i64, STREAM_SEEK_SET, None) }.expect("seek");
        let mut buf = vec![0u8; n];
        let mut got = 0u32;
        let hr = unsafe { s.Read(buf.as_mut_ptr() as *mut c_void, n as u32, Some(&mut got)) };
        buf.truncate(got as usize);
        (hr, buf)
    };
    let (hr, buf) = read_at(4000, 5000);
    assert_eq!(
        (hr, &buf[..]),
        (S_OK, &src[4000..9000]),
        "across two block edges"
    );
    let end = src.len();
    let (hr, buf) = read_at(end - 10, 100);
    assert_eq!((hr, &buf[..]), (S_FALSE, &src[end - 10..]), "into the end");
    let (hr, buf) = read_at(end + 5, 8);
    assert_eq!((hr, buf.len()), (S_FALSE, 0), "past the end");
}
