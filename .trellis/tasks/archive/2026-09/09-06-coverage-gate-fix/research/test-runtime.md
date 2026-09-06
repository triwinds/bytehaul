# Cache-configuration test runtime

## Change boundary

The six async manager tests for timeout/proxy/pool override and client reuse perform real downloads against fixed unavailable ports. Their assertions concern config selection/cache entries, but failures invoke default network retry waits. Change only the manager test module: reuse the repository's ephemeral Warp-server pattern, return nonretryable HTTP 403, use per-test temporary output paths, and assert exact HTTP status alongside cache counts. Proxy cases use the fixture as an HTTP proxy for a reserved `.invalid` origin. Production retry behavior and dedicated retry tests remain untouched.

## Validation

Root observed an earlier full native unit run taking about 174 seconds. This is a baseline for that full run, not a measured time for these six tests. After change, proxy-isolated native `cargo test -p bytehaul --lib manager::tests` passed all 26 tests; the test harness reported 1.21 seconds (compilation separately took 32.90 seconds). `git diff --check -- src/manager.rs` passed. No broad performance claim is inferred from unlike test scopes. The six tests retain default retry settings; HTTP 403 naturally avoids retry waits. Every attempted download now asserts the exact 403 result, including both calls in cache-reuse cases.
