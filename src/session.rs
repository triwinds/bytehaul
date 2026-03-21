use std::time::Instant;

use futures::StreamExt;
use tokio::sync::{mpsc, watch};

use crate::config::DownloadSpec;
use crate::error::DownloadError;
use crate::http::worker::HttpWorker;
use crate::progress::{DownloadState, ProgressSnapshot};
use crate::storage::file::create_output_file;
use crate::storage::writer::{WriteCommand, WriterTask};

/// Run a single-connection download to completion.
pub(crate) async fn run_download(
    client: reqwest::Client,
    spec: DownloadSpec,
    progress_tx: watch::Sender<ProgressSnapshot>,
    cancel_rx: watch::Receiver<bool>,
) -> Result<(), DownloadError> {
    let (write_tx, write_rx) = mpsc::channel::<WriteCommand>(spec.channel_buffer);

    // Send initial GET request
    let worker = HttpWorker::new(client, &spec);
    let (response, meta) = worker.send_get().await?;

    // Create output file with optional pre-allocation
    let file =
        create_output_file(&spec.output_path, meta.content_length, spec.file_allocation).await?;

    // Spawn writer task
    let writer_handle = tokio::spawn(WriterTask::new(write_rx, file).run());

    // Stream response body to the writer channel
    let stream_result = stream_response_body(
        response,
        &write_tx,
        &progress_tx,
        cancel_rx,
        meta.content_length,
    )
    .await;

    // Signal writer to finish by dropping the sender
    drop(write_tx);

    // Wait for writer to complete
    let writer_result = writer_handle
        .await
        .map_err(|e| DownloadError::Other(format!("writer task panicked: {e}")))?;

    // Propagate errors: stream error takes priority over writer error
    if let Err(e) = &stream_result {
        // If cancelled, update progress; if failed, it's already set in stream_response_body
        if !matches!(e, DownloadError::Cancelled) {
            progress_tx.send_modify(|p| p.state = DownloadState::Failed);
        }
        return stream_result;
    }
    writer_result?;

    progress_tx.send_modify(|p| p.state = DownloadState::Completed);
    Ok(())
}

async fn stream_response_body(
    response: reqwest::Response,
    write_tx: &mpsc::Sender<WriteCommand>,
    progress_tx: &watch::Sender<ProgressSnapshot>,
    cancel_rx: watch::Receiver<bool>,
    total_size: Option<u64>,
) -> Result<(), DownloadError> {
    let mut stream = response.bytes_stream();
    let mut downloaded: u64 = 0;
    let start_time = Instant::now();
    let mut cancel_rx = cancel_rx;

    progress_tx.send_modify(|p| {
        p.total_size = total_size;
        p.state = DownloadState::Downloading;
        p.start_time = Some(start_time);
    });

    loop {
        tokio::select! {
            biased;

            result = cancel_rx.changed() => {
                if result.is_ok() && *cancel_rx.borrow_and_update() {
                    progress_tx.send_modify(|p| p.state = DownloadState::Cancelled);
                    return Err(DownloadError::Cancelled);
                }
            }

            chunk = stream.next() => {
                match chunk {
                    Some(Ok(data)) => {
                        let offset = downloaded;
                        downloaded += data.len() as u64;

                        if write_tx
                            .send(WriteCommand { offset, data })
                            .await
                            .is_err()
                        {
                            return Err(DownloadError::ChannelClosed);
                        }

                        let elapsed = start_time.elapsed().as_secs_f64();
                        let speed = if elapsed > 0.0 {
                            downloaded as f64 / elapsed
                        } else {
                            0.0
                        };

                        progress_tx.send_modify(|p| {
                            p.downloaded = downloaded;
                            p.speed_bytes_per_sec = speed;
                        });
                    }
                    Some(Err(e)) => {
                        progress_tx.send_modify(|p| p.state = DownloadState::Failed);
                        return Err(DownloadError::Http(e));
                    }
                    None => break,
                }
            }
        }
    }

    Ok(())
}
