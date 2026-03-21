use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::error::DownloadError;
use crate::storage::cache::WriteBackCache;

/// A command sent from workers to the writer task.
pub(crate) struct WriteCommand {
    pub offset: u64,
    pub data: Bytes,
    /// Which piece this data belongs to (for cache aggregation).
    pub piece_id: Option<usize>,
}

/// Signals from the session to the writer that don't carry data.
pub(crate) enum WriterControl {
    /// A piece is fully downloaded; flush its cached data to disk.
    FlushPiece(usize),
    /// Flush all cached data (e.g. on pause / shutdown).
    FlushAll,
}

/// Writer task: receives data chunks via a bounded channel, aggregates them
/// in a write-back cache, and flushes to disk on piece completion or budget limits.
pub(crate) struct WriterTask {
    data_rx: mpsc::Receiver<WriteCommand>,
    control_rx: mpsc::Receiver<WriterControl>,
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
        data_rx: mpsc::Receiver<WriteCommand>,
        control_rx: mpsc::Receiver<WriterControl>,
        file: tokio::fs::File,
        written_bytes: Arc<AtomicU64>,
        budget_return: Arc<tokio::sync::Semaphore>,
        cache_high_watermark: usize,
    ) -> Self {
        Self {
            data_rx,
            control_rx,
            file,
            written_bytes,
            cache: WriteBackCache::new(),
            budget_return,
            cache_high_watermark,
        }
    }

    pub async fn run(mut self) -> Result<(), DownloadError> {
        let mut ctrl_closed = false;
        let mut data_closed = false;

        loop {
            if data_closed && ctrl_closed {
                break;
            }

            tokio::select! {
                biased;

                // Process data first to avoid losing buffered writes
                cmd = self.data_rx.recv(), if !data_closed => {
                    match cmd {
                        Some(cmd) => {
                            let data_len = cmd.data.len();
                            match cmd.piece_id {
                                Some(piece_id) => {
                                    self.cache.insert(piece_id, cmd.offset, cmd.data);
                                }
                                None => {
                                    // Single-connection mode: write directly
                                    self.write_block(cmd.offset, &cmd.data).await?;
                                    self.budget_return.add_permits(data_len);
                                }
                            }

                            // If cache exceeds high watermark, force a full flush
                            if self.cache.total_bytes() >= self.cache_high_watermark {
                                self.flush_all().await?;
                            }
                        }
                        None => {
                            data_closed = true;
                        }
                    }
                }

                ctrl = self.control_rx.recv(), if !ctrl_closed => {
                    match ctrl {
                        Some(WriterControl::FlushPiece(piece_id)) => {
                            self.flush_piece(piece_id).await?;
                        }
                        Some(WriterControl::FlushAll) => {
                            self.flush_all().await?;
                        }
                        None => {
                            ctrl_closed = true;
                        }
                    }
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
