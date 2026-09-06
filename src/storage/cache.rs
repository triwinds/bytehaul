use std::collections::BTreeMap;

use bytes::{Bytes, BytesMut};

use crate::storage::segment::LeaseKey;

/// Sequential HTTP body buffers, isolated by lease identity.
#[derive(Default)]
pub struct WriteBackCache {
    pieces: BTreeMap<LeaseKey, PieceCacheEntry>,
    total_bytes: usize,
}

struct PieceCacheEntry {
    offset: u64,
    data: BytesMut,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("noncontiguous lease write: expected offset {expected}, got {actual}")]
pub(crate) struct NonContiguousWrite {
    pub expected: u64,
    pub actual: u64,
}

/// A contiguous block of data ready to be flushed to disk.
pub struct FlushBlock {
    pub offset: u64,
    pub data: Bytes,
}

impl WriteBackCache {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    /// Append a body chunk. A lease must never overlap or skip buffered bytes.
    pub(crate) fn insert(
        &mut self,
        lease_key: LeaseKey,
        offset: u64,
        data: Bytes,
    ) -> Result<(), NonContiguousWrite> {
        if data.is_empty() {
            return Ok(());
        }
        let entry = self
            .pieces
            .entry(lease_key)
            .or_insert_with(|| PieceCacheEntry {
                offset,
                data: BytesMut::new(),
            });
        let expected = entry.offset + entry.data.len() as u64;
        if offset != expected {
            return Err(NonContiguousWrite {
                expected,
                actual: offset,
            });
        }
        entry.data.extend_from_slice(&data);
        self.total_bytes += data.len();
        Ok(())
    }

    pub(crate) fn drain_lease(&mut self, lease_key: LeaseKey) -> Vec<FlushBlock> {
        match self.pieces.remove(&lease_key) {
            Some(entry) => {
                self.total_bytes -= entry.data.len();
                vec![FlushBlock {
                    offset: entry.offset,
                    data: entry.data.freeze(),
                }]
            }
            None => Vec::new(),
        }
    }

    pub(crate) fn discard_lease(&mut self, lease_key: LeaseKey) -> usize {
        match self.pieces.remove(&lease_key) {
            Some(entry) => {
                self.total_bytes -= entry.data.len();
                entry.data.len()
            }
            None => 0,
        }
    }

    /// Lease order differs from file order after retries and subrange splitting.
    pub(crate) fn drain_all(&mut self) -> Vec<FlushBlock> {
        let mut blocks: Vec<_> = std::mem::take(&mut self.pieces)
            .into_values()
            .map(|entry| FlushBlock {
                offset: entry.offset,
                data: entry.data.freeze(),
            })
            .collect();
        self.total_bytes = 0;
        blocks.sort_unstable_by_key(|block| block.offset);
        blocks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lease(lease_id: u64) -> LeaseKey {
        LeaseKey {
            piece_id: 0,
            lease_id,
        }
    }

    #[test]
    fn sequential_append_preserves_bytes() {
        let mut cache = WriteBackCache::new();
        cache
            .insert(lease(1), 10, Bytes::from_static(b"abc"))
            .unwrap();
        cache
            .insert(lease(1), 13, Bytes::from_static(b"def"))
            .unwrap();
        assert_eq!(cache.total_bytes(), 6);
        let blocks = cache.drain_lease(lease(1));
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].offset, 10);
        assert_eq!(blocks[0].data, &b"abcdef"[..]);
        assert_eq!(cache.total_bytes(), 0);
        assert!(cache.drain_lease(lease(1)).is_empty());
    }

    #[test]
    fn gap_and_overlap_are_rejected_without_mutation() {
        let mut cache = WriteBackCache::new();
        cache
            .insert(lease(1), 10, Bytes::from_static(b"abc"))
            .unwrap();
        for offset in [9, 10, 12, 14] {
            assert_eq!(
                cache.insert(lease(1), offset, Bytes::from_static(b"x")),
                Err(NonContiguousWrite {
                    expected: 13,
                    actual: offset
                })
            );
            assert_eq!(cache.total_bytes(), 3);
        }
        assert_eq!(cache.drain_lease(lease(1))[0].data, &b"abc"[..]);
    }

    #[test]
    fn renewed_leases_drain_in_file_order_and_discard_independently() {
        let mut cache = WriteBackCache::new();
        cache
            .insert(lease(1), 100, Bytes::from_static(b"tail"))
            .unwrap();
        cache
            .insert(lease(2), 0, Bytes::from_static(b"old"))
            .unwrap();
        assert_eq!(cache.discard_lease(lease(2)), 3);
        assert_eq!(cache.discard_lease(lease(2)), 0);
        cache
            .insert(lease(3), 0, Bytes::from_static(b"head"))
            .unwrap();
        cache.insert(lease(3), 999, Bytes::new()).unwrap();
        assert_eq!(cache.total_bytes(), 8);
        let blocks = cache.drain_all();
        assert_eq!(
            blocks.iter().map(|b| b.offset).collect::<Vec<_>>(),
            vec![0, 100]
        );
        assert_eq!(blocks[0].data, &b"head"[..]);
        assert_eq!(blocks[1].data, &b"tail"[..]);
        assert_eq!(cache.total_bytes(), 0);
    }
}
