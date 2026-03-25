use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::stream::FuturesUnordered;
use futures::StreamExt;
use tokio::sync::{mpsc, watch, Semaphore};

use super::{
    flush_all_and_wait, flush_piece_and_wait, range_response_allowed,
    stop_signal_error, stop_signal_label, stop_signal_state,
    ETA_ALPHA, StopSignal,
};
use crate::config::{DownloadSpec, LogLevel};
use crate::error::DownloadError;
use crate::eta::EtaEstimator;
use crate::http::request::build_range_request;
use crate::http::response::ResponseMeta;
use crate::progress::{DownloadState, ProgressSnapshot};
use crate::rate_limiter::SpeedLimit;
use crate::scheduler::{Scheduler, SchedulerState};
use crate::storage::control::ControlSnapshot;
use crate::storage::file::{create_output_file, open_existing_file};
use crate::storage::piece_map::PieceMap;
use crate::storage::segment::Segment;
use crate::storage::writer::{WriterCommand, WriterTask};

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
    let mut eta_estimator = EtaEstimator::new(ETA_ALPHA);
    let mut last_progress_bytes = initial_downloaded;

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
    let mut progress_interval = tokio::time::interval(Duration::from_millis(200));
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
                            progress_tx.send_modify(|p| p.state = state);
                        }
                        if spec.resume {
                            save_multi_control(spec, meta, &scheduler, total_size, control_path, Some(&save_write_tx)).await;
                        }
                        download_error = Some(error);
                        break;
                    }
                }
            }

            _ = save_ticker.tick(), if spec.resume => {
                save_multi_control(spec, meta, &scheduler, total_size, control_path, Some(&save_write_tx)).await;
            }

            _ = progress_interval.tick() => {
                let d = downloaded.load(Ordering::Relaxed);
                let elapsed = start_time.elapsed().as_secs_f64();
                let speed = if elapsed > 0.0 {
                    d.saturating_sub(initial_downloaded) as f64 / elapsed
                } else {
                    0.0
                };
                let delta_bytes = d.saturating_sub(last_progress_bytes);
                eta_estimator.update_sample(delta_bytes, 0.2);
                last_progress_bytes = d;
                let remaining = total_size.saturating_sub(d);
                let eta_secs = if remaining == 0 {
                    Some(0.0)
                } else {
                    eta_estimator.estimate(remaining)
                };
                progress_tx.send_modify(|p| {
                    p.downloaded = d;
                    p.speed_bytes_per_sec = speed;
                    p.eta_secs = eta_secs;
                });
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
            save_multi_control(spec, meta, &scheduler, total_size, control_path, None).await;
        }
        log_error!(log_level, download_id = download_id, error = %e,
            "multi-worker download failed");
        return Err(e);
    }
    writer_result?;

    if !scheduler.lock().all_done() {
        if spec.resume {
            save_multi_control(spec, meta, &scheduler, total_size, control_path, None).await;
        }
        log_error!(log_level, download_id = download_id, "multi-worker download incomplete");
        return Err(DownloadError::Internal("download incomplete".into()));
    }

    let _ = ControlSnapshot::delete(control_path).await;
    // Final progress update
    let d = downloaded.load(Ordering::Relaxed);
    let elapsed = start_time.elapsed().as_secs_f64();
    let speed = if elapsed > 0.0 {
        d as f64 / elapsed
    } else {
        0.0
    };
    log_info!(log_level, download_id = download_id,
        total_bytes = d, elapsed_secs = format!("{elapsed:.2}"),
        "multi-worker download completed");
    progress_tx.send_modify(|p| {
        p.downloaded = d;
        p.speed_bytes_per_sec = speed;
        p.eta_secs = Some(0.0);
        p.state = DownloadState::Completed;
    });
    Ok(())
}

async fn save_multi_control(
    spec: &DownloadSpec,
    meta: &ResponseMeta,
    scheduler: &Scheduler,
    total_size: u64,
    control_path: &Path,
    write_tx: Option<&mpsc::Sender<WriterCommand>>,
) {
    if let Some(write_tx) = write_tx {
        if flush_all_and_wait(write_tx, true).await.is_err() {
            return;
        }
    }

    let snap = {
        let sched = scheduler.lock();
        ControlSnapshot {
            url: spec.url.clone(),
            total_size,
            piece_size: sched.piece_size(),
            piece_count: sched.piece_count(),
            completed_bitset: sched.snapshot_bitset(),
            downloaded_bytes: sched.completed_bytes(),
            etag: meta.etag.clone(),
            last_modified: meta.last_modified.clone(),
        }
    };
    let _ = snap.save(control_path).await;
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

        let segment = match scheduler.lock().assign() {
            Some(seg) => seg,
            None => {
                log_debug!(log_level, download_id = download_id, worker_id = worker_id, "no more segments, worker exiting");
                return Ok(());
            }
        };

        log_debug!(log_level, download_id = download_id, worker_id = worker_id,
            piece_id = segment.piece_id, "assigned piece");

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
