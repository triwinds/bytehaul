use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

use crate::error::DownloadError;
use crate::storage::cache::WriteBackCache;

#[derive(Debug, Clone, Copy)]
pub(crate) struct FlushAllStats {
    pub written_bytes: u64,
    pub flush_elapsed: Duration,
    pub sync_elapsed: Option<Duration>,
}

pub(crate) enum WriterCommand {
    Data {
        offset: u64,
        data: Bytes,
        /// Which piece this data belongs to (for cache aggregation).
        piece_id: Option<usize>,
    },
    /// A piece is fully downloaded; flush its cached data to disk.
    FlushPiece {
        piece_id: usize,
        ack: oneshot::Sender<()>,
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
            budget_return,
            cache_high_watermark,
        }
    }

    pub async fn run(mut self) -> Result<(), DownloadError> {
        while let Some(cmd) = self.rx.recv().await {
            match cmd {
                WriterCommand::Data {
                    offset,
                    data,
                    piece_id,
                } => {
                    let data_len = data.len();
                    match piece_id {
                        Some(piece_id) => {
                            self.cache.insert(piece_id, offset, data);
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
                WriterCommand::FlushPiece { piece_id, ack } => {
                    self.flush_piece(piece_id).await?;
                    let _ = ack.send(());
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

    async fn flush_piece(&mut self, piece_id: usize) -> Result<(), DownloadError> {
        let blocks = self.cache.drain_piece(piece_id);
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
            piece_id: None,
        })
        .await
        .unwrap();

        tx.send(WriterCommand::Data {
            offset: 100,
            data: Bytes::from(vec![0xBB; 100]),
            piece_id: None,
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

        // Send data with piece_id (cached write)
        tx.send(WriterCommand::Data {
            offset: 0,
            data: Bytes::from(vec![0xCC; 100]),
            piece_id: Some(0),
        })
        .await
        .unwrap();

        // Flush piece
        let (ack_tx, ack_rx) = oneshot::channel();
        tx.send(WriterCommand::FlushPiece {
            piece_id: 0,
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

        tx.send(WriterCommand::Data {
            offset: 0,
            data: Bytes::from(vec![0xDD; 50]),
            piece_id: Some(0),
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

        tx.send(WriterCommand::Data {
            offset: 0,
            data: Bytes::from(vec![0x11; 64]),
            piece_id: Some(0),
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

        // Send data that exceeds the high watermark
        tx.send(WriterCommand::Data {
            offset: 0,
            data: Bytes::from(vec![0xEE; 100]),
            piece_id: Some(0),
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
