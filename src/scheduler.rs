use std::collections::HashSet;
use std::sync::Arc;

use parking_lot::Mutex;

use crate::storage::piece_map::PieceMap;
use crate::storage::segment::Segment;

/// Shared scheduler handle.
pub(crate) type Scheduler = Arc<Mutex<SchedulerState>>;

/// Manages piece assignment, completion, and reclamation.
pub(crate) struct SchedulerState {
    piece_map: PieceMap,
    inflight: HashSet<usize>,
}

impl SchedulerState {
    pub fn new(piece_map: PieceMap) -> Self {
        Self {
            piece_map,
            inflight: HashSet::new(),
        }
    }

    /// Assign the next available piece. Returns `None` when there is no more work.
    pub fn assign(&mut self) -> Option<Segment> {
        let piece_id = self.piece_map.next_missing_excluding(&self.inflight)?;
        self.inflight.insert(piece_id);
        let (start, end) = self.piece_map.piece_range(piece_id);
        Some(Segment {
            piece_id,
            start,
            end,
        })
    }

    /// Mark a piece as completed and remove it from inflight.
    pub fn complete(&mut self, piece_id: usize) {
        self.inflight.remove(&piece_id);
        self.piece_map.mark_complete(piece_id);
    }

    /// Reclaim a piece (worker failed); it becomes available for reassignment.
    pub fn reclaim(&mut self, piece_id: usize) {
        self.inflight.remove(&piece_id);
    }

    pub fn all_done(&self) -> bool {
        self.piece_map.all_done()
    }

    pub fn completed_bytes(&self) -> u64 {
        self.piece_map.completed_bytes()
    }

    pub fn remaining_count(&self) -> usize {
        self.piece_map.remaining_count()
    }

    pub fn piece_count(&self) -> usize {
        self.piece_map.piece_count()
    }

    pub fn piece_size(&self) -> u64 {
        self.piece_map.piece_size()
    }

    pub fn total_size(&self) -> u64 {
        self.piece_map.total_size()
    }

    /// Snapshot the completed bitset for control-file persistence.
    pub fn snapshot_bitset(&self) -> Vec<u8> {
        self.piece_map.to_bitset_bytes()
    }
}
