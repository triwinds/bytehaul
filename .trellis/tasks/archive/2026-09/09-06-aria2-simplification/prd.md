# Simplify download internals after aria2 review

## Goal
Fix the confirmed flow-control failure and checkpoint durability race, reduce unnecessary runtime state and copying/merging machinery, and validate connection reuse using the existing opt-in setting.

## Authorization
The user reviewed the findings and recommendations in this conversation and replied “改吧”. This approves implementing the reviewed scope; task bookkeeping records that existing authorization rather than reopening approval.

## Requirements and acceptance
- R1: Accepted positive memory budgets must not hang because chunks exceed available budget or the writer waits for the same budget watermark. Single and multi paths preserve bytes, cancellation and typed failures. Regression tests cover small budgets, non-divisible chunks and concurrent leases.
- R2: Multi checkpoints may only describe completed pieces included before the writer sync barrier. A deterministic interleaving test excludes completions occurring after snapshot capture. V1/V2 compatibility remains.
- R3: Scheduler keeps detailed state only for touched/incomplete pieces; no full-piece scan merely to count active/available work per assignment. Lease renewal, partial completion, reclamation and splitting remain correct. Hint collection walks sparse state. O(1) all_done uses existing completion count.
- R4: Cache models sequential per-lease appends, rejects noncontiguous same-lease data, preserves stale-lease isolation and budget return. Unit tests enforce the real contract rather than arbitrary overlap support.
- R5: Remove redundant outer DNS answer cache while preserving Hickory TTL caching, lookup behavior and useful logging.
- R6: Measure current opt-in HTTP pool using reproducible local comparison and existing keep-alive/close tests. Change defaults only if evidence is sufficient for compatibility and reliability; otherwise document results and retain opt-in.

## Boundaries
Retain Range validation, lease identity, single versus multi recovery semantics, public Rust/Python APIs, and control-file V1/V2 reads. Do not add HTTP pipelining, custom socket pooling, multi-piece HTTP request scheduling or partial-piece durable recovery.
