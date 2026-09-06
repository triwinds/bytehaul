# Design

The smallest runtime gap is writer finalization returning an error before terminal state is published. Ownership remains src/session/single.rs; storage errors must not produce checkpoints at network-received offsets. Use minimal finalization error handling and portable deterministic fixture, preserving the Linux device regression.

Coverage automation belongs to scripts/ and .github/workflows/test.yml. One Linux script owns exact package/all-targets/LLVM/95% command, explicit pinned Rust and Tarpaulin versions, isolated report output and metadata; CI calls it and always uploads diagnostics. Export reports before preserving a failing gate exit. Windows uses llvm-cov line gate but does not claim Linux equivalence. README variants and testing spec mirror the new entry.

Do not reduce threshold, add coverage exclusions or cfg skips, change the storage format/public API, or refactor unrelated engine code. Additional tests will target measured misses. Linux execution uses a local disposable Docker container if host is macOS.

## Measured validation-cost adjustment
During the Linux run, six src/manager.rs cache-selection tests each spent over60seconds retrying fixed unavailable ports despite asserting only client cache shape. Extend test-only scope to those fixtures: ephemeral localhost nonretryable403, isolated temp paths, exact error + unchanged cache assertions. This makes the shared gate usable without altering production retry behavior or lowering coverage requirements. Root validates final Linux measurement after this bounded test change.
