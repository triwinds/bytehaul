use std::sync::Arc;

use parking_lot::Mutex;

use crate::storage::piece_map::PieceMap;
use crate::storage::segment::Segment;

/// Shared scheduler handle.
pub(crate) type Scheduler = Arc<Mutex<SchedulerState>>;

/// Manages piece assignment, completion, and reclamation.
pub(crate) struct SchedulerState {
    piece_map: PieceMap,
    inflight: Vec<bool>,
    next_candidate: usize,
}

impl SchedulerState {
    pub fn new(piece_map: PieceMap) -> Self {
        let piece_count = piece_map.piece_count();
        let next_candidate = piece_map.first_missing().unwrap_or(0);
        Self {
            piece_map,
            inflight: vec![false; piece_count],
            next_candidate,
        }
    }

    /// Assign the next available piece. Returns `None` when there is no more work.
    pub fn assign(&mut self) -> Option<Segment> {
        let piece_count = self.piece_map.piece_count();
        if piece_count == 0 || self.piece_map.all_done() {
            return None;
        }

        for step in 0..piece_count {
            let piece_id = (self.next_candidate + step) % piece_count;
            if self.piece_map.is_complete(piece_id) || self.inflight[piece_id] {
                continue;
            }
            self.inflight[piece_id] = true;
            self.next_candidate = (piece_id + 1) % piece_count;
            let (start, end) = self.piece_map.piece_range(piece_id);
            return Some(Segment {
                piece_id,
                start,
                end,
            });
        }

        None
    }

    /// Mark a piece as completed and remove it from inflight.
    pub fn complete(&mut self, piece_id: usize) {
        if piece_id < self.inflight.len() {
            self.inflight[piece_id] = false;
        }
        self.piece_map.mark_complete(piece_id);
    }

    /// Reclaim a piece (worker failed); it becomes available for reassignment.
    pub fn reclaim(&mut self, piece_id: usize) {
        if piece_id < self.inflight.len() {
            self.inflight[piece_id] = false;
            if piece_id < self.next_candidate {
                self.next_candidate = piece_id;
            }
        }
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

    #[allow(dead_code)]
    pub fn total_size(&self) -> u64 {
        self.piece_map.total_size()
    }

    /// Snapshot the completed bitset for control-file persistence.
    pub fn snapshot_bitset(&self) -> Vec<u8> {
        self.piece_map.to_bitset_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::piece_map::PieceMap;

    #[test]
    fn test_scheduler_assign_and_complete() {
        let pm = PieceMap::new(3_000_000, 1_000_000);
        let mut sched = SchedulerState::new(pm);
        assert!(!sched.all_done());
        assert_eq!(sched.remaining_count(), 3);
        assert_eq!(sched.piece_count(), 3);
        assert_eq!(sched.piece_size(), 1_000_000);
        assert_eq!(sched.total_size(), 3_000_000);
        assert_eq!(sched.completed_bytes(), 0);

        let seg = sched.assign().unwrap();
        assert_eq!(seg.piece_id, 0);
        assert_eq!(seg.start, 0);
        assert_eq!(seg.end, 1_000_000);

        sched.complete(0);
        assert_eq!(sched.remaining_count(), 2);
        assert_eq!(sched.completed_bytes(), 1_000_000);
    }

    #[test]
    fn test_scheduler_reclaim() {
        let pm = PieceMap::new(2_000_000, 1_000_000);
        let mut sched = SchedulerState::new(pm);

        let seg = sched.assign().unwrap();
        assert_eq!(seg.piece_id, 0);
        // Worker failed, reclaim the piece
        sched.reclaim(0);

        // Should be able to assign piece 0 again
        let seg = sched.assign().unwrap();
        assert_eq!(seg.piece_id, 0);
    }

    #[test]
    fn test_scheduler_all_done() {
        let pm = PieceMap::new(2_000_000, 1_000_000);
        let mut sched = SchedulerState::new(pm);

        let seg0 = sched.assign().unwrap();
        let seg1 = sched.assign().unwrap();
        assert!(sched.assign().is_none());

        sched.complete(seg0.piece_id);
        sched.complete(seg1.piece_id);
        assert!(sched.all_done());
    }

    #[test]
    fn test_scheduler_inflight_exclusion() {
        let pm = PieceMap::new(3_000_000, 1_000_000);
        let mut sched = SchedulerState::new(pm);

        let seg0 = sched.assign().unwrap();
        assert_eq!(seg0.piece_id, 0);
        let seg1 = sched.assign().unwrap();
        assert_eq!(seg1.piece_id, 1);
        let seg2 = sched.assign().unwrap();
        assert_eq!(seg2.piece_id, 2);
        assert!(sched.assign().is_none());
    }

    #[test]
    fn test_scheduler_reuses_reclaimed_lower_piece() {
        let pm = PieceMap::new(4_000_000, 1_000_000);
        let mut sched = SchedulerState::new(pm);

        let seg0 = sched.assign().unwrap();
        let _seg1 = sched.assign().unwrap();
        let _seg2 = sched.assign().unwrap();

        sched.reclaim(seg0.piece_id);
        let reassigned = sched.assign().unwrap();
        assert_eq!(reassigned.piece_id, 0);
    }

    #[test]
    fn test_scheduler_snapshot_bitset() {
        let pm = PieceMap::new(3_000_000, 1_000_000);
        let mut sched = SchedulerState::new(pm);

        sched.assign().unwrap();
        sched.complete(0);

        let bitset = sched.snapshot_bitset();
        assert!(!bitset.is_empty());
    }
}
