use std::collections::HashSet;

use bitvec::prelude::*;

/// Tracks piece-level completion state for a download.
pub struct PieceMap {
    piece_size: u64,
    total_size: u64,
    piece_count: usize,
    completed: BitVec<u8, Lsb0>,
}

impl PieceMap {
    /// Create a fresh PieceMap with all pieces missing.
    pub fn new(total_size: u64, piece_size: u64) -> Self {
        let piece_count = total_size.div_ceil(piece_size) as usize;
        Self {
            piece_size,
            total_size,
            piece_count,
            completed: bitvec![u8, Lsb0; 0; piece_count],
        }
    }

    /// Restore a PieceMap from a serialized bitset (control file).
    pub fn from_bitset(
        total_size: u64,
        piece_size: u64,
        bitset: &[u8],
        piece_count: usize,
    ) -> Self {
        let mut completed = BitVec::<u8, Lsb0>::from_slice(bitset);
        completed.resize(piece_count, false);
        Self {
            piece_size,
            total_size,
            piece_count,
            completed,
        }
    }

    /// Returns the configured piece size in bytes.
    pub fn piece_size(&self) -> u64 {
        self.piece_size
    }
    /// Returns the total file size in bytes.
    #[allow(dead_code)]
    pub fn total_size(&self) -> u64 {
        self.total_size
    }
    /// Returns the total number of pieces.
    pub fn piece_count(&self) -> usize {
        self.piece_count
    }

    /// Byte range `[start, end)` for the given piece.
    pub fn piece_range(&self, piece_id: usize) -> (u64, u64) {
        let start = piece_id as u64 * self.piece_size;
        let end = ((piece_id as u64 + 1) * self.piece_size).min(self.total_size);
        (start, end)
    }

    /// Returns `true` if the given piece has been marked as complete.
    #[allow(dead_code)]
    pub fn is_complete(&self, piece_id: usize) -> bool {
        piece_id < self.piece_count && self.completed[piece_id]
    }

    /// Mark the given piece as complete. Out-of-bounds IDs are silently ignored.
    pub fn mark_complete(&mut self, piece_id: usize) {
        if piece_id < self.piece_count {
            self.completed.set(piece_id, true);
        }
    }

    /// Return the first missing piece not in `exclude`.
    pub fn next_missing_excluding(&self, exclude: &HashSet<usize>) -> Option<usize> {
        (0..self.piece_count).find(|&i| !self.completed[i] && !exclude.contains(&i))
    }

    /// Returns `true` if every piece has been completed.
    pub fn all_done(&self) -> bool {
        self.completed.all()
    }

    /// Returns the number of completed pieces.
    pub fn completed_count(&self) -> usize {
        self.completed.count_ones()
    }

    /// Returns the total number of bytes in completed pieces.
    pub fn completed_bytes(&self) -> u64 {
        let mut bytes = 0u64;
        for i in 0..self.piece_count {
            if self.completed[i] {
                let (start, end) = self.piece_range(i);
                bytes += end - start;
            }
        }
        bytes
    }

    /// Returns the number of pieces that have not yet been completed.
    pub fn remaining_count(&self) -> usize {
        self.piece_count - self.completed_count()
    }

    /// Serialize the bitset for storage in the control file.
    pub fn to_bitset_bytes(&self) -> Vec<u8> {
        self.completed.as_raw_slice().to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_piece_map_basic() {
        let mut pm = PieceMap::new(10_000_000, 1_000_000); // 10 MB, 1 MB pieces
        assert_eq!(pm.piece_count(), 10);
        assert!(!pm.all_done());
        assert_eq!(pm.completed_bytes(), 0);

        pm.mark_complete(0);
        pm.mark_complete(5);
        assert!(pm.is_complete(0));
        assert!(!pm.is_complete(1));
        assert!(pm.is_complete(5));
        assert_eq!(pm.completed_count(), 2);
        assert_eq!(pm.completed_bytes(), 2_000_000);
    }

    #[test]
    fn test_piece_range_last_piece() {
        let pm = PieceMap::new(2_500_000, 1_000_000); // 2.5 MB, 1 MB pieces
        assert_eq!(pm.piece_count(), 3);
        assert_eq!(pm.piece_range(0), (0, 1_000_000));
        assert_eq!(pm.piece_range(1), (1_000_000, 2_000_000));
        assert_eq!(pm.piece_range(2), (2_000_000, 2_500_000)); // last piece smaller
    }

    #[test]
    fn test_next_missing_excluding() {
        let mut pm = PieceMap::new(5_000_000, 1_000_000);
        pm.mark_complete(0);
        pm.mark_complete(2);

        let mut excl = HashSet::new();
        assert_eq!(pm.next_missing_excluding(&excl), Some(1));
        excl.insert(1);
        assert_eq!(pm.next_missing_excluding(&excl), Some(3));
    }

    #[test]
    fn test_bitset_roundtrip() {
        let mut pm = PieceMap::new(5_000_000, 1_000_000);
        pm.mark_complete(0);
        pm.mark_complete(3);

        let bytes = pm.to_bitset_bytes();
        let pm2 = PieceMap::from_bitset(5_000_000, 1_000_000, &bytes, 5);

        assert!(pm2.is_complete(0));
        assert!(!pm2.is_complete(1));
        assert!(!pm2.is_complete(2));
        assert!(pm2.is_complete(3));
        assert!(!pm2.is_complete(4));
    }

    #[test]
    fn test_all_done() {
        let mut pm = PieceMap::new(2_000_000, 1_000_000);
        assert!(!pm.all_done());
        pm.mark_complete(0);
        assert!(!pm.all_done());
        pm.mark_complete(1);
        assert!(pm.all_done());
    }

    #[test]
    fn test_total_size_and_accessors() {
        let pm = PieceMap::new(2_500_000, 1_000_000);
        assert_eq!(pm.total_size(), 2_500_000);
        assert_eq!(pm.piece_size(), 1_000_000);
        assert_eq!(pm.piece_count(), 3);
        assert_eq!(pm.remaining_count(), 3);
        assert_eq!(pm.completed_count(), 0);
    }

    #[test]
    fn test_mark_complete_out_of_bounds() {
        let mut pm = PieceMap::new(1_000_000, 1_000_000);
        // Should not panic for out-of-bounds piece_id
        pm.mark_complete(999);
        assert!(!pm.is_complete(999));
    }

    #[test]
    fn test_is_complete_out_of_bounds() {
        let pm = PieceMap::new(1_000_000, 1_000_000);
        assert!(!pm.is_complete(999));
    }

    #[test]
    fn test_completed_bytes_partial() {
        let mut pm = PieceMap::new(2_500_000, 1_000_000);
        pm.mark_complete(2); // last piece is 500,000 bytes
        assert_eq!(pm.completed_bytes(), 500_000);
    }
}
