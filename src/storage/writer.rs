use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

use crate::error::DownloadError;
use crate::storage::cache::WriteBackCache;

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
        ack: oneshot::Sender<u64>,
    },
}

/// Writer task: receives data chunks via a bounded channel, aggregates them
/// in a write-back cache, and flushes to disk on piece completion or budget limits.
pub(crate) struct WriterTask {
    rx: mpsc::Receiver<WriterCommand>,
    file: tokio::fs::File,
    written_bytes: Arc<AtomicU64>,
    cache: WriteBackCache,
    /// Budget permit sender — returned when data is flushed to disk.
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
                    self.flush_all().await?;
                    if sync_data {
                        self.sync_file().await?;
                    }
                    let _ = ack.send(self.written_bytes.load(Ordering::Acquire));
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
        self.file
            .seek(std::io::SeekFrom::Start(offset))
            .await?;
        self.file.write_all(data).await?;
        let end = offset + data.len() as u64;
        // Update high-water mark
        self.written_bytes.fetch_max(end, Ordering::Release);
        Ok(())
    }
}
