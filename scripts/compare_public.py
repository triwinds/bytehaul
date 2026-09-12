"""Compare release bytehaul and aria2 against public HTTPS mirrors, with full logs.

Build first: cargo build --release --locked --example public_compare
Run: python scripts/compare_public.py --aria2 PATH_TO_ARIA2C
"""
import argparse
import csv
import datetime
import hashlib
import json
import os
from pathlib import Path
import platform
import statistics
import subprocess
import time
from urllib.parse import urlsplit
import zipfile

ROOT = Path(__file__).resolve().parents[1]
FILE = "alpine-virt-3.22.1-x86_64.iso"
SOURCES = {
    "ustc": f"https://mirrors.ustc.edu.cn/alpine/v3.22/releases/x86_64/{FILE}",
    "alpine_cdn": f"https://dl-cdn.alpinelinux.org/alpine/v3.22/releases/x86_64/{FILE}",
}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--aria2", required=True)
    parser.add_argument("--rounds", type=int, default=3)
    parser.add_argument("--timeout", type=int, default=180)
    parser.add_argument("--url", help="Use this URL instead of the default mirrors")
    parser.add_argument("--sha256", help="Trusted expected SHA-256 for --url, if available")
    args = parser.parse_args()
    if args.rounds < 1 or args.timeout < 1:
        parser.error("rounds and timeout must be positive")
    sources = {"custom": args.url} if args.url else SOURCES
    filename = Path(urlsplit(args.url).path).name if args.url else FILE
    if not filename or filename in {".", ".."}:
        parser.error("URL must have a filename")
    aria = str(Path(args.aria2).resolve())
    bytehaul = ROOT / "target/release/examples" / ("public_compare.exe" if os.name == "nt" else "public_compare")
    env = {k: v for k, v in os.environ.items() if k.lower() not in
           {"http_proxy", "https_proxy", "all_proxy", "no_proxy"}}
    run = ROOT / "target/public-compare" / datetime.datetime.now().strftime("%Y%m%d-%H%M%S")
    run.mkdir(parents=True)
    metadata = {"platform": platform.platform(), "created": datetime.datetime.now().astimezone().isoformat(),
                "revision": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
                "aria2": subprocess.check_output([aria, "--version"], text=True),
                "bytehaul_binary_sha256": hashlib.sha256(bytehaul.read_bytes()).hexdigest(),
                "sources": sources, "rounds": args.rounds, "timeout": args.timeout,
                "validation": "trusted SHA-256 when supplied/available; otherwise cross-tool SHA-256 agreement plus ZIP CRC checks (not publisher authentication)",
                "network": "IPv4, no explicit/environment proxy; system routing unchanged",
                "logging": "bytehaul all-target TRACE + progress; aria2 DEBUG + console DEBUG",
                "timing": "process wall time including initialization and log writing, excluding SHA-256"}
    (run / "metadata.json").write_text(json.dumps(metadata, indent=2), encoding="utf-8")
    (run / "worktree.patch").write_bytes(subprocess.check_output(["git", "diff", "--", "src", "Cargo.toml", "Cargo.lock"], cwd=ROOT))
    (run / "public_compare.rs").write_bytes((ROOT / "examples/public_compare.rs").read_bytes())
    (run / "compare_public.py").write_bytes(Path(__file__).read_bytes())
    rows = []
    print(f"Results: {run}", flush=True)
    expected = {}
    if args.sha256:
        if not args.url or len(args.sha256) != 64 or any(c not in "0123456789abcdefABCDEF" for c in args.sha256):
            parser.error("--sha256 requires --url and 64 hexadecimal characters")
        expected["custom"] = args.sha256.lower()
    for source, url in sources.items():
        for suffix in ([""] if args.url else [".sha256", ""]):
            command = ["curl.exe" if os.name == "nt" else "curl", "-4", "--noproxy", "*", "-fSL",
                       "--connect-timeout", "15", "--max-time", "30"]
            command += [url + suffix] if suffix else ["-I", url]
            result = subprocess.run(command, capture_output=True, env=env)
            (run / f"{source}{suffix or '.headers'}.txt").write_bytes(result.stdout + result.stderr)
            if suffix and result.returncode == 0:
                candidate = (result.stdout.decode().split() or [""])[0].lower()
                if len(candidate) == 64 and all(c in "0123456789abcdef" for c in candidate):
                    expected[source] = candidate
    for source, url in sources.items():
        for connections in [1, 8]:
            for repeat in range(1, args.rounds + 1):
                order = ["bytehaul", "aria2"] if repeat % 2 else ["aria2", "bytehaul"]
                for tool in order:
                    name = f"{source}-c{connections}-r{repeat}-{tool}"
                    folder = run / name
                    folder.mkdir()
                    output = folder / filename
                    if tool == "bytehaul":
                        command = [str(bytehaul), url, str(output), str(connections)]
                    else:
                        command = [aria, "--no-conf=true", "--no-netrc=true", "--disable-ipv6=true",
                                   f"--split={connections}", f"--max-connection-per-server={connections}",
                                   "--min-split-size=1M", "--piece-length=1M", "--file-allocation=none",
                                   "--connect-timeout=15", "--timeout=30", "--max-tries=3", "--retry-wait=1",
                                   "--user-agent=public-download-compare/1.0", "--enable-color=false",
                                   "--summary-interval=1", "--console-log-level=debug", "--log-level=debug",
                                   f"--log={folder / 'aria2.log'}", f"--dir={folder}", f"--out={filename}", url]
                    (folder / "command.json").write_text(json.dumps(command, indent=2), encoding="utf-8")
                    start = time.perf_counter()
                    with (folder / "stdout.log").open("wb") as stdout, (folder / "stderr.log").open("wb") as stderr:
                        try:
                            status = subprocess.run(command, stdout=stdout, stderr=stderr, env=env, timeout=args.timeout).returncode
                        except subprocess.TimeoutExpired:
                            status = "timeout"
                    elapsed = time.perf_counter() - start
                    size = output.stat().st_size if output.exists() else 0
                    digest = None
                    zip_ok = None
                    if status == 0 and output.exists():
                        with output.open("rb") as data:
                            digest = hashlib.file_digest(data, "sha256").hexdigest()
                        if filename.lower().endswith(".zip"):
                            try:
                                with zipfile.ZipFile(output) as archive:
                                    zip_ok = archive.testzip() is None
                            except (zipfile.BadZipFile, RuntimeError, OSError):
                                zip_ok = False
                    verified = digest is not None and digest == expected.get(source)
                    row = dict(source=source, connections=connections, repeat=repeat, tool=tool,
                               status=status, seconds=round(elapsed, 3), bytes=size,
                               mib_s=round(size / elapsed / 1048576, 3) if status == 0 else None,
                               sha256=digest, zip_crc_ok=zip_ok, verified=verified)
                    rows.append(row)
                    with (run / "results.csv").open("w", newline="", encoding="utf-8") as csvfile:
                        writer = csv.DictWriter(csvfile, fieldnames=list(row))
                        writer.writeheader()
                        writer.writerows(rows)
                    print(json.dumps(row), flush=True)
    for row in rows:
        if row["source"] not in expected:
            peers = [r for r in rows if r["source"] == row["source"] and r["tool"] != row["tool"] and r["status"] == 0]
            row["verified"] = (row["status"] == 0 and row["sha256"] is not None and row["zip_crc_ok"] is not False
                               and any(r["sha256"] == row["sha256"] and r["zip_crc_ok"] is not False for r in peers))
        else:
            row["verified"] = row["verified"] and row["zip_crc_ok"] is not False
    with (run / "results.csv").open("w", newline="", encoding="utf-8") as csvfile:
        writer = csv.DictWriter(csvfile, fieldnames=list(rows[0]))
        writer.writeheader()
        writer.writerows(rows)
    lines = ["# Public download comparison", "", f"Run: {run.name}", "",
             "All logging enabled. Process wall time; hash verification excluded. Fresh output per run.", "",
             "| Source | Connections | Tool | Verified/attempted | Median seconds | Median MiB/s |",
             "|---|---:|---|---:|---:|---:|"]
    for source in sources:
        for connections in [1, 8]:
            for tool in ["bytehaul", "aria2"]:
                group = [r for r in rows if (r['source'], r['connections'], r['tool']) == (source, connections, tool)]
                good = [r for r in group if r["verified"]]
                seconds = round(statistics.median(r["seconds"] for r in good), 3) if good else "N/A"
                speed = round(statistics.median(r["mib_s"] for r in good), 3) if good else "N/A"
                lines.append(f"| {source} | {connections} | {tool} | {len(good)}/{len(group)} | {seconds} | {speed} |")
    lines += ["", "Sources and exact commands are preserved in metadata.json and each run directory.",
              "Verification without a trusted digest means cross-tool SHA-256 agreement, plus full ZIP CRC checks for ZIP files; it does not authenticate the publisher.",
              "aria2 option reference: https://aria2.github.io/manual/en/html/aria2c.html",
              "Logging overhead, CDN/server caches, route variation and different TLS/DNS/scheduling implementations affect these results.",
              "Connection counts are configured maxima. Retry semantics and internal defaults differ; this is not a controlled throughput benchmark."]
    (run / "report.md").write_text("\n".join(lines) + "\n", encoding="utf-8")
    print("\n".join(lines), flush=True)
    return 0 if all(r["verified"] for r in rows) else 1


if __name__ == "__main__":
    raise SystemExit(main())
