# Bytehaul Rust and Python Binding Guidelines

## Scope

The `bytehaul-python` package label is historical. The repository is a Rust workspace whose root crate implements the download engine and whose `bindings/python/` member exposes it through PyO3. These guidelines apply to both parts; there is no application backend or database layer.

Primary evidence:

- `Cargo.toml` defines the `bytehaul` crate and includes `bindings/python` as a workspace member.
- `bindings/python/Cargo.toml` builds the `_bytehaul` `cdylib` against the root crate.
- `bindings/python/pyproject.toml` packages it with maturin as `bytehaul._bytehaul`.

## Guidelines

| Guide | Use it when |
| --- | --- |
| [Architecture and Module Boundaries](./architecture.md) | Placing engine, HTTP, session, storage, or workspace changes |
| [Configuration and Public API](./configuration-and-api.md) | Adding options, builders, exported types, defaults, or docs |
| [Errors and Observability](./errors-and-observability.md) | Changing failures, retries, Python exceptions, or logs |
| [Python Bindings](./python-bindings.md) | Editing PyO3 classes/functions, runtime/GIL behavior, exports, or packaging |
| [Testing and Quality](./testing-and-quality.md) | Choosing test scope or running quality gates |

## Pre-Development Checklist

1. Read [Architecture and Module Boundaries](./architecture.md) for every implementation task.
2. Read each affected contract guide. Configuration reaching Python requires both the configuration and binding guides.
3. Search every option, enum variant, exception, public symbol, or persisted field before changing it; public concepts commonly have Rust, Python, test, and documentation mirrors.
4. Read the closest `#[cfg(test)]` module and a matching integration test under `tests/` or `bindings/python/tests/`.
5. Keep the root crate independent of PyO3. Translation belongs in `bindings/python/src/lib.rs`.
6. Use [Testing and Quality](./testing-and-quality.md), escalating from affected-package checks to the full CI-equivalent set.

## Shared Thinking Guides

- Use `.trellis/spec/guides/cross-layer-thinking-guide.md` when an option, state, error, or payload crosses engine, binding, and Python layers.
- Use `.trellis/spec/guides/code-reuse-thinking-guide.md` before adding a parser, validator, server fixture, or helper.
