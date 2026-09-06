Bytehaul 0.2.1 improves download recovery and durable resume handling for Rust and Python callers.

- Retry response-body transport failures in single-connection downloads. Continue from the confirmed durable offset when range and object validators match, or restart safely when they do not.
- Report storage failures as `Failed` while preserving the last durable checkpoint; completion is published only after successful flush, close and checkpoint cleanup.
- Simplify bounded worker-to-writer flow, leased write caching, scheduler state and DNS caching. Pause and cancellation remain responsive while waiting for memory, channel capacity or rate limits.
- Use one reproducible Linux coverage gate with pinned tooling, the existing 95% line threshold, and retained diagnostics. Windows reports now enforce an explicit line threshold and identify their platform scope.
- Replace a flaky dynamic-split timing comparison with gated requests that verify concurrent disjoint ranges, exact output bytes and completed progress.

- Exclude internal development workflow files from the published Rust crate and update the development-only rand dependency to 0.9.

Existing resume-control file formats remain compatible.
