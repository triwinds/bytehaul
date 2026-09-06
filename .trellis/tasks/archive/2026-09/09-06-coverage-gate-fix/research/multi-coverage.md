# Multi-transfer behavioral coverage additions

## Scope

Only src/session/multi.rs test module changed. The observed Linux baseline covered336/405 production lines in this module. Tests target terminal failures, byte accounting and durability boundaries; no source exclusion, threshold change or product refactor was made for coverage.

## Regressions

- A resumed transfer receives403 for the remaining piece: exactly one request despite retry allowance, typed HttpStatus403, Failed progress, preserved completed256-byte output prefix, terminal control snapshot with only the completed bit and no inflight failed lease.
- A segment body ends short or exceeds its requested range: exact body/ResumeMismatch error, forwarded byte accounting consistent with received prefixes, contiguous offsets and correct lease identity, no surplus writes or lease flush. Assertions aggregate all Data messages and do not depend on TCP/frame chunking.
- Preexisting pause/cancel: typed stop error before any writer command, no inflight lease and the piece remains assignable.
- Reused probe with inconsistent total: no body forwarded, typed ResumeMismatch and lease reclaimed.
- Writer fails to acknowledge discard after a short body: received-byte count rolls back to0, ChannelClosed propagates, no inflight lease remains, piece can be reassigned. Fixture aggregates prefix chunks before dropping discard acknowledgement.
- Control-save IO failure caused by a regular file blocking the parent directory: the old file remains unchanged, tracker does not advertise saved progress, and the next attempt after filesystem repair saves the expected completed bit.
- Linux /dev/full: real multi writer failure publishes Failed and preserves the existing control file byte for byte; received bytes cannot create durable progress.

## Validation

Native macOS focused multi tests:17 passed (six new portable tests, existing11). Linux-only /dev/full case is delegated to the root's actual Linux gate. Root owns full coverage measurement and quality checks. Reviewer requested frame-independent body fixtures; this was incorporated without relaxing range or accounting assertions.
