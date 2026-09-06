# Implementation plan

- [x] Fix bounded flow control and checkpoint snapshot ordering, with focused regressions.
- [x] Simplify sparse scheduler and completion counting; benchmark actual multi-worker assignment path.
- [x] Simplify sequential lease cache and offset-ordered drains.
- [x] Remove redundant DNS answer caching; compare opt-in connection pooling.
- [x] Add public single/multi small-budget and pause/cancel integration coverage.
- [x] Update architecture/tuning docs and executable contracts.
- [x] Run affected tests, full Rust all-targets/doc tests, workspace clippy, workspace rustdoc and Python checks where applicable.
- [x] Independent full-scope check, record results; leave changes reviewable.

Commands use rtk. Local HTTP tests unset HTTP_PROXY/HTTPS_PROXY/ALL_PROXY and lowercase variants. User implementation approval already given after review. No remote push/deployment.

Implementation and validation complete. Changes are left uncommitted for review; no deployment, push or control-file format change.
