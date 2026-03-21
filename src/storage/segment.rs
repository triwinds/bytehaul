/// A download segment: the byte range assigned to a worker for one piece.
#[derive(Debug, Clone)]
pub(crate) struct Segment {
    pub piece_id: usize,
    /// Byte offset (inclusive).
    pub start: u64,
    /// Byte offset (exclusive).
    pub end: u64,
}
