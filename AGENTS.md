# Project Notes

- For Python work under `bindings/python`, use `uv` to manage the project environment and dev dependencies.
- Prefer `uv sync --project bindings/python` before running Python-side tooling.
- Prefer `uv run --project bindings/python maturin build`, `uv run --project bindings/python maturin develop`, and `uv run --project bindings/python pytest` over ad-hoc `pip install` or manual `venv` setup.
- After every code change, run `cargo tarpaulin --engine llvm --workspace --all-targets --out Stdout --fail-under 95` and make sure unit test coverage stays at or above 95%.
- After every push, check that GitHub Actions workflows complete successfully; if CI reports any warnings, fix them promptly.
