#!/usr/bin/env python3
"""Static server with HTTP Range support and per-connection bandwidth throttling.

For the #588 lazy-open benchmark (`bench_lazy_open.html`): `.djvu` responses
honour `Range:` headers and are throttled to a fixed bandwidth so both the
full-download and the lazy path see the same simulated network. Everything
else (HTML/JS/wasm) is served unthrottled.

    python3 examples/wasm/serve_lazy_bench.py [--port 8080] [--bandwidth-mib 12.5]

Then open http://localhost:<port>/bench_lazy_open.html
"""

import argparse
import http.server
import os
import time

ROOT = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(ROOT))

BANDWIDTH = 12.5 * 1024 * 1024  # bytes/sec, set from CLI


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def do_HEAD(self):
        path = self.path.split("?")[0].lstrip("/")
        if path.startswith("corpus/"):
            fs = os.path.join(REPO, "tests", "corpus", os.path.basename(path))
        else:
            fs = os.path.join(ROOT, path or "bench_lazy_open.html")
        if not os.path.isfile(fs):
            self.send_error(404)
            return
        self.send_response(200)
        self.send_header("Content-Length", str(os.path.getsize(fs)))
        self.send_header("Accept-Ranges", "bytes")
        self.end_headers()

    def do_GET(self):
        path = self.path.split("?")[0].lstrip("/")
        # Corpus files are addressed as /corpus/<name>; everything else is
        # served from examples/wasm/.
        if path.startswith("corpus/"):
            fs = os.path.join(REPO, "tests", "corpus", os.path.basename(path))
        else:
            fs = os.path.join(ROOT, path or "bench_lazy_open.html")
        if not os.path.isfile(fs):
            self.send_error(404)
            return
        size = os.path.getsize(fs)
        throttled = fs.endswith(".djvu")

        start, end = 0, size  # end exclusive
        rng = self.headers.get("Range")
        if rng and rng.startswith("bytes="):
            a, _, b = rng[len("bytes=") :].partition("-")
            start = int(a)
            end = min(int(b) + 1, size) if b else size
            self.send_response(206)
            self.send_header("Content-Range", f"bytes {start}-{end - 1}/{size}")
        else:
            self.send_response(200)

        body_len = end - start
        ctype = {
            ".html": "text/html",
            ".js": "text/javascript",
            ".wasm": "application/wasm",
        }.get(os.path.splitext(fs)[1], "application/octet-stream")
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(body_len))
        self.send_header("Accept-Ranges", "bytes")
        self.end_headers()

        with open(fs, "rb") as f:
            f.seek(start)
            remaining = body_len
            chunk = 16 * 1024
            while remaining > 0:
                data = f.read(min(chunk, remaining))
                if not data:
                    break
                try:
                    self.wfile.write(data)
                except BrokenPipeError:
                    return
                remaining -= len(data)
                if throttled:
                    time.sleep(len(data) / BANDWIDTH)


def main():
    global BANDWIDTH
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=8080)
    ap.add_argument("--bandwidth-mib", type=float, default=12.5)
    args = ap.parse_args()
    BANDWIDTH = args.bandwidth_mib * 1024 * 1024
    srv = http.server.ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    print(
        f"serving http://127.0.0.1:{args.port}/bench_lazy_open.html "
        f"(.djvu throttled to {args.bandwidth_mib} MiB/s)"
    )
    srv.serve_forever()


if __name__ == "__main__":
    main()
