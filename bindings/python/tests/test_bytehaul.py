"""Tests for the bytehaul Python bindings."""

import http.server
import pathlib
import threading
import time

import pytest

import bytehaul
from bytehaul import (
    BytehaulError,
    CancelledError,
    PausedError,
    ConfigError,
    DownloadFailedError,
    Downloader,
    DownloadTask,
    ProgressSnapshot,
    download,
)


# ---------------------------------------------------------------------------
# Helpers: local HTTP server
# ---------------------------------------------------------------------------

SAMPLE_BODY = b"hello bytehaul " * 64  # 960 bytes


class _Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):  # noqa: N802
        if self.path == "/ok":
            self.send_response(200)
            self.send_header("Content-Length", str(len(SAMPLE_BODY)))
            self.end_headers()
            self.wfile.write(SAMPLE_BODY)
        elif self.path == "/slow":
            self.send_response(200)
            self.send_header("Content-Length", str(len(SAMPLE_BODY)))
            self.end_headers()
            for chunk in (SAMPLE_BODY[i : i + 64] for i in range(0, len(SAMPLE_BODY), 64)):
                self.wfile.write(chunk)
                self.wfile.flush()
                time.sleep(0.05)
        elif self.path == "/404":
            self.send_response(404)
            self.send_header("Content-Length", "0")
            self.end_headers()
        else:
            self.send_response(500)
            self.end_headers()

    def log_message(self, format, *args):  # noqa: A002
        pass  # suppress log noise


@pytest.fixture(scope="module")
def server():
    srv = http.server.HTTPServer(("127.0.0.1", 0), _Handler)
    port = srv.server_address[1]
    thread = threading.Thread(target=srv.serve_forever, daemon=True)
    thread.start()
    yield f"http://127.0.0.1:{port}"
    srv.shutdown()


# ---------------------------------------------------------------------------
# Import / smoke tests
# ---------------------------------------------------------------------------


class TestImports:
    def test_version(self):
        assert isinstance(bytehaul.__version__, str)
        assert bytehaul.__version__

    def test_all_exports(self):
        for name in bytehaul.__all__:
            assert hasattr(bytehaul, name)

    def test_exception_hierarchy(self):
        assert issubclass(CancelledError, BytehaulError)
        assert issubclass(PausedError, BytehaulError)
        assert issubclass(ConfigError, BytehaulError)
        assert issubclass(DownloadFailedError, BytehaulError)


# ---------------------------------------------------------------------------
# Config validation tests
# ---------------------------------------------------------------------------


class TestConfigValidation:
    def test_connect_timeout_negative(self, server, tmp_path):
        with pytest.raises(ConfigError, match="connect_timeout"):
            download(f"{server}/ok", str(tmp_path / "out"), connect_timeout=-1.0)

    def test_connect_timeout_nan(self, server, tmp_path):
        with pytest.raises(ConfigError, match="connect_timeout"):
            download(f"{server}/ok", str(tmp_path / "out"), connect_timeout=float("nan"))

    def test_max_connections_zero(self, server, tmp_path):
        with pytest.raises(ConfigError, match="max_connections"):
            download(f"{server}/ok", str(tmp_path / "out"), max_connections=0)

    def test_piece_size_zero(self, server, tmp_path):
        with pytest.raises(ConfigError, match="piece_size"):
            download(f"{server}/ok", str(tmp_path / "out"), piece_size=0)

    def test_memory_budget_zero(self, server, tmp_path):
        with pytest.raises(ConfigError, match="memory_budget"):
            download(f"{server}/ok", str(tmp_path / "out"), memory_budget=0)

    def test_checksum_empty(self, server, tmp_path):
        with pytest.raises(ConfigError, match="checksum_sha256"):
            download(f"{server}/ok", str(tmp_path / "out"), checksum_sha256="")

    def test_file_allocation_invalid(self, server, tmp_path):
        with pytest.raises(ConfigError, match="file_allocation"):
            download(f"{server}/ok", str(tmp_path / "out"), file_allocation="invalid")

    def test_dns_servers_invalid(self, server, tmp_path):
        with pytest.raises(ConfigError, match="dns_servers"):
            download(f"{server}/ok", str(tmp_path / "out"), dns_servers=["not-an-ip"])


# ---------------------------------------------------------------------------
# download() convenience API tests
# ---------------------------------------------------------------------------


class TestDownloadFunction:
    def test_basic_download(self, server, tmp_path):
        out = tmp_path / "basic.bin"
        download(f"{server}/ok", str(out))
        assert out.read_bytes() == SAMPLE_BODY

    def test_pathlib_path(self, server, tmp_path):
        out = tmp_path / "pathlib.bin"
        download(f"{server}/ok", out)
        assert out.read_bytes() == SAMPLE_BODY

    def test_404_raises(self, server, tmp_path):
        with pytest.raises(DownloadFailedError):
            download(f"{server}/404", str(tmp_path / "err"))

    def test_with_options(self, server, tmp_path):
        out = tmp_path / "opts.bin"
        download(
            f"{server}/ok",
            str(out),
            max_connections=2,
            file_allocation="none",
            resume=False,
        )
        assert out.read_bytes() == SAMPLE_BODY

    def test_with_network_options(self, server, tmp_path):
        out = tmp_path / "netopts.bin"
        download(
            f"{server}/ok",
            str(out),
            dns_servers=["1.1.1.1"],
            enable_ipv6=False,
        )
        assert out.read_bytes() == SAMPLE_BODY


# ---------------------------------------------------------------------------
# Downloader / DownloadTask object API tests
# ---------------------------------------------------------------------------


class TestDownloaderAPI:
    def test_create_default(self):
        d = Downloader()
        assert d is not None

    def test_create_with_timeout(self):
        d = Downloader(connect_timeout=10.0)
        assert d is not None

    def test_invalid_timeout(self):
        with pytest.raises(ConfigError):
            Downloader(connect_timeout=-5.0)

    def test_create_with_network_options(self):
        d = Downloader(dns_servers=["1.1.1.1"], enable_ipv6=False)
        assert d is not None

    def test_invalid_dns_servers(self):
        with pytest.raises(ConfigError, match="dns_servers"):
            Downloader(dns_servers=["bad dns"])

    def test_download_and_wait(self, server, tmp_path):
        out = tmp_path / "dl.bin"
        d = Downloader()
        task = d.download(f"{server}/ok", str(out))
        assert isinstance(task, DownloadTask)
        task.wait()
        assert out.read_bytes() == SAMPLE_BODY

    def test_progress_before_wait(self, server, tmp_path):
        out = tmp_path / "prog.bin"
        d = Downloader()
        task = d.download(f"{server}/slow", str(out))
        snap = task.progress()
        assert isinstance(snap, ProgressSnapshot)
        assert snap.state in ("pending", "downloading", "completed")
        assert snap.downloaded >= 0
        assert snap.speed >= 0.0
        assert snap.eta_secs is None or snap.eta_secs >= 0.0
        task.wait()
        assert out.read_bytes() == SAMPLE_BODY

    def test_progress_snapshot_repr(self, server, tmp_path):
        out = tmp_path / "repr.bin"
        d = Downloader()
        task = d.download(f"{server}/ok", str(out))
        task.wait()
        # After wait the handle is consumed; test repr on a fresh task
        d2 = Downloader()
        task2 = d2.download(f"{server}/slow", str(tmp_path / "repr2.bin"))
        snap = task2.progress()
        r = repr(snap)
        assert "ProgressSnapshot" in r
        task2.wait()

    def test_cancel(self, server, tmp_path):
        out = tmp_path / "cancel.bin"
        d = Downloader()
        task = d.download(f"{server}/slow", str(out))
        task.cancel()
        with pytest.raises(CancelledError):
            task.wait()

    def test_pause(self, server, tmp_path):
        out = tmp_path / "pause.bin"
        d = Downloader()
        task = d.download(f"{server}/slow", str(out), resume=True)
        time.sleep(0.05)
        task.pause()
        with pytest.raises(PausedError):
            task.wait()

    def test_wait_twice_raises(self, server, tmp_path):
        out = tmp_path / "twice.bin"
        d = Downloader()
        task = d.download(f"{server}/ok", str(out))
        task.wait()
        with pytest.raises(BytehaulError, match="already consumed"):
            task.wait()

    def test_progress_after_wait_raises(self, server, tmp_path):
        out = tmp_path / "pa.bin"
        d = Downloader()
        task = d.download(f"{server}/ok", str(out))
        task.wait()
        with pytest.raises(BytehaulError, match="already consumed"):
            task.progress()

    def test_cancel_after_wait_is_noop(self, server, tmp_path):
        out = tmp_path / "cn.bin"
        d = Downloader()
        task = d.download(f"{server}/ok", str(out))
        task.wait()
        task.cancel()  # should not raise

    def test_download_with_options(self, server, tmp_path):
        out = tmp_path / "dlopts.bin"
        d = Downloader()
        task = d.download(
            f"{server}/ok",
            str(out),
            max_connections=2,
            file_allocation="none",
            resume=False,
        )
        task.wait()
        assert out.read_bytes() == SAMPLE_BODY

    def test_download_404(self, server, tmp_path):
        d = Downloader()
        task = d.download(f"{server}/404", str(tmp_path / "e404"))
        with pytest.raises(DownloadFailedError):
            task.wait()
