# Project Notes

- For Python work under `bindings/python`, use `uv` to manage the project environment and dev dependencies.
- Prefer `uv sync --project bindings/python` before running Python-side tooling.
- Prefer `uv run --project bindings/python maturin build`, `uv run --project bindings/python maturin develop`, and `uv run --project bindings/python pytest` over ad-hoc `pip install` or manual `venv` setup.
