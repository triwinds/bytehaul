use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, OwnedSemaphorePermit};

use crate::error::DownloadError;
use crate::storage::cache::{NonContiguousWrite, WriteBackCache};
use crate::storage::segment::LeaseKey;

#[derive(Debug, Clone, Copy)]
pub(crate) struct FlushAllStats {
    pub written_bytes: u64,
    pub flush_elapsed: Duration,
    pub sync_elapsed: Option<Duration>,
}

pub(crate) enum WriterCommand {
    BeginLease {
        lease_key: LeaseKey,
    },
    Data {
        offset: u64,
        data: Bytes,
        /// Travels with queued data so receiver failure also returns the budget.
        permit: Option<OwnedSemaphorePermit>,
        /// Which lease this data belongs to (for cache aggregation).
        lease_key: Option<LeaseKey>,
    },
    /// Flush a complete lease or a stopped attempt's confirmed prefix and
    /// retire its writer identity. The acknowledgement does not imply fsync.
    FlushLease {
        lease_key: LeaseKey,
        ack: oneshot::Sender<()>,
    },
    /// A lease attempt failed before completion; discard its cached bytes.
    DiscardLease {
        lease_key: LeaseKey,
        ack: oneshot::Sender<usize>,
    },
    /// Flush all cached data and optionally sync it before acknowledging.
    FlushAll {
        sync_data: bool,
        ack: oneshot::Sender<FlushAllStats>,
    },
}

/// Writer task: receives data chunks via a bounded channel, aggregates them
/// in a write-back cache, and flushes to disk on piece completion or budget limits.
pub(crate) struct WriterTask {
    rx: mpsc::Receiver<WriterCommand>,
    file: tokio::fs::File,
    written_bytes: Arc<AtomicU64>,
    cache: WriteBackCache,
    active_leases: HashMap<LeaseKey, Option<u64>>,
    single: Option<(u64, BytesMut)>,
    file_offset: Option<u64>,
    /// Retained through cache drains and write errors; Drop also settles failures.
    permits: Vec<(Option<LeaseKey>, OwnedSemaphorePermit)>,
    /// Maximum bytes to buffer before forcing a flush.
    cache_high_watermark: usize,
}

impl WriterTask {
    pub fn new(
        rx: mpsc::Receiver<WriterCommand>,
        file: tokio::fs::File,
        written_bytes: Arc<AtomicU64>,
        cache_high_watermark: usize,
    ) -> Self {
        Self {
            rx,
            file,
            written_bytes,
            cache: WriteBackCache::new(),
            active_leases: HashMap::new(),
            permits: Vec::new(),
            single: None,
            file_offset: None,
            cache_high_watermark,
        }
    }

    pub async fn run(mut self) -> Result<(), DownloadError> {
        while let Some(cmd) = self.rx.recv().await {
            match cmd {
                WriterCommand::BeginLease { lease_key } => {
                    self.active_leases.insert(lease_key, None);
                }
                WriterCommand::Data {
                    offset,
                    data,
                    lease_key,
                    permit,
                } => {
                    if !data.is_empty()
                        && self.single.as_ref().is_some_and(|(start, buffered)| {
                            lease_key.is_some() || *start + buffered.len() as u64 != offset
                        })
                    {
                        self.flush_single().await?;
                        self.permits.retain(|(key, _)| key.is_some());
                    }
                    if lease_key.is_none() && self.cache.total_bytes() > 0 {
                        self.flush_all().await?;
                    }
                    if let Some(permit) = permit {
                        self.permits.push((lease_key, permit));
                    }
                    let data_len = data.len();
                    match lease_key {
                        Some(lease_key) if self.active_leases.contains_key(&lease_key) => {
                            if !data.is_empty() {
                                if let Some(Some(expected)) = self.active_leases.get(&lease_key) {
                                    if offset != *expected {
                                        return Err(DownloadError::Internal(
                                            NonContiguousWrite {
                                                expected: *expected,
                                                actual: offset,
                                            }
                                            .to_string(),
                                        ));
                                    }
                                }
                                self.cache
                                    .insert(lease_key, offset, data)
                                    .map_err(|error| DownloadError::Internal(error.to_string()))?;
                                self.active_leases
                                    .insert(lease_key, Some(offset + data_len as u64));
                            }
                        }
                        Some(_) => {
                            self.permits.retain(|(key, _)| *key != lease_key);
                        }
                        None => {
                            // A bounded contiguous buffer shares the producer budget.
                            self.buffer_single(offset, &data).await?;
                        }
                    }

                    if self.cache.total_bytes() + self.single_bytes() >= self.cache_high_watermark {
                        self.flush_all().await?;
                    }
                }
                WriterCommand::FlushLease { lease_key, ack } => {
                    self.flush_lease(lease_key).await?;
                    self.active_leases.remove(&lease_key);
                    let _ = ack.send(());
                }
                WriterCommand::DiscardLease { lease_key, ack } => {
                    self.active_leases.remove(&lease_key);
                    let discarded = self.cache.discard_lease(lease_key);
                    self.permits.retain(|(key, _)| *key != Some(lease_key));
                    let _ = ack.send(discarded);
                }
                WriterCommand::FlushAll { sync_data, ack } => {
                    let flush_started = Instant::now();
                    self.flush_all().await?;
                    let flush_elapsed = flush_started.elapsed();
                    let sync_elapsed = if sync_data {
                        let sync_started = Instant::now();
                        self.sync_file().await?;
                        Some(sync_started.elapsed())
                    } else {
                        None
                    };
                    let _ = ack.send(FlushAllStats {
                        written_bytes: self.written_bytes.load(Ordering::Acquire),
                        flush_elapsed,
                        sync_elapsed,
                    });
                }
            }
        }

        // Final flush of any remaining cached data
        self.flush_all().await?;
        let started = crate::bench_stats::phase_start();
        self.file.flush().await?;
        self.file.sync_all().await?;
        crate::bench_stats::record_phase(started, crate::bench_stats::record_fsync);
        Ok(())
    }

    fn single_bytes(&self) -> usize {
        self.single.as_ref().map_or(0, |(_, data)| data.len())
    }

    async fn buffer_single(&mut self, offset: u64, data: &[u8]) -> Result<(), DownloadError> {
        if data.is_empty() {
            return Ok(());
        }
        if data.len() >= (256 * 1024).min(self.cache_high_watermark) {
            self.flush_single().await?;
            self.write_block(offset, data).await?;
            self.permits.retain(|(key, _)| key.is_some());
            return Ok(());
        }
        let (_, buffered) = self.single.get_or_insert_with(|| (offset, BytesMut::new()));
        buffered.extend_from_slice(data);
        if crate::bench_stats::enabled() {
            crate::bench_stats::record_cache_copy(data.len());
        }
        // Keep batching bounded even when the session has a large budget.
        if self.single_bytes() >= 256 * 1024 {
            self.flush_single().await?;
            self.permits.retain(|(key, _)| key.is_some());
        }
        Ok(())
    }

    async fn flush_single(&mut self) -> Result<(), DownloadError> {
        if let Some((offset, data)) = self.single.take() {
            self.write_block(offset, &data).await?;
            if crate::bench_stats::enabled() {
                crate::bench_stats::record_cache_evict(data.len());
            }
        }
        Ok(())
    }

    async fn flush_lease(&mut self, lease_key: LeaseKey) -> Result<(), DownloadError> {
        for block in self.cache.drain_lease(lease_key) {
            self.write_block(block.offset, &block.data).await?;
        }
        self.permits.retain(|(key, _)| *key != Some(lease_key));
        Ok(())
    }

    async fn flush_all(&mut self) -> Result<(), DownloadError> {
        self.flush_single().await?;
        for block in self.cache.drain_all() {
            self.write_block(block.offset, &block.data).await?;
        }
        self.permits.clear();
        Ok(())
    }

    async fn sync_file(&mut self) -> Result<(), DownloadError> {
        let started = crate::bench_stats::phase_start();
        self.file.flush().await?;
        self.file.sync_data().await?;
        crate::bench_stats::record_phase(started, crate::bench_stats::record_fsync);
        Ok(())
    }

    async fn write_block(&mut self, offset: u64, data: &[u8]) -> Result<(), DownloadError> {
        if crate::bench_stats::enabled() {
            crate::bench_stats::record_write_block(data.len() as u64);
        }
        if self.file_offset != Some(offset) {
            if crate::bench_stats::enabled() {
                crate::bench_stats::record_writer_seek();
            }
            self.file.seek(std::io::SeekFrom::Start(offset)).await?;
        }
        self.file_offset = None;
        self.file.write_all(data).await?;
        let end = offset + data.len() as u64;
        self.file_offset = Some(end);
        // Update high-water mark
        self.written_bytes.fetch_max(end, Ordering::Release);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn budgeted_data(
        budget: &Arc<tokio::sync::Semaphore>,
        offset: u64,
        data: &'static [u8],
        lease_key: Option<LeaseKey>,
    ) -> WriterCommand {
        WriterCommand::Data {
            offset,
            data: Bytes::from_static(data),
            lease_key,
            permit: Some(
                budget
                    .clone()
                    .acquire_many_owned(data.len() as u32)
                    .await
                    .unwrap(),
            ),
        }
    }

    #[tokio::test]
    async fn single_buffer_rewinds_and_flushes_with_a_tiny_budget() {
        for size in [1, 3, 9] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("data");
            let file = tokio::fs::File::create(&path).await.unwrap();
            let budget = Arc::new(tokio::sync::Semaphore::new(size));
            let written = Arc::new(AtomicU64::new(0));
            let (tx, rx) = mpsc::channel(2);
            let handle = tokio::spawn(WriterTask::new(rx, file, written, (size / 2).max(1)).run());
            tokio::time::timeout(Duration::from_secs(2), async {
                // A retry rewinds the output, then continues sequentially.
                for (offset, data) in [(0, b"a"), (1, b"b"), (0, b"c"), (1, b"d"), (2, b"e")] {
                    tx.send(budgeted_data(&budget, offset, data, None).await)
                        .await
                        .unwrap();
                }
                let (ack, done) = oneshot::channel();
                tx.send(WriterCommand::FlushAll {
                    sync_data: true,
                    ack,
                })
                .await
                .unwrap();
                assert_eq!(done.await.unwrap().written_bytes, 3);
                assert_eq!(std::fs::read(&path).unwrap(), b"cde");
                assert_eq!(budget.available_permits(), size);
                drop(tx);
                handle.await.unwrap().unwrap();
            })
            .await
            .expect("single buffering must preserve the next-chunk waterline");
        }
    }

    #[tokio::test]
    async fn writer_failure_returns_buffered_current_and_queued_permits() {
        for invalid_offset in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("data");
            std::fs::write(&path, b"unchanged").unwrap();
            let file = tokio::fs::File::open(&path).await.unwrap(); // read-only
            let budget = Arc::new(tokio::sync::Semaphore::new(16));
            let (tx, rx) = mpsc::channel(8);
            let key = LeaseKey {
                piece_id: 0,
                lease_id: 1,
            };
            tx.send(WriterCommand::BeginLease { lease_key: key })
                .await
                .unwrap();
            tx.send(budgeted_data(&budget, 0, b"ab", Some(key)).await)
                .await
                .unwrap();
            tx.send(
                budgeted_data(
                    &budget,
                    if invalid_offset { 1 } else { 2 },
                    b"cd",
                    Some(key),
                )
                .await,
            )
            .await
            .unwrap();
            let (ack, done) = oneshot::channel();
            tx.send(WriterCommand::FlushAll {
                sync_data: true,
                ack,
            })
            .await
            .unwrap();
            tx.send(budgeted_data(&budget, 4, b"ef", Some(key)).await)
                .await
                .unwrap();
            assert_eq!(budget.available_permits(), 10);
            let result = WriterTask::new(rx, file, Arc::new(AtomicU64::new(0)), 16)
                .run()
                .await;
            if invalid_offset {
                assert!(matches!(result, Err(DownloadError::Internal(_))));
            } else {
                assert!(matches!(result, Err(DownloadError::Io(_))));
            }
            assert!(done.await.is_err());
            assert_eq!(budget.available_permits(), 16);
            assert_eq!(std::fs::read(&path).unwrap(), b"unchanged");
        }
    }

    #[tokio::test]
    async fn abandoning_writer_or_receiver_returns_data_permits() {
        for run in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let file = tokio::fs::File::create(dir.path().join("data"))
                .await
                .unwrap();
            let budget = Arc::new(tokio::sync::Semaphore::new(16));
            let (tx, rx) = mpsc::channel(4);
            tx.send(budgeted_data(&budget, 0, b"ab", None).await)
                .await
                .unwrap();
            let writer = WriterTask::new(rx, file, Arc::new(AtomicU64::new(0)), 16);
            if run {
                let handle = tokio::spawn(writer.run());
                let (ack, done) = oneshot::channel();
                // A lease barrier confirms the previous command was consumed,
                // while leaving the unrelated single buffer held in memory.
                tx.send(WriterCommand::FlushLease {
                    lease_key: LeaseKey {
                        piece_id: 99,
                        lease_id: 99,
                    },
                    ack,
                })
                .await
                .unwrap();
                done.await.unwrap();
                assert_eq!(budget.available_permits(), 14);
                handle.abort();
                assert!(handle.await.unwrap_err().is_cancelled());
            } else {
                // Includes sends using a channel reservation acquired before close.
                let slot = tx.reserve().await.unwrap();
                drop(writer);
                slot.send(budgeted_data(&budget, 2, b"cd", None).await);
                drop(tx);
            }
            assert_eq!(budget.available_permits(), 16);
        }
    }

    #[tokio::test]
    async fn writer_rejects_gap_or_overlap_even_after_watermark_flush() {
        for watermark in [1, 100] {
            for next_offset in [1, 3] {
                let dir = tempfile::tempdir().unwrap();
                let file = tokio::fs::File::create(dir.path().join("data"))
                    .await
                    .unwrap();
                let (tx, rx) = mpsc::channel(4);
                let writer = tokio::spawn(
                    WriterTask::new(rx, file, Arc::new(AtomicU64::new(0)), watermark).run(),
                );
                let key = LeaseKey {
                    piece_id: 0,
                    lease_id: 1,
                };
                tx.send(WriterCommand::BeginLease { lease_key: key })
                    .await
                    .unwrap();
                for (offset, data) in [(0, b"ab".as_slice()), (next_offset, b"x".as_slice())] {
                    tx.send(WriterCommand::Data {
                        permit: None,
                        offset,
                        data: Bytes::copy_from_slice(data),
                        lease_key: Some(key),
                    })
                    .await
                    .unwrap();
                }
                drop(tx);
                assert!(
                    matches!(writer.await.unwrap(), Err(DownloadError::Internal(message))
                    if message == format!("noncontiguous lease write: expected offset 2, got {next_offset}"))
                );
            }
        }
    }

    #[tokio::test]
    async fn test_writer_single_connection_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.bin");
        let file = tokio::fs::File::create(&path).await.unwrap();

        let written = Arc::new(AtomicU64::new(0));
        let (tx, rx) = mpsc::channel(16);

        let writer = WriterTask::new(rx, file, written.clone(), 1024 * 1024);
        let handle = tokio::spawn(writer.run());

        // Send contiguous single-connection chunks
        tx.send(WriterCommand::Data {
            permit: None,
            offset: 0,
            data: Bytes::from(vec![0xAA; 100]),
            lease_key: None,
        })
        .await
        .unwrap();

        tx.send(WriterCommand::Data {
            permit: None,
            offset: 100,
            data: Bytes::from(vec![0xBB; 100]),
            lease_key: None,
        })
        .await
        .unwrap();

        drop(tx);
        handle.await.unwrap().unwrap();

        let content = std::fs::read(&path).unwrap();
        assert_eq!(content.len(), 200);
        assert!(content[..100].iter().all(|&b| b == 0xAA));
        assert!(content[100..].iter().all(|&b| b == 0xBB));
        assert_eq!(written.load(Ordering::Acquire), 200);
    }

    #[tokio::test]
    async fn test_writer_with_piece_cache_and_flush() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_piece.bin");
        // Pre-create file with enough space
        std::fs::write(&path, vec![0u8; 200]).unwrap();
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .await
            .unwrap();

        let written = Arc::new(AtomicU64::new(0));
        let (tx, rx) = mpsc::channel(16);

        let writer = WriterTask::new(rx, file, written.clone(), 1024 * 1024);
        let handle = tokio::spawn(writer.run());

        let lease_key = LeaseKey {
            piece_id: 0,
            lease_id: 1,
        };

        tx.send(WriterCommand::BeginLease { lease_key })
            .await
            .unwrap();

        // Send data with piece_id (cached write)
        tx.send(WriterCommand::Data {
            permit: None,
            offset: 0,
            data: Bytes::from(vec![0xCC; 100]),
            lease_key: Some(lease_key),
        })
        .await
        .unwrap();

        // Flush piece
        let (ack_tx, ack_rx) = oneshot::channel();
        tx.send(WriterCommand::FlushLease {
            lease_key,
            ack: ack_tx,
        })
        .await
        .unwrap();
        ack_rx.await.unwrap();

        drop(tx);
        handle.await.unwrap().unwrap();

        let content = std::fs::read(&path).unwrap();
        assert!(content[..100].iter().all(|&b| b == 0xCC));
    }

    #[tokio::test]
    async fn test_writer_flush_all_with_sync() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_flush_all.bin");
        std::fs::write(&path, vec![0u8; 200]).unwrap();
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .await
            .unwrap();

        let written = Arc::new(AtomicU64::new(0));
        let (tx, rx) = mpsc::channel(16);

        let writer = WriterTask::new(rx, file, written.clone(), 1024 * 1024);
        let handle = tokio::spawn(writer.run());

        let lease_key = LeaseKey {
            piece_id: 0,
            lease_id: 1,
        };
        tx.send(WriterCommand::BeginLease { lease_key })
            .await
            .unwrap();

        tx.send(WriterCommand::Data {
            permit: None,
            offset: 0,
            data: Bytes::from(vec![0xDD; 50]),
            lease_key: Some(lease_key),
        })
        .await
        .unwrap();

        // Flush all with sync
        let (ack_tx, ack_rx) = oneshot::channel();
        tx.send(WriterCommand::FlushAll {
            sync_data: true,
            ack: ack_tx,
        })
        .await
        .unwrap();
        let written_val = ack_rx.await.unwrap();
        assert!(written_val.written_bytes >= 50);
        assert!(written_val.sync_elapsed.is_some());

        drop(tx);
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn test_writer_discard_piece_drops_cached_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_discard_piece.bin");
        std::fs::write(&path, vec![0u8; 128]).unwrap();
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .await
            .unwrap();

        let budget = Arc::new(tokio::sync::Semaphore::new(64));
        let written = Arc::new(AtomicU64::new(0));
        let (tx, rx) = mpsc::channel(16);

        let writer = WriterTask::new(rx, file, written.clone(), 1024);
        let handle = tokio::spawn(writer.run());

        let lease_key = LeaseKey {
            piece_id: 0,
            lease_id: 1,
        };
        tx.send(WriterCommand::BeginLease { lease_key })
            .await
            .unwrap();

        tx.send(WriterCommand::Data {
            permit: Some(budget.clone().acquire_many_owned(64).await.unwrap()),
            offset: 0,
            data: Bytes::from(vec![0xEE; 64]),
            lease_key: Some(lease_key),
        })
        .await
        .unwrap();

        let (ack_tx, ack_rx) = oneshot::channel();
        tx.send(WriterCommand::DiscardLease {
            lease_key,
            ack: ack_tx,
        })
        .await
        .unwrap();
        assert_eq!(ack_rx.await.unwrap(), 64);
        assert_eq!(budget.available_permits(), 64);

        let (ack_tx, ack_rx) = oneshot::channel();
        tx.send(WriterCommand::FlushLease {
            lease_key,
            ack: ack_tx,
        })
        .await
        .unwrap();
        ack_rx.await.unwrap();

        drop(tx);
        handle.await.unwrap().unwrap();

        let content = std::fs::read(&path).unwrap();
        assert!(content.iter().all(|&b| b == 0));
        assert_eq!(written.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn test_writer_drops_late_data_for_discarded_lease() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_late_data.bin");
        std::fs::write(&path, vec![0u8; 64]).unwrap();
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .await
            .unwrap();

        let budget = Arc::new(tokio::sync::Semaphore::new(32));
        let written = Arc::new(AtomicU64::new(0));
        let (tx, rx) = mpsc::channel(16);

        let writer = WriterTask::new(rx, file, written.clone(), 1024);
        let handle = tokio::spawn(writer.run());

        let lease_key = LeaseKey {
            piece_id: 0,
            lease_id: 1,
        };
        tx.send(WriterCommand::BeginLease { lease_key })
            .await
            .unwrap();

        let (ack_tx, ack_rx) = oneshot::channel();
        tx.send(WriterCommand::DiscardLease {
            lease_key,
            ack: ack_tx,
        })
        .await
        .unwrap();
        assert_eq!(ack_rx.await.unwrap(), 0);

        tx.send(WriterCommand::Data {
            permit: Some(budget.clone().acquire_many_owned(32).await.unwrap()),
            offset: 0,
            data: Bytes::from(vec![0xAB; 32]),
            lease_key: Some(lease_key),
        })
        .await
        .unwrap();

        drop(tx);
        handle.await.unwrap().unwrap();

        let content = std::fs::read(&path).unwrap();
        assert!(content.iter().all(|&b| b == 0));

        assert_eq!(budget.available_permits(), 32);
        assert_eq!(written.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn test_writer_keeps_same_piece_leases_isolated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_isolated_leases.bin");
        std::fs::write(&path, vec![0u8; 128]).unwrap();
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .await
            .unwrap();

        let written = Arc::new(AtomicU64::new(0));
        let (tx, rx) = mpsc::channel(16);

        let writer = WriterTask::new(rx, file, written.clone(), 1024);
        let handle = tokio::spawn(writer.run());

        let first_lease = LeaseKey {
            piece_id: 0,
            lease_id: 1,
        };
        let second_lease = LeaseKey {
            piece_id: 0,
            lease_id: 2,
        };

        tx.send(WriterCommand::BeginLease {
            lease_key: first_lease,
        })
        .await
        .unwrap();
        tx.send(WriterCommand::BeginLease {
            lease_key: second_lease,
        })
        .await
        .unwrap();

        tx.send(WriterCommand::Data {
            permit: None,
            offset: 0,
            data: Bytes::from(vec![0x11; 32]),
            lease_key: Some(first_lease),
        })
        .await
        .unwrap();
        tx.send(WriterCommand::Data {
            permit: None,
            offset: 32,
            data: Bytes::from(vec![0x22; 32]),
            lease_key: Some(second_lease),
        })
        .await
        .unwrap();

        let (discard_tx, discard_rx) = oneshot::channel();
        tx.send(WriterCommand::DiscardLease {
            lease_key: first_lease,
            ack: discard_tx,
        })
        .await
        .unwrap();
        assert_eq!(discard_rx.await.unwrap(), 32);

        let (flush_tx, flush_rx) = oneshot::channel();
        tx.send(WriterCommand::FlushLease {
            lease_key: second_lease,
            ack: flush_tx,
        })
        .await
        .unwrap();
        flush_rx.await.unwrap();

        drop(tx);
        handle.await.unwrap().unwrap();

        let content = std::fs::read(&path).unwrap();
        assert!(content[..32].iter().all(|&b| b == 0));
        assert!(content[32..64].iter().all(|&b| b == 0x22));
        assert_eq!(written.load(Ordering::Acquire), 64);
    }

    #[tokio::test]
    async fn test_writer_flush_all_without_sync() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_flush_all_no_sync.bin");
        std::fs::write(&path, vec![0u8; 128]).unwrap();
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .await
            .unwrap();

        let written = Arc::new(AtomicU64::new(0));
        let (tx, rx) = mpsc::channel(16);

        let writer = WriterTask::new(rx, file, written.clone(), 1024 * 1024);
        let handle = tokio::spawn(writer.run());

        let lease_key = LeaseKey {
            piece_id: 0,
            lease_id: 1,
        };
        tx.send(WriterCommand::BeginLease { lease_key })
            .await
            .unwrap();

        tx.send(WriterCommand::Data {
            permit: None,
            offset: 0,
            data: Bytes::from(vec![0x11; 64]),
            lease_key: Some(lease_key),
        })
        .await
        .unwrap();

        let (ack_tx, ack_rx) = oneshot::channel();
        tx.send(WriterCommand::FlushAll {
            sync_data: false,
            ack: ack_tx,
        })
        .await
        .unwrap();

        let stats = ack_rx.await.unwrap();
        drop(tx);
        handle.await.unwrap().unwrap();

        assert_eq!(stats.written_bytes, 64);
        assert!(stats.sync_elapsed.is_none());
    }

    #[tokio::test]
    async fn test_writer_high_watermark_flush() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_hwm.bin");
        std::fs::write(&path, vec![0u8; 1024]).unwrap();
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .await
            .unwrap();

        let written = Arc::new(AtomicU64::new(0));
        let (tx, rx) = mpsc::channel(16);

        // Set a very low high-water mark to force automatic flush
        let writer = WriterTask::new(rx, file, written.clone(), 50);
        let handle = tokio::spawn(writer.run());

        let lease_key = LeaseKey {
            piece_id: 0,
            lease_id: 1,
        };
        tx.send(WriterCommand::BeginLease { lease_key })
            .await
            .unwrap();

        // Send data that exceeds the high watermark
        tx.send(WriterCommand::Data {
            permit: None,
            offset: 0,
            data: Bytes::from(vec![0xEE; 100]),
            lease_key: Some(lease_key),
        })
        .await
        .unwrap();

        // Give writer time to process
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        drop(tx);
        handle.await.unwrap().unwrap();

        let content = std::fs::read(&path).unwrap();
        assert!(content[..100].iter().all(|&b| b == 0xEE));
    }
}
