use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::stream::FuturesUnordered;
use futures::StreamExt;
use tokio::sync::{mpsc, watch, Semaphore};

use super::{
    flush_all_and_wait, flush_piece_and_wait, range_response_allowed,
    ControlSaveReason, ControlSaveTracker,
    stop_signal_error, stop_signal_label, stop_signal_state,
    MIN_SPEED_SAMPLE_SPAN, MULTI_PROGRESS_INTERVAL, SPEED_ESTIMATE_WINDOW, StopSignal,
};
use crate::config::{DownloadSpec, LogLevel};
use crate::error::DownloadError;
use crate::eta::EtaEstimator;
use crate::http::request::build_range_request;
use crate::http::response::ResponseMeta;
use crate::progress::{DownloadState, ProgressReporter, ProgressSnapshot, ProgressUpdate};
use crate::rate_limiter::SpeedLimit;
use crate::scheduler::{Scheduler, SchedulerState};
use crate::storage::control::ControlSnapshot;
use crate::storage::file::{create_output_file, open_existing_file};
use crate::storage::piece_map::PieceMap;
use crate::storage::segment::Segment;
use crate::storage::writer::{WriterCommand, WriterTask};

struct MultiControlSaveContext<'a> {
    spec: &'a DownloadSpec,
    meta: &'a ResponseMeta,
    scheduler: &'a Scheduler,
    total_size: u64,
    control_path: &'a Path,
    log_level: LogLevel,
    download_id: u64,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_multi_worker(
    client: reqwest::Client,
    spec: &DownloadSpec,
    output_path: &Path,
    meta: &ResponseMeta,
    total_size: u64,
    piece_map: PieceMap,
    probe_response: Option<(reqwest::Response, usize)>,
    progress_tx: &watch::Sender<ProgressSnapshot>,
    cancel_rx: watch::Receiver<StopSignal>,
    control_path: &Path,
    speed_limit: SpeedLimit,
    log_level: LogLevel,
    download_id: u64,
) -> Result<(), DownloadError> {
    // File
    let file = if piece_map.completed_count() > 0 {
        open_existing_file(output_path).await?
    } else {
        create_output_file(output_path, Some(total_size), spec.file_allocation).await?
    };

    // Memory budget semaphore
    let budget = Arc::new(Semaphore::new(spec.memory_budget));

    // Writer with cache
    let (write_tx, write_rx) = mpsc::channel::<WriterCommand>(spec.channel_buffer);
    let written_bytes = Arc::new(AtomicU64::new(0));
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

    // Scheduler
    let initial_downloaded = piece_map.completed_bytes();
    let scheduler: Scheduler = Arc::new(parking_lot::Mutex::new(SchedulerState::new(piece_map)));

    // Shared progress counter
    let downloaded = Arc::new(AtomicU64::new(initial_downloaded));
    let start_time = Instant::now();
    let mut eta_estimator = EtaEstimator::new(SPEED_ESTIMATE_WINDOW, MIN_SPEED_SAMPLE_SPAN);
    let mut progress_reporter =
        ProgressReporter::new(initial_downloaded, MULTI_PROGRESS_INTERVAL, 0, start_time);
    eta_estimator.record(initial_downloaded, start_time);

    progress_tx.send_modify(|p| {
        p.total_size = Some(total_size);
        p.downloaded = initial_downloaded;
        p.state = DownloadState::Downloading;
        p.start_time = Some(start_time);
        p.eta_secs = None;
    });

    // Spawn workers
    let remaining = scheduler.lock().remaining_count();
    let num_workers = (spec.max_connections as usize).min(remaining).max(1);
    log_info!(log_level, download_id = download_id,
        workers = num_workers, remaining_pieces = remaining,
        total_size = total_size, initial_downloaded = initial_downloaded,
        "multi-worker download started");

    let mut abort_handles = Vec::with_capacity(num_workers);
    let mut workers = FuturesUnordered::new();
    let mut probe_response = probe_response;
    let save_write_tx = write_tx.clone();
    let mut control_save_tracker = ControlSaveTracker::new(initial_downloaded);
    let control_save_ctx = MultiControlSaveContext {
        spec,
        meta,
        scheduler: &scheduler,
        total_size,
        control_path,
        log_level,
        download_id,
    };

    let worker_cfg = Arc::new(WorkerConfig {
        url: spec.url.clone(),
        headers: spec.headers.clone(),
        read_timeout: spec.read_timeout,
        max_retries: spec.max_retries,
        retry_base_delay: spec.retry_base_delay,
        retry_max_delay: spec.retry_max_delay,
        max_retry_elapsed: spec.max_retry_elapsed,
    });

    for worker_id in 0..num_workers {
        let first = if worker_id == 0 {
            probe_response.take()
        } else {
            None
        };
        let handle = tokio::spawn(worker_loop(
            worker_id,
            client.clone(),
            worker_cfg.clone(),
            scheduler.clone(),
            write_tx.clone(),
            downloaded.clone(),
            cancel_rx.clone(),
            budget.clone(),
            speed_limit.clone(),
            first,
            log_level,
            download_id,
        ));
        abort_handles.push(handle.abort_handle());
        workers.push(handle);
    }

    // Drop our sender so writer can finish once workers are done
    drop(write_tx);

    // ── Monitor loop ──
    let mut cancel_rx = cancel_rx;
    let mut save_ticker = tokio::time::interval(spec.control_save_interval);
    save_ticker.tick().await;
    let mut progress_interval = tokio::time::interval(MULTI_PROGRESS_INTERVAL);
    progress_interval.tick().await;

    let mut download_error: Option<DownloadError> = None;

    loop {
        tokio::select! {
            biased;

            result = cancel_rx.changed() => {
                if result.is_ok() {
                    let signal = *cancel_rx.borrow_and_update();
                    if let Some(error) = stop_signal_error(signal) {
                        log_info!(
                            log_level,
                            download_id = download_id,
                            stop = stop_signal_label(signal),
                            "multi-worker download stopped"
                        );
                        for ah in &abort_handles { ah.abort(); }
                        if let Some(state) = stop_signal_state(signal) {
                            let now = Instant::now();
                            let update = sampled_progress_update(
                                &mut eta_estimator,
                                downloaded.load(Ordering::Relaxed),
                                total_size,
                                now,
                            )
                            .with_state(state);
                            progress_reporter.force_report(progress_tx, update, now);
                        }
                        if spec.resume {
                            persist_multi_control_snapshot(
                                ControlSaveReason::Terminal,
                                Some(&save_write_tx),
                                &mut control_save_tracker,
                                &control_save_ctx,
                            ).await;
                        }
                        download_error = Some(error);
                        break;
                    }
                }
            }

            _ = save_ticker.tick(), if spec.resume => {
                persist_multi_control_snapshot(
                    ControlSaveReason::Autosave,
                    Some(&save_write_tx),
                    &mut control_save_tracker,
                    &control_save_ctx,
                ).await;
            }

            _ = progress_interval.tick() => {
                let now = Instant::now();
                let update = sampled_progress_update(
                    &mut eta_estimator,
                    downloaded.load(Ordering::Relaxed),
                    total_size,
                    now,
                );
                progress_reporter.force_report(progress_tx, update, now);
            }

            result = workers.next() => {
                match result {
                    Some(Ok(Ok(()))) => {
                        if workers.is_empty() { break; }
                    }
                    Some(Ok(Err(e))) => {
                        if download_error.is_none() && !matches!(e, DownloadError::ChannelClosed) {
                            log_warn!(log_level, download_id = download_id,
                                error = %e, "worker failed");
                            download_error = Some(e);
                        }
                        if workers.is_empty() { break; }
                    }
                    Some(Err(join_err)) => {
                        if !join_err.is_cancelled() {
                            log_error!(log_level, download_id = download_id,
                                error = %join_err, "worker panicked");
                            download_error = Some(DownloadError::TaskFailed(
                                format!("worker panicked: {join_err}"),
                            ));
                        }
                        if workers.is_empty() { break; }
                    }
                    None => break,
                }
            }
        }
    }

    // Drain remaining workers on error
    if download_error.is_some() {
        for ah in &abort_handles {
            ah.abort();
        }
        while workers.next().await.is_some() {}
    }

    drop(save_write_tx);

    // Wait for writer
    let writer_result = writer_handle
        .await
        .map_err(|e| DownloadError::TaskFailed(format!("writer panicked: {e}")))?;

    if let Some(e) = download_error {
        if spec.resume {
            persist_multi_control_snapshot(
                ControlSaveReason::Terminal,
                None,
                &mut control_save_tracker,
                &control_save_ctx,
            ).await;
        }
        if !matches!(e, DownloadError::Cancelled | DownloadError::Paused) {
            let now = Instant::now();
            let update = sampled_progress_update(
                &mut eta_estimator,
                downloaded.load(Ordering::Relaxed),
                total_size,
                now,
            )
            .with_state(DownloadState::Failed);
            progress_reporter.force_report(progress_tx, update, now);
        }
        log_error!(log_level, download_id = download_id, error = %e,
            "multi-worker download failed");
        return Err(e);
    }
    if let Err(error) = writer_result {
        let now = Instant::now();
        let update = sampled_progress_update(
            &mut eta_estimator,
            downloaded.load(Ordering::Relaxed),
            total_size,
            now,
        )
        .with_state(DownloadState::Failed);
        progress_reporter.force_report(progress_tx, update, now);
        log_error!(log_level, download_id = download_id, error = %error,
            "multi-worker writer failed");
        return Err(error);
    }

    if !scheduler.lock().all_done() {
        if spec.resume {
            persist_multi_control_snapshot(
                ControlSaveReason::Terminal,
                None,
                &mut control_save_tracker,
                &control_save_ctx,
            ).await;
        }
        let now = Instant::now();
        let update = sampled_progress_update(
            &mut eta_estimator,
            downloaded.load(Ordering::Relaxed),
            total_size,
            now,
        )
        .with_state(DownloadState::Failed);
        progress_reporter.force_report(progress_tx, update, now);
        log_error!(log_level, download_id = download_id, "multi-worker download incomplete");
        return Err(DownloadError::Internal("download incomplete".into()));
    }

    let _ = ControlSnapshot::delete(control_path).await;
    // Final progress update
    let d = downloaded.load(Ordering::Relaxed);
    let now = Instant::now();
    let update = sampled_progress_update(&mut eta_estimator, d, total_size, now)
        .with_state(DownloadState::Completed);
    log_info!(log_level, download_id = download_id,
        total_bytes = d, elapsed_secs = format!("{:.2}", start_time.elapsed().as_secs_f64()),
        "multi-worker download completed");
    progress_reporter.force_report(progress_tx, update, now);
    Ok(())
}

fn sampled_progress_update(
    eta_estimator: &mut EtaEstimator,
    downloaded: u64,
    total_size: u64,
    now: Instant,
) -> ProgressUpdate {
    eta_estimator.record(downloaded, now);
    let speed = eta_estimator.speed_bytes_per_sec().unwrap_or(0.0);
    let remaining = total_size.saturating_sub(downloaded);
    let eta_secs = if remaining == 0 {
        Some(0.0)
    } else {
        eta_estimator.estimate(remaining)
    };
    ProgressUpdate::new(downloaded, speed, eta_secs)
}

async fn persist_multi_control_snapshot(
    reason: ControlSaveReason,
    write_tx: Option<&mpsc::Sender<WriterCommand>>,
    control_save_tracker: &mut ControlSaveTracker,
    ctx: &MultiControlSaveContext<'_>,
) {
    let current_downloaded = ctx.scheduler.lock().completed_bytes();
    if !control_save_tracker.should_save(reason, current_downloaded, ctx.spec.autosave_sync_every) {
        if matches!(reason, ControlSaveReason::Autosave)
            && current_downloaded > control_save_tracker.last_saved_downloaded_bytes()
        {
            log_debug!(ctx.log_level, download_id = ctx.download_id,
                checkpoint = reason.label(), downloaded_bytes = current_downloaded,
                pending_autosaves = control_save_tracker.pending_autosaves(),
                autosave_sync_every = ctx.spec.autosave_sync_every,
                "control snapshot deferred");
        }
        return;
    }

    let flush_stats = match write_tx {
        Some(write_tx) => match flush_all_and_wait(write_tx, true).await {
            Ok(stats) => Some(stats),
            Err(error) => {
                log_warn!(ctx.log_level, download_id = ctx.download_id, checkpoint = reason.label(),
                    error = %error, "control snapshot flush failed");
                return;
            }
        },
        None => None,
    };

    let snap = {
        let sched = ctx.scheduler.lock();
        ControlSnapshot {
            url: ctx.spec.url.clone(),
            total_size: ctx.total_size,
            piece_size: sched.piece_size(),
            piece_count: sched.piece_count(),
            completed_bitset: sched.snapshot_bitset(),
            downloaded_bytes: sched.completed_bytes(),
            etag: ctx.meta.etag.clone(),
            last_modified: ctx.meta.last_modified.clone(),
        }
    };
    if snap.downloaded_bytes <= control_save_tracker.last_saved_downloaded_bytes() {
        return;
    }

    let save_started = Instant::now();
    match snap.save(ctx.control_path).await {
        Ok(()) => {
            control_save_tracker.mark_saved(snap.downloaded_bytes);
            log_debug!(ctx.log_level, download_id = ctx.download_id,
                checkpoint = reason.label(), downloaded_bytes = snap.downloaded_bytes,
                flush_all_ms = flush_stats.map(|stats| stats.flush_elapsed.as_millis() as u64).unwrap_or(0),
                sync_data_ms = flush_stats.and_then(|stats| stats.sync_elapsed.map(|elapsed| elapsed.as_millis() as u64)).unwrap_or(0),
                control_save_ms = save_started.elapsed().as_millis() as u64,
                "control snapshot saved");
        }
        Err(error) => {
            log_warn!(ctx.log_level, download_id = ctx.download_id, checkpoint = reason.label(),
                error = %error, "control snapshot save failed");
        }
    }
}

// ──────────────────────────────────────────────────────────────
//  Worker loop
// ──────────────────────────────────────────────────────────────

/// Immutable per-download configuration shared by all workers via `Arc`.
struct WorkerConfig {
    url: String,
    headers: std::collections::HashMap<String, String>,
    read_timeout: Duration,
    max_retries: u32,
    retry_base_delay: Duration,
    retry_max_delay: Duration,
    max_retry_elapsed: Option<Duration>,
}

#[allow(clippy::too_many_arguments)]
async fn worker_loop(
    worker_id: usize,
    client: reqwest::Client,
    cfg: Arc<WorkerConfig>,
    scheduler: Scheduler,
    write_tx: mpsc::Sender<WriterCommand>,
    downloaded: Arc<AtomicU64>,
    cancel_rx: watch::Receiver<StopSignal>,
    budget: Arc<Semaphore>,
    speed_limit: SpeedLimit,
    first_response: Option<(reqwest::Response, usize)>,
    log_level: LogLevel,
    download_id: u64,
) -> Result<(), DownloadError> {
    let mut cancel_rx = cancel_rx;
    let mut first_response = first_response;
    log_debug!(log_level, download_id = download_id, worker_id = worker_id, "worker started");

    loop {
        let signal = *cancel_rx.borrow();
        if signal.is_stop_requested() {
            log_debug!(
                log_level,
                download_id = download_id,
                worker_id = worker_id,
                stop = stop_signal_label(signal),
                "worker stopped"
            );
            return Err(stop_signal_error(signal).expect("stop signal must map to an error"));
        }

        let assign_started = Instant::now();
        let segment = match scheduler.lock().assign() {
            Some(seg) => seg,
            None => {
                log_debug!(log_level, download_id = download_id, worker_id = worker_id,
                    scheduler_lock_us = assign_started.elapsed().as_micros() as u64,
                    "no more segments, worker exiting");
                return Ok(());
            }
        };

        log_debug!(log_level, download_id = download_id, worker_id = worker_id,
            piece_id = segment.piece_id,
            scheduler_lock_us = assign_started.elapsed().as_micros() as u64,
            "assigned piece");

        let mut attempt = 0u32;
    let retry_started_at = Instant::now();

        loop {
            let result = if let Some((resp, pid)) = first_response.take() {
                if pid == segment.piece_id {
                    stream_segment(
                        resp,
                        &segment,
                        &write_tx,
                        &downloaded,
                        &mut cancel_rx,
                        &budget,
                        &speed_limit,
                    )
                    .await
                } else {
                    download_segment(
                        &client,
                        &cfg.url,
                        &cfg.headers,
                        cfg.read_timeout,
                        &segment,
                        &write_tx,
                        &downloaded,
                        &mut cancel_rx,
                        &budget,
                        &speed_limit,
                    )
                    .await
                }
            } else {
                download_segment(
                    &client,
                    &cfg.url,
                    &cfg.headers,
                    cfg.read_timeout,
                    &segment,
                    &write_tx,
                    &downloaded,
                    &mut cancel_rx,
                    &budget,
                    &speed_limit,
                )
                .await
            };

            match result {
                Ok(()) => {
                    flush_piece_and_wait(&write_tx, segment.piece_id).await?;
                    scheduler.lock().complete(segment.piece_id);
                    log_debug!(log_level, download_id = download_id, worker_id = worker_id,
                        piece_id = segment.piece_id, "piece completed");
                    break;
                }
                Err(e) => {
                    if !e.is_retryable() || attempt >= cfg.max_retries {
                        log_warn!(log_level, download_id = download_id, worker_id = worker_id,
                            piece_id = segment.piece_id, error = %e,
                            "segment failed, reclaiming (non-retryable or max retries exceeded)");
                        scheduler.lock().reclaim(segment.piece_id);
                        return Err(e);
                    }

                    if let Some(limit) = cfg.max_retry_elapsed {
                        let elapsed = retry_started_at.elapsed();
                        if elapsed >= limit {
                            scheduler.lock().reclaim(segment.piece_id);
                            return Err(DownloadError::RetryBudgetExceeded { elapsed, limit });
                        }
                    }

                    attempt += 1;

                    // Compute backoff delay
                    let backoff = if let Some(retry_secs) = e.retry_after_secs() {
                        Duration::from_secs(retry_secs)
                    } else {
                        let exp = cfg.retry_base_delay.saturating_mul(1u32 << attempt.min(10));
                        exp.min(cfg.retry_max_delay)
                    };

                    if let Some(limit) = cfg.max_retry_elapsed {
                        let elapsed = retry_started_at.elapsed();
                        if elapsed.saturating_add(backoff) > limit {
                            scheduler.lock().reclaim(segment.piece_id);
                            return Err(DownloadError::RetryBudgetExceeded { elapsed, limit });
                        }
                    }

                    log_warn!(log_level, download_id = download_id, worker_id = worker_id,
                        piece_id = segment.piece_id, attempt = attempt, error = %e,
                        backoff_ms = backoff.as_millis() as u64,
                        "segment failed, retrying after backoff");

                    // Wait with cancellation awareness
                    tokio::select! {
                        biased;
                        result = cancel_rx.changed() => {
                            if result.is_ok() {
                                let signal = *cancel_rx.borrow_and_update();
                                scheduler.lock().reclaim(segment.piece_id);
                                if let Some(error) = stop_signal_error(signal) {
                                    return Err(error);
                                }
                            }
                        }
                        _ = tokio::time::sleep(backoff) => {}
                    }
                }
            }
        }
    }
}

/// Check the HTTP status of a segment response and return an appropriate error
/// for non-206 statuses.
fn check_segment_status(
    status: u16,
    retry_after_header: Option<&str>,
) -> Result<(), DownloadError> {
    if status == 200 {
        // Server returned full content instead of partial — Range not supported
        return Err(DownloadError::HttpStatus {
            status: 200,
            message: "server returned 200 instead of 206; Range not supported".into(),
        });
    }
    if status == 429 || status == 503 {
        // Extract Retry-After for backoff
        let retry_after = retry_after_header.and_then(|s| s.trim().parse::<u64>().ok());
        let message = match retry_after {
            Some(secs) => format!("retry-after:{secs}"),
            None => format!("HTTP {status}"),
        };
        return Err(DownloadError::HttpStatus { status, message });
    }
    if status != 206 {
        return Err(DownloadError::HttpStatus {
            status,
            message: format!("expected 206, got {status}"),
        });
    }
    Ok(())
}

/// Validate that a segment response's metadata matches the expected range.
fn validate_segment_meta(meta: &ResponseMeta, segment: &Segment) -> Result<(), DownloadError> {
    if !range_response_allowed(meta) {
        return Err(DownloadError::ResumeMismatch(
            "range responses with content-encoding are not supported".into(),
        ));
    }
    if meta.content_range_total.is_none()
        || meta.content_range_start != Some(segment.start)
        || meta.content_range_end != Some(segment.end - 1)
    {
        return Err(DownloadError::ResumeMismatch(
            "server returned a mismatched Content-Range".into(),
        ));
    }
    Ok(())
}

/// Download a segment by sending a fresh Range request.
#[allow(clippy::too_many_arguments)]
async fn download_segment(
    client: &reqwest::Client,
    url: &str,
    headers: &std::collections::HashMap<String, String>,
    timeout: Duration,
    segment: &Segment,
    write_tx: &mpsc::Sender<WriterCommand>,
    downloaded: &Arc<AtomicU64>,
    cancel_rx: &mut watch::Receiver<StopSignal>,
    budget: &Arc<Semaphore>,
    speed_limit: &SpeedLimit,
) -> Result<(), DownloadError> {
    let req = build_range_request(
        client,
        url,
        headers,
        timeout,
        segment.start,
        segment.end - 1,
    );
    let response = req.send().await?;

    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok().map(|s| s.to_owned()));
    check_segment_status(status, retry_after.as_deref())?;

    let meta = ResponseMeta::from_response(&response);
    validate_segment_meta(&meta, segment)?;

    stream_segment(
        response,
        segment,
        write_tx,
        downloaded,
        cancel_rx,
        budget,
        speed_limit,
    )
    .await
}

/// Stream an already-opened response into the writer channel.
async fn stream_segment(
    response: reqwest::Response,
    segment: &Segment,
    write_tx: &mpsc::Sender<WriterCommand>,
    downloaded: &Arc<AtomicU64>,
    cancel_rx: &mut watch::Receiver<StopSignal>,
    budget: &Arc<Semaphore>,
    speed_limit: &SpeedLimit,
) -> Result<(), DownloadError> {
    let mut stream = response.bytes_stream();
    let mut offset = segment.start;

    loop {
        tokio::select! {
            biased;

            result = cancel_rx.changed() => {
                if result.is_ok() {
                    if let Some(error) = stop_signal_error(*cancel_rx.borrow_and_update()) {
                        return Err(error);
                    }
                }
            }

            chunk = stream.next() => {
                match chunk {
                    Some(Ok(data)) => {
                        let len = data.len();
                        // Rate limiting
                        speed_limit.acquire(len).await;
                        // Acquire budget permits before buffering
                        let permit = budget
                            .acquire_many(len as u32)
                            .await
                            .map_err(|_| DownloadError::Internal("budget semaphore closed".into()))?;

                        if write_tx.send(WriterCommand::Data {
                            offset,
                            data,
                            piece_id: Some(segment.piece_id),
                        }).await.is_err() {
                            return Err(DownloadError::ChannelClosed);
                        }
                        permit.forget(); // permits returned by writer after flush
                        offset += len as u64;
                        downloaded.fetch_add(len as u64, Ordering::Relaxed);
                    }
                    Some(Err(e)) => return Err(DownloadError::Http(e)),
                    None => break,
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod coverage_tests {
    use super::*;

    fn response_meta() -> ResponseMeta {
        ResponseMeta {
            content_length: Some(1024),
            content_range_start: Some(0),
            content_range_end: Some(255),
            content_range_total: Some(1024),
            accept_ranges: true,
            etag: Some("\"multi\"".into()),
            last_modified: Some("Thu, 01 Jan 2026 00:00:00 GMT".into()),
            content_disposition: None,
            content_encoding: None,
        }
    }

    fn build_scheduler(total_size: u64, piece_size: u64) -> Scheduler {
        Arc::new(parking_lot::Mutex::new(SchedulerState::new(PieceMap::new(
            total_size,
            piece_size,
        ))))
    }

    fn complete_one_piece(scheduler: &Scheduler) {
        let segment = scheduler.lock().assign().unwrap();
        scheduler.lock().complete(segment.piece_id);
    }

    #[tokio::test]
    async fn test_persist_multi_control_snapshot_defers_then_saves_autosave() {
        let dir = tempfile::tempdir().unwrap();
        let control_path = dir.path().join("multi.bytehaul");
        let scheduler = build_scheduler(1024, 256);
        let meta = response_meta();
        let spec = DownloadSpec::new("https://example.com/multi.bin")
            .resume(true)
            .piece_size(256)
            .autosave_sync_every(2);
        let ctx = MultiControlSaveContext {
            spec: &spec,
            meta: &meta,
            scheduler: &scheduler,
            total_size: 1024,
            control_path: &control_path,
            log_level: LogLevel::Off,
            download_id: 3,
        };
        let mut tracker = ControlSaveTracker::new(0);

        complete_one_piece(&scheduler);
        persist_multi_control_snapshot(
            ControlSaveReason::Autosave,
            None,
            &mut tracker,
            &ctx,
        )
        .await;

        assert!(!control_path.exists());
        assert_eq!(tracker.last_saved_downloaded_bytes(), 0);
        assert_eq!(tracker.pending_autosaves(), 1);

        complete_one_piece(&scheduler);
        persist_multi_control_snapshot(
            ControlSaveReason::Autosave,
            None,
            &mut tracker,
            &ctx,
        )
        .await;

        let loaded = ControlSnapshot::load(&control_path).await.unwrap();
        assert_eq!(loaded.downloaded_bytes, 512);
        assert_eq!(loaded.total_size, 1024);
        assert_eq!(tracker.last_saved_downloaded_bytes(), 512);
        assert_eq!(tracker.pending_autosaves(), 0);
    }

    #[tokio::test]
    async fn test_persist_multi_control_snapshot_returns_on_flush_failure() {
        let dir = tempfile::tempdir().unwrap();
        let control_path = dir.path().join("multi-failed.bytehaul");
        let scheduler = build_scheduler(1024, 256);
        let meta = response_meta();
        let spec = DownloadSpec::new("https://example.com/multi.bin")
            .resume(true)
            .piece_size(256)
            .autosave_sync_every(1);
        let ctx = MultiControlSaveContext {
            spec: &spec,
            meta: &meta,
            scheduler: &scheduler,
            total_size: 1024,
            control_path: &control_path,
            log_level: LogLevel::Off,
            download_id: 4,
        };
        let mut tracker = ControlSaveTracker::new(0);
        let (write_tx, write_rx) = mpsc::channel(1);
        drop(write_rx);

        complete_one_piece(&scheduler);
        persist_multi_control_snapshot(
            ControlSaveReason::Terminal,
            Some(&write_tx),
            &mut tracker,
            &ctx,
        )
        .await;

        assert!(!control_path.exists());
        assert_eq!(tracker.last_saved_downloaded_bytes(), 0);
    }

    #[tokio::test]
    async fn test_persist_multi_control_snapshot_skips_when_downloaded_does_not_advance() {
        let dir = tempfile::tempdir().unwrap();
        let control_path = dir.path().join("multi-stable.bytehaul");
        let scheduler = build_scheduler(1024, 256);
        let meta = response_meta();
        let spec = DownloadSpec::new("https://example.com/multi.bin")
            .resume(true)
            .piece_size(256)
            .autosave_sync_every(1);
        let ctx = MultiControlSaveContext {
            spec: &spec,
            meta: &meta,
            scheduler: &scheduler,
            total_size: 1024,
            control_path: &control_path,
            log_level: LogLevel::Off,
            download_id: 5,
        };
        let mut tracker = ControlSaveTracker::new(0);

        complete_one_piece(&scheduler);
        persist_multi_control_snapshot(
            ControlSaveReason::Terminal,
            None,
            &mut tracker,
            &ctx,
        )
        .await;
        persist_multi_control_snapshot(
            ControlSaveReason::Terminal,
            None,
            &mut tracker,
            &ctx,
        )
        .await;

        let loaded = ControlSnapshot::load(&control_path).await.unwrap();
        assert_eq!(loaded.downloaded_bytes, 256);
        assert_eq!(tracker.last_saved_downloaded_bytes(), 256);
    }

    #[test]
    fn test_sampled_progress_update_sets_zero_eta_when_complete() {
        let mut eta_estimator = EtaEstimator::new(Duration::from_secs(5), Duration::from_secs(1));
        let now = Instant::now();
        eta_estimator.record(512, now - Duration::from_secs(2));

        let update = sampled_progress_update(&mut eta_estimator, 1024, 1024, now);

        assert_eq!(update.downloaded, 1024);
        assert_eq!(update.eta_secs, Some(0.0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── check_segment_status tests ──

    #[test]
    fn test_check_segment_status_206_ok() {
        assert!(check_segment_status(206, None).is_ok());
    }

    #[test]
    fn test_check_segment_status_200_range_not_supported() {
        let err = check_segment_status(200, None).unwrap_err();
        match err {
            DownloadError::HttpStatus { status, message } => {
                assert_eq!(status, 200);
                assert!(message.contains("Range not supported"));
            }
            _ => panic!("expected HttpStatus"),
        }
    }

    #[test]
    fn test_check_segment_status_429_with_retry_after() {
        let err = check_segment_status(429, Some("5")).unwrap_err();
        match err {
            DownloadError::HttpStatus { status, message } => {
                assert_eq!(status, 429);
                assert_eq!(message, "retry-after:5");
            }
            _ => panic!("expected HttpStatus"),
        }
    }

    #[test]
    fn test_check_segment_status_503_without_retry_after() {
        let err = check_segment_status(503, None).unwrap_err();
        match err {
            DownloadError::HttpStatus { status, message } => {
                assert_eq!(status, 503);
                assert_eq!(message, "HTTP 503");
            }
            _ => panic!("expected HttpStatus"),
        }
    }

    #[test]
    fn test_check_segment_status_503_with_non_numeric_retry_after() {
        let err = check_segment_status(503, Some("later")).unwrap_err();
        match err {
            DownloadError::HttpStatus { status, message } => {
                assert_eq!(status, 503);
                assert_eq!(message, "HTTP 503");
            }
            _ => panic!("expected HttpStatus"),
        }
    }

    #[test]
    fn test_check_segment_status_404_other() {
        let err = check_segment_status(404, None).unwrap_err();
        match err {
            DownloadError::HttpStatus { status, message } => {
                assert_eq!(status, 404);
                assert!(message.contains("expected 206, got 404"));
            }
            _ => panic!("expected HttpStatus"),
        }
    }

    // ── validate_segment_meta tests ──

    #[test]
    fn test_validate_segment_meta_ok() {
        let meta = ResponseMeta {
            content_length: Some(1000),
            content_range_start: Some(0),
            content_range_end: Some(999),
            content_range_total: Some(5000),
            accept_ranges: true,
            etag: None,
            last_modified: None,
            content_disposition: None,
            content_encoding: None,
        };
        let segment = Segment {
            piece_id: 0,
            start: 0,
            end: 1000,
        };
        assert!(validate_segment_meta(&meta, &segment).is_ok());
    }

    #[test]
    fn test_validate_segment_meta_gzip_encoding() {
        let meta = ResponseMeta {
            content_length: Some(1000),
            content_range_start: Some(0),
            content_range_end: Some(999),
            content_range_total: Some(5000),
            accept_ranges: true,
            etag: None,
            last_modified: None,
            content_disposition: None,
            content_encoding: Some("gzip".into()),
        };
        let segment = Segment {
            piece_id: 0,
            start: 0,
            end: 1000,
        };
        assert!(matches!(
            validate_segment_meta(&meta, &segment),
            Err(DownloadError::ResumeMismatch(_))
        ));
    }

    #[test]
    fn test_validate_segment_meta_no_total() {
        let meta = ResponseMeta {
            content_length: Some(1000),
            content_range_start: Some(0),
            content_range_end: Some(999),
            content_range_total: None,
            accept_ranges: true,
            etag: None,
            last_modified: None,
            content_disposition: None,
            content_encoding: None,
        };
        let segment = Segment {
            piece_id: 0,
            start: 0,
            end: 1000,
        };
        assert!(matches!(
            validate_segment_meta(&meta, &segment),
            Err(DownloadError::ResumeMismatch(_))
        ));
    }

    #[test]
    fn test_validate_segment_meta_wrong_start() {
        let meta = ResponseMeta {
            content_length: Some(1000),
            content_range_start: Some(100),
            content_range_end: Some(999),
            content_range_total: Some(5000),
            accept_ranges: true,
            etag: None,
            last_modified: None,
            content_disposition: None,
            content_encoding: None,
        };
        let segment = Segment {
            piece_id: 0,
            start: 0,
            end: 1000,
        };
        assert!(matches!(
            validate_segment_meta(&meta, &segment),
            Err(DownloadError::ResumeMismatch(_))
        ));
    }

    #[test]
    fn test_validate_segment_meta_wrong_end() {
        let meta = ResponseMeta {
            content_length: Some(1000),
            content_range_start: Some(0),
            content_range_end: Some(500),
            content_range_total: Some(5000),
            accept_ranges: true,
            etag: None,
            last_modified: None,
            content_disposition: None,
            content_encoding: None,
        };
        let segment = Segment {
            piece_id: 0,
            start: 0,
            end: 1000,
        };
        assert!(matches!(
            validate_segment_meta(&meta, &segment),
            Err(DownloadError::ResumeMismatch(_))
        ));
    }
}
