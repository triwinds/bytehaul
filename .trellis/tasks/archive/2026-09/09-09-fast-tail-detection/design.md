# Design

Detection and eligibility live in src/session/multi/adaptive.rs. Add a bounded short observation window alongside ordinary sampling, sharing phase/wire/forwarding accounting. Tail thresholds cap the configured sample window/grace/duration at 1/1/2 seconds so existing shorter test/user policies remain effective. Use the existing relative 25% threshold and conservative full-range replacement benefit calculation. Require a healthy baseline, not absolute floor alone, for the fast path.

Fast eligibility requires a range <= 1 MiB, no scheduler-available work and a spare request slot. Fast baseline must check live peers using short-window evidence: an immature/blocked peer must not be interpreted as healthy, and collective slowdown must suppress acceleration. Preserve recent bounded completed history. Reset fast sustained evidence on loss of eligibility/healthy baseline, and clear short samples after a short-window local block. Never overwrite ordinary observation history or reset ordinary detection when entering/exiting tail mode. Timer must sample fast enough for the short window (bounded normal tick cost).

Fast baseline additionally requires the target observation to exist and the occupied-slot count to equal the observation count. A slot can be held across a writer setup barrier or retry backoff without a registered observation; these gaps cannot endorse history as healthy live evidence.

Recovery execution remains identical: only the detection timing changes. No lease/writer/public field changes. Log the selected detection path. Document configured durations as ordinary-stage settings with bounded tail acceleration. Rollback consists of removing fast eligibility/detection, leaving the original slow path intact.

Files: adaptive.rs and m12_slow_transfer.rs own behavior/tests; config.rs rustdoc and advanced English/Chinese docs explain semantics; slow-transfer spec records contract. examples/tail_compare.rs ports the supplied controlled harness for reproducible workspace measurements; comparison results record evidence.
