Response-body transport failures and storage finalization errors could interrupt downloads or leave misleading terminal state. This change retries recoverable transfers safely, preserves durable resume checkpoints, and publishes completion only after successful storage finalization.

- Simplify worker flow control, leased write caching, scheduling and DNS caching while keeping pause/cancel responsive.
- Make the Linux 95% coverage gate reproducible and retain diagnostic reports; replace the Windows-sensitive timing assertion with deterministic concurrency checks.
- Prepare Rust/Python 0.2.1, exclude internal workflow files from the Rust package, and incorporate the compatible rand 0.9 development dependency update from #17 (resolved to 0.9.4).
- Close stale dependency proposals #7, #9, #16 and #18 with documented incompatibility or supersession reasons.

Validation: the repair branch previously passed all three OS jobs, Python tests, and Linux coverage at 96.59% (3404/3524). Release verification passed 429 Rust tests, 42 Python tests, Clippy, rustdoc, formatting, crate compilation and Python sdist auditing; current PR checks must pass before merge and tag publication. Existing resume-control format remains unchanged.
