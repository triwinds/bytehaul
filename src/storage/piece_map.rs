use bitvec::prelude::*;

/// Tracks piece-level completion state for a download.
pub struct PieceMap {
    piece_size: u64,
    total_size: u64,
    piece_count: usize,
    completed: BitVec<u8, Lsb0>,
    completed_pieces: usize,
    completed_bytes: u64,
    next_candidate: usize,
    highest_completed_piece: Option<usize>,
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
            completed_pieces: 0,
            completed_bytes: 0,
            next_candidate: 0,
            highest_completed_piece: None,
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
        let mut completed_pieces = 0usize;
        let mut completed_bytes = 0u64;
        let mut next_candidate = piece_count;
        let mut highest_completed_piece = None;

        for piece_id in 0..piece_count {
            if completed[piece_id] {
                completed_pieces += 1;
                completed_bytes += piece_len(total_size, piece_size, piece_id);
                highest_completed_piece = Some(piece_id);
            } else if next_candidate == piece_count {
                next_candidate = piece_id;
            }
        }

        Self {
            piece_size,
            total_size,
            piece_count,
            completed,
            completed_pieces,
            completed_bytes,
            next_candidate,
            highest_completed_piece,
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
        let end = start + piece_len(self.total_size, self.piece_size, piece_id);
        (start, end)
    }

    /// Returns `true` if the given piece has been marked as complete.
    #[allow(dead_code)]
    pub fn is_complete(&self, piece_id: usize) -> bool {
        piece_id < self.piece_count && self.completed[piece_id]
    }

    /// Mark the given piece as complete. Out-of-bounds IDs are silently ignored.
    pub fn mark_complete(&mut self, piece_id: usize) {
        if piece_id < self.piece_count && !self.completed[piece_id] {
            self.completed.set(piece_id, true);
            self.completed_pieces += 1;
            self.completed_bytes += piece_len(self.total_size, self.piece_size, piece_id);
            self.highest_completed_piece = Some(
                self.highest_completed_piece
                    .map_or(piece_id, |current| current.max(piece_id)),
            );
            if piece_id == self.next_candidate {
                self.advance_next_candidate();
            }
        }
    }

    /// Return the first missing piece.
    pub fn first_missing(&self) -> Option<usize> {
        (self.next_candidate < self.piece_count).then_some(self.next_candidate)
    }

    /// Return the highest completed piece, if any.
    pub fn highest_completed_piece(&self) -> Option<usize> {
        self.highest_completed_piece
    }

    /// Returns `true` if every piece has been completed.
    pub fn all_done(&self) -> bool {
        self.completed.all()
    }

    /// Returns the number of completed pieces.
    pub fn completed_count(&self) -> usize {
        self.completed_pieces
    }

    /// Returns the total number of bytes in completed pieces.
    pub fn completed_bytes(&self) -> u64 {
        self.completed_bytes
    }

    /// Returns the number of pieces that have not yet been completed.
    pub fn remaining_count(&self) -> usize {
        self.piece_count - self.completed_pieces
    }

    /// Serialize the bitset for storage in the control file.
    pub fn to_bitset_bytes(&self) -> Vec<u8> {
        self.completed.as_raw_slice().to_vec()
    }

    fn advance_next_candidate(&mut self) {
        while self.next_candidate < self.piece_count && self.completed[self.next_candidate] {
            self.next_candidate += 1;
        }
    }
}

fn piece_len(total_size: u64, piece_size: u64, piece_id: usize) -> u64 {
    let start = piece_id as u64 * piece_size;
    ((piece_id as u64 + 1) * piece_size).min(total_size) - start
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
    fn test_first_missing_tracks_next_gap() {
        let mut pm = PieceMap::new(5_000_000, 1_000_000);
        pm.mark_complete(0);
        pm.mark_complete(2);

        assert_eq!(pm.first_missing(), Some(1));
        pm.mark_complete(1);
        assert_eq!(pm.first_missing(), Some(3));
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

    #[test]
    fn test_highest_completed_piece_updates() {
        let mut pm = PieceMap::new(4_500_000, 1_000_000);
        assert_eq!(pm.highest_completed_piece(), None);

        pm.mark_complete(1);
        assert_eq!(pm.highest_completed_piece(), Some(1));

        pm.mark_complete(3);
        assert_eq!(pm.highest_completed_piece(), Some(3));
    }

    #[test]
    fn test_from_bitset_restores_counters() {
        let bytes = vec![0b0000_0101];
        let pm = PieceMap::from_bitset(3_500_000, 1_000_000, &bytes, 4);

        assert_eq!(pm.completed_count(), 2);
        assert_eq!(pm.completed_bytes(), 2_000_000);
        assert_eq!(pm.first_missing(), Some(1));
        assert_eq!(pm.highest_completed_piece(), Some(2));
    }
}
