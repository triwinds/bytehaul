use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use tokio::sync::{mpsc, watch};

use crate::config::DownloadSpec;
use crate::error::DownloadError;
use crate::http::response::ResponseMeta;
use crate::http::worker::HttpWorker;
use crate::progress::{DownloadState, ProgressSnapshot};
use crate::storage::control::ControlSnapshot;
use crate::storage::file::{create_output_file, open_existing_file};
use crate::storage::writer::{WriteCommand, WriterTask};

const CONTROL_SAVE_INTERVAL: Duration = Duration::from_secs(5);

/// Run a download to completion, with optional resume from a control file.
pub(crate) async fn run_download(
    client: reqwest::Client,
    spec: DownloadSpec,
    progress_tx: watch::Sender<ProgressSnapshot>,
    cancel_rx: watch::Receiver<bool>,
) -> Result<(), DownloadError> {
    let control_path = ControlSnapshot::control_path(&spec.output_path);

    // Try to load existing control file for resume
    let resume_state = if spec.resume {
        ControlSnapshot::load(&control_path).await.ok()
    } else {
        None
    };

    let worker = HttpWorker::new(client, &spec);

    let (response, meta, start_offset) = match resume_state {
        Some(ctrl) => match attempt_resume(&worker, &ctrl).await {
            Ok(result) => result,
            Err(_) => {
                // Resume failed — clean slate
                let _ = ControlSnapshot::delete(&control_path).await;
                let (resp, m) = worker.send_get().await?;
                (resp, m, 0u64)
            }
        },
        None => {
            let (resp, m) = worker.send_get().await?;
            (resp, m, 0u64)
        }
    };

    // Only use control file when total size is known
    let use_control = spec.resume && meta.content_length.is_some();

    // Open or create the output file
    let file = if start_offset > 0 {
        open_existing_file(&spec.output_path).await?
    } else {
        create_output_file(&spec.output_path, meta.content_length, spec.file_allocation).await?
    };

    // Writer with flush tracking
    let (write_tx, write_rx) = mpsc::channel::<WriteCommand>(spec.channel_buffer);
    let written_bytes = Arc::new(AtomicU64::new(start_offset));
    let writer_handle =
        tokio::spawn(WriterTask::new(write_rx, file, written_bytes.clone()).run());

    let total_size = meta.content_length.unwrap_or(0);
    let snapshot_template = ControlSnapshot {
        url: spec.url.clone(),
        total_size,
        downloaded_bytes: start_offset,
        etag: meta.etag.clone(),
        last_modified: meta.last_modified.clone(),
        piece_size: total_size,
    };

    // Stream body (with periodic control-file saves when applicable)
    let stream_result = stream_response_body(
        response,
        &write_tx,
        &progress_tx,
        cancel_rx,
        meta.content_length,
        start_offset,
        if use_control {
            Some((&written_bytes, &control_path, &snapshot_template))
        } else {
            None
        },
    )
    .await;

    // Signal writer to finish
    drop(write_tx);

    let writer_result = writer_handle
        .await
        .map_err(|e| DownloadError::Other(format!("writer panicked: {e}")))?;

    if let Err(ref e) = stream_result {
        // Persist control file so a future run can resume
        if use_control {
            let written = written_bytes.load(Ordering::Acquire);
            let mut ctrl = snapshot_template.clone();
            ctrl.downloaded_bytes = written;
            let _ = ctrl.save(&control_path).await;
        }
        if !matches!(e, DownloadError::Cancelled) {
            progress_tx.send_modify(|p| p.state = DownloadState::Failed);
        }
        return stream_result;
    }
    writer_result?;

    // Success — remove control file
    if use_control {
        let _ = ControlSnapshot::delete(&control_path).await;
    }
    progress_tx.send_modify(|p| p.state = DownloadState::Completed);
    Ok(())
}

/// Try to resume by sending a Range request and validating metadata.
async fn attempt_resume(
    worker: &HttpWorker,
    ctrl: &ControlSnapshot,
) -> Result<(reqwest::Response, ResponseMeta, u64), DownloadError> {
    let start = ctrl.downloaded_bytes;
    let end = ctrl.total_size.saturating_sub(1);

    let (response, meta) = worker.send_range(start, end).await?;

    // If server returned 200 instead of 206, it doesn't support Range
    if response.status().as_u16() == 200 {
        return Err(DownloadError::ResumeMismatch(
            "server returned 200 instead of 206".into(),
        ));
    }

    // Validate ETag
    if let (Some(expected), Some(actual)) = (&ctrl.etag, &meta.etag) {
        if expected != actual {
            return Err(DownloadError::ResumeMismatch(format!(
                "ETag mismatch: expected {expected}, got {actual}"
            )));
        }
    }

    // Validate Last-Modified
    if let (Some(expected), Some(actual)) = (&ctrl.last_modified, &meta.last_modified) {
        if expected != actual {
            return Err(DownloadError::ResumeMismatch(format!(
                "Last-Modified mismatch: expected {expected}, got {actual}"
            )));
        }
    }

    Ok((response, meta, start))
}

async fn stream_response_body(
    response: reqwest::Response,
    write_tx: &mpsc::Sender<WriteCommand>,
    progress_tx: &watch::Sender<ProgressSnapshot>,
    cancel_rx: watch::Receiver<bool>,
    total_size: Option<u64>,
    start_offset: u64,
    control: Option<(&Arc<AtomicU64>, &PathBuf, &ControlSnapshot)>,
) -> Result<(), DownloadError> {
    let mut stream = response.bytes_stream();
    let mut downloaded: u64 = start_offset;
    let start_time = Instant::now();
    let mut cancel_rx = cancel_rx;

    // Set up periodic control file saves
    let mut save_ticker = tokio::time::interval(CONTROL_SAVE_INTERVAL);
    save_ticker.tick().await; // skip first immediate tick

    progress_tx.send_modify(|p| {
        p.total_size = total_size;
        p.downloaded = start_offset;
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

            _ = save_ticker.tick(), if control.is_some() => {
                if let Some((written_bytes, ctrl_path, template)) = &control {
                    let written = written_bytes.load(Ordering::Acquire);
                    let mut snap = (*template).clone();
                    snap.downloaded_bytes = written;
                    let _ = snap.save(ctrl_path).await;
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
