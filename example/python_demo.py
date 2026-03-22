#!/usr/bin/env python3
"""Self-contained demo for the bytehaul Python bindings.

Run from the repository root:

    uv sync --project bindings/python
    uv run --project bindings/python maturin develop
    uv run --project bindings/python python example/python_demo.py
"""

from __future__ import annotations

import hashlib
import http.server
import tempfile
import threading
import time
from pathlib import Path

from bytehaul import CancelledError, ConfigError, DownloadFailedError, Downloader

DEMO_PATH = "/demo.bin"
DEMO_SIZE = 4 * 1024 * 1024
PIECE_SIZE = 256 * 1024
STREAM_CHUNK_SIZE = 64 * 1024
STREAM_DELAY_SECS = 0.02


def build_demo_payload(size: int) -> bytes:
    pattern = b"bytehaul-python-demo-"
    repeats = (size // len(pattern)) + 1
    return (pattern * repeats)[:size]


DEMO_PAYLOAD = build_demo_payload(DEMO_SIZE)
DEMO_SHA256 = hashlib.sha256(DEMO_PAYLOAD).hexdigest()


def parse_range_header(value: str | None, total_size: int) -> tuple[int, int] | None:
    if not value:
        return None

    try:
        unit, spec = value.split("=", 1)
        if unit.strip().lower() != "bytes" or "," in spec:
            raise ValueError

        start_text, end_text = spec.split("-", 1)
        if not start_text:
            length = int(end_text)
            if length <= 0:
                raise ValueError
            start = max(total_size - length, 0)
            end = total_size - 1
        else:
            start = int(start_text)
            end = total_size - 1 if not end_text else int(end_text)

        if start < 0 or end < start or start >= total_size:
            raise ValueError

        return start, min(end, total_size - 1)
    except (IndexError, ValueError) as exc:
        raise ValueError(f"invalid range header: {value!r}") from exc


class DemoHandler(http.server.BaseHTTPRequestHandler):
    server_version = "BytehaulDemo/1.0"

    def do_GET(self) -> None:  # noqa: N802
        self.serve_payload(head_only=False)

    def do_HEAD(self) -> None:  # noqa: N802
        self.serve_payload(head_only=True)

    def serve_payload(self, *, head_only: bool) -> None:
        if self.path != DEMO_PATH:
            self.send_error(404, "not found")
            return

        total_size = len(DEMO_PAYLOAD)

        try:
            requested_range = parse_range_header(self.headers.get("Range"), total_size)
        except ValueError:
            self.send_response(416)
            self.send_header("Content-Range", f"bytes */{total_size}")
            self.end_headers()
            return

        if requested_range is None:
            status = 200
            start = 0
            end = total_size - 1
        else:
            status = 206
            start, end = requested_range

        body = DEMO_PAYLOAD[start : end + 1]

        self.send_response(status)
        self.send_header("Accept-Ranges", "bytes")
        self.send_header("Cache-Control", "no-store")
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("ETag", f"\"{DEMO_SHA256}\"")
        if status == 206:
            self.send_header("Content-Range", f"bytes {start}-{end}/{total_size}")
        self.end_headers()

        if head_only:
            return

        try:
            for offset in range(0, len(body), STREAM_CHUNK_SIZE):
                chunk = body[offset : offset + STREAM_CHUNK_SIZE]
                self.wfile.write(chunk)
                self.wfile.flush()
                time.sleep(STREAM_DELAY_SECS)
        except BrokenPipeError:
            pass

    def log_message(self, format: str, *args: object) -> None:  # noqa: A002
        pass


def format_bytes(value: float | int | None) -> str:
    if value is None:
        return "unknown"

    size = float(value)
    for unit in ("B", "KiB", "MiB", "GiB"):
        if size < 1024.0 or unit == "GiB":
            if unit == "B":
                return f"{int(size)} {unit}"
            return f"{size:.1f} {unit}"
        size /= 1024.0

    return f"{size:.1f} TiB"


def print_progress(task) -> None:
    last_render = None

    while True:
        snap = task.progress()
        total = format_bytes(snap.total_size)
        current = format_bytes(snap.downloaded)
        speed = format_bytes(snap.speed)
        elapsed = "n/a" if snap.elapsed_secs is None else f"{snap.elapsed_secs:.1f}s"
        render = (
            f"state={snap.state:<11} downloaded={current:>10} / {total:<10} "
            f"speed={speed:>10}/s elapsed={elapsed}"
        )
        if render != last_render:
            print(render)
            last_render = render

        if snap.state in {"completed", "failed", "cancelled"}:
            break

        time.sleep(0.2)


def start_demo_server() -> tuple[http.server.ThreadingHTTPServer, str]:
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), DemoHandler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    url = f"http://127.0.0.1:{server.server_port}{DEMO_PATH}"
    return server, url


def main() -> int:
    output_root = Path(tempfile.mkdtemp(prefix="bytehaul-demo-"))
    output_path = output_root / "downloaded.bin"
    server, url = start_demo_server()

    print("Starting local demo server...")
    print(f"Source URL: {url}")
    print(f"Output file: {output_path}")
    print(f"Expected SHA-256: {DEMO_SHA256}")

    try:
        downloader = Downloader(connect_timeout=5.0)
        task = downloader.download(
            url,
            output_path,
            checksum_sha256=DEMO_SHA256,
            file_allocation="none",
            max_connections=4,
            min_split_size=PIECE_SIZE,
            piece_size=PIECE_SIZE,
            resume=True,
        )
        print_progress(task)
        task.wait()

        actual_sha256 = hashlib.sha256(output_path.read_bytes()).hexdigest()
        print(f"Download complete: {format_bytes(output_path.stat().st_size)} written")
        print(f"Verified SHA-256: {actual_sha256}")
        print(f"Artifacts kept at: {output_root}")
        return 0
    except ConfigError as exc:
        print(f"Configuration error: {exc}")
        return 2
    except CancelledError:
        print("Download was cancelled")
        return 3
    except DownloadFailedError as exc:
        print(f"Download failed: {exc}")
        return 4
    finally:
        server.shutdown()
        server.server_close()


if __name__ == "__main__":
    raise SystemExit(main())
