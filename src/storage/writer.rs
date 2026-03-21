use bytes::Bytes;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::error::DownloadError;

/// A command sent from workers to the writer task.
pub(crate) struct WriteCommand {
    pub offset: u64,
    pub data: Bytes,
}

/// Writer task: receives data chunks via a bounded channel and writes them to disk.
pub(crate) struct WriterTask {
    rx: mpsc::Receiver<WriteCommand>,
    file: tokio::fs::File,
}

impl WriterTask {
    pub fn new(rx: mpsc::Receiver<WriteCommand>, file: tokio::fs::File) -> Self {
        Self { rx, file }
    }

    pub async fn run(mut self) -> Result<(), DownloadError> {
        while let Some(cmd) = self.rx.recv().await {
            self.file
                .seek(std::io::SeekFrom::Start(cmd.offset))
                .await?;
            self.file.write_all(&cmd.data).await?;
        }

        self.file.flush().await?;
        self.file.sync_all().await?;
        Ok(())
    }
}
