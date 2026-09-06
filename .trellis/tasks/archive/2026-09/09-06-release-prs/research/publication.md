# Publication verification

PR #20 merged into existing default branch master at 5ebda09a64de994c5616ea77f0efe078541b0b7f. Its tree equals tested release commit 654f78378d1b48dd82031cefaace9e27b1b78cb4.

Both push run 34043620089 and PR run 34043623674 passed all five jobs. Downloaded PR coverage artifact reports 96.59%, 3404/3524 lines, gate exit success.

Tagged merge commit as v0.2.1 and created https://github.com/triwinds/bytehaul/releases/tag/v0.2.1. Registry workflows: Rust 34044126103, Python 34044126189. Both publication workflows succeeded. crates.io API confirms 0.2.1, not yanked, checksum 3863a898253cf5e3e8b2121df2d88c020037ce405497cb360ad553b04477650c. PyPI version API confirms 0.2.1 with four abi3 wheels and one sdist, all not yanked (full filenames/digests in pypi-verification.json).

Closed #18 as superseded by libc 0.2.186; #7, #9 and #16 with incompatibility/migration rationale; #17 as incorporated by merged #20 using rand 0.9.4. No open PRs remain as of release dispatch.
