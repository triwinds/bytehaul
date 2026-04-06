use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use tokio::sync::{mpsc, watch, Semaphore};

use super::{
    flush_all_and_wait, stop_signal_error, stop_signal_state, ControlSaveReason,
    ControlSaveTracker,
    MIN_SPEED_SAMPLE_SPAN, SPEED_ESTIMATE_WINDOW, StopSignal,
};
use crate::config::{DownloadSpec, LogLevel};
use crate::error::DownloadError;
use crate::eta::EtaEstimator;
use crate::http::response::ResponseMeta;
use crate::progress::{
    DownloadState, ProgressReporter, ProgressSnapshot, ProgressUpdate, PROGRESS_REPORT_BYTES,
    PROGRESS_REPORT_INTERVAL,
};
use crate::rate_limiter::SpeedLimit;
use crate::storage::control::ControlSnapshot;
use crate::storage::file::{create_output_file, open_existing_file};
use crate::storage::writer::{WriterCommand, WriterTask};

#[derive(Debug, Clone, Copy)]
struct SingleStreamSummary {
    downloaded: u64,
    speed_bytes_per_sec: f64,
}

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
    let mut control_save_tracker = ControlSaveTracker::new(start_offset);

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
        &mut control_save_tracker,
        spec.autosave_sync_every,
        log_level,
        download_id,
    )
    .await;

    drop(write_tx);
    let writer_result = writer_handle
        .await
        .map_err(|e| DownloadError::TaskFailed(format!("writer panicked: {e}")))?;

    if let Err(ref e) = stream_result {
        if use_control {
            persist_single_control_snapshot(
                ControlSaveReason::Terminal,
                written_bytes.load(Ordering::Acquire),
                None,
                control_path,
                &snap_template,
                &mut control_save_tracker,
                spec.autosave_sync_every,
                log_level,
                download_id,
            )
            .await;
        }
        if !matches!(e, DownloadError::Cancelled | DownloadError::Paused) {
            log_error!(log_level, download_id = download_id, error = %e,
                "single-connection download failed");
        }
        return stream_result.map(|_| ());
    }
    let summary = stream_result?;
    if let Err(error) = writer_result {
        if use_control {
            persist_single_control_snapshot(
                ControlSaveReason::Terminal,
                written_bytes.load(Ordering::Acquire),
                None,
                control_path,
                &snap_template,
                &mut control_save_tracker,
                spec.autosave_sync_every,
                log_level,
                download_id,
            )
            .await;
        }
        log_error!(log_level, download_id = download_id, error = %error,
            "single-connection writer failed");
        progress_tx.send_modify(|p| {
            p.downloaded = summary.downloaded;
            p.speed_bytes_per_sec = summary.speed_bytes_per_sec;
            p.state = DownloadState::Failed;
        });
        return Err(error);
    }

    if use_control {
        let _ = ControlSnapshot::delete(control_path).await;
    }
    log_debug!(log_level, download_id = download_id, "single-connection download completed");
    progress_tx.send_modify(|p| {
        p.downloaded = summary.downloaded;
        p.speed_bytes_per_sec = summary.speed_bytes_per_sec;
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
    control_save_tracker: &mut ControlSaveTracker,
    autosave_sync_every: u32,
    log_level: LogLevel,
    download_id: u64,
) -> Result<SingleStreamSummary, DownloadError> {
    let mut stream = response.bytes_stream();
    let mut downloaded: u64 = start_offset;
    let start_time = Instant::now();
    let mut eta_estimator = EtaEstimator::new(SPEED_ESTIMATE_WINDOW, MIN_SPEED_SAMPLE_SPAN);
    let mut progress_reporter = ProgressReporter::new(
        start_offset,
        PROGRESS_REPORT_INTERVAL,
        PROGRESS_REPORT_BYTES,
        start_time,
    );
    let mut cancel_rx = cancel_rx;
    let mut save_ticker = tokio::time::interval(control_save_interval);
    save_ticker.tick().await;
    eta_estimator.record(start_offset, start_time);
    let mut last_speed = 0.0;
    let mut last_eta_secs = None;

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
                            progress_reporter.force_report(
                                progress_tx,
                                ProgressUpdate::new(downloaded, last_speed, last_eta_secs)
                                    .with_state(state),
                                Instant::now(),
                            );
                        }
                        if let Some((cp, tmpl)) = &control {
                            persist_single_control_snapshot(
                                ControlSaveReason::Terminal,
                                downloaded,
                                Some(write_tx),
                                cp,
                                tmpl,
                                control_save_tracker,
                                autosave_sync_every,
                                log_level,
                                download_id,
                            )
                            .await;
                        }
                        return Err(error);
                    }
                }
            }

            _ = save_ticker.tick(), if control.is_some() => {
                if let Some((cp, tmpl)) = &control {
                    persist_single_control_snapshot(
                        ControlSaveReason::Autosave,
                        downloaded,
                        Some(write_tx),
                        cp,
                        tmpl,
                        control_save_tracker,
                        autosave_sync_every,
                        log_level,
                        download_id,
                    )
                    .await;
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
                        if write_tx.send(WriterCommand::Data { offset, data, piece_id: None }).await.is_err() {
                            return Err(DownloadError::ChannelClosed);
                        }
                        permit.forget(); // permits returned by writer after flush
                        let now = Instant::now();
                        eta_estimator.record(downloaded, now);
                        let speed = eta_estimator.speed_bytes_per_sec().unwrap_or(0.0);
                        let eta_secs = total_size.and_then(|total| {
                            let remaining = total.saturating_sub(downloaded);
                            if remaining == 0 {
                                Some(0.0)
                            } else {
                                eta_estimator.estimate(remaining)
                            }
                        });
                        last_speed = speed;
                        last_eta_secs = eta_secs;
                        progress_reporter.report_if_due(
                            progress_tx,
                            ProgressUpdate::new(downloaded, speed, eta_secs),
                            now,
                        );
                    }
                    Some(Err(e)) => {
                        progress_reporter.force_report(
                            progress_tx,
                            ProgressUpdate::new(downloaded, last_speed, last_eta_secs)
                                .with_state(DownloadState::Failed),
                            Instant::now(),
                        );
                        return Err(DownloadError::Http(e));
                    }
                    None => break,
                }
            }
        }
    }
    Ok(SingleStreamSummary {
        downloaded,
        speed_bytes_per_sec: last_speed,
    })
}

async fn persist_single_control_snapshot(
    reason: ControlSaveReason,
    downloaded: u64,
    write_tx: Option<&mpsc::Sender<WriterCommand>>,
    control_path: &Path,
    snap_template: &ControlSnapshot,
    control_save_tracker: &mut ControlSaveTracker,
    autosave_sync_every: u32,
    log_level: LogLevel,
    download_id: u64,
) {
    if !control_save_tracker.should_save(reason, downloaded, autosave_sync_every) {
        if matches!(reason, ControlSaveReason::Autosave)
            && downloaded > control_save_tracker.last_saved_downloaded_bytes()
        {
            log_debug!(log_level, download_id = download_id,
                checkpoint = reason.label(), downloaded_bytes = downloaded,
                pending_autosaves = control_save_tracker.pending_autosaves(),
                autosave_sync_every = autosave_sync_every,
                "control snapshot deferred");
        }
        return;
    }

    let flush_stats = match write_tx {
        Some(write_tx) => match flush_all_and_wait(write_tx, true).await {
            Ok(stats) => Some(stats),
            Err(error) => {
                log_warn!(log_level, download_id = download_id, checkpoint = reason.label(),
                    error = %error, "control snapshot flush failed");
                return;
            }
        },
        None => None,
    };
    let persisted_downloaded = flush_stats.map_or(downloaded, |stats| stats.written_bytes);
    if persisted_downloaded <= control_save_tracker.last_saved_downloaded_bytes() {
        return;
    }

    let mut snapshot = snap_template.clone();
    snapshot.downloaded_bytes = persisted_downloaded;
    let save_started = Instant::now();
    match snapshot.save(control_path).await {
        Ok(()) => {
            control_save_tracker.mark_saved(persisted_downloaded);
            log_debug!(log_level, download_id = download_id,
                checkpoint = reason.label(), downloaded_bytes = persisted_downloaded,
                flush_all_ms = flush_stats.map(|stats| stats.flush_elapsed.as_millis() as u64).unwrap_or(0),
                sync_data_ms = flush_stats.and_then(|stats| stats.sync_elapsed.map(|elapsed| elapsed.as_millis() as u64)).unwrap_or(0),
                control_save_ms = save_started.elapsed().as_millis() as u64,
                "control snapshot saved");
        }
        Err(error) => {
            log_warn!(log_level, download_id = download_id, checkpoint = reason.label(),
                error = %error, "control snapshot save failed");
        }
    }
}
