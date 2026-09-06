# Design

- Bound forwarded data chunks to available configured budget geometry; choose a cache flush watermark below the budget with enough headroom for at least one maximum chunk. Ensure semaphore acquisition/channel/rate waits can terminate on cancellation or writer closure. Share flow-control helpers between single/multi where this reduces duplication.
- Multi checkpoint captures bitset and hints under the scheduler lock before awaiting the FIFO flush/sync barrier, then writes that frozen snapshot. Do not hold scheduler locks across await.
- Scheduler uses sparse touched-piece state plus bitset/global incremental counters; preserve round-robin selection and explicit subrange behavior. Completion removes heavyweight state. Compute hints from sparse state, retaining serialized fields.
- Cache entries contain contiguous buffered data and offset per lease. Reject out-of-order/gapped/overlapping data instead of silently merging it; failures are typed internal errors at the writer boundary. Global drains remain in file-offset order even after lease renewal.
- Hickory already provides bounded TTL cache; remove the outer HashMap and its tests, replacing with tests of resolver-observable behavior where needed.
- Connection pool comparison is a local diagnostic benchmark, not evidence of WAN/TLS reliability. Existing default remains unless sufficiently supported.

## Ownership
Scheduler worker: src/scheduler.rs, src/storage/piece_map.rs, src/lib.rs bench helpers and benches/storage_bench.rs.
Flow worker: src/storage/cache.rs, src/storage/writer.rs, src/session/{mod,single,multi}.rs and any focused internal flow helper; owns deterministic checkpoint and flow tests.
Network worker: src/network.rs and a new standalone pool comparison example if useful.
Root: task artifacts, integration tests in a new dedicated test file, documentation/specs, integration validation and review.

## Compatibility and rollback
No file-format version bump or supported API changes. Each optimization must preserve focused regression behavior. Retain opt-in pooling until broader evidence warrants a separate default decision.
