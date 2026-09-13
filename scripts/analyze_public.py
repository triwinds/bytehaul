"""Extract request/error/IP evidence from a compare_public.py run directory."""
import argparse
from collections import Counter
import csv
import json
from pathlib import Path
import re

TIMING_FIELDS = (
    "first_multi_body_parallel_ms",
    "completion_90_to_end_ms",
    "last_body_to_final_flush_ms",
)


def extract_timing(content):
    lines = [line for line in content.splitlines()
             if "multi-worker timing diagnostics" in line]
    timing = {field: None for field in TIMING_FIELDS}
    if lines:
        line = lines[-1]
        for field in TIMING_FIELDS:
            match = re.search(rf"\b{field}=Some\((\d+)\)", line)
            if match:
                timing[field] = int(match.group(1))
    return timing, lines


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("run", type=Path)
    args = parser.parse_args()
    rows = list(csv.DictReader((args.run / "results.csv").open(encoding="utf-8")))
    evidence = []
    for row in rows:
        strategy = row.get("strategy")
        folder_name = f"{row['source']}-c{row['connections']}-r{row['repeat']}"
        if strategy:
            folder_name += f"-{strategy}"
        folder = args.run / f"{folder_name}-{row['tool']}"
        bytehaul = row["tool"].startswith("bytehaul")
        log = folder / ("stderr.log" if bytehaul else "aria2.log")
        content = (log.read_text(encoding="utf-8", errors="replace")
                   if log.is_file() else "")
        if bytehaul:
            responses = Counter(re.findall(r"HTTP response headers received[^\n]* status=(\d+)", content))
            request_count = content.count("HTTP request started")
            ips = sorted(set(
                re.findall(r"primary_ip=Some\(\"([^\"]+)\"\)", content)
                + re.findall(r"connected to ([\d.]+):\d+", content)
            ))
            transfer_lines = [
                line for line in content.splitlines()
                if "libcurl transfer finished" in line or "libcurl transfer cancelled" in line
            ]
            connection_establishments = sum(
                int(match.group(1))
                for line in transfer_lines
                if (match := re.search(r"\bconnections=(\d+)", line))
            )
            header_ms = [int(s) for s in re.findall(r"HTTP response headers received[^\n]* headers_ms=(\d+)", content)]
            allocation_events = [line for line in content.splitlines()
                                 if "ordinary request range assigned" in line]
            candidate_share_events = [line for line in content.splitlines()
                                     if "dynamic candidate shares" in line]
            online_range_events = [line for line in content.splitlines()
                                   if "HTTP request started" in line
                                   or "adaptive response associated with lease" in line]
            body_events = [line for line in content.splitlines()
                           if "adaptive piece writer acknowledged" in line
                           or "adaptive interrupted request writer acknowledged" in line]
            effective_config = [line for line in content.splitlines()
                                if "multi-worker download started" in line]
            diagnostics = [line for line in content.splitlines()
                           if "adaptive transfer diagnostics" in line
                           or "HTTP request counters" in line
                           or "ordinary request range assigned" in line
                           or "dynamic candidate shares" in line
                           or "multi-worker timing diagnostics" in line]
            errors = [line for line in content.splitlines() if re.search(r"\b(?:WARN|ERROR)\b", line)]
            timing_diagnostics, timing_events = extract_timing(content)
        else:
            responses = Counter(re.findall(r"^HTTP/1\.[01] (\d+)", content, re.M))
            request_count = len(re.findall(r"^(?:GET|HEAD) \S+ HTTP/1\.[01]", content, re.M))
            ips = sorted(set(re.findall(r"Connecting to ([\d.]+):443", content)))
            connection_establishments = None
            header_ms = []
            allocation_events = []
            candidate_share_events = []
            online_range_events = []
            body_events = []
            effective_config = []
            diagnostics = []
            timing_diagnostics = {field: None for field in TIMING_FIELDS}
            timing_events = []
            errors = [line for line in content.splitlines() if re.search(r"\[(?:WARN|ERROR)\]", line)]
        evidence.append({**row, "requests_started": request_count, "response_status_counts": dict(responses),
                        "connection_ips": ips, "response_headers_ms": header_ms,
                         "connection_establishments": connection_establishments,
                         "allocation_events": allocation_events,
                         "candidate_share_events": candidate_share_events,
                         "online_range_events": online_range_events,
                         "body_completion_events": body_events,
                         "effective_config_events": effective_config,
                         "timing_diagnostics": timing_diagnostics,
                         "timing_events": timing_events,
                         "log_bytes": sum(p.stat().st_size for p in folder.glob("*.log")),
                         "diagnostics": diagnostics, "warning_error_lines": errors})
    (args.run / "diagnostics.json").write_text(json.dumps(evidence, indent=2, ensure_ascii=False), encoding="utf-8")
    lines = ["# Request diagnostics", "", "Counts include probes and retries; bytehaul headers_ms includes connection setup and pool wait.", "",
             "Allocation events contain scheduler candidate/final ranges, candidate slots, the pre-truncation target, lease count and truncation reason. TRACE-only candidate-share events retain the other dynamic candidates without imposing that collection cost on low-log runs. Online range events and body completion events retain separate HTTP/body observations.", "The timing columns use monotonic download markers: first simultaneous body pair, durable 90% piece completion to final writer flush, and last body completion to final writer flush.", "",
             "| Run | Requests | HTTP responses | Allocations | Connections opened | First body parallel ms | 90% to end ms | Last body to final flush ms | Connection IPs | Logs MiB |", "|---|---:|---|---:|---:|---:|---:|---:|---|---:|"]
    for item in evidence:
        label = f"{item['source']}-c{item['connections']}-r{item['repeat']}"
        if item.get("strategy"):
            label += f"-{item['strategy']}"
        label += f"-{item['tool']}"
        timing = item["timing_diagnostics"]
        values = [timing[field] if timing[field] is not None else "N/A" for field in TIMING_FIELDS]
        lines.append(f"| {label} | {item['requests_started']} | {item['response_status_counts']} | {len(item['allocation_events'])} | {item['connection_establishments'] if item['connection_establishments'] is not None else 'N/A'} | {values[0]} | {values[1]} | {values[2]} | {', '.join(item['connection_ips'])} | {item['log_bytes'] / 1048576:.2f} |")
    lines += ["", "Full extracted scheduler allocations, online ranges, body completion timing and adaptive counters are in diagnostics.json. Original logs remain authoritative."]
    (args.run / "diagnostics.md").write_text("\n".join(lines) + "\n", encoding="utf-8")
    print("\n".join(lines))


if __name__ == "__main__":
    main()
