use std::collections::BTreeMap;

use bytes::{Bytes, BytesMut};

/// A write-back cache that aggregates small data chunks by offset before flushing.
///
/// Data is stored per piece, and within each piece by contiguous byte ranges.
/// Adjacent or overlapping writes are merged to reduce the number of disk I/O operations.
#[derive(Default)]
pub struct WriteBackCache {
    /// Cached entries keyed by piece_id.
    pieces: BTreeMap<usize, PieceCacheEntry>,
    /// Total bytes currently held in the cache.
    total_bytes: usize,
}

/// Cached data for a single piece, stored as a set of contiguous byte ranges.
struct PieceCacheEntry {
    /// Sorted, non-overlapping ranges: (offset, data).
    ranges: Vec<(u64, BytesMut)>,
}

/// A contiguous block of data ready to be flushed to disk.
pub struct FlushBlock {
    pub offset: u64,
    pub data: Bytes,
}

impl WriteBackCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Total bytes currently buffered in the cache.
    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    /// Insert data for a specific piece at the given file offset.
    pub fn insert(&mut self, piece_id: usize, offset: u64, data: Bytes) {
        let len = data.len();
        if len == 0 {
            return;
        }
        let entry = self
            .pieces
            .entry(piece_id)
            .or_insert_with(|| PieceCacheEntry { ranges: Vec::new() });
        entry.merge(offset, data);
        self.total_bytes += len;
    }

    /// Drain all cached data for the given piece, returning flush blocks sorted by offset.
    pub fn drain_piece(&mut self, piece_id: usize) -> Vec<FlushBlock> {
        match self.pieces.remove(&piece_id) {
            Some(entry) => {
                let blocks: Vec<FlushBlock> = entry
                    .ranges
                    .into_iter()
                    .map(|(offset, data)| {
                        self.total_bytes -= data.len();
                        FlushBlock {
                            offset,
                            data: data.freeze(),
                        }
                    })
                    .collect();
                blocks
            }
            None => Vec::new(),
        }
    }

    /// Drain ALL cached data across all pieces, returning flush blocks sorted by offset.
    pub fn drain_all(&mut self) -> Vec<FlushBlock> {
        let mut blocks = Vec::new();
        let pieces = std::mem::take(&mut self.pieces);
        for (_piece_id, entry) in pieces {
            for (offset, data) in entry.ranges {
                self.total_bytes -= data.len();
                blocks.push(FlushBlock {
                    offset,
                    data: data.freeze(),
                });
            }
        }
        blocks.sort_by_key(|b| b.offset);
        blocks
    }
}

impl PieceCacheEntry {
    /// Merge new data at `offset` into the existing ranges, coalescing adjacent/overlapping ranges.
    fn merge(&mut self, offset: u64, data: Bytes) {
        let end = offset + data.len() as u64;

        // Find the insertion point
        let pos = self.ranges.partition_point(|(o, _)| *o < offset);

        // Check if we can extend the previous range
        if pos > 0 {
            let prev = &self.ranges[pos - 1];
            let prev_end = prev.0 + prev.1.len() as u64;
            if prev_end >= offset {
                // The new data overlaps or is contiguous with the previous range.
                // Extend the previous range.
                let idx = pos - 1;
                if end > prev_end {
                    let skip = (prev_end - offset) as usize;
                    if skip < data.len() {
                        self.ranges[idx].1.extend_from_slice(&data[skip..]);
                    }
                }
                // Now coalesce with subsequent ranges if they overlap
                self.coalesce_from(idx);
                return;
            }
        }

        // Check if we overlap with the next range
        if pos < self.ranges.len() {
            let next = &self.ranges[pos];
            if end >= next.0 {
                // Overlaps with next range — insert and coalesce
                let mut buf = BytesMut::with_capacity(data.len());
                buf.extend_from_slice(&data);
                self.ranges.insert(pos, (offset, buf));
                self.coalesce_from(pos);
                return;
            }
        }

        // No overlap — insert a new range
        let mut buf = BytesMut::with_capacity(data.len());
        buf.extend_from_slice(&data);
        self.ranges.insert(pos, (offset, buf));
    }

    /// Starting from index `idx`, merge all subsequent ranges that are now contiguous/overlapping.
    fn coalesce_from(&mut self, idx: usize) {
        while idx + 1 < self.ranges.len() {
            let cur_end = self.ranges[idx].0 + self.ranges[idx].1.len() as u64;
            let next_start = self.ranges[idx + 1].0;
            if cur_end >= next_start {
                let next = self.ranges.remove(idx + 1);
                let next_end = next.0 + next.1.len() as u64;
                if next_end > cur_end {
                    let skip = (cur_end - next.0) as usize;
                    if skip < next.1.len() {
                        self.ranges[idx].1.extend_from_slice(&next.1[skip..]);
                    }
                }
            } else {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_insert_and_drain() {
        let mut cache = WriteBackCache::new();
        cache.insert(0, 0, Bytes::from(vec![1u8; 100]));
        cache.insert(0, 100, Bytes::from(vec![2u8; 100]));
        assert_eq!(cache.total_bytes(), 200);

        let blocks = cache.drain_piece(0);
        // Should merge into one contiguous block
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].offset, 0);
        assert_eq!(blocks[0].data.len(), 200);
        assert_eq!(cache.total_bytes(), 0);
    }

    #[test]
    fn test_non_contiguous_ranges() {
        let mut cache = WriteBackCache::new();
        cache.insert(0, 0, Bytes::from(vec![1u8; 100]));
        cache.insert(0, 200, Bytes::from(vec![2u8; 100]));
        assert_eq!(cache.total_bytes(), 200);

        let blocks = cache.drain_piece(0);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].offset, 0);
        assert_eq!(blocks[0].data.len(), 100);
        assert_eq!(blocks[1].offset, 200);
        assert_eq!(blocks[1].data.len(), 100);
    }

    #[test]
    fn test_overlapping_merge() {
        let mut cache = WriteBackCache::new();
        cache.insert(0, 0, Bytes::from(vec![1u8; 100]));
        cache.insert(0, 50, Bytes::from(vec![2u8; 100]));
        // total_bytes tracks inserts, not net unique bytes
        assert_eq!(cache.total_bytes(), 200);

        let blocks = cache.drain_piece(0);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].offset, 0);
        assert_eq!(blocks[0].data.len(), 150);
    }

    #[test]
    fn test_drain_all() {
        let mut cache = WriteBackCache::new();
        cache.insert(0, 0, Bytes::from(vec![1u8; 100]));
        cache.insert(1, 1000, Bytes::from(vec![2u8; 100]));

        let blocks = cache.drain_all();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].offset, 0);
        assert_eq!(blocks[1].offset, 1000);
        assert_eq!(cache.total_bytes(), 0);
    }

    #[test]
    fn test_multiple_pieces() {
        let mut cache = WriteBackCache::new();
        cache.insert(0, 0, Bytes::from(vec![1u8; 100]));
        cache.insert(1, 1000, Bytes::from(vec![2u8; 200]));
        cache.insert(0, 100, Bytes::from(vec![3u8; 50]));

        assert_eq!(cache.total_bytes(), 350);

        let blocks = cache.drain_piece(0);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].data.len(), 150);

        let blocks = cache.drain_piece(1);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].data.len(), 200);
    }

    #[test]
    fn test_drain_piece_empty() {
        let mut cache = WriteBackCache::new();
        let blocks = cache.drain_piece(99);
        assert!(blocks.is_empty());
    }

    #[test]
    fn test_insert_empty_data() {
        let mut cache = WriteBackCache::new();
        cache.insert(0, 0, Bytes::new());
        assert_eq!(cache.total_bytes(), 0);
        let blocks = cache.drain_piece(0);
        assert!(blocks.is_empty());
    }

    #[test]
    fn test_overlap_with_next_range() {
        let mut cache = WriteBackCache::new();
        // Insert a range at offset 100
        cache.insert(0, 100, Bytes::from(vec![2u8; 50]));
        // Insert a range at offset 0 that overlaps with the one at 100
        cache.insert(0, 0, Bytes::from(vec![1u8; 120]));

        let blocks = cache.drain_piece(0);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].offset, 0);
        assert_eq!(blocks[0].data.len(), 150); // 0..120 merged with 100..150
    }

    #[test]
    fn test_coalesce_multiple_ranges() {
        let mut cache = WriteBackCache::new();
        // Create three separate ranges
        cache.insert(0, 0, Bytes::from(vec![1u8; 50]));
        cache.insert(0, 100, Bytes::from(vec![2u8; 50]));
        cache.insert(0, 200, Bytes::from(vec![3u8; 50]));
        // Now insert something that bridges all three
        cache.insert(0, 40, Bytes::from(vec![4u8; 170]));

        let blocks = cache.drain_piece(0);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].offset, 0);
        assert_eq!(blocks[0].data.len(), 250);
    }

    #[test]
    fn test_insert_contiguous_extends_previous() {
        let mut cache = WriteBackCache::new();
        cache.insert(0, 0, Bytes::from(vec![1u8; 100]));
        // Exactly contiguous
        cache.insert(0, 100, Bytes::from(vec![2u8; 100]));
        let blocks = cache.drain_piece(0);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].data.len(), 200);
    }

    #[test]
    fn test_fully_overlapping_insert() {
        let mut cache = WriteBackCache::new();
        cache.insert(0, 0, Bytes::from(vec![1u8; 200]));
        // Insert completely within existing range
        cache.insert(0, 50, Bytes::from(vec![2u8; 50]));
        let blocks = cache.drain_piece(0);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].offset, 0);
        assert_eq!(blocks[0].data.len(), 200);
    }

    #[test]
    fn test_drain_all_sorts_by_offset() {
        let mut cache = WriteBackCache::new();
        cache.insert(5, 5000, Bytes::from(vec![1u8; 10]));
        cache.insert(0, 0, Bytes::from(vec![2u8; 10]));
        cache.insert(3, 3000, Bytes::from(vec![3u8; 10]));

        let blocks = cache.drain_all();
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0].offset, 0);
        assert_eq!(blocks[1].offset, 3000);
        assert_eq!(blocks[2].offset, 5000);
        assert_eq!(cache.total_bytes(), 0);
    }

    #[test]
    fn test_coalesce_stops_at_gap() {
        let mut cache = WriteBackCache::new();
        // Three ranges with a gap between second and third
        cache.insert(0, 0, Bytes::from(vec![1u8; 50]));
        cache.insert(0, 50, Bytes::from(vec![2u8; 50]));
        cache.insert(0, 200, Bytes::from(vec![3u8; 50]));

        let blocks = cache.drain_piece(0);
        // First two should merge, third should be separate
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].offset, 0);
        assert_eq!(blocks[0].data.len(), 100);
        assert_eq!(blocks[1].offset, 200);
        assert_eq!(blocks[1].data.len(), 50);
    }

    #[test]
    fn test_coalesce_breaks_when_next_range_is_still_gapped() {
        let mut cache = WriteBackCache::new();
        cache.insert(0, 0, Bytes::from(vec![1u8; 50]));
        cache.insert(0, 100, Bytes::from(vec![2u8; 50]));
        cache.insert(0, 200, Bytes::from(vec![3u8; 50]));

        // This extends the first range, but still leaves a gap before the second.
        cache.insert(0, 40, Bytes::from(vec![4u8; 40]));

        let blocks = cache.drain_piece(0);
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0].offset, 0);
        assert_eq!(blocks[0].data.len(), 80);
        assert_eq!(blocks[1].offset, 100);
        assert_eq!(blocks[2].offset, 200);
    }
}
