#!/usr/bin/env python3
"""The Linux coverage gate used both locally and by GitHub Actions."""

import argparse
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import platform
import re
import shlex
import subprocess
import sys
import uuid

ROOT = Path(__file__).resolve().parents[1]


def capture(command):
    result = subprocess.run(command, cwd=ROOT, text=True, capture_output=True)
    if result.returncode:
        raise RuntimeError(result.stderr.strip() or result.stdout.strip())
    return result.stdout.strip()


def run_logged(command, log, env=None):
    rendered = "+ " + shlex.join(map(str, command))
    print(rendered, flush=True)
    log.write(rendered + "\n")
    log.flush()
    with subprocess.Popen(command, cwd=ROOT, stdout=subprocess.PIPE,
                          stderr=subprocess.STDOUT, text=True, env=env) as process:
        for line in process.stdout:
            print(line, end="", flush=True)
            log.write(line)
            log.flush()
        return process.wait()


def classify(code, output):
    # Tests can abort collection before a percentage exists. Never call that a
    # coverage shortfall, or treat a partial report as a successful measurement.
    if "test result: FAILED" in output or "Test failed during run" in output:
        return "Tests failed; coverage is incomplete."
    if "Coverage is below" in output:
        return "Measured line coverage is below the required threshold."
    if code:
        return "Coverage collection or tooling failed; see run.log."
    return "Line coverage gate passed."


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--install", action="store_true",
                        help="install the pinned Rust and Tarpaulin versions first")
    args = parser.parse_args()
    config = json.loads((ROOT / "scripts/coverage-config.json").read_text())
    run_id = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ-") + uuid.uuid4().hex[:8]
    report = ROOT / "target/coverage/reports" / run_id
    report.mkdir(parents=True)
    target = ROOT / "target/coverage/build" / run_id
    try:
        os_release = platform.freedesktop_os_release()
    except (AttributeError, OSError):
        os_release = {}
    metadata = {"config": config, "platform": platform.platform(),
                "architecture": platform.machine(), "started_utc": run_id,
                "os_release": {key: os_release[key] for key in ("ID", "VERSION_ID", "PRETTY_NAME") if key in os_release},
                "cargo_build_jobs": os.environ.get("CARGO_BUILD_JOBS"),
                "report_dir": str(report), "target_dir": str(target)}
    (report / "metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")
    code = 1
    summary = "Coverage setup failed; see run.log."
    with (report / "run.log").open("w") as log:
        try:
            try:
                metadata["revision"] = capture(["git", "rev-parse", "HEAD"])
                metadata["dirty"] = bool(capture(["git", "status", "--porcelain"]))
            except RuntimeError:
                metadata["revision"] = "unavailable"
            if platform.system() != "Linux":
                raise RuntimeError("Run this gate on Linux (CI uses Ubuntu 24.04 x86_64).")
            rust = config["rust"]
            if args.install:
                install = [
                    ["rustup", "toolchain", "install", rust, "--profile", "minimal",
                     "--component", "llvm-tools-preview"],
                    ["rustup", "run", rust, "cargo", "install", "cargo-tarpaulin",
                     "--version", config["tarpaulin"], "--locked"],
                ]
                for command in install:
                    code = run_logged(command, log)
                    if code:
                        raise RuntimeError("Pinned tool installation failed.")
            metadata["rustc"] = capture(["rustup", "run", rust, "rustc", "--version"])
            metadata["cargo"] = capture(["rustup", "run", rust, "cargo", "--version"])
            metadata["tarpaulin"] = capture(["rustup", "run", rust, "cargo", "tarpaulin", "--version"])
            if metadata["tarpaulin"].split()[-1] != config["tarpaulin"]:
                raise RuntimeError("Tarpaulin version mismatch; run again with --install.")
            command = ["rustup", "run", rust, "cargo", "tarpaulin", "--engine", "llvm",
                       "-p", "bytehaul", "--all-targets", "--locked", "--ignore-config", "--fail-under",
                       str(config["minimum_lines"]), "--out", "Stdout", "Json", "Html",
                       "--target-dir", str(target), "--output-dir", str(report)]
            metadata["command"] = command
            environment = dict(os.environ)
            for name in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy"):
                environment.pop(name, None)
            metadata["proxy_environment_isolated"] = True
            (report / "metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")
            code = run_logged(command, log, env=environment)
            log.flush()
            output = re.sub(r"\x1b\[[0-9;]*m", "", (report / "run.log").read_text())
            summary = classify(code, output)
            totals = re.findall(r"[0-9.]+% coverage, [0-9/]+ lines covered", output)
            if totals:
                metadata["coverage"] = totals[-1]
                summary += " " + totals[-1] + "."
            if code == 0 and not (report / "tarpaulin-report.json").is_file():
                raise RuntimeError("Tarpaulin returned success without a fresh JSON report.")
        except (OSError, RuntimeError) as error:
            code = code or 1
            summary = "Coverage setup or report generation failed; see run.log."
            print(str(error), file=log)
            print(str(error), file=sys.stderr)
    metadata["exit_code"] = code
    metadata["result"] = summary
    (report / "metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")
    markdown = (f"### Linux coverage\n\n{summary}\n\n"
                f"Threshold: {config['minimum_lines']}% lines. Exit code: {code}.\n"
                f"Reports and metadata: `{report.relative_to(ROOT)}`.\n")
    (report / "summary.md").write_text(markdown)
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as stream:
            stream.write(markdown)
    print(markdown)
    return code if code >= 0 else 128 - code


if __name__ == "__main__":
    sys.exit(main())
