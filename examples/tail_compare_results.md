# Fast tail detection comparison

Measured 2026-09-09 on Windows x86_64 MSVC with rustc 1.92.0. The workspace
still declares version 0.2.2; these source changes are not a published release.

## Fixture and reproduction

`tail_compare.rs` ports the supplied ns-emu-tools comparison: 128 MiB, four
connections, default 1 MiB pieces, 4 MiB split threshold, no allocation or speed
cap, fresh temporary output, loopback HTTP Range support and a strong ETag.
Only the first response reaching the final MiB becomes slow (4 KiB per requested
125 ms sleep); subsequent responses run normally. Normal chunks are up to
64 KiB per requested 1 ms sleep. Windows timer scheduling affects actual rates.
Every output is validated byte by byte. Elapsed time excludes validation and
compilation. The bytehaul harness uses the dev profile; aria2 is the installed
1.37.0 binary. No proxy environment variables were supplied to the new runs.

```powershell
cargo run --example tail_compare -- C:/path/to/aria2c.exe 3
# Only the adaptive slow-tail cases, without aria2:
cargo run --example tail_compare -- - 3 recovery-only
```

CSV is written to stdout and request/recovery diagnostics to stderr. Recovery
logs contain `fast_tail`, `hedge`, duplicate bytes and reserved extra-work budget.

## Published baseline

The supplied three-round report measured slow-tail medians of 27.995 seconds
for Adaptive, 28.102 for Hedging, and 10.463 for aria2. Immediately before the
new comparison, the existing ns-emu-tools binary (built against registry bytehaul
0.2.2, before these source edits) was rerun for three hedge-only rounds.
[Baseline CSV](tail_compare_baseline.csv): **28.106 seconds median**. All three
outputs matched; median duplicate bytes were 626,688.

## Initial full comparison

[Pilot CSV](tail_compare_pilot.csv) contains three rotating-order rounds of all
four modes, normal and slow tail. This build already included fast detection but
preceded the final guard for occupied slots without registered observations.
All 24 outputs matched.

| Mode | Normal median (s) | Slow-tail median (s) |
| --- | ---: | ---: |
| Disabled | 7.113 | 42.355 |
| Adaptive | 7.376 | 10.090 |
| AdaptiveWithHedging | 7.085 | 10.232 |
| aria2 | 8.051 | 10.405 |

## Final guard verification

[Final CSV](tail_compare_final.csv) contains three recovery-only rounds after
rebuilding with the occupied-slot/observation guard. All six outputs matched.

| Mode | Final slow-tail median (s) | Duplicate bytes, median |
| --- | ---: | ---: |
| Adaptive | 10.298 | 94,208 |
| AdaptiveWithHedging | 10.295 | 102,400 |

Hedging elapsed time fell **63.4%** relative to the freshly rerun published
baseline (28.106 seconds). Median duplicate traffic fell **83.7%**. All six
recovery logs reported `fast_tail=true`, one action and 1,048,576 reserved extra
bytes against a 1,342,177-byte budget. Adaptive made four replacement requests;
Hedging made one challenger request. Request counts do not prove simultaneous
server activity. The final guard did not remove the observed speedup.

Final measured source SHA-256:

- `src/session/multi/adaptive.rs`: `d33175fae9aa08213ee30622c60add874cf65586da87bab2d575f0f1cc3f081d`
- `examples/tail_compare.rs`: `4259fa068fbbdccb1d2e23ba54a9469df4f21501addb56aab188782c1dc4931b`

## Interpretation

The improvement comes from shortening detection: ordinary 5-second sampling
plus 15-second sustained evidence remains available, while eligible small tails
use a separate window/grace/duration capped at 1/1/2 seconds. Full-range recovery,
writer ownership, the 1% extra-work budget and cooldown remain unchanged.

This fixture deliberately makes a replacement fast. The measurements do not
predict WAN/CDN throughput or improvements when every replacement remains slow.
Three samples cannot establish a performance advantage over aria2. Unit and
integration tests cover suppression and correctness separately from timing.

## Code verification

On this Windows workspace, 450 Rust tests and one doctest passed. Workspace
Clippy and rustdoc passed with warnings denied. No Linux coverage result or
Python runtime test result is claimed for this change.
