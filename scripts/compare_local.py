#!/usr/bin/env python3
"""Controlled localhost comparison for bytehaul and aria2.

The fixture supports Range and keep-alive. Each run gets a fresh server so the
first request beginning in the selected byte band can be normal, throttled, or
stalled once. Server-side JSONL records make retries and redistribution visible.
"""

import argparse
import csv
import hashlib
import json
import os
import random
import socket
import statistics
import subprocess
import threading
import time
from dataclasses import dataclass, field
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
MIB = 1024 * 1024
CHUNK = 64 * 1024
PATTERN = bytes(range(251))


def fixture_bytes(start, count):
    offset = start % len(PATTERN)
    repeats = (offset + count + len(PATTERN) - 1) // len(PATTERN)
    return (PATTERN * repeats)[offset:offset + count]


@dataclass
class FixtureState:
    size: int
    scenario: str
    normal_rate: int
    slow_rate: int
    stall_seconds: float
    slow_band: tuple[int, int]
    started: float = field(default_factory=time.perf_counter)
    lock: threading.Lock = field(default_factory=threading.Lock)
    next_request: int = 1
    next_connection: int = 1
    injected: bool = False
    events: list[dict] = field(default_factory=list)

    def allocate(self, start, end, connection):
        with self.lock:
            request_id = self.next_request
            self.next_request += 1
            behavior = "normal"
            if (not self.injected and self.scenario != "normal"
                    and self.slow_band[0] <= start < self.slow_band[1]):
                behavior = self.scenario
                self.injected = True
            event = {
                "request_id": request_id,
                "connection_id": connection,
                "start": start,
                "end": end,
                "behavior": behavior,
                "started_ms": round((time.perf_counter() - self.started) * 1000, 3),
                "bytes_sent": 0,
                "disconnected": False,
            }
            self.events.append(event)
            return event


class FixtureServer(ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = True

    def __init__(self, state):
        super().__init__(("127.0.0.1", 0), FixtureHandler)
        self.state = state


class FixtureHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def setup(self):
        super().setup()
        with self.server.state.lock:
            self.connection_id = self.server.state.next_connection
            self.server.state.next_connection += 1

    def log_message(self, *_):
        pass

    def do_HEAD(self):
        self._headers(200, 0, self.server.state.size)

    def do_GET(self):
        state = self.server.state
        start, end = 0, state.size
        status = 200
        value = self.headers.get("Range")
        if value:
            try:
                unit, bounds = value.split("=", 1)
                first, last = bounds.split("-", 1)
                if unit != "bytes" or not first:
                    raise ValueError
                start = int(first)
                end = min(state.size, int(last) + 1 if last else state.size)
                if start < 0 or start >= end:
                    raise ValueError
                status = 206
            except ValueError:
                self.send_response(416)
                self.send_header("Content-Range", f"bytes */{state.size}")
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
        event = state.allocate(start, end, self.connection_id)
        self._headers(status, start, end)
        if event["behavior"] == "stall":
            time.sleep(state.stall_seconds)
        rate = state.slow_rate if event["behavior"] == "slow" else state.normal_rate
        at = start
        try:
            while at < end:
                count = min(CHUNK, end - at)
                # Deterministic file byte at offset n is n modulo 251.
                data = fixture_bytes(at, count)
                self.wfile.write(data)
                self.wfile.flush()
                at += count
                event["bytes_sent"] += count
                if rate:
                    time.sleep(count / rate)
        except (BrokenPipeError, ConnectionAbortedError, ConnectionResetError, socket.timeout):
            event["disconnected"] = True
            self.close_connection = True
        finally:
            event["ended_ms"] = round((time.perf_counter() - state.started) * 1000, 3)

    def _headers(self, status, start, end):
        state = self.server.state
        self.send_response(status)
        self.send_header("Content-Length", str(end - start if status == 206 else state.size))
        self.send_header("Accept-Ranges", "bytes")
        self.send_header("ETag", '"local-fixture-v1"')
        if status == 206:
            self.send_header("Content-Range", f"bytes {start}-{end - 1}/{state.size}")
        self.end_headers()


def expected_hash(size):
    digest = hashlib.sha256()
    at = 0
    while at < size:
        end = min(size, at + MIB)
        digest.update(fixture_bytes(at, end - at))
        at = end
    return digest.hexdigest()


def run_one(args, run_dir, scenario, repeat, tool, expected):
    state = FixtureState(
        size=args.size_mib * MIB,
        scenario=scenario,
        normal_rate=args.normal_mib * MIB,
        slow_rate=args.slow_kib * 1024,
        stall_seconds=args.stall_seconds,
        slow_band=(args.slow_band_mib * MIB, (args.slow_band_mib + 4) * MIB),
    )
    server = FixtureServer(state)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    folder = run_dir / f"{scenario}-r{repeat}-{tool}"
    folder.mkdir()
    output = folder / "fixture.bin"
    url = f"http://127.0.0.1:{server.server_port}/fixture.bin"
    io_timeout = 2 if scenario == "stall" else 30
    if tool == "bytehaul":
        command = [str(Path(args.bytehaul).resolve()), url, str(output), str(args.connections),
                   "--range-scheduling-mode", "dynamic", "--request-batch-size", "0",
                   "--dynamic-min-split-size", str(MIB),
                   "--dynamic-max-request-size", str(64 * MIB),
                   "--read-timeout-secs", str(io_timeout), "--headers-timeout-secs", "5",
                   "--log-level", args.log_level]
    else:
        lowest = "1M" if tool == "aria2-low-speed" else "0"
        command = [str(Path(args.aria2).resolve()), "--no-conf=true", "--no-netrc=true",
                   "--disable-ipv6=true", f"--split={args.connections}",
                   f"--max-connection-per-server={args.connections}", "--min-split-size=1M",
                   "--piece-length=1M", "--file-allocation=none", "--connect-timeout=5",
                   f"--timeout={io_timeout}", "--max-tries=3", "--retry-wait=1",
                   f"--lowest-speed-limit={lowest}", "--summary-interval=0",
                   "--console-log-level=notice", f"--dir={folder}", "--out=fixture.bin", url]
    (folder / "command.json").write_text(json.dumps(command, indent=2), encoding="utf-8")
    env = os.environ.copy()
    for key in list(env):
        if key.lower() in {"http_proxy", "https_proxy", "all_proxy", "no_proxy"}:
            env.pop(key)
    started = time.perf_counter()
    try:
        with (folder / "stdout.log").open("wb") as stdout, (folder / "stderr.log").open("wb") as stderr:
            status = subprocess.run(command, stdout=stdout, stderr=stderr, env=env,
                                    timeout=args.process_timeout).returncode
    except subprocess.TimeoutExpired:
        status = "timeout"
    seconds = time.perf_counter() - started
    server.shutdown()
    server.server_close()
    thread.join(timeout=2)
    (folder / "server.jsonl").write_text(
        "".join(json.dumps(event) + "\n" for event in state.events), encoding="utf-8")
    digest = None
    if status == 0 and output.exists():
        with output.open("rb") as stream:
            digest = hashlib.file_digest(stream, "sha256").hexdigest()
    injected = next((event for event in state.events if event["behavior"] != "normal"), None)
    return {
        "scenario": scenario, "repeat": repeat, "tool": tool, "status": status,
        "seconds": round(seconds, 3), "verified": digest == expected,
        "requests": len(state.events),
        "connections": len({event["connection_id"] for event in state.events}),
        "injected_disconnect": any(event["behavior"] != "normal" and event["disconnected"]
                                   for event in state.events),
        "injected_range": (f'{injected["start"]}-{injected["end"]}' if injected else None),
        "injected_bytes": injected["bytes_sent"] if injected else 0,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--aria2", required=True)
    parser.add_argument("--bytehaul", default=ROOT / "target/release/examples/public_compare.exe")
    parser.add_argument("--rounds", type=int, default=1)
    parser.add_argument("--connections", type=int, default=8)
    parser.add_argument("--size-mib", type=int, default=32)
    parser.add_argument("--normal-mib", type=int, default=16)
    parser.add_argument("--slow-kib", type=int, default=256)
    parser.add_argument("--slow-band-mib", type=int, default=8)
    parser.add_argument("--stall-seconds", type=float, default=4)
    parser.add_argument("--process-timeout", type=int, default=45)
    parser.add_argument("--log-level", choices=("off", "error", "warn", "info", "debug", "trace"), default="debug")
    parser.add_argument("--scenarios", nargs="+", choices=("normal", "slow", "stall"),
                        default=("normal", "slow", "stall"))
    args = parser.parse_args()
    if args.rounds < 1 or args.connections < 1 or args.size_mib < 16:
        parser.error("rounds/connections must be positive and size-mib must be at least 16")
    stamp = time.strftime("%Y%m%d-%H%M%S")
    run_dir = ROOT / "target/local-compare" / stamp
    run_dir.mkdir(parents=True)
    expected = expected_hash(args.size_mib * MIB)
    tools = ["bytehaul", "aria2", "aria2-low-speed"]
    rows = []
    print(f"Results: {run_dir}", flush=True)
    for scenario in args.scenarios:
        for repeat in range(1, args.rounds + 1):
            order = tools.copy()
            random.Random(repeat + len(scenario) * 1009).shuffle(order)
            for tool in order:
                row = run_one(args, run_dir, scenario, repeat, tool, expected)
                rows.append(row)
                print(json.dumps(row), flush=True)
    with (run_dir / "results.csv").open("w", newline="", encoding="utf-8") as stream:
        writer = csv.DictWriter(stream, fieldnames=rows[0].keys())
        writer.writeheader()
        writer.writerows(rows)
    report = [
        "# Local controlled download comparison", "",
        f"Fixture: {args.size_mib} MiB, {args.connections} connections, "
        f"normal {args.normal_mib} MiB/s per response, slow {args.slow_kib} KiB/s, "
        f"stall {args.stall_seconds:g} s once.", "",
        "`aria2-low-speed` differs only by `--lowest-speed-limit=1M`; ordinary aria2 uses its default 0.", "",
        "| Scenario | Tool | Verified | Median seconds | Requests median |",
        "|---|---|---:|---:|---:|",
    ]
    for scenario in args.scenarios:
        for tool in tools:
            selected = [row for row in rows if row["scenario"] == scenario and row["tool"] == tool]
            report.append(
                f'| {scenario} | {tool} | {sum(row["verified"] for row in selected)}/{len(selected)} '
                f'| {statistics.median(row["seconds"] for row in selected):.3f} '
                f'| {statistics.median(row["requests"] for row in selected):g} |'
            )
    report += ["", "Every tool run uses a fresh server. Slow/stall is injected only once, selected by Range start; retries are served normally.", ""]
    (run_dir / "report.md").write_text("\n".join(report), encoding="utf-8")
    (run_dir / "metadata.json").write_text(json.dumps({
        "created": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
        "args": vars(args), "sha256": expected,
        "semantics": "slow/stall is injected once per fresh server when Range start is in the selected band",
    }, indent=2, default=str), encoding="utf-8")


if __name__ == "__main__":
    main()
