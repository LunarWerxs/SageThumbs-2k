# Lazy Range Reader Sketch

This is the browser-side shape intended for `from_async_reader_lazy_local`.
Applications still need their own bundler and CORS setup, but the important
piece is that page reads use HTTP `Range` requests and the reader is allowed to
be `!Send` on `wasm32`.

```rust
use std::{
    io::{Error, ErrorKind, SeekFrom},
    pin::Pin,
    task::{Context, Poll},
};

use djvu_rs::djvu_async::from_async_reader_lazy_local;
use gloo_net::http::Request;
use tokio::io::{AsyncRead, AsyncSeek, ReadBuf};

struct RangeReader {
    url: String,
    pos: u64,
    len: u64,
    buf: Vec<u8>,
}

impl RangeReader {
    async fn open(url: String) -> Result<Self, gloo_net::Error> {
        let head = Request::head(&url).send().await?;
        let len = head
            .headers()
            .get("content-length")
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        Ok(Self { url, pos: 0, len, buf: Vec::new() })
    }

    async fn fetch(&self, start: u64, size: usize) -> Result<Vec<u8>, gloo_net::Error> {
        let end = start.saturating_add(size as u64).saturating_sub(1);
        Request::get(&self.url)
            .header("Range", &format!("bytes={start}-{end}"))
            .send()
            .await?
            .binary()
            .await
    }
}

impl AsyncSeek for RangeReader {
    fn start_seek(mut self: Pin<&mut Self>, pos: SeekFrom) -> std::io::Result<()> {
        self.buf.clear();
        self.pos = match pos {
            SeekFrom::Start(n) => n,
            SeekFrom::End(n) => self.len.saturating_add_signed(n),
            SeekFrom::Current(n) => self.pos.saturating_add_signed(n),
        };
        Ok(())
    }

    fn poll_complete(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<u64>> {
        Poll::Ready(Ok(self.pos))
    }
}

impl AsyncRead for RangeReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        out: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.buf.is_empty() {
            return Poll::Ready(Err(Error::new(
                ErrorKind::WouldBlock,
                "drive RangeReader::fetch(pos, out.remaining()) from the browser task",
            )));
        }
        let n = out.remaining().min(self.buf.len());
        out.put_slice(&self.buf[..n]);
        self.buf.drain(..n);
        self.pos += n as u64;
        Poll::Ready(Ok(()))
    }
}

async fn open_first_page(url: String) -> Result<(), Box<dyn std::error::Error>> {
    let reader = RangeReader::open(url).await?;
    let doc = from_async_reader_lazy_local(reader).await?;
    let page = doc.page_async(0).await?;
    web_sys::console::log_1(&format!("{}x{}", page.width(), page.height()).into());
    Ok(())
}
```

Production code should turn the `WouldBlock` placeholder into a real pending
future bridge (for example by storing a `JsFuture` in the reader), but the HTTP
contract stays the same: every fetch is `Range: bytes=start-end`, and the lazy
document asks only for the DIRM and page/component byte ranges it touches.
