use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::collections::HashSet;

use bytes::Bytes;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

use crate::error::DownloadError;
use crate::storage::cache::WriteBackCache;
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
        /// Which lease this data belongs to (for cache aggregation).
        lease_key: Option<LeaseKey>,
    },
    /// A lease is fully downloaded; flush its cached data to disk.
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
    active_leases: HashSet<LeaseKey>,
    /// Budget permit sender 鈥?returned when data is flushed to disk.
    budget_return: Arc<tokio::sync::Semaphore>,
    /// Maximum bytes to buffer before forcing a flush.
    cache_high_watermark: usize,
}

impl WriterTask {
    pub fn new(
        rx: mpsc::Receiver<WriterCommand>,
        file: tokio::fs::File,
        written_bytes: Arc<AtomicU64>,
        budget_return: Arc<tokio::sync::Semaphore>,
        cache_high_watermark: usize,
    ) -> Self {
        Self {
            rx,
            file,
            written_bytes,
            cache: WriteBackCache::new(),
            active_leases: HashSet::new(),
            budget_return,
            cache_high_watermark,
        }
    }

    pub async fn run(mut self) -> Result<(), DownloadError> {
        while let Some(cmd) = self.rx.recv().await {
            match cmd {
                WriterCommand::BeginLease { lease_key } => {
                    self.active_leases.insert(lease_key);
                }
                WriterCommand::Data {
                    offset,
                    data,
                    lease_key,
                } => {
                    let data_len = data.len();
                    match lease_key {
                        Some(lease_key) if self.active_leases.contains(&lease_key) => {
                            self.cache.insert(lease_key, offset, data);
                        }
                        Some(_) => {
                            self.budget_return.add_permits(data_len);
                        }
                        None => {
                            // Single-connection mode: write directly.
                            self.write_block(offset, &data).await?;
                            self.budget_return.add_permits(data_len);
                        }
                    }

                    if self.cache.total_bytes() >= self.cache_high_watermark {
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
                    if discarded > 0 {
                        self.budget_return.add_permits(discarded);
                    }
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
        self.file.flush().await?;
        self.file.sync_all().await?;
        Ok(())
    }

    async fn flush_lease(&mut self, lease_key: LeaseKey) -> Result<(), DownloadError> {
        let blocks = self.cache.drain_lease(lease_key);
        let mut flushed = 0usize;
        for block in blocks {
            self.write_block(block.offset, &block.data).await?;
            flushed += block.data.len();
        }
        if flushed > 0 {
            self.budget_return.add_permits(flushed);
        }
        Ok(())
    }

    async fn flush_all(&mut self) -> Result<(), DownloadError> {
        let blocks = self.cache.drain_all();
        let mut flushed = 0usize;
        for block in blocks {
            self.write_block(block.offset, &block.data).await?;
            flushed += block.data.len();
        }
        if flushed > 0 {
            self.budget_return.add_permits(flushed);
        }
        Ok(())
    }

    async fn sync_file(&mut self) -> Result<(), DownloadError> {
        self.file.flush().await?;
        self.file.sync_data().await?;
        Ok(())
    }

    async fn write_block(&mut self, offset: u64, data: &[u8]) -> Result<(), DownloadError> {
        self.file.seek(std::io::SeekFrom::Start(offset)).await?;
        self.file.write_all(data).await?;
        let end = offset + data.len() as u64;
        // Update high-water mark
        self.written_bytes.fetch_max(end, Ordering::Release);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_writer_direct_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.bin");
        let file = tokio::fs::File::create(&path).await.unwrap();

        let budget = Arc::new(tokio::sync::Semaphore::new(1024 * 1024));
        let written = Arc::new(AtomicU64::new(0));
        let (tx, rx) = mpsc::channel(16);

        let writer = WriterTask::new(rx, file, written.clone(), budget.clone(), 1024 * 1024);
        let handle = tokio::spawn(writer.run());

        // Send data with no piece_id (direct write)
        tx.send(WriterCommand::Data {
            offset: 0,
            data: Bytes::from(vec![0xAA; 100]),
            lease_key: None,
        })
        .await
        .unwrap();

        tx.send(WriterCommand::Data {
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

        let budget = Arc::new(tokio::sync::Semaphore::new(1024 * 1024));
        let written = Arc::new(AtomicU64::new(0));
        let (tx, rx) = mpsc::channel(16);

        let writer = WriterTask::new(rx, file, written.clone(), budget.clone(), 1024 * 1024);
        let handle = tokio::spawn(writer.run());

        let lease_key = LeaseKey {
            piece_id: 0,
            lease_id: 1,
        };

        tx.send(WriterCommand::BeginLease { lease_key }).await.unwrap();

        // Send data with piece_id (cached write)
        tx.send(WriterCommand::Data {
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

        let budget = Arc::new(tokio::sync::Semaphore::new(1024 * 1024));
        let written = Arc::new(AtomicU64::new(0));
        let (tx, rx) = mpsc::channel(16);

        let writer = WriterTask::new(rx, file, written.clone(), budget.clone(), 1024 * 1024);
        let handle = tokio::spawn(writer.run());

        let lease_key = LeaseKey {
            piece_id: 0,
            lease_id: 1,
        };
        tx.send(WriterCommand::BeginLease { lease_key }).await.unwrap();

        tx.send(WriterCommand::Data {
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

        let budget = Arc::new(tokio::sync::Semaphore::new(1024));
        let written = Arc::new(AtomicU64::new(0));
        let (tx, rx) = mpsc::channel(16);

        let writer = WriterTask::new(rx, file, written.clone(), budget.clone(), 1024);
        let handle = tokio::spawn(writer.run());

        let lease_key = LeaseKey {
            piece_id: 0,
            lease_id: 1,
        };
        tx.send(WriterCommand::BeginLease { lease_key }).await.unwrap();

        tx.send(WriterCommand::Data {
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

        let budget = Arc::new(tokio::sync::Semaphore::new(0));
        let written = Arc::new(AtomicU64::new(0));
        let (tx, rx) = mpsc::channel(16);

        let writer = WriterTask::new(rx, file, written.clone(), budget.clone(), 1024);
        let handle = tokio::spawn(writer.run());

        let lease_key = LeaseKey {
            piece_id: 0,
            lease_id: 1,
        };
        tx.send(WriterCommand::BeginLease { lease_key }).await.unwrap();

        let (ack_tx, ack_rx) = oneshot::channel();
        tx.send(WriterCommand::DiscardLease { lease_key, ack: ack_tx })
            .await
            .unwrap();
        assert_eq!(ack_rx.await.unwrap(), 0);

        tx.send(WriterCommand::Data {
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

        let budget = Arc::new(tokio::sync::Semaphore::new(1024));
        let written = Arc::new(AtomicU64::new(0));
        let (tx, rx) = mpsc::channel(16);

        let writer = WriterTask::new(rx, file, written.clone(), budget.clone(), 1024);
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
            offset: 0,
            data: Bytes::from(vec![0x11; 32]),
            lease_key: Some(first_lease),
        })
        .await
        .unwrap();
        tx.send(WriterCommand::Data {
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

        let budget = Arc::new(tokio::sync::Semaphore::new(1024 * 1024));
        let written = Arc::new(AtomicU64::new(0));
        let (tx, rx) = mpsc::channel(16);

        let writer = WriterTask::new(rx, file, written.clone(), budget.clone(), 1024 * 1024);
        let handle = tokio::spawn(writer.run());

        let lease_key = LeaseKey {
            piece_id: 0,
            lease_id: 1,
        };
        tx.send(WriterCommand::BeginLease { lease_key }).await.unwrap();

        tx.send(WriterCommand::Data {
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

        let budget = Arc::new(tokio::sync::Semaphore::new(1024 * 1024));
        let written = Arc::new(AtomicU64::new(0));
        let (tx, rx) = mpsc::channel(16);

        // Set a very low high-water mark to force automatic flush
        let writer = WriterTask::new(rx, file, written.clone(), budget.clone(), 50);
        let handle = tokio::spawn(writer.run());

        let lease_key = LeaseKey {
            piece_id: 0,
            lease_id: 1,
        };
        tx.send(WriterCommand::BeginLease { lease_key }).await.unwrap();

        // Send data that exceeds the high watermark
        tx.send(WriterCommand::Data {
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
