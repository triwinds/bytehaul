"""Exercise the gate process boundary without compiling Rust."""

import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("coverage_gate", Path(__file__).parents[1] / "coverage.py")
gate = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(gate)

STUB = """#!/usr/bin/env python3
import json, os, pathlib, sys
args = sys.argv[1:]
if '--version' in args:
    if 'tarpaulin' in args:
        print('cargo-tarpaulin ' + os.environ.get('STUB_VERSION', '0.37.2'))
    else:
        print('rust tool 1.98.1')
elif 'install' in args:
    print('tool installation')
    sys.exit(int(os.environ.get('STUB_INSTALL_EXIT', '0')))
else:
    assert args[0:4] == ['run', '1.98.1', 'cargo', 'tarpaulin'], args
    assert args[args.index('--engine') + 1] == 'llvm'
    assert args[args.index('-p') + 1] == 'bytehaul'
    assert args[args.index('--fail-under') + 1] == '95'
    assert '--all-targets' in args and '--locked' in args and '--ignore-config' in args
    assert not any(name in os.environ for name in ['HTTP_PROXY', 'HTTPS_PROXY', 'ALL_PROXY', 'http_proxy', 'https_proxy', 'all_proxy'])
    assert args[args.index('--out') + 1:args.index('--out') + 4] == ['Stdout', 'Json', 'Html']
    report = pathlib.Path(args[args.index('--output-dir') + 1])
    target = pathlib.Path(args[args.index('--target-dir') + 1])
    assert report.name == target.name
    if os.environ.get('STUB_REPORT', '1') == '1':
        (report / 'tarpaulin-report.json').write_text('{}')
    print(os.environ.get('STUB_OUTPUT', '96.00% coverage, 96/100 lines covered'))
    sys.exit(int(os.environ.get('STUB_EXIT', '0')))
"""


class GateTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "scripts").mkdir()
        config = Path(__file__).parents[1] / "coverage-config.json"
        (self.root / "scripts/coverage-config.json").write_text(config.read_text())
        executable = self.root / "rustup"
        executable.write_text(STUB)
        executable.chmod(0o755)

    def run_gate(self, install=False, **env):
        args = ["coverage.py"] + (["--install"] if install else [])
        with patch.object(gate, "ROOT", self.root), patch.object(gate.platform, "system", return_value="Linux"), \
             patch("sys.argv", args), patch.dict(os.environ, {"PATH": str(self.root) + os.pathsep + os.environ["PATH"], "GITHUB_STEP_SUMMARY": "", **env}), \
             contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            return gate.main()

    def reports(self):
        return list((self.root / "target/coverage/reports").iterdir())

    def test_success_records_fresh_measurement_and_command(self):
        with patch.object(gate.platform, "freedesktop_os_release", return_value={"ID": "ubuntu", "VERSION_ID": "24.04", "PRETTY_NAME": "Ubuntu 24.04 LTS"}):
            self.assertEqual(self.run_gate(HTTP_PROXY="http://127.0.0.1:1", all_proxy="socks5://127.0.0.1:1", CARGO_BUILD_JOBS="1"), 0)
        metadata = json.loads((self.reports()[0] / "metadata.json").read_text())
        self.assertEqual(metadata["exit_code"], 0)
        self.assertEqual(metadata["os_release"]["ID"], "ubuntu")
        self.assertEqual(metadata["os_release"]["VERSION_ID"], "24.04")
        self.assertEqual(metadata["cargo_build_jobs"], "1")
        self.assertEqual(metadata["coverage"], "96.00% coverage, 96/100 lines covered")
        self.assertIn("--all-targets", metadata["command"])

    def test_stub_measurements_do_not_pollute_actions_summary(self):
        summary = self.root / "actions-summary.md"
        summary.write_text("Real coverage results only.\n")
        with patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": str(summary)}):
            self.assertEqual(self.run_gate(), 0)
        self.assertEqual(summary.read_text(), "Real coverage results only.\n")

    def test_threshold_failure_retains_reports_and_exit(self):
        self.assertEqual(self.run_gate(STUB_EXIT="1", STUB_OUTPUT="Coverage is below the failure threshold 94.75% < 95.00%"), 1)
        report = self.reports()[0]
        self.assertTrue((report / "tarpaulin-report.json").is_file())
        self.assertIn("Measured line coverage is below", (report / "summary.md").read_text())
        self.assertIn("94.75%", (report / "run.log").read_text())

    def test_test_failure_is_not_classified_as_low_coverage(self):
        self.assertEqual(self.run_gate(STUB_EXIT="101", STUB_REPORT="0", STUB_OUTPUT="test result: FAILED. 1 failed"), 101)
        self.assertIn("Tests failed; coverage is incomplete", (self.reports()[0] / "summary.md").read_text())

    def test_previous_success_cannot_supply_a_missing_report(self):
        self.assertEqual(self.run_gate(), 0)
        self.assertEqual(self.run_gate(STUB_REPORT="0"), 1)
        reports = self.reports()
        self.assertEqual(len(reports), 2)
        self.assertEqual(sum((report / "tarpaulin-report.json").exists() for report in reports), 1)
        targets = {json.loads((report / "metadata.json").read_text())["target_dir"] for report in reports}
        self.assertEqual(len(targets), 2)

    def test_wrong_tool_version_fails_before_collection(self):
        with patch.object(gate.platform, "freedesktop_os_release", side_effect=OSError("missing os-release")):
            self.assertEqual(self.run_gate(STUB_VERSION="0.1.0"), 1)
        metadata = json.loads((self.reports()[0] / "metadata.json").read_text())
        self.assertEqual(metadata["os_release"], {})
        self.assertIn("version mismatch", (self.reports()[0] / "run.log").read_text())

    def test_installation_failure_preserves_exit(self):
        self.assertEqual(self.run_gate(install=True, STUB_INSTALL_EXIT="7"), 7)
        self.assertIn("installation failed", (self.reports()[0] / "run.log").read_text())


if __name__ == "__main__":
    unittest.main()
