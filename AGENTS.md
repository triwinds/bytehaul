# Project Notes

- For Python work under `bindings/python`, use `uv` to manage the project environment and dev dependencies.
- Prefer `uv sync --project bindings/python` before running Python-side tooling.
- Prefer `uv run --project bindings/python maturin build`, `uv run --project bindings/python maturin develop`, and `uv run --project bindings/python pytest` over ad-hoc `pip install` or manual `venv` setup.
- After every code change, run `cargo tarpaulin --engine llvm --workspace --all-targets --out Stdout --fail-under 95` on Linux/CI and make sure local unit test coverage stays at or above 96% so CI keeps headroom above the 95% gate.
- On Windows, generate local coverage reports with `powershell -ExecutionPolicy Bypass -File scripts/coverage-windows.ps1 -Scope all-targets -Format html` or `-Format json`; the helper defaults to the same `all-targets` scope as CI and uses `cargo-llvm-cov` with `CARGO_BUILD_JOBS=1` plus a fresh isolated target dir per run to avoid locked files and incomplete reports.
- After every push, check that GitHub Actions workflows complete successfully; if CI reports any warnings, fix them promptly.
