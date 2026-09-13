use std::sync::Arc;
use std::{cmp, collections::BTreeMap};

use bitvec::prelude::*;
use parking_lot::Mutex;

use crate::config::RangeSchedulingMode;
use crate::storage::control::ControlHints;
use crate::storage::piece_map::PieceMap;
use crate::storage::segment::{LeaseKey, Segment};

/// Shared scheduler handle.
pub(crate) type Scheduler = Arc<Mutex<SchedulerState>>;
/// Maximum number of piece leases that one HTTP request may own.
pub(crate) const MAX_REQUEST_LEASES: usize = 64;

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

/// The leases issued for one ordinary HTTP request.
///
/// A request owns one slot, while the returned vector may contain several
/// piece leases. Keeping this result together makes the scheduler decision
/// atomic: no other worker can claim the gap between the first lease and the
/// rest of the request.
#[derive(Debug)]
pub(crate) struct RequestAssignment {
    pub(crate) segments: Vec<Segment>,
    pub(crate) candidate_start: u64,
    pub(crate) candidate_end: u64,
    pub(crate) candidate_boundary_scanned: bool,
    pub(crate) candidate_slots: usize,
    pub(crate) target_end: u64,
    pub(crate) candidate_shares: Option<Vec<DynamicCandidateShare>>,
    pub(crate) final_start: u64,
    pub(crate) final_end: u64,
    pub(crate) available_request_slots: usize,
    pub(crate) occupied_requests: usize,
    pub(crate) planned_ranges: usize,
    pub(crate) truncation_reason: &'static str,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct DynamicCandidateShare {
    pub(crate) start: u64,
    pub(crate) end: u64,
    pub(crate) slots: usize,
    pub(crate) share_bytes: u64,
}

impl RequestAssignment {
    pub(crate) fn single(
        segment: Segment,
        available_request_slots: usize,
        occupied_requests: usize,
    ) -> Self {
        Self {
            candidate_start: segment.start,
            candidate_end: segment.end,
            candidate_boundary_scanned: false,
            candidate_slots: 1,
            target_end: segment.end,
            candidate_shares: None,
            final_start: segment.start,
            final_end: segment.end,
            segments: vec![segment],
            available_request_slots,
            occupied_requests,
            planned_ranges: 1,
            truncation_reason: "none",
        }
    }

    pub(crate) fn from_segments(
        segments: Vec<Segment>,
        available_request_slots: usize,
        occupied_requests: usize,
        truncation_reason: &'static str,
    ) -> Option<Self> {
        let first = segments.first()?;
        let last = segments.last()?;
        Some(Self {
            candidate_start: first.start,
            candidate_end: last.end,
            candidate_boundary_scanned: false,
            candidate_slots: 1,
            target_end: last.end,
            candidate_shares: None,
            final_start: first.start,
            final_end: last.end,
            segments,
            available_request_slots,
            occupied_requests,
            planned_ranges: 1,
            truncation_reason,
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct AllocationCandidate {
    start: u64,
    end: u64,
    first_piece: usize,
    end_piece: usize,
}

#[derive(Debug)]
struct DynamicPlan {
    candidate: AllocationCandidate,
    candidate_slots: usize,
    planned_ranges: usize,
    candidate_shares: Option<Vec<DynamicCandidateShare>>,
}

impl AllocationCandidate {
    fn len(self) -> u64 {
        self.end.saturating_sub(self.start)
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
    stopped: bool,
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
            stopped: false,
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

    /// Assign using occupied HTTP requests rather than reserved piece leases
    /// when deciding whether to split work for spare request slots.
    /// `occupied_requests` excludes the request currently seeking an assignment.
    #[allow(dead_code)]
    pub fn assign_to_with_request_split(
        &mut self,
        worker_id: usize,
        max_requests: usize,
        min_segment_size: u64,
        occupied_requests: usize,
    ) -> Option<Segment> {
        let available_request_slots = max_requests.saturating_sub(occupied_requests);
        self.assign_to_with_split(
            worker_id,
            self.active_lease_count
                .saturating_add(available_request_slots),
            min_segment_size,
        )
    }

    /// Atomically assign all piece leases that will be consumed by one new
    /// ordinary HTTP request.
    ///
    /// `occupied_requests` counts requests that already hold a slot and does
    /// not count the request being created. The caller must reserve that slot
    /// before entering the scheduler lock. The scheduler never waits for a
    /// permit or performs I/O while constructing this result.
    #[allow(dead_code)]
    #[allow(clippy::too_many_arguments)]
    pub fn assign_request(
        &mut self,
        worker_id: usize,
        max_requests: usize,
        min_segment_size: u64,
        occupied_requests: usize,
        mode: RangeSchedulingMode,
        request_batch_size: u64,
        dynamic_min_split_size: u64,
        dynamic_max_request_size: u64,
        allow_batch: bool,
    ) -> Option<RequestAssignment> {
        self.assign_request_with_diagnostics(
            worker_id,
            max_requests,
            min_segment_size,
            occupied_requests,
            mode,
            request_batch_size,
            dynamic_min_split_size,
            dynamic_max_request_size,
            allow_batch,
            true,
        )
    }

    /// Atomically assign a request, optionally collecting the full fixed-mode
    /// candidate boundary used only by debug diagnostics. The ordinary path
    /// does not need that boundary to issue its bounded request.
    #[allow(clippy::too_many_arguments)]
    pub fn assign_request_with_diagnostics(
        &mut self,
        worker_id: usize,
        max_requests: usize,
        min_segment_size: u64,
        occupied_requests: usize,
        mode: RangeSchedulingMode,
        request_batch_size: u64,
        dynamic_min_split_size: u64,
        dynamic_max_request_size: u64,
        allow_batch: bool,
        collect_diagnostics: bool,
    ) -> Option<RequestAssignment> {
        self.assign_request_with_trace(
            worker_id,
            max_requests,
            min_segment_size,
            occupied_requests,
            mode,
            request_batch_size,
            dynamic_min_split_size,
            dynamic_max_request_size,
            allow_batch,
            collect_diagnostics,
            false,
        )
    }

    /// Variant of [`Self::assign_request_with_diagnostics`] that also retains
    /// the complete dynamic candidate-share table for TRACE logging.
    #[allow(clippy::too_many_arguments)]
    pub fn assign_request_with_trace(
        &mut self,
        worker_id: usize,
        max_requests: usize,
        min_segment_size: u64,
        occupied_requests: usize,
        mode: RangeSchedulingMode,
        request_batch_size: u64,
        dynamic_min_split_size: u64,
        dynamic_max_request_size: u64,
        allow_batch: bool,
        collect_diagnostics: bool,
        collect_candidate_shares: bool,
    ) -> Option<RequestAssignment> {
        let available_request_slots = max_requests.saturating_sub(occupied_requests);
        if available_request_slots == 0 || self.available_range_count == 0 {
            return None;
        }

        // A fresh probe must never be made part of a newly planned multi-piece
        // request. It covers the first whole piece exactly, so all modes use
        // the exact single-lease path here. Fixed mode retains its legacy
        // underutilized-piece split for ordinary requests without a probe.
        // Recovery assignments use their own lineage path and do not call
        // this method.
        if !allow_batch {
            let segment = self.assign_to(worker_id)?;
            return Some(RequestAssignment::single(
                segment,
                available_request_slots,
                occupied_requests,
            ));
        }

        match mode {
            RangeSchedulingMode::Fixed => self.assign_fixed_request(
                worker_id,
                max_requests,
                min_segment_size,
                occupied_requests,
                request_batch_size,
                available_request_slots,
                collect_diagnostics,
            ),
            RangeSchedulingMode::Dynamic => self.assign_dynamic_request(
                worker_id,
                occupied_requests,
                available_request_slots,
                dynamic_min_split_size,
                dynamic_max_request_size,
                collect_candidate_shares,
            ),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn assign_fixed_request(
        &mut self,
        worker_id: usize,
        max_requests: usize,
        min_segment_size: u64,
        occupied_requests: usize,
        request_batch_size: u64,
        available_request_slots: usize,
        collect_diagnostics: bool,
    ) -> Option<RequestAssignment> {
        let max_active_leases = self
            .active_lease_count
            .saturating_add(available_request_slots);
        let first = self.assign_to_with_split(worker_id, max_active_leases, min_segment_size)?;
        let batch_allowed = first.attempt == 1;
        let scan_candidate_boundary = collect_diagnostics
            && batch_allowed
            && request_batch_size > first.end.saturating_sub(first.start)
            && self.is_fresh_whole_lease(&first);
        let candidate_end = if scan_candidate_boundary {
            self.fresh_run_end(first.piece_id)
        } else {
            first.end
        };
        let mut segments = vec![first];
        let (additional, truncation_reason) = if batch_allowed {
            self.extend_batch_with_occupied(
                &segments[0],
                worker_id,
                max_requests,
                occupied_requests,
                request_batch_size,
            )
        } else {
            (Vec::new(), "recovered_or_retried")
        };
        segments.extend(additional);
        let final_end = segments.last().map_or(0, |segment| segment.end);
        Some(RequestAssignment {
            final_start: segments[0].start,
            final_end,
            candidate_start: segments[0].start,
            candidate_end,
            candidate_boundary_scanned: scan_candidate_boundary,
            candidate_slots: 1,
            target_end: candidate_end,
            candidate_shares: None,
            available_request_slots,
            occupied_requests,
            planned_ranges: available_request_slots
                .min(self.available_range_count + segments.len()),
            truncation_reason,
            segments,
        })
    }

    fn assign_dynamic_request(
        &mut self,
        worker_id: usize,
        occupied_requests: usize,
        available_request_slots: usize,
        dynamic_min_split_size: u64,
        dynamic_max_request_size: u64,
        collect_candidate_shares: bool,
    ) -> Option<RequestAssignment> {
        let candidates = self.dynamic_candidates();
        if candidates.is_empty() {
            // Touched pieces (including recovered suffixes and fragmented
            // holes) stay on the exact single-lease path. This keeps the
            // dynamic planner's invariants limited to complete piece runs,
            // while still allowing the download to finish when no untouched
            // run remains.
            return self.assign_to(worker_id).map(|segment| {
                RequestAssignment::single(segment, available_request_slots, occupied_requests)
            });
        }

        let min_split_size = self.aligned_dynamic_min_split_size(dynamic_min_split_size);
        let plan = self.plan_dynamic_share(
            candidates,
            available_request_slots,
            min_split_size,
            collect_candidate_shares,
        )?;
        let desired_end_piece =
            self.dynamic_share_end_piece(plan.candidate, plan.candidate_slots, min_split_size);
        let target_end = self.piece_map.piece_range(desired_end_piece - 1).1;
        let (final_end, truncation_reason) = self.dynamic_request_end(
            plan.candidate,
            desired_end_piece,
            min_split_size,
            dynamic_max_request_size,
        );
        let segments = self.issue_dynamic_candidate(plan.candidate, final_end, worker_id)?;
        let final_start = segments.first()?.start;
        let final_end = segments.last()?.end;

        Some(RequestAssignment {
            segments,
            candidate_start: plan.candidate.start,
            candidate_end: plan.candidate.end,
            candidate_boundary_scanned: true,
            candidate_slots: plan.candidate_slots,
            target_end,
            candidate_shares: plan.candidate_shares,
            final_start,
            final_end,
            available_request_slots,
            occupied_requests,
            planned_ranges: plan.planned_ranges,
            truncation_reason,
        })
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
        if self.stopped {
            return None;
        }
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

    fn is_untouched_piece(&self, piece_id: usize) -> bool {
        piece_id < self.piece_map.piece_count()
            && self.available_pieces[piece_id]
            && !self.piece_map.is_complete(piece_id)
            && !self.pieces.contains_key(&piece_id)
    }

    fn is_fresh_whole_lease(&self, segment: &Segment) -> bool {
        if self.piece_map.piece_range(segment.piece_id) != (segment.start, segment.end) {
            return false;
        }
        let Some(piece) = self.pieces.get(&segment.piece_id) else {
            return false;
        };
        piece.active_leases.len() == 1
            && !piece.has_completed_ranges
            && piece
                .active_leases
                .get(&segment.lease_id)
                .is_some_and(|lease| {
                    lease.range
                        == ByteRange {
                            start: segment.start,
                            end: segment.end,
                        }
                })
            && piece.missing_ranges.ranges.is_empty()
    }

    fn fresh_run_end(&self, first_piece: usize) -> u64 {
        let mut end_piece = first_piece + 1;
        while end_piece < self.piece_map.piece_count() && self.is_untouched_piece(end_piece) {
            end_piece += 1;
        }
        self.piece_map.piece_range(end_piece - 1).1
    }

    /// Build independent allocation candidates from untouched complete pieces.
    /// Touched pieces are deliberately left to the exact single-lease path so
    /// recovery suffixes and fragmented holes cannot be merged by the dynamic
    /// planner. The availability bitset finds whole runs in word-sized scans;
    /// only sparse runtime pieces need to be visited to cut those runs apart.
    fn dynamic_candidates(&self) -> Vec<AllocationCandidate> {
        let mut candidates = Vec::new();
        let piece_count = self.piece_map.piece_count();
        let mut search_from = 0;
        while search_from < piece_count {
            let Some(offset) = self.available_pieces[search_from..].first_one() else {
                break;
            };
            let first_available = search_from + offset;
            let available_end = self.available_pieces[first_available..]
                .first_zero()
                .map_or(piece_count, |offset| first_available + offset);

            let mut first_piece = first_available;
            for &touched_piece in self
                .pieces
                .range(first_available..available_end)
                .map(|(piece_id, _)| piece_id)
            {
                if first_piece < touched_piece {
                    let (start, _) = self.piece_map.piece_range(first_piece);
                    let (_, end) = self.piece_map.piece_range(touched_piece - 1);
                    candidates.push(AllocationCandidate {
                        start,
                        end,
                        first_piece,
                        end_piece: touched_piece,
                    });
                }
                first_piece = touched_piece.saturating_add(1);
            }
            if first_piece < available_end {
                let (start, _) = self.piece_map.piece_range(first_piece);
                let (_, end) = self.piece_map.piece_range(available_end - 1);
                candidates.push(AllocationCandidate {
                    start,
                    end,
                    first_piece,
                    end_piece: available_end,
                });
            }
            search_from = available_end;
        }
        candidates
    }

    fn aligned_dynamic_min_split_size(&self, configured: u64) -> u64 {
        crate::config::effective_dynamic_min_split_size(self.piece_map.piece_size(), configured)
    }

    fn dynamic_legal_split_bounds(
        &self,
        candidate: AllocationCandidate,
        min_split_size: u64,
    ) -> Option<(usize, usize)> {
        if candidate.end_piece <= candidate.first_piece + 1
            || min_split_size == 0
            || candidate.len() / min_split_size < 2
        {
            return None;
        }

        let piece_size = self.piece_map.piece_size().max(1);
        let min_pieces = usize::try_from(min_split_size.div_ceil(piece_size)).ok()?;
        let first_legal_piece = candidate.first_piece.checked_add(min_pieces)?;
        let last_legal_piece =
            usize::try_from(candidate.end.saturating_sub(min_split_size) / piece_size)
                .ok()?
                .min(candidate.end_piece.saturating_sub(1));
        (first_legal_piece <= last_legal_piece).then_some((first_legal_piece, last_legal_piece))
    }

    fn max_dynamic_slots(&self, candidate: AllocationCandidate, min_split_size: u64) -> usize {
        let piece_count = candidate.end_piece - candidate.first_piece;
        let by_minimum = if min_split_size == 0 {
            piece_count
        } else {
            usize::try_from(candidate.len() / min_split_size)
                .unwrap_or(usize::MAX)
                .max(1)
        };
        if by_minimum < 2
            || self
                .dynamic_legal_split_bounds(candidate, min_split_size)
                .is_none()
        {
            return 1;
        }
        piece_count.min(by_minimum).max(1)
    }

    fn compare_dynamic_shares(
        left: AllocationCandidate,
        left_slots: usize,
        right: AllocationCandidate,
        right_slots: usize,
    ) -> cmp::Ordering {
        // Compare `left.len() / left_slots` with
        // `right.len() / right_slots` without losing precision.
        let left_share = u128::from(left.len()) * right_slots as u128;
        let right_share = u128::from(right.len()) * left_slots as u128;
        left_share
            .cmp(&right_share)
            .then_with(|| right.start.cmp(&left.start))
    }

    /// Allocate virtual slots to independent ranges, then choose the range
    /// with the largest current share. This avoids materializing a fresh set
    /// of ephemeral splits on every request and keeps a single range close to
    /// `remaining_bytes / available_slots` as requests are issued in order.
    fn plan_dynamic_share(
        &self,
        candidates: Vec<AllocationCandidate>,
        requested_ranges: usize,
        min_split_size: u64,
        collect_candidate_shares: bool,
    ) -> Option<DynamicPlan> {
        if requested_ranges == 0 || candidates.is_empty() {
            return None;
        }

        // When there are at least as many candidates as request slots, every
        // candidate has one nominal slot and only the largest candidate is
        // needed for this request. Do not sort or allocate a slot-count table
        // for all fragmented runs.
        if candidates.len() >= requested_ranges {
            let chosen = (0..candidates.len()).max_by(|&left, &right| {
                Self::compare_dynamic_shares(candidates[left], 1, candidates[right], 1)
            })?;
            let candidate_shares = collect_candidate_shares.then(|| {
                candidates
                    .iter()
                    .map(|candidate| DynamicCandidateShare {
                        start: candidate.start,
                        end: candidate.end,
                        slots: 1,
                        share_bytes: candidate.len(),
                    })
                    .collect()
            });
            return Some(DynamicPlan {
                candidate: candidates[chosen],
                candidate_slots: 1,
                planned_ranges: requested_ranges,
                candidate_shares,
            });
        }

        let mut slot_counts = vec![1usize; candidates.len()];
        let mut extra_slots = requested_ranges.saturating_sub(candidates.len());
        while extra_slots != 0 {
            let index = (0..candidates.len())
                .filter(|&index| {
                    slot_counts[index] < self.max_dynamic_slots(candidates[index], min_split_size)
                })
                .max_by(|&left, &right| {
                    Self::compare_dynamic_shares(
                        candidates[left],
                        slot_counts[left],
                        candidates[right],
                        slot_counts[right],
                    )
                });
            let Some(index) = index else { break };
            slot_counts[index] += 1;
            extra_slots -= 1;
        }

        let chosen = (0..candidates.len()).max_by(|&left, &right| {
            Self::compare_dynamic_shares(
                candidates[left],
                slot_counts[left],
                candidates[right],
                slot_counts[right],
            )
        })?;
        let candidate_shares = collect_candidate_shares.then(|| {
            candidates
                .iter()
                .zip(slot_counts.iter().copied())
                .map(|(candidate, slots)| DynamicCandidateShare {
                    start: candidate.start,
                    end: candidate.end,
                    slots,
                    share_bytes: candidate.len().div_ceil(slots as u64),
                })
                .collect()
        });
        Some(DynamicPlan {
            candidate: candidates[chosen],
            candidate_slots: slot_counts[chosen],
            planned_ranges: slot_counts.iter().sum(),
            candidate_shares,
        })
    }

    fn dynamic_share_end_piece(
        &self,
        candidate: AllocationCandidate,
        slots: usize,
        min_split_size: u64,
    ) -> usize {
        if slots <= 1 {
            return candidate.end_piece;
        }

        let Some((first_legal_piece, last_legal_piece)) =
            self.dynamic_legal_split_bounds(candidate, min_split_size)
        else {
            return candidate.end_piece;
        };
        let piece_size = self.piece_map.piece_size().max(1);
        let target_end = candidate.start + candidate.len().div_ceil(slots as u64);
        let target_piece = target_end / piece_size;
        let target_remainder = target_end % piece_size;
        let ideal_piece = target_piece.saturating_add(if target_remainder > piece_size / 2 {
            1
        } else {
            0
        });
        usize::try_from(ideal_piece)
            .unwrap_or(last_legal_piece)
            .clamp(first_legal_piece, last_legal_piece)
    }

    fn dynamic_request_end(
        &self,
        candidate: AllocationCandidate,
        desired_end_piece: usize,
        min_split_size: u64,
        configured_max_request_size: u64,
    ) -> (u64, &'static str) {
        let piece_size = self.piece_map.piece_size().max(1);
        let max_request_size = configured_max_request_size.max(piece_size).max(1);
        let byte_bound = candidate.start.saturating_add(max_request_size);
        let desired_end_piece = desired_end_piece
            .max(candidate.first_piece.saturating_add(1))
            .min(candidate.end_piece);
        let byte_end_piece = if byte_bound >= candidate.end {
            candidate.end_piece
        } else {
            usize::try_from(byte_bound / piece_size)
                .unwrap_or(candidate.first_piece)
                .max(candidate.first_piece.saturating_add(1))
                .min(candidate.end_piece)
        };
        let lease_end_piece = candidate
            .first_piece
            .saturating_add(MAX_REQUEST_LEASES)
            .min(desired_end_piece);
        let raw_end_piece = desired_end_piece
            .min(byte_end_piece)
            .min(lease_end_piece)
            .max(candidate.first_piece.saturating_add(1))
            .min(candidate.end_piece);
        let mut end_piece = raw_end_piece;
        let mut min_split_adjusted = false;
        let mut min_split_conflict = false;
        if end_piece < candidate.end_piece {
            if let Some((first_legal_piece, last_legal_piece)) =
                self.dynamic_legal_split_bounds(candidate, min_split_size)
            {
                if end_piece >= first_legal_piece {
                    let legal_end_piece = end_piece.min(last_legal_piece);
                    min_split_adjusted = legal_end_piece < end_piece;
                    end_piece = legal_end_piece;
                } else {
                    // A hard byte/lease cap below the first legal boundary
                    // wins; retain the reason so this deliberate exception is
                    // visible in diagnostics.
                    min_split_conflict = true;
                }
            }
        }
        let byte_limited = desired_end_piece > byte_end_piece;
        let lease_limited = desired_end_piece > lease_end_piece;
        let reason = match (byte_limited, lease_limited) {
            (true, true) => match (min_split_adjusted, min_split_conflict) {
                (true, false) => "max_request_bytes_and_lease_limit_and_min_split",
                (false, true) => "max_request_bytes_and_lease_limit_and_min_split_conflict",
                _ => "max_request_bytes_and_lease_limit",
            },
            (true, false) => match (min_split_adjusted, min_split_conflict) {
                (true, false) => "max_request_bytes_and_min_split",
                (false, true) => "max_request_bytes_and_min_split_conflict",
                _ => "max_request_bytes",
            },
            (false, true) => match (min_split_adjusted, min_split_conflict) {
                (true, false) => "lease_limit_and_min_split",
                (false, true) => "lease_limit_and_min_split_conflict",
                _ => "lease_limit",
            },
            (false, false) => match (min_split_adjusted, min_split_conflict) {
                (true, false) => "min_split_boundary",
                (false, true) => "min_split_conflict",
                _ => "none",
            },
        };
        (
            self.piece_map
                .piece_range(end_piece - 1)
                .1
                .min(candidate.end),
            reason,
        )
    }

    fn issue_dynamic_candidate(
        &mut self,
        candidate: AllocationCandidate,
        end: u64,
        worker_id: usize,
    ) -> Option<Vec<Segment>> {
        if self.stopped || end <= candidate.start {
            return None;
        }
        let mut segments: Vec<Segment> = Vec::new();
        for piece_id in candidate.first_piece..candidate.end_piece {
            let (start, piece_end) = self.piece_map.piece_range(piece_id);
            if piece_end > end {
                break;
            }
            let Some(segment) = self.issue_lease(
                piece_id,
                ByteRange {
                    start,
                    end: piece_end,
                },
                worker_id,
            ) else {
                for issued in segments.iter().rev() {
                    self.reclaim(issued.lease_key());
                }
                return None;
            };
            segments.push(segment);
        }
        (!segments.is_empty()).then_some(segments)
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

    pub fn is_whole_piece_segment(&self, segment: &Segment) -> bool {
        segment.piece_id < self.piece_map.piece_count()
            && self.piece_map.piece_range(segment.piece_id) == (segment.start, segment.end)
    }

    /// Atomically issue a contiguous set of already planned recovery leases.
    ///
    /// Recovery keeps whole pieces together so an interrupted multi-piece
    /// request can be submitted again without manufacturing a smaller split
    /// for every queued piece. The caller still owns one request slot; this
    /// method only changes scheduler state and rolls back all leases if any
    /// member is no longer available.
    pub fn assign_recovery_batch(
        &mut self,
        pieces: &[(usize, u64, u64)],
        worker_id: usize,
    ) -> Option<Vec<Segment>> {
        if pieces.is_empty() || pieces.len() > MAX_REQUEST_LEASES {
            return None;
        }

        let mut previous: Option<(usize, u64)> = None;
        for &(piece_id, start, end) in pieces {
            if piece_id >= self.piece_map.piece_count() {
                return None;
            }
            let (piece_start, piece_end) = self.piece_map.piece_range(piece_id);
            if start >= end
                || start < piece_start
                || end > piece_end
                || self.piece_map.is_complete(piece_id)
                || previous.is_some_and(|(previous_piece, previous_end)| {
                    piece_id != previous_piece.saturating_add(1) || start != previous_end
                })
            {
                return None;
            }
            previous = Some((piece_id, end));
        }

        let mut segments: Vec<Segment> = Vec::with_capacity(pieces.len());
        for &(piece_id, start, end) in pieces {
            let Some(segment) = self.issue_lease(piece_id, ByteRange { start, end }, worker_id)
            else {
                for issued in segments.iter().rev() {
                    self.reclaim(issued.lease_key());
                }
                return None;
            };
            segments.push(segment);
        }
        Some(segments)
    }

    /// Reserve contiguous untouched pieces after an existing whole-piece lease.
    /// The returned leases exclude `first`; the byte and 64-lease limits include it.
    /// Leave independent ranges for the other configured request workers, without
    /// treating reserved piece leases as occupied HTTP request slots.
    #[allow(dead_code)]
    pub fn extend_batch(
        &mut self,
        first: &Segment,
        worker_id: usize,
        max_connections: usize,
        byte_cap: u64,
    ) -> Vec<Segment> {
        self.extend_batch_with_occupied(first, worker_id, max_connections, 0, byte_cap)
            .0
    }

    fn extend_batch_with_occupied(
        &mut self,
        first: &Segment,
        worker_id: usize,
        max_connections: usize,
        occupied_requests: usize,
        byte_cap: u64,
    ) -> (Vec<Segment>, &'static str) {
        let mut additional = Vec::new();
        let Some(piece) = self.pieces.get(&first.piece_id) else {
            return (additional, "first_lease_not_found");
        };
        let Some(active) = piece.active_leases.get(&first.lease_id) else {
            return (additional, "first_lease_not_found");
        };
        let (start, end) = self.piece_map.piece_range(first.piece_id);
        if first.owner_worker_id != worker_id
            || (first.start, first.end) != (start, end)
            || active.range != (ByteRange { start, end })
        {
            return (additional, "first_lease_not_whole");
        }
        if byte_cap == 0 {
            return (additional, "batch_disabled");
        }
        let reserved_for_peers =
            max_connections.saturating_sub(occupied_requests.saturating_add(1));
        let mut truncation_reason = "candidate_exhausted";
        let mut request_end = first.end;
        for piece_id in first.piece_id + 1..self.piece_map.piece_count() {
            if additional.len() + 1 >= MAX_REQUEST_LEASES {
                truncation_reason = "lease_limit";
                break;
            }
            if self.available_range_count <= reserved_for_peers {
                truncation_reason = "request_slots_reserved";
                break;
            }
            if !self.available_pieces[piece_id] || self.pieces.contains_key(&piece_id) {
                truncation_reason = "candidate_boundary";
                break;
            }
            let (start, end) = self.piece_map.piece_range(piece_id);
            if start != request_end || end - first.start > byte_cap {
                truncation_reason = "max_request_bytes";
                break;
            }
            let Some(segment) = self.issue_lease(piece_id, ByteRange { start, end }, worker_id)
            else {
                truncation_reason = "lease_issue_failed";
                break;
            };
            additional.push(segment);
            request_end = end;
            self.next_candidate = (piece_id + 1) % self.piece_map.piece_count();
        }
        (additional, truncation_reason)
    }

    /// Retain a writer-confirmed prefix in runtime state, leaving the same lease
    /// identity active for its suffix. The caller must first retire writer access
    /// and then renew or reclaim this lease before producing any more data.
    /// Partial progress never publishes a durable whole-piece completion bit.
    pub fn retain_prefix(&mut self, lease_key: LeaseKey, confirmed_end: u64) -> bool {
        let Some(piece) = self.pieces.get_mut(&lease_key.piece_id) else {
            return false;
        };
        let Some(active) = piece.active_leases.get_mut(&lease_key.lease_id) else {
            return false;
        };
        if confirmed_end <= active.range.start || confirmed_end >= active.range.end {
            return false;
        }
        active.range.start = confirmed_end;
        piece.has_completed_ranges = true;
        true
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

    /// Atomically prevent fresh retry lineages before publishing a fatal worker
    /// error. Reclaim still clears checkpoint hints; resuming builds a new scheduler.
    pub fn stop_and_reclaim(&mut self, lease_key: LeaseKey) {
        self.stopped = true;
        self.reclaim(lease_key);
    }

    pub fn has_available(&self) -> bool {
        !self.stopped && self.available_range_count != 0
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

    #[test]
    fn fatal_reclaim_prevents_sibling_reassignment_but_preserves_resume() {
        let mut scheduler = SchedulerState::new(PieceMap::new(64, 32));
        let failed = scheduler.assign_to(0).unwrap();
        scheduler.stop_and_reclaim(failed.lease_key());
        assert!(!scheduler.has_available());
        assert!(scheduler.assign_to(1).is_none());
        assert!(scheduler.assign_subrange(0, 0, 16, 1).is_none());
        assert!(scheduler.control_hints().inflight_piece_ids.is_empty());
        assert_eq!(scheduler.remaining_count(), 2);
        assert_eq!(scheduler.completed_bytes(), 0);
        // The stop flag is runtime-only; a new session may download the piece.
        let mut resumed = SchedulerState::new(scheduler.piece_map);
        assert_eq!(resumed.assign().unwrap().start, 0);
    }
    use crate::storage::piece_map::PieceMap;

    #[test]
    fn request_aware_assignment_splits_despite_reserved_piece_leases() {
        let mut sched = SchedulerState::new(PieceMap::new(4_096, 1_024));
        // One HTTP request owns three contiguous pieces; three request slots
        // remain free when the last missing piece is assigned.
        let reserved = (0..3)
            .map(|piece| {
                sched
                    .assign_subrange(piece, piece as u64 * 1_024, (piece as u64 + 1) * 1_024, 0)
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let first = sched.assign_to_with_request_split(1, 4, 256, 1).unwrap();
        let second = sched.assign_to_with_request_split(2, 4, 256, 2).unwrap();
        let third = sched.assign_to_with_request_split(3, 4, 256, 3).unwrap();
        assert_eq!((first.start, first.end), (3_072, 3_414));
        assert_eq!((second.start, second.end), (3_414, 3_755));
        assert_eq!((third.start, third.end), (3_755, 4_096));
        assert_eq!(
            (sched.active_lease_count, sched.available_range_count),
            (6, 0)
        );
        for segment in reserved.into_iter().chain([first, second, third]) {
            assert!(sched.complete(segment.lease_key()));
        }
        assert!(sched.all_done());
        assert_eq!(sched.completed_bytes(), 4_096);
    }

    #[test]
    fn request_aware_assignment_preserves_minimum_and_occupied_slot_limits() {
        let mut sched = SchedulerState::new(PieceMap::new(900, 900));
        let whole = sched.assign_to_with_request_split(0, 4, 512, 0).unwrap();
        assert_eq!((whole.start, whole.end), (0, 900));
        let mut sched = SchedulerState::new(PieceMap::new(1_024, 1_024));
        // Occupied requests may include setup/backoff without any lease.
        let whole = sched.assign_to_with_request_split(0, 4, 128, 3).unwrap();
        assert_eq!((whole.start, whole.end), (0, 1_024));
    }

    #[test]
    fn retained_prefix_survives_renewal_and_reclaim_without_durable_bits() {
        let mut sched = SchedulerState::new(PieceMap::new(1_000, 1_000));
        let first = sched.assign().unwrap();
        for invalid in [0, 1_000, 1_001] {
            assert!(!sched.retain_prefix(first.lease_key(), invalid));
        }
        assert!(sched.retain_prefix(first.lease_key(), 300));
        assert_eq!(
            (sched.active_lease_count, sched.available_range_count),
            (1, 0)
        );
        assert_eq!(sched.completed_bytes(), 0);
        assert_eq!(sched.snapshot_bitset(), vec![0]);
        let renewed = sched.renew(first.lease_key(), 2).unwrap();
        assert_eq!(
            (renewed.start, renewed.end, renewed.attempt),
            (300, 1_000, 2)
        );
        assert!(!sched.retain_prefix(first.lease_key(), 600));
        assert!(!sched.complete(first.lease_key()));
        assert!(!sched.reclaim(first.lease_key()));
        assert!(!sched.retain_prefix(renewed.lease_key(), 200));
        assert!(sched.retain_prefix(renewed.lease_key(), 600));
        assert!(sched.reclaim(renewed.lease_key()));
        assert_eq!(
            (sched.active_lease_count, sched.available_range_count),
            (0, 1)
        );
        let hints = sched.control_hints();
        assert_eq!(hints.dirty_piece_ids, vec![0]);
        assert!(hints.inflight_piece_ids.is_empty());
        let suffix = sched.assign().unwrap();
        assert_eq!((suffix.start, suffix.end, suffix.attempt), (600, 1_000, 3));
        assert!(sched.complete(suffix.lease_key()));
        assert_eq!(sched.completed_bytes(), 1_000);
        assert_eq!(
            (sched.active_lease_count, sched.available_range_count),
            (0, 0)
        );
        assert!(sched.pieces.is_empty());
    }

    #[test]
    fn retained_prefix_does_not_cover_other_missing_subranges() {
        let mut sched = SchedulerState::new(PieceMap::new(1_000, 1_000));
        let middle = sched.assign_subrange(0, 200, 800, 0).unwrap();
        assert!(sched.retain_prefix(middle.lease_key(), 400));
        assert!(sched.reclaim(middle.lease_key()));
        assert_eq!(sched.available_range_count, 2);
        let left = sched.assign().unwrap();
        let right = sched.assign().unwrap();
        assert_eq!((left.start, left.end), (0, 200));
        assert_eq!((right.start, right.end), (400, 1_000));
        assert!(sched.complete(right.lease_key()));
        assert_eq!(sched.completed_bytes(), 0);
        assert!(sched.complete(left.lease_key()));
        assert!(sched.all_done());
    }

    #[test]
    fn batches_bound_bytes_leases_and_preserve_independent_work() {
        let mut sched = SchedulerState::new(PieceMap::new(10_000, 1_000));
        let first = sched.assign_to(4).unwrap();
        let extra = sched.extend_batch(&first, 4, 4, 3_500);
        assert_eq!(
            extra.iter().map(|s| (s.start, s.end)).collect::<Vec<_>>(),
            vec![(1_000, 2_000), (2_000, 3_000)]
        );
        assert_eq!(
            (sched.active_lease_count, sched.available_range_count),
            (3, 7)
        );
        let next = sched.assign_to(5).unwrap();
        assert_eq!(next.start, 3_000);
        let extra = sched.extend_batch(&next, 5, 4, u64::MAX);
        assert_eq!(extra.len(), 3);
        assert_eq!(sched.available_range_count, 3);
        for worker in 0..3 {
            assert!(sched.assign_to(worker).is_some());
        }
        let mut tiny = SchedulerState::new(PieceMap::new(200, 1));
        let first = tiny.assign().unwrap();
        let extra = tiny.extend_batch(&first, 0, 1, u64::MAX);
        assert_eq!(extra.len(), 63);
        assert_eq!(tiny.active_lease_count, 64);
        for lease in std::iter::once(first).chain(extra) {
            assert!(tiny.complete(lease.lease_key()));
        }
        assert_eq!(tiny.completed_bytes(), 64);
    }

    #[test]
    fn batches_stop_at_completed_active_partial_or_touched_holes() {
        for hole in 0..4 {
            let mut sched = SchedulerState::new(PieceMap::new(5_000, 1_000));
            let end = if hole == 2 { 2_500 } else { 3_000 };
            let blocked = sched.assign_subrange(2, 2_000, end, 7).unwrap();
            match hole {
                0 | 2 => {
                    assert!(sched.complete(blocked.lease_key()));
                }
                3 => {
                    assert!(sched.reclaim(blocked.lease_key()));
                }
                _ => {}
            }
            let first = sched.assign().unwrap();
            let extra = sched.extend_batch(&first, 0, 1, u64::MAX);
            assert_eq!(extra.len(), 1);
            assert_eq!(extra[0].piece_id, 1);
            assert!(sched.available_pieces[3]);
        }
    }

    #[test]
    fn batches_reject_stale_partial_or_mismatched_first_leases() {
        let mut sched = SchedulerState::new(PieceMap::new(5_000, 1_000));
        let first = sched.assign().unwrap();
        assert!(sched.extend_batch(&first, 1, 1, u64::MAX).is_empty());
        assert!(sched.extend_batch(&first, 0, 1, 999).is_empty());
        let renewed = sched.renew(first.lease_key(), 0).unwrap();
        assert!(sched.extend_batch(&first, 0, 1, u64::MAX).is_empty());
        assert!(sched.retain_prefix(renewed.lease_key(), 500));
        assert!(sched.extend_batch(&renewed, 0, 1, u64::MAX).is_empty());
        let suffix = sched.renew(renewed.lease_key(), 0).unwrap();
        assert!(sched.extend_batch(&suffix, 0, 1, u64::MAX).is_empty());
        assert_eq!(
            (sched.active_lease_count, sched.available_range_count),
            (1, 4)
        );
    }

    #[test]
    fn atomic_fixed_assignment_owns_the_whole_request_batch() {
        let mut sched = SchedulerState::new(PieceMap::new(10_000, 1_000));
        let assignment = sched
            .assign_request(
                0,
                4,
                256,
                0,
                RangeSchedulingMode::Fixed,
                3_500,
                1_000,
                64 * 1024 * 1024,
                true,
            )
            .unwrap();
        assert_eq!(
            assignment
                .segments
                .iter()
                .map(|segment| (segment.start, segment.end))
                .collect::<Vec<_>>(),
            vec![(0, 1_000), (1_000, 2_000), (2_000, 3_000)]
        );
        assert_eq!(assignment.candidate_end, 10_000);
        assert_eq!(assignment.final_end, 3_000);
        assert_eq!(assignment.truncation_reason, "max_request_bytes");

        // A second worker sees the first request's complete lease set, so it
        // starts after the batch rather than taking its adjacent pieces.
        let next = sched
            .assign_request(
                1,
                4,
                256,
                1,
                RangeSchedulingMode::Fixed,
                3_500,
                1_000,
                64 * 1024 * 1024,
                true,
            )
            .unwrap();
        assert_eq!(next.segments[0].start, 3_000);
    }

    #[test]
    fn fresh_probe_assignment_keeps_the_first_piece_exact() {
        let mut sched = SchedulerState::new(PieceMap::new(2_000, 1_000));
        let assignment = sched
            .assign_request(
                0,
                4,
                256,
                0,
                RangeSchedulingMode::Fixed,
                4_000,
                1_000,
                64 * 1_024,
                false,
            )
            .unwrap();

        assert_eq!(assignment.segments.len(), 1);
        assert_eq!((assignment.final_start, assignment.final_end), (0, 1_000));
        assert_eq!(
            (assignment.segments[0].start, assignment.segments[0].end),
            (0, 1_000)
        );
    }

    #[test]
    fn fixed_retries_keep_their_single_lease_recovery_path() {
        let mut sched = SchedulerState::new(PieceMap::new(4_000, 1_000));
        let first = sched.assign_to(0).unwrap();
        assert!(sched.reclaim(first.lease_key()));

        let assignment = sched
            .assign_request(
                1,
                4,
                256,
                0,
                RangeSchedulingMode::Fixed,
                4_000,
                1_000,
                64 * 1_024,
                true,
            )
            .unwrap();
        assert_eq!(assignment.segments.len(), 1);
        assert_eq!(assignment.truncation_reason, "recovered_or_retried");
        assert_eq!(assignment.segments[0].attempt, 2);
    }

    #[test]
    fn recovery_batch_issues_whole_contiguous_pieces_atomically() {
        let mut sched = SchedulerState::new(PieceMap::new(4_000, 1_000));
        let pieces = (0..4)
            .map(|piece| (piece, piece as u64 * 1_000, (piece as u64 + 1) * 1_000))
            .collect::<Vec<_>>();
        let segments = sched.assign_recovery_batch(&pieces, 7).unwrap();
        assert_eq!(segments.len(), 4);
        assert!(segments
            .iter()
            .all(|segment| segment.end - segment.start == 1_000));
        assert_eq!(sched.active_lease_count, 4);
        for segment in segments {
            assert!(sched.complete(segment.lease_key()));
        }
        assert!(sched.all_done());
    }

    #[test]
    fn dynamic_assignment_balances_remaining_share_for_available_slots() {
        let mut sched = SchedulerState::new(PieceMap::new(64 * 1_024, 1_024));
        let first = sched
            .assign_request(
                0,
                8,
                256,
                0,
                RangeSchedulingMode::Dynamic,
                4 * 1_024 * 1_024,
                1_024,
                64 * 1_024,
                true,
            )
            .unwrap();
        assert_eq!(first.planned_ranges, 8);
        assert_eq!(first.segments.len(), 8);
        assert_eq!((first.final_start, first.final_end), (0, 8 * 1_024));
        assert_eq!(first.truncation_reason, "none");

        let second = sched
            .assign_request(
                1,
                8,
                256,
                1,
                RangeSchedulingMode::Dynamic,
                4 * 1_024 * 1_024,
                1_024,
                64 * 1_024,
                true,
            )
            .unwrap();
        assert_eq!(second.final_end - second.final_start, 8 * 1_024);
        assert_eq!(second.planned_ranges, 7);
        assert!(second.final_start >= first.final_end || second.final_end <= first.final_start);

        let mut assignments = vec![first, second];
        for worker_id in 2..8 {
            let assignment = sched
                .assign_request(
                    worker_id,
                    8,
                    256,
                    worker_id,
                    RangeSchedulingMode::Dynamic,
                    4 * 1_024 * 1_024,
                    1_024,
                    64 * 1_024,
                    true,
                )
                .unwrap();
            assert_eq!(assignment.final_end - assignment.final_start, 8 * 1_024);
            assignments.push(assignment);
        }
        assert_eq!(
            assignments.iter().map(|a| a.segments.len()).sum::<usize>(),
            64
        );
        assert_eq!(sched.available_range_count, 0);
    }

    #[test]
    fn dynamic_assigns_extra_slots_by_current_byte_share() {
        let mut piece_map = PieceMap::new(19_000, 1_000);
        piece_map.mark_complete(10);
        let mut sched = SchedulerState::new(piece_map);
        let assignment = sched
            .assign_request(
                0,
                3,
                256,
                0,
                RangeSchedulingMode::Dynamic,
                0,
                1_000,
                64 * 1_024,
                true,
            )
            .unwrap();

        // The 10-piece range receives two virtual slots and the 8-piece
        // range one. Its current shares are therefore 5 KiB and 8 KiB, so
        // the first request must serve the latter range.
        assert_eq!(
            (assignment.final_start, assignment.final_end),
            (11_000, 19_000)
        );
        assert_eq!(assignment.planned_ranges, 3);
    }

    #[test]
    fn dynamic_trace_diagnostics_include_target_and_all_candidate_shares() {
        let mut piece_map = PieceMap::new(19_000, 1_000);
        piece_map.mark_complete(10);
        let mut sched = SchedulerState::new(piece_map);
        let assignment = sched
            .assign_request_with_trace(
                0,
                3,
                256,
                0,
                RangeSchedulingMode::Dynamic,
                0,
                1_000,
                64 * 1_024,
                true,
                true,
                true,
            )
            .unwrap();

        assert_eq!(assignment.candidate_slots, 1);
        assert_eq!(assignment.target_end, 19_000);
        let shares = assignment.candidate_shares.unwrap();
        assert_eq!(shares.len(), 2);
        assert_eq!(shares[0].slots, 2);
        assert_eq!(shares[0].share_bytes, 5_000);
        assert_eq!(shares[1].slots, 1);
        assert_eq!(shares[1].share_bytes, 8_000);
    }

    #[test]
    fn dynamic_max_request_prefers_a_legal_minimum_split_boundary() {
        let mut sched = SchedulerState::new(PieceMap::new(9_000, 1_000));
        let assignment = sched
            .assign_request(
                0,
                1,
                1,
                0,
                RangeSchedulingMode::Dynamic,
                0,
                4_000,
                6_000,
                true,
            )
            .unwrap();

        // A raw six-piece cap would leave a three-piece tail. The nearest
        // legal boundary below that cap is five pieces, leaving four.
        assert_eq!((assignment.final_start, assignment.final_end), (0, 5_000));
        assert_eq!(assignment.segments.len(), 5);
        assert_eq!(
            assignment.truncation_reason,
            "max_request_bytes_and_min_split"
        );
    }

    #[test]
    fn dynamic_hard_cap_wins_when_it_is_below_the_first_legal_boundary() {
        let mut sched = SchedulerState::new(PieceMap::new(9_000, 1_000));
        let assignment = sched
            .assign_request(
                0,
                1,
                1,
                0,
                RangeSchedulingMode::Dynamic,
                0,
                4_000,
                3_000,
                true,
            )
            .unwrap();

        assert_eq!((assignment.final_start, assignment.final_end), (0, 3_000));
        assert_eq!(
            assignment.truncation_reason,
            "max_request_bytes_and_min_split_conflict"
        );
    }

    #[test]
    fn dynamic_lease_cap_also_avoids_a_short_tail_when_possible() {
        let mut sched = SchedulerState::new(PieceMap::new(67_000, 1_000));
        let assignment = sched
            .assign_request(
                0,
                1,
                1,
                0,
                RangeSchedulingMode::Dynamic,
                0,
                4_000,
                u64::MAX,
                true,
            )
            .unwrap();

        // The 64-lease hard cap would leave three pieces. Move back one
        // legal boundary so both sides remain at least four pieces.
        assert_eq!((assignment.final_start, assignment.final_end), (0, 63_000));
        assert_eq!(assignment.segments.len(), 63);
        assert_eq!(assignment.truncation_reason, "lease_limit_and_min_split");
    }

    #[test]
    fn fixed_assignment_skips_unneeded_candidate_boundary_scan() {
        let mut sched = SchedulerState::new(PieceMap::new(10_000, 1_000));
        let assignment = sched
            .assign_request_with_diagnostics(
                0,
                4,
                256,
                0,
                RangeSchedulingMode::Fixed,
                3_500,
                1_000,
                64 * 1_024 * 1_024,
                true,
                false,
            )
            .unwrap();
        assert!(!assignment.candidate_boundary_scanned);
        assert_eq!(assignment.candidate_end, 1_000);
        assert_eq!(assignment.final_end, 3_000);
    }

    #[test]
    fn dynamic_planning_limits_independent_ranges_to_available_slots() {
        let piece_map = PieceMap::from_bitset(8_000, 1_000, &[0b1010_1010], 8);
        let mut sched = SchedulerState::new(piece_map);
        let assignment = sched
            .assign_request(
                0,
                2,
                256,
                0,
                RangeSchedulingMode::Dynamic,
                0,
                1_000,
                64 * 1_024,
                true,
            )
            .unwrap();
        assert_eq!(assignment.planned_ranges, 2);
        assert_eq!(assignment.segments[0].piece_id, 0);
    }

    #[test]
    fn dynamic_minimum_is_piece_aligned_and_maximum_keeps_one_piece() {
        let mut sched = SchedulerState::new(PieceMap::new(16_000, 1_000));
        let assignment = sched
            .assign_request(
                0,
                4,
                256,
                0,
                RangeSchedulingMode::Dynamic,
                0,
                1_500,
                64 * 1_024,
                true,
            )
            .unwrap();
        assert_eq!(assignment.segments.len(), 4);
        assert_eq!(assignment.final_end - assignment.final_start, 4_000);
        assert!(assignment
            .segments
            .iter()
            .all(|segment| segment.end - segment.start == 1_000));

        let mut tiny = SchedulerState::new(PieceMap::new(2_000, 1_000));
        let assignment = tiny
            .assign_request(
                0,
                1,
                256,
                0,
                RangeSchedulingMode::Dynamic,
                0,
                1_000,
                128,
                true,
            )
            .unwrap();
        assert_eq!(assignment.segments.len(), 1);
        assert_eq!((assignment.final_start, assignment.final_end), (0, 1_000));
    }

    #[test]
    fn dynamic_leaves_touched_pieces_to_the_single_lease_path() {
        let mut sched = SchedulerState::new(PieceMap::new(4_000, 1_000));
        let touched = sched.assign_subrange(0, 128, 512, 7).unwrap();

        let untouched = sched
            .assign_request(
                0,
                1,
                256,
                0,
                RangeSchedulingMode::Dynamic,
                0,
                1_000,
                64 * 1_024,
                true,
            )
            .unwrap();
        assert_eq!(
            untouched
                .segments
                .iter()
                .map(|segment| segment.piece_id)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        for segment in untouched.segments {
            assert!(sched.complete(segment.lease_key()));
        }

        // Once the untouched runs are exhausted, dynamic scheduling still
        // makes progress, but does not turn the fragmented piece into a
        // multi-piece dynamic request.
        let recovered = sched
            .assign_request(
                1,
                1,
                256,
                0,
                RangeSchedulingMode::Dynamic,
                0,
                1_000,
                64 * 1_024,
                true,
            )
            .unwrap();
        assert_eq!(recovered.segments.len(), 1);
        assert_eq!(
            (recovered.segments[0].piece_id, recovered.segments[0].start),
            (0, 0)
        );
        assert!(sched.complete(touched.lease_key()));
    }

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
