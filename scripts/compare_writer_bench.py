#!/usr/bin/env python3
"""Alternate two pipeline_bench binaries to reduce host-load drift in P4 samples.

Build each binary first. This script never builds while measuring, strips proxy
variables for the localhost fixture, and keeps raw rounds and binary identities.
"""
import argparse
import csv
import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess
import tempfile
from datetime import datetime, timezone


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--before", type=Path, required=True)
    parser.add_argument("--after", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--rounds", type=int, default=10)
    parser.add_argument("--filter", default="writer/")
    args = parser.parse_args()
    if args.rounds < 1:
        parser.error("--rounds must be positive")
    binaries = {label: path.resolve() for label, path in
                (("before", args.before), ("after", args.after))}
    args.out.mkdir(parents=True, exist_ok=True)
    env = {key: value for key, value in os.environ.items()
           if key.lower() not in {"http_proxy", "https_proxy", "all_proxy"}}
    rows = {label: [] for label in binaries}
    metadata = {
        "started_utc": datetime.now(timezone.utc).isoformat(),
        "platform": platform.platform(), "rounds": args.rounds, "filter": args.filter,
        "binaries": {label: {"path": str(path),
            "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}
            for label, path in binaries.items()},
        "order": [],
    }
    with tempfile.TemporaryDirectory(prefix="bytehaul-p4-") as temporary:
        for index in range(args.rounds):
            order = ("before", "after") if index % 2 == 0 else ("after", "before")
            for label in order:
                work = Path(temporary) / f"{index}-{label}"
                command = [str(binaries[label]), "--filter", args.filter, "--rounds", "1",
                           "--out", str(work)]
                result = subprocess.run(command, env=env, text=True,
                    stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=120)
                if result.returncode:
                    raise RuntimeError(result.stdout)
                with (work / "samples.csv").open(newline="") as source:
                    sample = list(csv.DictReader(source))
                verified = [row for row in sample if row["metric"] == "verified"]
                if not verified or any(float(row["value"]) != 1 for row in verified):
                    raise RuntimeError(f"unverified round {index} {label}: {result.stdout}")
                for row in sample:
                    row["round"] = index
                rows[label].extend(sample)
                with (args.out / f"{label}.csv").open("w", newline="") as output:
                    writer = csv.DictWriter(output, fieldnames=list(sample[0]))
                    writer.writeheader()
                    writer.writerows(rows[label])
                metadata["order"].append({"round": index, "binary": label})
                (args.out / "metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")
                print(f"round {index + 1}/{args.rounds}: {label} verified", flush=True)
    metadata["completed_utc"] = datetime.now(timezone.utc).isoformat()
    (args.out / "metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")


if __name__ == "__main__":
    main()
