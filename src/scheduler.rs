use std::sync::Arc;
use std::{cmp, collections::BTreeMap};

use bitvec::prelude::*;
use parking_lot::Mutex;

use crate::storage::control::ControlHints;
use crate::storage::piece_map::PieceMap;
use crate::storage::segment::{LeaseKey, Segment};

/// Shared scheduler handle.
pub(crate) type Scheduler = Arc<Mutex<SchedulerState>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ByteRange {
    start: u64,
    end: u64,
}

impl ByteRange {
    fn new(start: u64, end: u64) -> Option<Self> {
        (start < end).then_some(Self { start, end })
    }
}

#[derive(Debug, Clone, Default)]
struct RangeSet {
    ranges: Vec<ByteRange>,
}

impl RangeSet {
    fn new_full(start: u64, end: u64) -> Self {
        let mut set = Self::default();
        if let Some(range) = ByteRange::new(start, end) {
            set.ranges.push(range);
        }
        set
    }

    fn first(&self) -> Option<ByteRange> {
        self.ranges.first().copied()
    }

    fn len(&self) -> usize {
        self.ranges.len()
    }

    fn insert(&mut self, mut range: ByteRange) {
        let mut index = 0;
        while index < self.ranges.len() {
            let current = self.ranges[index];
            if current.end < range.start {
                index += 1;
                continue;
            }
            if range.end < current.start {
                break;
            }

            range.start = cmp::min(range.start, current.start);
            range.end = cmp::max(range.end, current.end);
            self.ranges.remove(index);
        }
        self.ranges.insert(index, range);
    }

    fn remove(&mut self, range: ByteRange) -> bool {
        let Some(index) = self
            .ranges
            .iter()
            .position(|current| current.start <= range.start && current.end >= range.end)
        else {
            return false;
        };

        let current = self.ranges.remove(index);
        let mut insert_at = index;
        if current.start < range.start {
            self.ranges.insert(
                insert_at,
                ByteRange {
                    start: current.start,
                    end: range.start,
                },
            );
            insert_at += 1;
        }
        if range.end < current.end {
            self.ranges.insert(
                insert_at,
                ByteRange {
                    start: range.end,
                    end: current.end,
                },
            );
        }
        true
    }
}

#[derive(Debug, Clone, Copy)]
struct ActiveLeaseState {
    range: ByteRange,
}

#[derive(Debug, Clone)]
struct PieceRuntimeState {
    missing_ranges: RangeSet,
    has_completed_ranges: bool,
    active_leases: BTreeMap<u64, ActiveLeaseState>,
    attempt_counter: u32,
}

impl PieceRuntimeState {
    fn new(full_range: ByteRange) -> Self {
        Self {
            missing_ranges: RangeSet::new_full(full_range.start, full_range.end),
            has_completed_ranges: false,
            active_leases: BTreeMap::new(),
            attempt_counter: 0,
        }
    }

    fn next_assignable_range(&self) -> Option<ByteRange> {
        self.missing_ranges.first()
    }

    fn missing_range_count(&self) -> usize {
        self.missing_ranges.len()
    }

    fn issue_lease(
        &mut self,
        range: ByteRange,
        worker_id: usize,
        next_lease_id: &mut u64,
        piece_id: usize,
    ) -> Option<Segment> {
        if !self.missing_ranges.remove(range) {
            return None;
        }

        let lease_id = *next_lease_id;
        *next_lease_id = next_lease_id.saturating_add(1);
        let attempt = self.attempt_counter.saturating_add(1);
        self.attempt_counter = attempt;
        self.active_leases
            .insert(lease_id, ActiveLeaseState { range });

        Some(Segment {
            piece_id,
            lease_id,
            start: range.start,
            end: range.end,
            owner_worker_id: worker_id,
            attempt,
        })
    }

    fn renew(
        &mut self,
        lease_key: LeaseKey,
        worker_id: usize,
        next_lease_id: &mut u64,
        piece_id: usize,
    ) -> Option<Segment> {
        let previous = self.active_leases.remove(&lease_key.lease_id)?;
        let lease_id = *next_lease_id;
        *next_lease_id = next_lease_id.saturating_add(1);
        let attempt = self.attempt_counter.saturating_add(1);
        self.attempt_counter = attempt;
        self.active_leases.insert(
            lease_id,
            ActiveLeaseState {
                range: previous.range,
            },
        );

        Some(Segment {
            piece_id,
            lease_id,
            start: previous.range.start,
            end: previous.range.end,
            owner_worker_id: worker_id,
            attempt,
        })
    }

    fn complete(&mut self, lease_key: LeaseKey) -> Option<bool> {
        self.active_leases.remove(&lease_key.lease_id)?;
        self.has_completed_ranges = true;
        // Issuing removes missing bytes and reclaiming puts them back. Once both
        // sets are empty, every byte has been acknowledged exactly once.
        Some(self.missing_ranges.ranges.is_empty() && self.active_leases.is_empty())
    }

    fn reclaim(&mut self, lease_key: LeaseKey) -> bool {
        let Some(lease) = self.active_leases.remove(&lease_key.lease_id) else {
            return false;
        };
        self.missing_ranges.insert(lease.range);
        true
    }
}

/// Manages piece assignment, completion, and reclamation.
pub(crate) struct SchedulerState {
    piece_map: PieceMap,
    // Untouched pieces need only their completion/availability bits. Retain
    // detailed state after reclaim to preserve the per-piece attempt counter.
    pieces: BTreeMap<usize, PieceRuntimeState>,
    available_pieces: BitVec<u8, Lsb0>,
    active_lease_count: usize,
    available_range_count: usize,
    next_candidate: usize,
    next_lease_id: u64,
    snapshot_seq: u64,
}

impl SchedulerState {
    pub fn new(piece_map: PieceMap) -> Self {
        let next_candidate = piece_map.first_missing().unwrap_or(0);
        let available_pieces = piece_map.missing_bitset();
        let available_range_count = piece_map.remaining_count();
        Self {
            piece_map,
            pieces: BTreeMap::new(),
            available_pieces,
            active_lease_count: 0,
            available_range_count,
            next_candidate,
            next_lease_id: 1,
            snapshot_seq: 0,
        }
    }

    /// Assign the next available piece. Returns `None` when there is no more work.
    pub fn assign(&mut self) -> Option<Segment> {
        self.assign_to(0)
    }

    /// Assign the next available piece to a specific worker.
    pub fn assign_to(&mut self, worker_id: usize) -> Option<Segment> {
        self.assign_to_with_split(worker_id, 1, u64::MAX)
    }

    pub fn assign_to_with_split(
        &mut self,
        worker_id: usize,
        max_active_leases: usize,
        min_segment_size: u64,
    ) -> Option<Segment> {
        if self.available_range_count == 0 {
            return None;
        }

        let piece_id = self.available_pieces[self.next_candidate..]
            .first_one()
            .map(|offset| self.next_candidate + offset)
            .or_else(|| self.available_pieces[..self.next_candidate].first_one())?;
        let range = match self.pieces.get(&piece_id) {
            Some(piece) => piece.next_assignable_range()?,
            None => {
                let (start, end) = self.piece_map.piece_range(piece_id);
                ByteRange { start, end }
            }
        };
        let range = self.assignment_range_for(range, max_active_leases, min_segment_size);
        let segment = self.issue_lease(piece_id, range, worker_id)?;
        self.next_candidate = (piece_id + 1) % self.piece_map.piece_count();
        Some(segment)
    }

    fn issue_lease(
        &mut self,
        piece_id: usize,
        range: ByteRange,
        worker_id: usize,
    ) -> Option<Segment> {
        let (start, end) = self.piece_map.piece_range(piece_id);
        let piece = self
            .pieces
            .entry(piece_id)
            .or_insert_with(|| PieceRuntimeState::new(ByteRange { start, end }));
        let previous_ranges = piece.missing_range_count();
        let segment = piece.issue_lease(range, worker_id, &mut self.next_lease_id, piece_id)?;
        self.available_range_count =
            self.available_range_count - previous_ranges + piece.missing_range_count();
        self.active_lease_count += 1;
        self.available_pieces
            .set(piece_id, piece.missing_range_count() != 0);
        Some(segment)
    }

    #[allow(dead_code)]
    pub fn assign_subrange(
        &mut self,
        piece_id: usize,
        start: u64,
        end: u64,
        worker_id: usize,
    ) -> Option<Segment> {
        if piece_id >= self.piece_map.piece_count() || self.piece_map.is_complete(piece_id) {
            return None;
        }
        let range = ByteRange::new(start, end)?;
        let (piece_start, piece_end) = self.piece_map.piece_range(piece_id);
        if start < piece_start || end > piece_end {
            return None;
        }
        self.issue_lease(piece_id, range, worker_id)
    }

    /// Replace an active lease with a fresh lease identity for the same piece.
    pub fn renew(&mut self, lease_key: LeaseKey, worker_id: usize) -> Option<Segment> {
        if self.piece_map.is_complete(lease_key.piece_id) {
            return None;
        }

        self.pieces.get_mut(&lease_key.piece_id)?.renew(
            lease_key,
            worker_id,
            &mut self.next_lease_id,
            lease_key.piece_id,
        )
    }

    /// Mark a piece as completed and remove it from inflight.
    pub fn complete(&mut self, lease_key: LeaseKey) -> bool {
        let Some(piece_state) = self.pieces.get_mut(&lease_key.piece_id) else {
            return false;
        };

        let Some(piece_complete) = piece_state.complete(lease_key) else {
            return false;
        };
        self.active_lease_count -= 1;
        if piece_complete {
            self.piece_map.mark_complete(lease_key.piece_id);
            self.pieces.remove(&lease_key.piece_id);
        }
        true
    }

    /// Reclaim a piece (worker failed); it becomes available for reassignment.
    pub fn reclaim(&mut self, lease_key: LeaseKey) -> bool {
        let Some(piece_state) = self.pieces.get_mut(&lease_key.piece_id) else {
            return false;
        };
        let previous_ranges = piece_state.missing_range_count();
        if !piece_state.reclaim(lease_key) {
            return false;
        }
        self.active_lease_count -= 1;
        self.available_range_count =
            self.available_range_count - previous_ranges + piece_state.missing_range_count();
        self.available_pieces.set(lease_key.piece_id, true);
        if lease_key.piece_id < self.next_candidate {
            self.next_candidate = lease_key.piece_id;
        }
        true
    }

    pub fn has_available(&self) -> bool {
        self.available_range_count != 0
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

    pub fn control_hints(&mut self) -> ControlHints {
        self.snapshot_seq = self.snapshot_seq.saturating_add(1);
        let mut hints = ControlHints {
            snapshot_seq: self.snapshot_seq,
            ..ControlHints::default()
        };
        for (&piece_id, piece) in &self.pieces {
            if !piece.active_leases.is_empty() {
                hints.inflight_piece_ids.push(piece_id);
            }
            if !piece.active_leases.is_empty() || piece.has_completed_ranges {
                hints.dirty_piece_ids.push(piece_id);
            }
        }
        hints
    }

    fn assignment_range_for(
        &self,
        range: ByteRange,
        max_active_leases: usize,
        min_segment_size: u64,
    ) -> ByteRange {
        if max_active_leases <= 1 || min_segment_size == 0 {
            return range;
        }

        let total_work_units = self.active_lease_count + self.available_range_count;
        if total_work_units >= max_active_leases {
            return range;
        }

        let range_len = range.end - range.start;
        if range_len <= min_segment_size {
            return range;
        }

        let desired_units_from_range = max_active_leases.saturating_sub(total_work_units) + 1;
        if desired_units_from_range <= 1 {
            return range;
        }

        let desired_units_from_range = desired_units_from_range as u64;
        let target_len = range_len.div_ceil(desired_units_from_range);
        let segment_len = target_len.max(min_segment_size);

        if segment_len >= range_len || range_len - segment_len < min_segment_size {
            return range;
        }

        ByteRange {
            start: range.start,
            end: range.start + segment_len,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::piece_map::PieceMap;

    #[test]
    fn test_runtime_state_is_sparse_and_released_on_completion() {
        let mut sched = SchedulerState::new(PieceMap::new(100_000_000, 1_000));
        assert!(sched.pieces.is_empty());
        assert_eq!(sched.available_range_count, 100_000);

        let segment = sched.assign_to_with_split(0, 4, 250).unwrap();
        assert_eq!(sched.pieces.len(), 1);
        assert_eq!(sched.active_lease_count, 1);
        assert_eq!(sched.available_range_count, 99_999);
        assert!(sched.complete(segment.lease_key()));
        assert!(sched.pieces.is_empty());
        assert_eq!(sched.active_lease_count, 0);
        assert!(sched.control_hints().dirty_piece_ids.is_empty());
    }

    #[test]
    fn test_restored_pieces_need_no_runtime_state_and_skip_completed_bits() {
        // Only pieces 1 and 8 are missing; ignore padding bits beyond piece 8.
        let pm = PieceMap::from_bitset(8_500, 1_000, &[0b1111_1101, 0b1111_1110], 9);
        let mut sched = SchedulerState::new(pm);
        assert!(sched.pieces.is_empty());
        assert_eq!(sched.available_range_count, 2);
        assert!(sched.control_hints().dirty_piece_ids.is_empty());

        let first = sched.assign().unwrap();
        let last = sched.assign().unwrap();
        assert_eq!(first.piece_id, 1);
        assert_eq!((last.piece_id, last.start, last.end), (8, 8_000, 8_500));
        assert!(sched.assign().is_none());
        assert!(sched.complete(last.lease_key()));
        assert!(sched.complete(first.lease_key()));
        assert!(sched.all_done());
        assert_eq!(sched.completed_bytes(), 8_500);
        assert!(sched.pieces.is_empty());
    }

    #[test]
    fn test_fragmentation_reclaim_renewal_and_split_preserve_work_counts() {
        let mut sched = SchedulerState::new(PieceMap::new(1_200, 1_000));
        let middle = sched.assign_subrange(0, 250, 750, 0).unwrap();
        assert_eq!(sched.available_range_count, 3);
        let tail = sched.assign_subrange(1, 1_000, 1_200, 1).unwrap();
        assert_eq!(
            (sched.active_lease_count, sched.available_range_count),
            (2, 2)
        );

        // Reclaim joins both missing sides into a single range.
        assert!(sched.reclaim(middle.lease_key()));
        assert_eq!(
            (sched.active_lease_count, sched.available_range_count),
            (1, 1)
        );
        let tail = sched.renew(tail.lease_key(), 2).unwrap();
        assert_eq!(tail.attempt, 2);
        assert!(!sched.reclaim(middle.lease_key()));
        assert!(!sched.complete(middle.lease_key()));
        assert!(sched.renew(middle.lease_key(), 0).is_none());
        assert!(sched.assign_subrange(1, 1_050, 1_150, 0).is_none());

        // The current work counts leave room for three pieces of the 1,000-byte range.
        let first = sched.assign_to_with_split(0, 4, 250).unwrap();
        let second = sched.assign_to_with_split(1, 4, 250).unwrap();
        let third = sched.assign_to_with_split(2, 4, 250).unwrap();
        assert_eq!((first.start, first.end, first.attempt), (0, 334, 2));
        assert_eq!((second.start, second.end, second.attempt), (334, 667, 3));
        assert_eq!((third.start, third.end, third.attempt), (667, 1_000, 4));
        assert_eq!(
            (sched.active_lease_count, sched.available_range_count),
            (4, 0)
        );
        assert!(sched.assign_to_with_split(4, 8, 100).is_none());

        assert!(sched.complete(tail.lease_key()));
        assert!(sched.complete(second.lease_key()));
        assert!(sched.complete(third.lease_key()));
        assert_eq!(sched.completed_bytes(), 200);
        assert_eq!(sched.control_hints().dirty_piece_ids, vec![0]);
        assert!(sched.complete(first.lease_key()));
        assert_eq!(sched.completed_bytes(), 1_200);
        assert!(sched.all_done());
        assert!(sched.pieces.is_empty());
        assert_eq!(
            (sched.active_lease_count, sched.available_range_count),
            (0, 0)
        );
    }

    #[test]
    fn test_invalid_subranges_do_not_create_runtime_state() {
        let mut sched = SchedulerState::new(PieceMap::new(2_000, 1_000));
        assert!(sched.assign_subrange(1, 900, 1_100, 0).is_none());
        assert!(sched.assign_subrange(0, 900, 1_100, 0).is_none());
        assert!(sched.assign_subrange(0, 100, 100, 0).is_none());
        assert!(sched.assign_subrange(99, 0, 100, 0).is_none());
        assert!(sched.pieces.is_empty());
        assert_eq!(sched.available_range_count, 2);
    }

    #[test]
    fn test_empty_and_restored_complete_schedulers_have_no_work() {
        for pm in [
            PieceMap::new(0, 1_000),
            PieceMap::from_bitset(1_500, 1_000, &[0b0000_0011], 2),
        ] {
            let mut sched = SchedulerState::new(pm);
            assert!(sched.all_done());
            assert!(sched.assign_to_with_split(0, 4, 100).is_none());
            assert!(sched.pieces.is_empty());
            assert_eq!(sched.available_range_count, 0);
        }
    }

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
        assert_eq!(seg.owner_worker_id, 0);
        assert_eq!(seg.attempt, 1);
        assert_eq!(seg.start, 0);
        assert_eq!(seg.end, 1_000_000);

        assert!(sched.complete(seg.lease_key()));
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
        assert!(sched.reclaim(seg.lease_key()));

        // Should be able to assign piece 0 again
        let seg = sched.assign().unwrap();
        assert_eq!(seg.piece_id, 0);
        assert_eq!(seg.attempt, 2);
    }

    #[test]
    fn test_scheduler_all_done() {
        let pm = PieceMap::new(2_000_000, 1_000_000);
        let mut sched = SchedulerState::new(pm);

        let seg0 = sched.assign().unwrap();
        let seg1 = sched.assign().unwrap();
        assert!(sched.assign().is_none());

        assert!(sched.complete(seg0.lease_key()));
        assert!(sched.complete(seg1.lease_key()));
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

        assert!(sched.reclaim(seg0.lease_key()));
        let reassigned = sched.assign().unwrap();
        assert_eq!(reassigned.piece_id, 0);
    }

    #[test]
    fn test_scheduler_snapshot_bitset() {
        let pm = PieceMap::new(3_000_000, 1_000_000);
        let mut sched = SchedulerState::new(pm);

        let segment = sched.assign().unwrap();
        assert!(sched.complete(segment.lease_key()));

        let bitset = sched.snapshot_bitset();
        assert!(!bitset.is_empty());
    }

    #[test]
    fn test_scheduler_rejects_stale_lease_completion() {
        let pm = PieceMap::new(2_000_000, 1_000_000);
        let mut sched = SchedulerState::new(pm);

        let first = sched.assign_to(7).unwrap();
        let renewed = sched.renew(first.lease_key(), 7).unwrap();

        assert!(!sched.complete(first.lease_key()));
        assert!(sched.complete(renewed.lease_key()));
    }

    #[test]
    fn test_scheduler_partial_subranges_do_not_complete_piece_early() {
        let pm = PieceMap::new(1_000, 1_000);
        let mut sched = SchedulerState::new(pm);

        let first = sched.assign_subrange(0, 0, 500, 1).unwrap();
        let second = sched.assign_subrange(0, 500, 1_000, 2).unwrap();

        assert!(sched.complete(first.lease_key()));
        assert!(!sched.all_done());
        assert_eq!(sched.completed_bytes(), 0);

        assert!(sched.complete(second.lease_key()));
        assert!(sched.all_done());
        assert_eq!(sched.completed_bytes(), 1_000);
    }

    #[test]
    fn test_scheduler_reclaims_only_failed_subrange() {
        let pm = PieceMap::new(1_000, 1_000);
        let mut sched = SchedulerState::new(pm);

        let first = sched.assign_subrange(0, 0, 500, 1).unwrap();
        let second = sched.assign_subrange(0, 500, 1_000, 2).unwrap();

        assert!(sched.reclaim(first.lease_key()));
        let reassigned = sched.assign().unwrap();

        assert_eq!(second.start, 500);
        assert_eq!(reassigned.start, 0);
        assert_eq!(reassigned.end, 500);
    }

    #[test]
    fn test_range_set_merges_overlapping_ranges() {
        let mut ranges = RangeSet::default();
        ranges.insert(ByteRange::new(0, 600).unwrap());
        ranges.insert(ByteRange::new(400, 1_000).unwrap());

        assert_eq!(ranges.ranges.len(), 1);
        assert_eq!(ranges.ranges[0], ByteRange::new(0, 1_000).unwrap());
    }

    #[test]
    fn test_range_set_insert_handles_non_overlapping_positions() {
        let mut ranges = RangeSet::default();
        ranges.insert(ByteRange::new(10, 20).unwrap());
        ranges.insert(ByteRange::new(30, 40).unwrap());
        ranges.insert(ByteRange::new(0, 5).unwrap());

        assert_eq!(
            ranges.ranges,
            vec![
                ByteRange::new(0, 5).unwrap(),
                ByteRange::new(10, 20).unwrap(),
                ByteRange::new(30, 40).unwrap(),
            ]
        );
    }

    #[test]
    fn test_range_set_remove_rejects_missing_range_and_splits_existing_range() {
        let mut missing = RangeSet::new_full(0, 100);
        assert!(!missing.remove(ByteRange::new(150, 200).unwrap()));

        let mut split = RangeSet::new_full(0, 100);
        assert!(split.remove(ByteRange::new(25, 75).unwrap()));
        assert_eq!(
            split.ranges,
            vec![
                ByteRange::new(0, 25).unwrap(),
                ByteRange::new(75, 100).unwrap()
            ]
        );
    }

    #[test]
    fn test_scheduler_invalid_lease_operations_return_false_or_none() {
        let pm = PieceMap::new(1_000, 1_000);
        let mut sched = SchedulerState::new(pm);

        assert!(sched.assign_subrange(99, 0, 100, 1).is_none());

        let lease = LeaseKey {
            piece_id: 0,
            lease_id: 999,
        };
        assert!(!sched.complete(lease));
        assert!(!sched.reclaim(lease));
    }

    #[test]
    fn test_scheduler_rejects_completed_piece_subrange_and_renewal() {
        let pm = PieceMap::new(1_000, 1_000);
        let mut sched = SchedulerState::new(pm);

        let segment = sched.assign().unwrap();
        assert!(sched.complete(segment.lease_key()));
        assert!(sched.assign_subrange(0, 0, 100, 1).is_none());
        assert!(sched.renew(segment.lease_key(), 1).is_none());
    }

    #[test]
    fn test_scheduler_control_hints_track_dirty_and_inflight_pieces() {
        let pm = PieceMap::new(1_000, 500);
        let mut sched = SchedulerState::new(pm);

        let first = sched.assign_subrange(0, 0, 250, 1).unwrap();
        let second = sched.assign_subrange(0, 250, 500, 2).unwrap();
        assert!(sched.complete(first.lease_key()));

        let hints = sched.control_hints();
        assert_eq!(hints.dirty_piece_ids, vec![0]);
        assert_eq!(hints.inflight_piece_ids, vec![0]);
        assert_eq!(hints.snapshot_seq, 1);

        assert!(sched.reclaim(second.lease_key()));
        let next_hints = sched.control_hints();
        assert_eq!(next_hints.dirty_piece_ids, vec![0]);
        assert!(next_hints.inflight_piece_ids.is_empty());
        assert_eq!(next_hints.snapshot_seq, 2);
    }

    #[test]
    fn test_scheduler_splits_large_missing_range_when_workers_exceed_work_units() {
        let pm = PieceMap::new(1_024, 1_024);
        let mut sched = SchedulerState::new(pm);

        let first = sched.assign_to_with_split(0, 4, 256).unwrap();
        let second = sched.assign_to_with_split(1, 4, 256).unwrap();
        let third = sched.assign_to_with_split(2, 4, 256).unwrap();
        let fourth = sched.assign_to_with_split(3, 4, 256).unwrap();

        assert_eq!((first.start, first.end), (0, 256));
        assert_eq!((second.start, second.end), (256, 512));
        assert_eq!((third.start, third.end), (512, 768));
        assert_eq!((fourth.start, fourth.end), (768, 1_024));
        assert!(sched.assign_to_with_split(4, 4, 256).is_none());
    }

    #[test]
    fn test_scheduler_keeps_full_piece_when_remaining_tail_is_too_small_to_split() {
        let pm = PieceMap::new(900, 900);
        let mut sched = SchedulerState::new(pm);

        let first = sched.assign_to_with_split(0, 2, 512).unwrap();

        assert_eq!((first.start, first.end), (0, 900));
        assert!(sched.assign_to_with_split(1, 2, 512).is_none());
    }
}
