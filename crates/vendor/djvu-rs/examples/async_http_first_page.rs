//! HTTP-Range time-to-first-page probe for the async lazy loader (#584).
//!
//! Self-contained: spawns a local throttled HTTP/1.1 server (std::net, Range
//! support, fixed bandwidth) and opens the document through an
//! `AsyncRead + AsyncSeek` adapter that fetches 64 KiB blocks over Range GETs
//! with a small LRU block cache — the seek-heavy DIRM walk must not issue one
//! GET per read.
//!
//! Measures time-to-first-page and bytes fetched for LazyDocument-over-HTTP
//! vs full-download-then-open.
//!
//! ```sh
//! cargo run --release --example async_http_first_page --features async -- \
//!   tests/corpus/pathogenic_bacteria_1896.djvu --bandwidth-mib 12.5 --dpi 150
//! ```

use std::{
    collections::HashMap,
    io::{Read, SeekFrom, Write},
    net::{TcpListener, TcpStream},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::{Duration, Instant},
};

use djvu_rs::{
    djvu_async::from_async_reader_lazy,
    djvu_render::{self, RenderOptions},
};
use tokio::io::{AsyncRead, AsyncSeek, ReadBuf};

const BLOCK: u64 = 64 * 1024;
const CACHE_BLOCKS: usize = 64; // 4 MiB block cache

// ── Local throttled Range server ─────────────────────────────────────────────

fn spawn_range_server(data: Arc<Vec<u8>>, bandwidth_bytes_per_sec: f64) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let data = Arc::clone(&data);
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                loop {
                    // Read one request (headers end with \r\n\r\n).
                    let mut req = Vec::new();
                    loop {
                        match stream.read(&mut buf) {
                            Ok(0) => return,
                            Ok(n) => {
                                req.extend_from_slice(&buf[..n]);
                                if req.windows(4).any(|w| w == b"\r\n\r\n") {
                                    break;
                                }
                            }
                            Err(_) => return,
                        }
                    }
                    let text = String::from_utf8_lossy(&req);
                    let range = text
                        .lines()
                        .find(|l| l.to_ascii_lowercase().starts_with("range:"))
                        .and_then(|l| l.split('=').nth(1))
                        .and_then(|spec| {
                            let (a, b) = spec.trim().split_once('-')?;
                            Some((a.parse::<u64>().ok()?, b.parse::<u64>().ok()?))
                        });
                    let (start, end) = match range {
                        Some((a, b)) => (a as usize, ((b + 1) as usize).min(data.len())),
                        None => (0, data.len()),
                    };
                    let body = &data[start.min(data.len())..end];
                    let head = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nConnection: keep-alive\r\n\r\n",
                        body.len(),
                        start,
                        end.saturating_sub(1),
                        data.len()
                    );
                    if stream.write_all(head.as_bytes()).is_err() {
                        return;
                    }
                    // Throttle the body at the configured bandwidth.
                    let chunk = 16 * 1024;
                    for part in body.chunks(chunk) {
                        if stream.write_all(part).is_err() {
                            return;
                        }
                        let secs = part.len() as f64 / bandwidth_bytes_per_sec;
                        std::thread::sleep(Duration::from_secs_f64(secs));
                    }
                }
            });
        }
    });
    port
}

// ── Blocking Range client with a block LRU, exposed as AsyncRead+AsyncSeek ──
//
// The blocking GET inside poll_read is deliberate for this measurement
// example (multi-thread tokio runtime; the server lives on its own OS
// threads): it keeps the adapter dependency-free and deterministic.

struct HttpRangeReader {
    port: u16,
    len: u64,
    pos: u64,
    conn: Option<TcpStream>,
    cache: HashMap<u64, Vec<u8>>, // block index → bytes
    lru: Vec<u64>,
    bytes_fetched: Arc<AtomicU64>,
    requests: Arc<AtomicU64>,
}

impl HttpRangeReader {
    fn new(port: u16, len: u64, bytes: Arc<AtomicU64>, reqs: Arc<AtomicU64>) -> Self {
        Self {
            port,
            len,
            pos: 0,
            conn: None,
            cache: HashMap::new(),
            lru: Vec::new(),
            bytes_fetched: bytes,
            requests: reqs,
        }
    }

    fn fetch_block(&mut self, block: u64) -> std::io::Result<()> {
        if self.cache.contains_key(&block) {
            return Ok(());
        }
        let start = block * BLOCK;
        let end = (start + BLOCK - 1).min(self.len.saturating_sub(1));
        if self.conn.is_none() {
            self.conn = Some(TcpStream::connect(("127.0.0.1", self.port))?);
        }
        let conn = self.conn.as_mut().unwrap();
        write!(
            conn,
            "GET /doc HTTP/1.1\r\nHost: localhost\r\nRange: bytes={start}-{end}\r\n\r\n"
        )?;
        // Read headers.
        let mut header = Vec::new();
        let mut byte = [0u8; 1];
        while !header.windows(4).any(|w| w == b"\r\n\r\n") {
            conn.read_exact(&mut byte)?;
            header.push(byte[0]);
        }
        let text = String::from_utf8_lossy(&header);
        let clen: usize = text
            .lines()
            .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
            .and_then(|l| l.split(':').nth(1))
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        let mut body = vec![0u8; clen];
        conn.read_exact(&mut body)?;
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.bytes_fetched
            .fetch_add(body.len() as u64, Ordering::Relaxed);
        self.cache.insert(block, body);
        self.lru.push(block);
        if self.lru.len() > CACHE_BLOCKS {
            let old = self.lru.remove(0);
            self.cache.remove(&old);
        }
        Ok(())
    }
}

impl AsyncRead for HttpRangeReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.pos >= self.len {
            return Poll::Ready(Ok(()));
        }
        let block = self.pos / BLOCK;
        if let Err(e) = self.fetch_block(block) {
            return Poll::Ready(Err(e));
        }
        let data = &self.cache[&block];
        let off = (self.pos - block * BLOCK) as usize;
        let n = buf.remaining().min(data.len().saturating_sub(off));
        buf.put_slice(&data[off..off + n]);
        self.pos += n as u64;
        Poll::Ready(Ok(()))
    }
}

impl AsyncSeek for HttpRangeReader {
    fn start_seek(mut self: Pin<&mut Self>, position: SeekFrom) -> std::io::Result<()> {
        self.pos = match position {
            SeekFrom::Start(o) => o,
            SeekFrom::End(o) => (self.len as i64 + o).max(0) as u64,
            SeekFrom::Current(o) => (self.pos as i64 + o).max(0) as u64,
        };
        Ok(())
    }
    fn poll_complete(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<u64>> {
        Poll::Ready(Ok(self.pos))
    }
}

// ── Probe ─────────────────────────────────────────────────────────────────────

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .unwrap_or_else(|| "tests/corpus/pathogenic_bacteria_1896.djvu".to_string());
    let mut bandwidth_mib = 12.5f64;
    let mut dpi = 150.0f32;
    let mut page_no = 0usize;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--bandwidth-mib" => bandwidth_mib = args.next().unwrap().parse().unwrap(),
            "--dpi" => dpi = args.next().unwrap().parse().unwrap(),
            "--page" => page_no = args.next().unwrap().parse().unwrap(),
            _ => {}
        }
    }
    let data = Arc::new(std::fs::read(&path).unwrap());
    let len = data.len() as u64;
    let bw = bandwidth_mib * 1024.0 * 1024.0;
    let port = spawn_range_server(Arc::clone(&data), bw);
    println!(
        "{path}: {:.1} MiB at {:.1} MiB/s (port {port})",
        len as f64 / 1048576.0,
        bandwidth_mib
    );

    // A: full download, then open + render page 0.
    let t0 = Instant::now();
    let full = {
        let secs = len as f64 / bw;
        std::thread::sleep(Duration::from_secs_f64(secs)); // modeled download
        djvu_rs::djvu_document::DjVuDocument::parse(&data).unwrap()
    };
    let page = full.page(page_no).unwrap();
    let scale = dpi / page.dpi().max(1) as f32;
    let w = ((page.width() as f32 * scale).round() as u32).max(1);
    let h = ((page.height() as f32 * scale).round() as u32).max(1);
    let full_px = djvu_render::render_pixmap(
        page,
        &RenderOptions {
            width: w,
            height: h,
            ..Default::default()
        },
    )
    .unwrap();
    let full_ttfp = t0.elapsed().as_secs_f64();

    // B: lazy over HTTP Range.
    let bytes = Arc::new(AtomicU64::new(0));
    let reqs = Arc::new(AtomicU64::new(0));
    let reader = HttpRangeReader::new(port, len, Arc::clone(&bytes), Arc::clone(&reqs));
    let t1 = Instant::now();
    let lazy = from_async_reader_lazy(reader).await.unwrap();
    eprintln!(
        "  after open: {:.1} KiB in {} GETs",
        bytes.load(Ordering::Relaxed) as f64 / 1024.0,
        reqs.load(Ordering::Relaxed)
    );
    let p0 = lazy.page_async(page_no).await.unwrap();
    eprintln!(
        "  after page {page_no} fetch: {:.1} KiB in {} GETs",
        bytes.load(Ordering::Relaxed) as f64 / 1024.0,
        reqs.load(Ordering::Relaxed)
    );
    let scale = dpi / p0.dpi().max(1) as f32;
    let w = ((p0.width() as f32 * scale).round() as u32).max(1);
    let h = ((p0.height() as f32 * scale).round() as u32).max(1);
    let lazy_px = djvu_render::render_pixmap(
        &p0,
        &RenderOptions {
            width: w,
            height: h,
            ..Default::default()
        },
    )
    .unwrap();
    let lazy_ttfp = t1.elapsed().as_secs_f64();
    assert_eq!(
        full_px.data, lazy_px.data,
        "lazy render must be byte-identical to the full-document render"
    );

    println!(
        "full-download-then-open: {:.2}s   lazy-over-http: {:.2}s ({:.1}x)   fetched {:.1} KiB in {} GETs ({:.2}% of file)",
        full_ttfp,
        lazy_ttfp,
        full_ttfp / lazy_ttfp,
        bytes.load(Ordering::Relaxed) as f64 / 1024.0,
        reqs.load(Ordering::Relaxed),
        100.0 * bytes.load(Ordering::Relaxed) as f64 / len as f64,
    );
}
