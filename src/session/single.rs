use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use tokio::sync::{mpsc, watch, Semaphore};

use super::{
    flush_all_and_wait, stop_signal_error, stop_signal_state,
    ETA_ALPHA, SINGLE_ETA_SAMPLE_INTERVAL, StopSignal,
};
use crate::config::{DownloadSpec, LogLevel};
use crate::error::DownloadError;
use crate::eta::EtaEstimator;
use crate::http::response::ResponseMeta;
use crate::progress::{DownloadState, ProgressSnapshot};
use crate::rate_limiter::SpeedLimit;
use crate::storage::control::ControlSnapshot;
use crate::storage::file::{create_output_file, open_existing_file};
use crate::storage::writer::{WriterCommand, WriterTask};

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_single_connection(
    response: reqwest::Response,
    meta: &ResponseMeta,
    spec: &DownloadSpec,
    output_path: &Path,
    start_offset: u64,
    progress_tx: &watch::Sender<ProgressSnapshot>,
    cancel_rx: watch::Receiver<StopSignal>,
    control_path: &Path,
    total_size: Option<u64>,
    speed_limit: SpeedLimit,
    log_level: LogLevel,
    download_id: u64,
) -> Result<(), DownloadError> {
    let use_control = spec.resume && total_size.is_some();
    log_debug!(log_level, download_id = download_id,
        start_offset = start_offset,
        total_size = total_size.unwrap_or(0),
        resume_control = use_control,
        "single-connection download started");

    let file = if start_offset > 0 {
        open_existing_file(output_path).await?
    } else {
        create_output_file(output_path, total_size, spec.file_allocation).await?
    };

    let budget = Arc::new(Semaphore::new(spec.memory_budget));
    let (write_tx, write_rx) = mpsc::channel::<WriterCommand>(spec.channel_buffer);
    let written_bytes = Arc::new(AtomicU64::new(start_offset));
    let writer_handle = tokio::spawn(
        WriterTask::new(
            write_rx,
            file,
            written_bytes.clone(),
            budget.clone(),
            spec.memory_budget,
        )
        .run(),
    );

    let ts = total_size.unwrap_or(0);
    let snap_template = ControlSnapshot {
        url: spec.url.clone(),
        total_size: ts,
        piece_size: ts,
        piece_count: 1,
        completed_bitset: vec![0],
        downloaded_bytes: start_offset,
        etag: meta.etag.clone(),
        last_modified: meta.last_modified.clone(),
    };

    let stream_result = stream_single(
        response,
        &write_tx,
        progress_tx,
        cancel_rx,
        total_size,
        start_offset,
        if use_control {
            Some((control_path, &snap_template))
        } else {
            None
        },
        budget.clone(),
        &speed_limit,
        spec.control_save_interval,
    )
    .await;

    drop(write_tx);
    let writer_result = writer_handle
        .await
        .map_err(|e| DownloadError::TaskFailed(format!("writer panicked: {e}")))?;

    if let Err(ref e) = stream_result {
        if use_control {
            let w = written_bytes.load(Ordering::Acquire);
            let mut s = snap_template.clone();
            s.downloaded_bytes = w;
            let _ = s.save(control_path).await;
        }
        if !matches!(e, DownloadError::Cancelled | DownloadError::Paused) {
            log_error!(log_level, download_id = download_id, error = %e,
                "single-connection download failed");
            progress_tx.send_modify(|p| p.state = DownloadState::Failed);
        }
        return stream_result;
    }
    writer_result?;

    if use_control {
        let _ = ControlSnapshot::delete(control_path).await;
    }
    log_debug!(log_level, download_id = download_id, "single-connection download completed");
    progress_tx.send_modify(|p| {
        p.state = DownloadState::Completed;
        p.eta_secs = Some(0.0);
    });
    Ok(())
}

/// Stream a single HTTP response body to the writer channel.
#[allow(clippy::too_many_arguments)]
async fn stream_single(
    response: reqwest::Response,
    write_tx: &mpsc::Sender<WriterCommand>,
    progress_tx: &watch::Sender<ProgressSnapshot>,
    cancel_rx: watch::Receiver<StopSignal>,
    total_size: Option<u64>,
    start_offset: u64,
    control: Option<(&Path, &ControlSnapshot)>,
    budget: Arc<Semaphore>,
    speed_limit: &SpeedLimit,
    control_save_interval: Duration,
) -> Result<(), DownloadError> {
    let mut stream = response.bytes_stream();
    let mut downloaded: u64 = start_offset;
    let start_time = Instant::now();
    let mut eta_estimator = EtaEstimator::new(ETA_ALPHA);
    let mut last_sample_time = start_time;
    let mut bytes_since_last_sample = 0u64;
    let mut cancel_rx = cancel_rx;
    let mut save_ticker = tokio::time::interval(control_save_interval);
    save_ticker.tick().await;

    progress_tx.send_modify(|p| {
        p.total_size = total_size;
        p.downloaded = start_offset;
        p.state = DownloadState::Downloading;
        p.start_time = Some(start_time);
        p.eta_secs = None;
    });

    loop {
        tokio::select! {
            biased;

            result = cancel_rx.changed() => {
                if result.is_ok() {
                    let signal = *cancel_rx.borrow_and_update();
                    if let Some(error) = stop_signal_error(signal) {
                        if let Some(state) = stop_signal_state(signal) {
                            progress_tx.send_modify(|p| p.state = state);
                        }
                        if let Some((cp, tmpl)) = &control {
                            if let Ok(w) = flush_all_and_wait(write_tx, true).await {
                                let mut s = (*tmpl).clone();
                                s.downloaded_bytes = w;
                                let _ = s.save(cp).await;
                            }
                        }
                        return Err(error);
                    }
                }
            }

            _ = save_ticker.tick(), if control.is_some() => {
                if let Some((cp, tmpl)) = &control {
                    if let Ok(w) = flush_all_and_wait(write_tx, true).await {
                        let mut s = (*tmpl).clone();
                        s.downloaded_bytes = w;
                        let _ = s.save(cp).await;
                    }
                }
            }

            chunk = stream.next() => {
                match chunk {
                    Some(Ok(data)) => {
                        let len = data.len();
                        // Rate limiting
                        speed_limit.acquire(len).await;
                        // Acquire budget permits before buffering data
                        let permit = budget
                            .acquire_many(len as u32)
                            .await
                            .map_err(|_| DownloadError::Internal("budget semaphore closed".into()))?;

                        let offset = downloaded;
                        downloaded += len as u64;
                        bytes_since_last_sample += len as u64;
                        if write_tx.send(WriterCommand::Data { offset, data, piece_id: None }).await.is_err() {
                            return Err(DownloadError::ChannelClosed);
                        }
                        permit.forget(); // permits returned by writer after flush
                        let elapsed = start_time.elapsed().as_secs_f64();
                        let speed = if elapsed > 0.0 {
                            downloaded.saturating_sub(start_offset) as f64 / elapsed
                        } else {
                            0.0
                        };
                        let now = Instant::now();
                        if now.duration_since(last_sample_time) >= SINGLE_ETA_SAMPLE_INTERVAL {
                            let delta_secs = now.duration_since(last_sample_time).as_secs_f64();
                            eta_estimator.update_sample(bytes_since_last_sample, delta_secs);
                            last_sample_time = now;
                            bytes_since_last_sample = 0;
                        }
                        let eta_secs = total_size.and_then(|total| {
                            let remaining = total.saturating_sub(downloaded);
                            if remaining == 0 {
                                Some(0.0)
                            } else {
                                eta_estimator.estimate(remaining)
                            }
                        });
                        progress_tx.send_modify(|p| {
                            p.downloaded = downloaded;
                            p.speed_bytes_per_sec = speed;
                            p.eta_secs = eta_secs;
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
