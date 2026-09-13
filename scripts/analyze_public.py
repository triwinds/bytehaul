"""Extract request/error/IP evidence from a compare_public.py run directory."""
import argparse
from collections import Counter
import csv
import json
from pathlib import Path
import re


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("run", type=Path)
    args = parser.parse_args()
    rows = list(csv.DictReader((args.run / "results.csv").open(encoding="utf-8")))
    evidence = []
    for row in rows:
        folder = args.run / f"{row['source']}-c{row['connections']}-r{row['repeat']}-{row['tool']}"
        bytehaul = row["tool"].startswith("bytehaul")
        log = folder / ("stderr.log" if bytehaul else "aria2.log")
        content = log.read_text(encoding="utf-8", errors="replace")
        if bytehaul:
            responses = Counter(re.findall(r"HTTP response headers received[^\n]* status=(\d+)", content))
            request_count = content.count("HTTP request started")
            ips = sorted(set(re.findall(r"connected to ([\d.]+):443", content)))
            header_ms = [int(s) for s in re.findall(r"HTTP response headers received[^\n]* headers_ms=(\d+)", content)]
            diagnostics = [line for line in content.splitlines() if "adaptive transfer diagnostics" in line or "HTTP request counters" in line]
            errors = [line for line in content.splitlines() if re.search(r"\b(?:WARN|ERROR)\b", line)]
        else:
            responses = Counter(re.findall(r"^HTTP/1\.[01] (\d+)", content, re.M))
            request_count = len(re.findall(r"^(?:GET|HEAD) \S+ HTTP/1\.[01]", content, re.M))
            ips = sorted(set(re.findall(r"Connecting to ([\d.]+):443", content)))
            header_ms = []
            diagnostics = []
            errors = [line for line in content.splitlines() if re.search(r"\[(?:WARN|ERROR)\]", line)]
        evidence.append({**row, "requests_started": request_count, "response_status_counts": dict(responses),
                         "connection_ips": ips, "response_headers_ms": header_ms,
                         "log_bytes": sum(p.stat().st_size for p in folder.glob("*.log")),
                         "diagnostics": diagnostics, "warning_error_lines": errors})
    (args.run / "diagnostics.json").write_text(json.dumps(evidence, indent=2, ensure_ascii=False), encoding="utf-8")
    lines = ["# Request diagnostics", "", "Counts include probes and retries; bytehaul headers_ms includes connection setup and pool wait.", "",
             "| Run | Requests | HTTP responses | Connection IPs | Logs MiB |", "|---|---:|---|---|---:|"]
    for item in evidence:
        label = f"{item['source']}-c{item['connections']}-r{item['repeat']}-{item['tool']}"
        lines.append(f"| {label} | {item['requests_started']} | {item['response_status_counts']} | {', '.join(item['connection_ips'])} | {item['log_bytes'] / 1048576:.2f} |")
    lines += ["", "Full extracted warnings and adaptive counters are in diagnostics.json. Original logs remain authoritative."]
    (args.run / "diagnostics.md").write_text("\n".join(lines) + "\n", encoding="utf-8")
    print("\n".join(lines))


if __name__ == "__main__":
    main()
