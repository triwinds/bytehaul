# Review evidence

aria2 inspected at 9e7273583f83e881e3ec067b523ba88724088d2f in /tmp/bytehaul-aria2-review. Key references: DefaultPieceStorage.cc getPiece/checkOutPiece and bitfield; WrDiskCacheEntry.cc append; WrDiskCache.cc ensureLimit; HttpDownloadCommand.cc prepareForNextSegment.

Original source harness /tmp/bytehaul-review-harness: 100 budget, 60-byte cached chunk then acquire 60 stalls until explicit flush. Same 1,000 assignments/completions with 1k/10k/100k pieces: approximately 0.75/34/429 ms release (single local sample). Source checkpoint snapshots scheduler after sync, permitting post-sync completion inclusion.

Hickory 0.25.2 resolver.rs builds DnsLru and CachingClient. Extra network.rs HashMap duplicates answer TTL caching and lacks a size bound.
