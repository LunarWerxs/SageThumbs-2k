//! First-page latency probe for the async lazy loader (#233).
//!
//! Usage:
//!
//! ```sh
//! cargo run --example async_lazy_first_page --features async -- \
//!   tests/corpus/pathogenic_bacteria_1896.djvu --bandwidth-mib 12.5 --dpi 150 --pad-to-mib 100
//! ```

use std::{
    env, fs,
    io::{Cursor, SeekFrom},
    path::PathBuf,
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

struct ThrottledCursor {
    inner: Cursor<Vec<u8>>,
    bytes_read: Arc<AtomicU64>,
    bandwidth_bytes_per_sec: Option<f64>,
}

impl ThrottledCursor {
    fn new(data: Vec<u8>, bandwidth_mib: Option<f64>, bytes_read: Arc<AtomicU64>) -> Self {
        Self {
            inner: Cursor::new(data),
            bytes_read,
            bandwidth_bytes_per_sec: bandwidth_mib.map(|mib| mib * 1024.0 * 1024.0),
        }
    }
}

impl AsyncRead for ThrottledCursor {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let available = self.inner.get_ref().len() as u64;
        let pos = self.inner.position();
        let remaining = available.saturating_sub(pos) as usize;
        let len = remaining.min(buf.remaining());
        let end = pos as usize + len;
        buf.put_slice(&self.inner.get_ref()[pos as usize..end]);
        self.inner.set_position(end as u64);

        let read = (buf.filled().len() - before) as u64;
        self.bytes_read.fetch_add(read, Ordering::Relaxed);
        if let Some(bytes_per_sec) = self.bandwidth_bytes_per_sec {
            let delay = Duration::from_secs_f64(read as f64 / bytes_per_sec);
            if !delay.is_zero() {
                std::thread::sleep(delay);
            }
        }
        Poll::Ready(Ok(()))
    }
}

impl AsyncSeek for ThrottledCursor {
    fn start_seek(mut self: Pin<&mut Self>, position: SeekFrom) -> std::io::Result<()> {
        use std::io::Seek;
        self.inner.seek(position)?;
        Ok(())
    }

    fn poll_complete(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<u64>> {
        Poll::Ready(Ok(self.inner.position()))
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    let path = PathBuf::from(
        args.next()
            .ok_or("usage: async_lazy_first_page <file.djvu>")?,
    );
    let mut bandwidth_mib = None;
    let mut dpi = 150u32;
    let mut pad_to_mib = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bandwidth-mib" => {
                bandwidth_mib = Some(
                    args.next()
                        .ok_or("--bandwidth-mib requires a value")?
                        .parse::<f64>()?,
                );
            }
            "--dpi" => {
                dpi = args
                    .next()
                    .ok_or("--dpi requires a value")?
                    .parse::<u32>()?;
            }
            "--pad-to-mib" => {
                pad_to_mib = Some(
                    args.next()
                        .ok_or("--pad-to-mib requires a value")?
                        .parse::<usize>()?,
                );
            }
            _ => return Err(format!("unknown argument: {arg}").into()),
        }
    }

    let mut data = fs::read(&path)?;
    if let Some(mib) = pad_to_mib {
        pad_djvu_to_len(&mut data, mib * 1024 * 1024)?;
    }
    let file_bytes = data.len() as u64;
    let bytes_read = Arc::new(AtomicU64::new(0));
    let reader = ThrottledCursor::new(data, bandwidth_mib, bytes_read.clone());

    let start = Instant::now();
    let lazy = from_async_reader_lazy(reader).await?;
    let indexed = start.elapsed();
    let page = lazy.page_async(0).await?;
    let fetched = start.elapsed();

    let width = ((page.width() as u32 * dpi) / page.dpi() as u32).max(1);
    let height = ((page.height() as u32 * dpi) / page.dpi() as u32).max(1);
    let pixmap = djvu_render::render_pixmap(
        &page,
        &RenderOptions {
            width,
            height,
            permissive: true,
            ..RenderOptions::default()
        },
    )?;
    let first_pixel = start.elapsed();

    println!("file={}", path.display());
    println!("file_bytes={file_bytes}");
    println!("page_count={}", lazy.page_count());
    println!("bytes_read={}", bytes_read.load(Ordering::Relaxed));
    println!("indexed_ms={:.3}", indexed.as_secs_f64() * 1000.0);
    println!("page_fetched_ms={:.3}", fetched.as_secs_f64() * 1000.0);
    println!("first_pixel_ms={:.3}", first_pixel.as_secs_f64() * 1000.0);
    println!("rendered={}x{}", pixmap.width, pixmap.height);

    Ok(())
}

fn pad_djvu_to_len(
    data: &mut Vec<u8>,
    target_len: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    if data.len() >= target_len {
        return Ok(());
    }
    if data.len() < 12 || &data[..4] != b"AT&T" || &data[4..8] != b"FORM" {
        return Err("--pad-to-mib requires an AT&T FORM DjVu document".into());
    }

    let needed = target_len
        .checked_sub(data.len())
        .ok_or("target length overflow")?;
    if needed < 8 {
        return Ok(());
    }

    let payload_len = needed - 8;
    if payload_len > u32::MAX as usize {
        return Err("padding chunk exceeds u32 chunk length".into());
    }
    data.extend_from_slice(b"JUNK");
    data.extend_from_slice(&(payload_len as u32).to_be_bytes());
    data.resize(target_len, 0);

    let form_len = data.len().checked_sub(8).ok_or("FORM length underflow")?;
    if form_len > u32::MAX as usize {
        return Err("padded FORM exceeds u32 length".into());
    }
    data[8..12].copy_from_slice(&(form_len as u32).to_be_bytes());
    Ok(())
}
