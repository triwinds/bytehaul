/// Stable identity for a scheduler-issued segment lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct LeaseKey {
    pub piece_id: usize,
    pub lease_id: u64,
}

/// A download segment lease: the byte range currently assigned to a worker.
#[derive(Debug, Clone)]
pub(crate) struct Segment {
    pub piece_id: usize,
    pub lease_id: u64,
    /// Byte offset (inclusive).
    pub start: u64,
    /// Byte offset (exclusive).
    pub end: u64,
    pub owner_worker_id: usize,
    pub attempt: u32,
}

impl Segment {
    pub fn lease_key(&self) -> LeaseKey {
        LeaseKey {
            piece_id: self.piece_id,
            lease_id: self.lease_id,
        }
    }
}
