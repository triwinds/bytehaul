mod adaptive;

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::stream::FuturesUnordered;
use futures::StreamExt;
use tokio::sync::{mpsc, watch, Mutex as TokioMutex};

use super::flow::MemoryBudget;

use super::range_validate::{
    validate_range_response, ExpectedRange, RangeValidationDecision, RangeValidationMode,
};
use super::retry::{sleep_with_backoff, RetryDecision, RetryState};
use super::{
    begin_lease_and_wait, discard_lease_and_wait, flush_all_and_wait, flush_lease_and_wait,
    stop_signal_error, stop_signal_label, stop_signal_state, ControlSaveReason, ControlSaveTracker,
    StopSignal, MIN_SPEED_SAMPLE_SPAN, MULTI_PROGRESS_INTERVAL, SPEED_ESTIMATE_WINDOW,
};
use crate::config::{DownloadSpec, LogLevel};
use crate::error::DownloadError;
use crate::eta::EtaEstimator;
use crate::http::response::ResponseMeta;
use crate::http::worker::HttpWorker;
use crate::http::{next_data_chunk, HttpResponse};
use crate::network::BytehaulClient;
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
    request_url: &'a str,
    scheduler: &'a Scheduler,
    total_size: u64,
    control_path: &'a Path,
    log_level: LogLevel,
    download_id: u64,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_multi_worker(
    client: BytehaulClient,
    spec: &DownloadSpec,
    request_url: &str,
    output_path: &Path,
    meta: &ResponseMeta,
    total_size: u64,
    piece_map: PieceMap,
    probe_response: Option<(HttpResponse, ResponseMeta, usize)>,
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
    let budget = Arc::new(MemoryBudget::new(spec.memory_budget));

    // Writer with cache
    let (write_tx, write_rx) = mpsc::channel::<WriterCommand>(spec.channel_buffer);
    let written_bytes = Arc::new(AtomicU64::new(0));
    let writer_handle = tokio::spawn(
        WriterTask::new(
            write_rx,
            file,
            written_bytes.clone(),
            budget.semaphore.clone(),
            budget.watermark,
        )
        .run(),
    );

    // Scheduler
    let initial_completed_bytes = piece_map.completed_bytes();
    let scheduler: Scheduler = Arc::new(parking_lot::Mutex::new(SchedulerState::new(piece_map)));

    // Shared progress counter
    let received_bytes = Arc::new(AtomicU64::new(initial_completed_bytes));
    let start_time = Instant::now();
    let mut eta_estimator = EtaEstimator::new(SPEED_ESTIMATE_WINDOW, MIN_SPEED_SAMPLE_SPAN);
    let mut progress_reporter = ProgressReporter::new(
        initial_completed_bytes,
        MULTI_PROGRESS_INTERVAL,
        0,
        start_time,
    );
    eta_estimator.record(initial_completed_bytes, start_time);

    progress_tx.send_modify(|p| {
        p.total_size = Some(total_size);
        p.downloaded = initial_completed_bytes;
        p.state = DownloadState::Downloading;
        p.start_time = Some(start_time);
        p.eta_secs = None;
    });

    // Spawn workers
    let remaining = scheduler.lock().remaining_count();
    let num_workers = (spec.max_connections as usize).max(1);
    log_info!(
        log_level,
        download_id = download_id,
        workers = num_workers,
        remaining_pieces = remaining,
        total_size = total_size,
        initial_completed_bytes = initial_completed_bytes,
        "multi-worker download started"
    );

    let mut abort_handles = Vec::with_capacity(num_workers);
    let mut workers = FuturesUnordered::new();
    let first_response = Arc::new(TokioMutex::new(probe_response));
    let save_write_tx = write_tx.clone();
    let mut control_save_tracker = ControlSaveTracker::new(initial_completed_bytes);
    let control_save_ctx = MultiControlSaveContext {
        spec,
        meta,
        request_url,
        scheduler: &scheduler,
        total_size,
        control_path,
        log_level,
        download_id,
    };

    let worker_cfg = Arc::new(WorkerConfig {
        worker: HttpWorker::new(client.clone(), spec),
        read_timeout: spec.read_timeout,
        max_retries: spec.max_retries,
        retry_base_delay: spec.retry_base_delay,
        retry_max_delay: spec.retry_max_delay,
        max_retry_elapsed: spec.max_retry_elapsed,
        max_active_leases: num_workers,
        min_segment_size: spec.min_segment_size.min(spec.piece_size),
        recovery: adaptive::Coordinator::new(spec, meta, output_path, total_size),
    });

    for worker_id in 0..num_workers {
        let handle = tokio::spawn(worker_loop(
            worker_id,
            worker_cfg.clone(),
            scheduler.clone(),
            write_tx.clone(),
            received_bytes.clone(),
            cancel_rx.clone(),
            budget.clone(),
            speed_limit.clone(),
            first_response.clone(),
            total_size,
            log_level,
            download_id,
        ));
        abort_handles.push(handle.abort_handle());
        workers.push(handle);
    }

    // Drop our sender so writer can finish once workers are done
    drop(write_tx);

    // 鈹€鈹€ Monitor loop 鈹€鈹€
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
                                received_bytes.load(Ordering::Relaxed),
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
                    received_bytes.load(Ordering::Relaxed),
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
                        if download_error.is_none() && (worker_cfg.recovery.is_some() || !matches!(e, DownloadError::ChannelClosed)) {
                            log_warn!(log_level, download_id = download_id,
                                error = %e, "worker failed");
                            download_error = Some(e);
                        }
                        if worker_cfg.recovery.is_some() || workers.is_empty() { break; }
                    }
                    Some(Err(join_err)) => {
                        if !join_err.is_cancelled() {
                            log_error!(log_level, download_id = download_id,
                                error = %join_err, "worker panicked");
                            download_error = Some(DownloadError::TaskFailed(
                                format!("worker panicked: {join_err}"),
                            ));
                        }
                        if worker_cfg.recovery.is_some() || workers.is_empty() { break; }
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

    if let Some(recovery) = &worker_cfg.recovery {
        recovery.report(log_level, download_id);
    }

    drop(save_write_tx);

    // Wait for writer
    let writer_result = writer_handle
        .await
        .map_err(|e| DownloadError::TaskFailed(format!("writer panicked: {e}")))?;

    let writer_succeeded = writer_result.is_ok();
    if let Err(error) = writer_result {
        download_error = Some(error);
    }

    if let Some(e) = download_error {
        // A closed writer is a durability barrier only when its final sync succeeded.
        if spec.resume && writer_succeeded {
            persist_multi_control_snapshot(
                ControlSaveReason::Terminal,
                None,
                &mut control_save_tracker,
                &control_save_ctx,
            )
            .await;
        }
        if !matches!(e, DownloadError::Cancelled | DownloadError::Paused) {
            let now = Instant::now();
            let update = sampled_progress_update(
                &mut eta_estimator,
                received_bytes.load(Ordering::Relaxed),
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

    if !scheduler.lock().all_done() {
        if spec.resume {
            persist_multi_control_snapshot(
                ControlSaveReason::Terminal,
                None,
                &mut control_save_tracker,
                &control_save_ctx,
            )
            .await;
        }
        let now = Instant::now();
        let update = sampled_progress_update(
            &mut eta_estimator,
            received_bytes.load(Ordering::Relaxed),
            total_size,
            now,
        )
        .with_state(DownloadState::Failed);
        progress_reporter.force_report(progress_tx, update, now);
        log_error!(
            log_level,
            download_id = download_id,
            "multi-worker download incomplete"
        );
        return Err(DownloadError::Internal("download incomplete".into()));
    }

    let _ = ControlSnapshot::delete(control_path).await;
    // Final progress update
    let received_bytes = received_bytes.load(Ordering::Relaxed);
    let now = Instant::now();
    let update = sampled_progress_update(&mut eta_estimator, received_bytes, total_size, now)
        .with_state(DownloadState::Completed);
    log_info!(
        log_level,
        download_id = download_id,
        received_bytes = received_bytes,
        elapsed_secs = format!("{:.2}", start_time.elapsed().as_secs_f64()),
        "multi-worker download completed"
    );
    progress_reporter.force_report(progress_tx, update, now);
    Ok(())
}

fn sampled_progress_update(
    eta_estimator: &mut EtaEstimator,
    received_bytes: u64,
    total_size: u64,
    now: Instant,
) -> ProgressUpdate {
    eta_estimator.record(received_bytes, now);
    let speed = eta_estimator.speed_bytes_per_sec().unwrap_or(0.0);
    let remaining = total_size.saturating_sub(received_bytes);
    let eta_secs = if remaining == 0 {
        Some(0.0)
    } else {
        eta_estimator.estimate(remaining)
    };
    ProgressUpdate::new(received_bytes, speed, eta_secs)
}

async fn persist_multi_control_snapshot(
    reason: ControlSaveReason,
    write_tx: Option<&mpsc::Sender<WriterCommand>>,
    control_save_tracker: &mut ControlSaveTracker,
    ctx: &MultiControlSaveContext<'_>,
) {
    let completed_bytes = ctx.scheduler.lock().completed_bytes();
    let force_terminal_snapshot = matches!(reason, ControlSaveReason::Terminal);
    if !force_terminal_snapshot
        && !control_save_tracker.should_save(reason, completed_bytes, ctx.spec.autosave_sync_every)
    {
        if matches!(reason, ControlSaveReason::Autosave)
            && completed_bytes > control_save_tracker.last_saved_downloaded_bytes()
        {
            log_debug!(
                ctx.log_level,
                download_id = ctx.download_id,
                checkpoint = reason.label(),
                completed_bytes = completed_bytes,
                pending_autosaves = control_save_tracker.pending_autosaves(),
                autosave_sync_every = ctx.spec.autosave_sync_every,
                "control snapshot deferred"
            );
        }
        return;
    }

    // Freeze both completed bits and advisory hints before the writer barrier.
    // Workers may complete more pieces as soon as the sync acknowledges.
    let (snap, hints) = {
        let mut sched = ctx.scheduler.lock();
        let snap = ControlSnapshot {
            url: ctx.request_url.to_string(),
            total_size: ctx.total_size,
            piece_size: sched.piece_size(),
            piece_count: sched.piece_count(),
            completed_bitset: sched.snapshot_bitset(),
            downloaded_bytes: sched.completed_bytes(),
            etag: ctx.meta.etag.clone(),
            last_modified: ctx.meta.last_modified.clone(),
        };
        let hints = sched.control_hints();
        (snap, hints)
    };
    if !force_terminal_snapshot
        && snap.downloaded_bytes <= control_save_tracker.last_saved_downloaded_bytes()
    {
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

    let save_started = Instant::now();
    let dirty_piece_count = hints.dirty_piece_ids.len();
    let inflight_piece_count = hints.inflight_piece_ids.len();
    let snapshot_seq = hints.snapshot_seq;
    match snap.save_with_hints(ctx.control_path, hints).await {
        Ok(()) => {
            control_save_tracker.mark_saved(snap.downloaded_bytes);
            log_debug!(
                ctx.log_level,
                download_id = ctx.download_id,
                checkpoint = reason.label(),
                completed_bytes = snap.downloaded_bytes,
                dirty_piece_count = dirty_piece_count,
                inflight_piece_count = inflight_piece_count,
                snapshot_seq = snapshot_seq,
                flush_all_ms = flush_stats
                    .as_ref()
                    .map(|stats| stats.flush_elapsed.as_millis() as u64)
                    .unwrap_or(0),
                sync_data_ms = flush_stats
                    .as_ref()
                    .and_then(|stats| stats.sync_elapsed.map(|elapsed| elapsed.as_millis() as u64))
                    .unwrap_or(0),
                control_save_ms = save_started.elapsed().as_millis() as u64,
                "control snapshot saved"
            );
        }
        Err(error) => {
            log_warn!(ctx.log_level, download_id = ctx.download_id, checkpoint = reason.label(),
                error = %error, "control snapshot save failed");
        }
    }
}

// 鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€
//  Worker loop
// 鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€

/// Immutable per-download configuration shared by all workers via `Arc`.
struct WorkerConfig {
    worker: HttpWorker,
    read_timeout: Duration,
    max_retries: u32,
    retry_base_delay: Duration,
    retry_max_delay: Duration,
    max_retry_elapsed: Option<Duration>,
    max_active_leases: usize,
    min_segment_size: u64,
    recovery: Option<Arc<adaptive::Coordinator>>,
}

#[allow(clippy::too_many_arguments)]
async fn worker_loop(
    worker_id: usize,
    cfg: Arc<WorkerConfig>,
    scheduler: Scheduler,
    write_tx: mpsc::Sender<WriterCommand>,
    received_bytes: Arc<AtomicU64>,
    cancel_rx: watch::Receiver<StopSignal>,
    budget: Arc<MemoryBudget>,
    speed_limit: SpeedLimit,
    first_response: Arc<TokioMutex<Option<(HttpResponse, ResponseMeta, usize)>>>,
    total_size: u64,
    log_level: LogLevel,
    download_id: u64,
) -> Result<(), DownloadError> {
    if let Some(recovery) = &cfg.recovery {
        return adaptive::worker_loop(
            worker_id,
            &cfg,
            recovery,
            scheduler,
            write_tx,
            received_bytes,
            cancel_rx,
            budget,
            speed_limit,
            first_response,
            total_size,
            log_level,
            download_id,
        )
        .await;
    }
    let mut cancel_rx = cancel_rx;
    log_debug!(
        log_level,
        download_id = download_id,
        worker_id = worker_id,
        "worker started"
    );

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
        let segment = match scheduler.lock().assign_to_with_split(
            worker_id,
            cfg.max_active_leases,
            cfg.min_segment_size,
        ) {
            Some(seg) => seg,
            None => {
                log_debug!(
                    log_level,
                    download_id = download_id,
                    worker_id = worker_id,
                    scheduler_lock_us = assign_started.elapsed().as_micros() as u64,
                    "no more segments, worker exiting"
                );
                return Ok(());
            }
        };

        log_debug!(
            log_level,
            download_id = download_id,
            worker_id = worker_id,
            piece_id = segment.piece_id,
            lease_id = segment.lease_id,
            attempt = segment.attempt,
            owner_worker_id = segment.owner_worker_id,
            scheduler_lock_us = assign_started.elapsed().as_micros() as u64,
            "assigned piece"
        );

        let mut retry_state = RetryState::new(
            cfg.max_retries,
            cfg.retry_base_delay,
            cfg.retry_max_delay,
            cfg.max_retry_elapsed,
        );
        let mut segment = segment;

        loop {
            begin_lease_and_wait(&write_tx, segment.lease_key()).await?;
            let response = take_matching_probe_response(&first_response, &segment).await;
            let result = if let Some((resp, meta)) = response {
                match validate_segment_response(206, None, &meta, &segment, total_size) {
                    Ok(()) => {
                        stream_segment(
                            resp,
                            cfg.read_timeout,
                            total_size,
                            &segment,
                            &write_tx,
                            &received_bytes,
                            &mut cancel_rx,
                            &budget,
                            &speed_limit,
                        )
                        .await
                    }
                    Err(error) => Err((error, 0)),
                }
            } else {
                download_segment(
                    &cfg.worker,
                    cfg.read_timeout,
                    total_size,
                    &segment,
                    &write_tx,
                    &received_bytes,
                    &mut cancel_rx,
                    &budget,
                    &speed_limit,
                )
                .await
            };

            match result {
                Ok(_bytes_read) => {
                    flush_lease_and_wait(&write_tx, segment.lease_key()).await?;
                    if !scheduler.lock().complete(segment.lease_key()) {
                        return Err(DownloadError::Internal(
                            "stale segment lease completion rejected".into(),
                        ));
                    }
                    log_debug!(
                        log_level,
                        download_id = download_id,
                        worker_id = worker_id,
                        piece_id = segment.piece_id,
                        lease_id = segment.lease_id,
                        "piece completed"
                    );
                    break;
                }
                Err((e, bytes_read)) => {
                    if bytes_read > 0 {
                        received_bytes.fetch_sub(bytes_read, Ordering::Relaxed);
                    }
                    if let Err(error) = discard_lease_and_wait(&write_tx, segment.lease_key()).await
                    {
                        let _ = scheduler.lock().reclaim(segment.lease_key());
                        return Err(error);
                    }

                    match retry_state.decide(e) {
                        RetryDecision::Stop(error) => {
                            log_warn!(log_level, download_id = download_id, worker_id = worker_id,
                                piece_id = segment.piece_id, error = %error,
                                "segment failed, reclaiming (non-retryable or retry budget exhausted)");
                            let _ = scheduler.lock().reclaim(segment.lease_key());
                            return Err(error);
                        }
                        RetryDecision::Retry {
                            error,
                            retry_count,
                            backoff,
                            elapsed,
                        } => {
                            segment = scheduler
                                .lock()
                                .renew(segment.lease_key(), worker_id)
                                .ok_or_else(|| {
                                    DownloadError::Internal(
                                        "failed to renew segment lease for retry".into(),
                                    )
                                })?;

                            log_warn!(log_level, download_id = download_id, worker_id = worker_id,
                                piece_id = segment.piece_id, attempt = retry_count, error = %error,
                                backoff_ms = backoff.as_millis() as u64,
                                elapsed_ms = elapsed.as_millis() as u64,
                                "segment failed, retrying after backoff");

                            if let Err(stop_error) =
                                sleep_with_backoff(backoff, &mut cancel_rx).await
                            {
                                let _ = scheduler.lock().reclaim(segment.lease_key());
                                return Err(stop_error);
                            }
                        }
                    }
                }
            }
        }
    }
}

async fn take_matching_probe_response(
    first_response: &Arc<TokioMutex<Option<(HttpResponse, ResponseMeta, usize)>>>,
    segment: &Segment,
) -> Option<(HttpResponse, ResponseMeta)> {
    let mut first_response = first_response.lock().await;
    if first_response.as_ref().is_some_and(|(_, meta, piece_id)| {
        *piece_id == segment.piece_id && probe_response_matches_segment(meta, segment)
    }) {
        first_response
            .take()
            .map(|(response, meta, _)| (response, meta))
    } else {
        None
    }
}

fn probe_response_matches_segment(meta: &ResponseMeta, segment: &Segment) -> bool {
    meta.content_range_start == Some(segment.start)
        && meta.content_range_end == Some(segment.end - 1)
}

fn validate_segment_response(
    status: u16,
    retry_after_header: Option<&str>,
    meta: &ResponseMeta,
    segment: &Segment,
    total_size: u64,
) -> Result<(), DownloadError> {
    match validate_range_response(
        status,
        retry_after_header,
        meta,
        RangeValidationMode::Segment,
        ExpectedRange {
            start: segment.start,
            end_inclusive: segment.end - 1,
            total_size: Some(total_size),
        },
    )? {
        RangeValidationDecision::Accept => Ok(()),
        RangeValidationDecision::FallbackToSingle(_) => Err(DownloadError::Internal(
            "segment validation unexpectedly requested a single-connection fallback".into(),
        )),
    }
}

/// Download a segment by sending a fresh Range request.
#[allow(clippy::too_many_arguments)]
async fn download_segment(
    worker: &HttpWorker,
    timeout: Duration,
    total_size: u64,
    segment: &Segment,
    write_tx: &mpsc::Sender<WriterCommand>,
    received_bytes: &Arc<AtomicU64>,
    cancel_rx: &mut watch::Receiver<StopSignal>,
    budget: &Arc<MemoryBudget>,
    speed_limit: &SpeedLimit,
) -> Result<u64, (DownloadError, u64)> {
    let (response, meta) = worker
        .send_range(segment.start, segment.end - 1)
        .await
        .map_err(|error| (error, 0))?;

    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok().map(|s| s.to_owned()));
    validate_segment_response(status, retry_after.as_deref(), &meta, segment, total_size)
        .map_err(|error| (error, 0))?;

    stream_segment(
        response,
        timeout,
        total_size,
        segment,
        write_tx,
        received_bytes,
        cancel_rx,
        budget,
        speed_limit,
    )
    .await
}

/// Stream an already-opened response into the writer channel.
#[allow(clippy::too_many_arguments)]
async fn stream_segment(
    response: HttpResponse,
    read_timeout: Duration,
    total_size: u64,
    segment: &Segment,
    write_tx: &mpsc::Sender<WriterCommand>,
    received_bytes: &Arc<AtomicU64>,
    cancel_rx: &mut watch::Receiver<StopSignal>,
    budget: &Arc<MemoryBudget>,
    speed_limit: &SpeedLimit,
) -> Result<u64, (DownloadError, u64)> {
    let mut body = response.into_body();
    let mut offset = segment.start;
    let mut bytes_read = 0u64;
    let expected_len = segment.end - segment.start;
    debug_assert!(total_size >= segment.end);

    loop {
        tokio::select! {
            biased;

            result = cancel_rx.changed() => {
                if result.is_ok() {
                    if let Some(error) = stop_signal_error(*cancel_rx.borrow_and_update()) {
                        return Err((error, bytes_read));
                    }
                }
            }

            chunk = next_data_chunk(&mut body, read_timeout) => {
                match chunk {
                    Ok(Some(data)) => {
                        let len = data.len();
                        let len_u64 = len as u64;
                        if bytes_read.saturating_add(len_u64) > expected_len {
                            return Err((
                                DownloadError::ResumeMismatch(
                                    "server sent more bytes than requested for segment".into(),
                                ),
                                bytes_read,
                            ));
                        }
                        budget.forward(
                            data, offset, Some(segment.lease_key()), write_tx,
                            cancel_rx, speed_limit,
                            |sent| {
                                offset += sent;
                                bytes_read += sent;
                                received_bytes.fetch_add(sent, Ordering::Relaxed);
                            },
                        ).await.map_err(|error| (error, bytes_read))?;
                    }
                    Ok(None) => break,
                    Err(error) => return Err((error, bytes_read)),
                }
            }
        }
    }
    if bytes_read != expected_len {
        return Err((
            DownloadError::Transport(crate::error::TransportError::new(
                crate::error::TransportErrorKind::Body,
                std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    format!("segment body ended after {bytes_read} bytes, expected {expected_len}"),
                ),
            )),
            bytes_read,
        ));
    }
    Ok(bytes_read)
}

#[cfg(test)]
mod coverage_tests {
    use super::*;
    use warp::Filter;

    fn worker_for(url: String) -> HttpWorker {
        let mut spec = DownloadSpec::new(url).output_path("unused.bin");
        spec.read_timeout = Duration::from_secs(5);
        let client = crate::network::ClientNetworkConfig::default()
            .build_client()
            .unwrap();
        HttpWorker::new(client, &spec)
    }

    fn spawn_writer_ack(
        mut write_rx: mpsc::Receiver<WriterCommand>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            while let Some(command) = write_rx.recv().await {
                match command {
                    WriterCommand::BeginLease { .. } => {}
                    WriterCommand::Data { .. } => {}
                    WriterCommand::FlushLease { ack, .. } => {
                        let _ = ack.send(());
                    }
                    WriterCommand::DiscardLease { ack, .. } => {
                        let _ = ack.send(0);
                    }
                    WriterCommand::FlushAll { ack, .. } => {
                        let _ = ack.send(crate::storage::writer::FlushAllStats {
                            written_bytes: 0,
                            flush_elapsed: Duration::ZERO,
                            sync_elapsed: Some(Duration::ZERO),
                        });
                    }
                }
            }
        })
    }

    fn spawn_flaky_range_server(
        fail_count: usize,
        retry_after_secs: Option<u64>,
    ) -> (std::net::SocketAddr, impl std::future::Future<Output = ()>) {
        let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = requests.clone();
        let route = warp::path("piece")
            .and(warp::header::optional::<String>("range"))
            .map(move |range_header: Option<String>| {
                let request_index = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if request_index < fail_count {
                    let mut builder = warp::http::Response::builder().status(503);
                    if let Some(retry_after_secs) = retry_after_secs {
                        builder = builder.header("retry-after", retry_after_secs.to_string());
                    }
                    return builder.body(Vec::<u8>::new()).unwrap();
                }

                let total = 256u64;
                let (start, end) = match range_header {
                    Some(range) => {
                        let range = range.trim_start_matches("bytes=");
                        let parts: Vec<&str> = range.split('-').collect();
                        let start = parts[0].parse::<u64>().unwrap_or(0);
                        let end = parts[1].parse::<u64>().unwrap_or(total - 1);
                        (start, end)
                    }
                    None => (0, total - 1),
                };
                let len = (end - start + 1) as usize;
                warp::http::Response::builder()
                    .status(206)
                    .header("content-length", len.to_string())
                    .header(
                        "content-range",
                        format!("bytes {}-{}/{}", start, end, total),
                    )
                    .body(vec![0xAB; len])
                    .unwrap()
            });

        warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0))
    }

    fn spawn_slow_range_server() -> (std::net::SocketAddr, impl std::future::Future<Output = ()>) {
        let route = warp::path("slow-piece")
            .and(warp::header::optional::<String>("range"))
            .map(move |range_header: Option<String>| {
                let total = 512u64;
                let (start, end) = match range_header {
                    Some(range) => {
                        let range = range.trim_start_matches("bytes=");
                        let parts: Vec<&str> = range.split('-').collect();
                        let start = parts[0].parse::<u64>().unwrap_or(0);
                        let end = parts[1].parse::<u64>().unwrap_or(total - 1);
                        (start, end)
                    }
                    None => (0, total - 1),
                };
                let len = (end - start + 1) as usize;
                let data = vec![0xCD; len];
                let stream = futures::stream::once(async move {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    Ok::<_, std::convert::Infallible>(data)
                });
                let body = warp::hyper::Body::wrap_stream(stream);
                warp::http::Response::builder()
                    .status(206)
                    .header("content-length", len.to_string())
                    .header(
                        "content-range",
                        format!("bytes {}-{}/{}", start, end, total),
                    )
                    .body(body)
                    .unwrap()
            });

        warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0))
    }

    fn spawn_gated_range_server(
        path_segment: &'static str,
        arrivals: mpsc::UnboundedSender<(u64, u64, tokio::sync::oneshot::Sender<()>)>,
    ) -> (std::net::SocketAddr, impl std::future::Future<Output = ()>) {
        let route = warp::path(path_segment)
            .and(warp::header::<String>("range"))
            .and_then(move |range_header: String| {
                let arrivals = arrivals.clone();
                async move {
                    let (start, end) = range_header
                        .strip_prefix("bytes=")
                        .unwrap()
                        .split_once('-')
                        .unwrap();
                    let (start, end) = (start.parse::<u64>().unwrap(), end.parse::<u64>().unwrap());
                    assert!(start <= end && end < 1024);
                    let (release, released) = tokio::sync::oneshot::channel();
                    arrivals.send((start, end, release)).unwrap();
                    // No response headers or body can leave before the test has
                    // observed every expected request in flight concurrently.
                    released.await.unwrap();
                    let body: Vec<u8> = (start..=end).map(|offset| (offset % 251) as u8).collect();
                    Ok::<_, std::convert::Infallible>(
                        warp::http::Response::builder()
                            .status(206)
                            .header("content-length", body.len().to_string())
                            .header("content-range", format!("bytes {start}-{end}/1024"))
                            .body(body)
                            .unwrap(),
                    )
                }
            });

        warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0))
    }

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
            total_size, piece_size,
        ))))
    }

    fn complete_one_piece(scheduler: &Scheduler) {
        let segment = scheduler.lock().assign().unwrap();
        assert!(scheduler.lock().complete(segment.lease_key()));
    }

    #[tokio::test]
    async fn checkpoint_freezes_completed_bits_and_hints_before_sync() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("frozen.bytehaul");
        let scheduler = build_scheduler(1024, 256);
        complete_one_piece(&scheduler);
        let pending = scheduler.lock().assign().unwrap();
        let meta = response_meta();
        let spec = DownloadSpec::new("https://example.com/multi.bin").autosave_sync_every(1);
        let ctx = MultiControlSaveContext {
            spec: &spec,
            meta: &meta,
            request_url: &spec.url,
            scheduler: &scheduler,
            total_size: 1024,
            control_path: &path,
            log_level: LogLevel::Off,
            download_id: 0,
        };
        let (tx, mut rx) = mpsc::channel(1);
        let mut tracker = ControlSaveTracker::new(0);
        let barrier = async {
            let WriterCommand::FlushAll { sync_data, ack } = rx.recv().await.unwrap() else {
                panic!()
            };
            assert!(sync_data);
            ack.send(crate::storage::writer::FlushAllStats {
                written_bytes: 256,
                flush_elapsed: Duration::ZERO,
                sync_elapsed: Some(Duration::ZERO),
            })
            .unwrap();
            // The sync has acknowledged. Before the save future resumes,
            // a worker completes a lease written after that barrier.
            assert!(scheduler.lock().complete(pending.lease_key()));
        };
        tokio::join!(
            persist_multi_control_snapshot(
                ControlSaveReason::Autosave,
                Some(&tx),
                &mut tracker,
                &ctx
            ),
            barrier,
        );
        let (snapshot, hints) = ControlSnapshot::load_with_hints(&path).await.unwrap();
        assert_eq!(snapshot.downloaded_bytes, 256);
        assert_eq!(snapshot.completed_bitset, vec![1]);
        assert_eq!(hints.inflight_piece_ids, vec![pending.piece_id]);
        assert_eq!(scheduler.lock().completed_bytes(), 512);
        assert_eq!(tracker.last_saved_downloaded_bytes(), 256);
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
            request_url: "https://example.com/multi.bin",
            scheduler: &scheduler,
            total_size: 1024,
            control_path: &control_path,
            log_level: LogLevel::Off,
            download_id: 3,
        };
        let mut tracker = ControlSaveTracker::new(0);

        complete_one_piece(&scheduler);
        persist_multi_control_snapshot(ControlSaveReason::Autosave, None, &mut tracker, &ctx).await;

        assert!(!control_path.exists());
        assert_eq!(tracker.last_saved_downloaded_bytes(), 0);
        assert_eq!(tracker.pending_autosaves(), 1);

        complete_one_piece(&scheduler);
        persist_multi_control_snapshot(ControlSaveReason::Autosave, None, &mut tracker, &ctx).await;

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
            request_url: "https://example.com/multi.bin",
            scheduler: &scheduler,
            total_size: 1024,
            control_path: &control_path,
            log_level: LogLevel::Off,
            download_id: 4,
        };
        let mut tracker = ControlSaveTracker::new(0);
        complete_one_piece(&scheduler);
        persist_multi_control_snapshot(ControlSaveReason::Terminal, None, &mut tracker, &ctx).await;
        let previous_file = tokio::fs::read(&control_path).await.unwrap();
        assert_eq!(tracker.last_saved_downloaded_bytes(), 256);

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

        assert_eq!(tokio::fs::read(&control_path).await.unwrap(), previous_file);
        assert_eq!(tracker.last_saved_downloaded_bytes(), 256);

        // A writer that accepts the barrier but fails during flush/sync drops
        // its acknowledgement. That failure must preserve the same checkpoint.
        let (write_tx, mut write_rx) = mpsc::channel(1);
        tokio::join!(
            persist_multi_control_snapshot(
                ControlSaveReason::Terminal,
                Some(&write_tx),
                &mut tracker,
                &ctx,
            ),
            async {
                let WriterCommand::FlushAll { sync_data, ack } = write_rx.recv().await.unwrap()
                else {
                    panic!("expected a checkpoint durability barrier")
                };
                assert!(sync_data);
                drop(ack);
            },
        );
        assert_eq!(tokio::fs::read(&control_path).await.unwrap(), previous_file);
        let snapshot = ControlSnapshot::load(&control_path).await.unwrap();
        assert_eq!(snapshot.downloaded_bytes, 256);
        assert_eq!(snapshot.completed_bitset, vec![1]);
        assert_eq!(scheduler.lock().completed_bytes(), 512);
        assert_eq!(tracker.last_saved_downloaded_bytes(), 256);
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
            request_url: "https://example.com/multi.bin",
            scheduler: &scheduler,
            total_size: 1024,
            control_path: &control_path,
            log_level: LogLevel::Off,
            download_id: 5,
        };
        let mut tracker = ControlSaveTracker::new(0);

        complete_one_piece(&scheduler);
        persist_multi_control_snapshot(ControlSaveReason::Terminal, None, &mut tracker, &ctx).await;
        persist_multi_control_snapshot(ControlSaveReason::Terminal, None, &mut tracker, &ctx).await;

        let loaded = ControlSnapshot::load(&control_path).await.unwrap();
        assert_eq!(loaded.downloaded_bytes, 256);
        assert_eq!(tracker.last_saved_downloaded_bytes(), 256);
    }

    #[tokio::test]
    async fn test_persist_multi_control_snapshot_saves_terminal_snapshot_without_progress() {
        let dir = tempfile::tempdir().unwrap();
        let control_path = dir.path().join("multi-terminal.bytehaul");
        let scheduler = build_scheduler(1024, 256);
        let meta = response_meta();
        let spec = DownloadSpec::new("https://example.com/multi.bin")
            .resume(true)
            .piece_size(256)
            .autosave_sync_every(1);
        let ctx = MultiControlSaveContext {
            spec: &spec,
            meta: &meta,
            request_url: "https://example.com/multi.bin",
            scheduler: &scheduler,
            total_size: 1024,
            control_path: &control_path,
            log_level: LogLevel::Off,
            download_id: 6,
        };
        let mut tracker = ControlSaveTracker::new(0);

        persist_multi_control_snapshot(ControlSaveReason::Terminal, None, &mut tracker, &ctx).await;

        let loaded = ControlSnapshot::load(&control_path).await.unwrap();
        assert_eq!(loaded.downloaded_bytes, 0);
        assert_eq!(loaded.total_size, 1024);
        assert_eq!(tracker.last_saved_downloaded_bytes(), 0);
    }

    #[tokio::test]
    async fn test_persist_multi_control_snapshot_writes_v2_hints_for_partial_piece_state() {
        let dir = tempfile::tempdir().unwrap();
        let control_path = dir.path().join("multi-partial.bytehaul");
        let scheduler = build_scheduler(1024, 256);
        let meta = response_meta();
        let spec = DownloadSpec::new("https://example.com/multi.bin")
            .resume(true)
            .piece_size(256)
            .autosave_sync_every(1);
        let ctx = MultiControlSaveContext {
            spec: &spec,
            meta: &meta,
            request_url: "https://example.com/multi.bin",
            scheduler: &scheduler,
            total_size: 1024,
            control_path: &control_path,
            log_level: LogLevel::Off,
            download_id: 7,
        };
        let mut tracker = ControlSaveTracker::new(0);

        {
            let mut sched = scheduler.lock();
            let completed = sched.assign_subrange(0, 0, 128, 1).unwrap();
            let inflight = sched.assign_subrange(0, 128, 256, 2).unwrap();
            assert!(sched.complete(completed.lease_key()));
            assert_eq!(inflight.piece_id, 0);
        }

        persist_multi_control_snapshot(ControlSaveReason::Terminal, None, &mut tracker, &ctx).await;

        let (loaded, hints) = ControlSnapshot::load_with_hints(&control_path)
            .await
            .unwrap();
        assert_eq!(loaded.downloaded_bytes, 0);
        assert_eq!(hints.dirty_piece_ids, vec![0]);
        assert_eq!(hints.inflight_piece_ids, vec![0]);
        assert_eq!(hints.snapshot_seq, 1);
        assert_eq!(tracker.last_saved_downloaded_bytes(), 0);
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

    #[tokio::test]
    async fn test_worker_loop_retries_then_completes_piece() {
        let (addr, server) = spawn_flaky_range_server(2, Some(0));
        tokio::spawn(server);

        let scheduler = build_scheduler(256, 256);
        let (write_tx, write_rx) = mpsc::channel(8);
        let writer = spawn_writer_ack(write_rx);
        let downloaded = Arc::new(AtomicU64::new(0));
        let (_cancel_tx, cancel_rx) = watch::channel(StopSignal::Running);
        let cfg = Arc::new(WorkerConfig {
            worker: worker_for(format!("http://{addr}/piece")),
            read_timeout: Duration::from_secs(2),
            max_retries: 3,
            retry_base_delay: Duration::from_millis(10),
            retry_max_delay: Duration::from_millis(20),
            max_retry_elapsed: Some(Duration::from_secs(2)),
            max_active_leases: 1,
            min_segment_size: 256,
            recovery: None,
        });

        worker_loop(
            0,
            cfg,
            scheduler.clone(),
            write_tx,
            downloaded.clone(),
            cancel_rx,
            Arc::new(MemoryBudget::new(1024)),
            SpeedLimit::new(0),
            None,
            256,
            LogLevel::Off,
            7,
        )
        .await
        .unwrap();

        writer.await.unwrap();
        assert!(scheduler.lock().all_done());
        assert_eq!(downloaded.load(Ordering::Relaxed), 256);
    }

    #[tokio::test]
    async fn test_worker_loop_reclaims_piece_when_paused_during_backoff() {
        let (addr, server) = spawn_flaky_range_server(10, Some(1));
        tokio::spawn(server);

        let scheduler = build_scheduler(256, 256);
        let (write_tx, write_rx) = mpsc::channel(8);
        let writer = spawn_writer_ack(write_rx);
        let downloaded = Arc::new(AtomicU64::new(0));
        let (cancel_tx, cancel_rx) = watch::channel(StopSignal::Running);
        let cfg = Arc::new(WorkerConfig {
            worker: worker_for(format!("http://{addr}/piece")),
            read_timeout: Duration::from_secs(2),
            max_retries: 5,
            retry_base_delay: Duration::from_millis(10),
            retry_max_delay: Duration::from_secs(2),
            max_retry_elapsed: Some(Duration::from_secs(5)),
            max_active_leases: 1,
            min_segment_size: 256,
            recovery: None,
        });

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let _ = cancel_tx.send(StopSignal::Pause);
        });

        let err = worker_loop(
            0,
            cfg,
            scheduler.clone(),
            write_tx,
            downloaded,
            cancel_rx,
            Arc::new(MemoryBudget::new(1024)),
            SpeedLimit::new(0),
            None,
            256,
            LogLevel::Off,
            8,
        )
        .await
        .unwrap_err();

        writer.await.unwrap();
        assert!(matches!(err, DownloadError::Paused));
        assert_eq!(scheduler.lock().remaining_count(), 1);
        assert_eq!(scheduler.lock().assign().unwrap().piece_id, 0);
    }

    #[tokio::test]
    async fn test_run_multi_worker_pauses_before_work_starts() {
        let (addr, server) = spawn_slow_range_server();
        tokio::spawn(server);
        let dir = tempfile::tempdir().unwrap();
        let output_path = dir.path().join("paused-multi.bin");
        let control_path = dir.path().join("paused-multi.bytehaul");
        let meta = ResponseMeta {
            content_length: Some(256),
            content_range_start: Some(0),
            content_range_end: Some(255),
            content_range_total: Some(512),
            accept_ranges: true,
            etag: Some("\"paused\"".into()),
            last_modified: Some("Thu, 01 Jan 2026 00:00:00 GMT".into()),
            content_disposition: None,
            content_encoding: None,
        };
        let spec = DownloadSpec::new(format!("http://{addr}/slow-piece"))
            .resume(true)
            .piece_size(256)
            .file_allocation(crate::config::FileAllocation::None)
            .channel_buffer(4)
            .memory_budget(1024);
        let piece_map = PieceMap::new(512, 256);
        let (progress_tx, _progress_rx) = watch::channel(ProgressSnapshot::default());
        let (cancel_tx, cancel_rx) = watch::channel(StopSignal::Running);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let _ = cancel_tx.send(StopSignal::Pause);
        });

        let client = crate::network::ClientNetworkConfig::default()
            .build_client()
            .unwrap();
        let err = run_multi_worker(
            client,
            &spec,
            &format!("http://{addr}/slow-piece"),
            &output_path,
            &meta,
            512,
            piece_map,
            None,
            &progress_tx,
            cancel_rx,
            &control_path,
            SpeedLimit::new(0),
            LogLevel::Off,
            9,
        )
        .await
        .unwrap_err();

        assert!(matches!(err, DownloadError::Paused));
        assert_eq!(progress_tx.borrow().state, DownloadState::Paused);
        assert!(control_path.exists());
    }

    #[tokio::test]
    async fn nonretryable_worker_failure_preserves_completed_prefix() {
        let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = requests.clone();
        let route = warp::any().map(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            warp::http::Response::builder()
                .status(403)
                .body("forbidden")
        });
        let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
        let server = tokio::spawn(server);
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("partial.bin");
        let control = ControlSnapshot::control_path(&output);
        tokio::fs::write(&output, vec![0xAC; 256]).await.unwrap();
        let spec = DownloadSpec::new(format!("http://{addr}/piece"))
            .resume(true)
            .piece_size(256)
            .min_segment_size(256)
            .max_connections(1);
        let mut pieces = PieceMap::new(512, 256);
        pieces.mark_complete(0);
        let (progress, _) = watch::channel(ProgressSnapshot::default());
        let (_stop, stop_rx) = watch::channel(StopSignal::Running);
        let client = crate::network::ClientNetworkConfig::default()
            .build_client()
            .unwrap();
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            run_multi_worker(
                client,
                &spec,
                &spec.url,
                &output,
                &response_meta(),
                512,
                pieces,
                None,
                &progress,
                stop_rx,
                &control,
                SpeedLimit::new(0),
                LogLevel::Off,
                0,
            ),
        )
        .await
        .unwrap()
        .unwrap_err();
        server.abort();

        assert!(
            matches!(error, DownloadError::HttpStatus { status: 403, .. }),
            "{error:?}"
        );
        assert_eq!(requests.load(Ordering::SeqCst), 1, "403 must not retry");
        assert_eq!(progress.borrow().state, DownloadState::Failed);
        assert_eq!(progress.borrow().downloaded, 256);
        assert_eq!(tokio::fs::read(output).await.unwrap(), vec![0xAC; 256]);
        let (saved, hints) = ControlSnapshot::load_with_hints(&control).await.unwrap();
        assert_eq!(saved.downloaded_bytes, 256);
        assert_eq!(saved.completed_bitset, vec![1]);
        assert!(
            hints.inflight_piece_ids.is_empty(),
            "failed lease must be reclaimed"
        );
    }

    #[tokio::test]
    async fn stream_segment_rejects_short_and_oversized_bodies_without_false_completion() {
        for body_len in [2, 5] {
            let route = warp::any().map(move || vec![0xAC; body_len]);
            let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
            let server = tokio::spawn(server);
            let response = worker_for(format!("http://{addr}/piece"))
                .send_get()
                .await
                .unwrap()
                .0;
            let scheduler = build_scheduler(4, 4);
            let segment = scheduler.lock().assign().unwrap();
            let (tx, mut rx) = mpsc::channel(4);
            let (_stop, mut stop_rx) = watch::channel(StopSignal::Running);
            let downloaded = Arc::new(AtomicU64::new(0));
            let (error, accepted) = stream_segment(
                response,
                Duration::from_secs(2),
                4,
                &segment,
                &tx,
                &downloaded,
                &mut stop_rx,
                &Arc::new(MemoryBudget::new(64)),
                &SpeedLimit::new(0),
            )
            .await
            .unwrap_err();
            server.abort();
            if body_len == 2 {
                assert!(
                    matches!(error, DownloadError::Transport(ref e)
                    if e.kind() == crate::error::TransportErrorKind::Body),
                    "{error:?}"
                );
                assert_eq!(accepted, 2);
            } else {
                assert!(
                    matches!(error, DownloadError::ResumeMismatch(_)),
                    "{error:?}"
                );
                assert!(
                    accepted <= 4,
                    "must not forward bytes past the requested range"
                );
            }
            let mut forwarded = Vec::new();
            while let Ok(command) = rx.try_recv() {
                let WriterCommand::Data {
                    data,
                    offset,
                    lease_key,
                    ..
                } = command
                else {
                    panic!("incomplete segment must not flush its lease");
                };
                assert_eq!(offset, forwarded.len() as u64);
                assert_eq!(lease_key, Some(segment.lease_key()));
                forwarded.extend_from_slice(&data);
            }
            assert_eq!(forwarded, vec![0xAC; accepted as usize]);
            assert_eq!(downloaded.load(Ordering::Relaxed), accepted);
            assert!(!scheduler.lock().all_done());
        }
    }

    fn failure_test_worker_config(worker: HttpWorker) -> Arc<WorkerConfig> {
        Arc::new(WorkerConfig {
            worker,
            read_timeout: Duration::from_secs(2),
            max_retries: 0,
            retry_base_delay: Duration::ZERO,
            retry_max_delay: Duration::ZERO,
            max_retry_elapsed: None,
            max_active_leases: 1,
            min_segment_size: 4,
            recovery: None,
        })
    }

    #[tokio::test]
    async fn worker_preexisting_stop_leaves_piece_unleased() {
        for signal in [StopSignal::Pause, StopSignal::Cancel] {
            let scheduler = build_scheduler(4, 4);
            let (tx, mut rx) = mpsc::channel(1);
            let (_stop, stop_rx) = watch::channel(signal);
            let error = worker_loop(
                0,
                failure_test_worker_config(worker_for("http://example.invalid/unused".into())),
                scheduler.clone(),
                tx,
                Arc::new(AtomicU64::new(0)),
                stop_rx,
                Arc::new(MemoryBudget::new(64)),
                SpeedLimit::new(0),
                None,
                4,
                LogLevel::Off,
                0,
            )
            .await
            .unwrap_err();
            match signal {
                StopSignal::Pause => assert!(matches!(error, DownloadError::Paused)),
                StopSignal::Cancel => assert!(matches!(error, DownloadError::Cancelled)),
                StopSignal::Running => unreachable!(),
            }
            assert!(
                rx.recv().await.is_none(),
                "stopped worker must not begin a lease"
            );
            let mut scheduler = scheduler.lock();
            assert_eq!(scheduler.remaining_count(), 1);
            assert!(scheduler.control_hints().inflight_piece_ids.is_empty());
            assert_eq!(scheduler.assign().unwrap().start, 0);
        }
    }

    #[tokio::test]
    async fn invalid_probe_total_reclaims_lease_without_forwarding_data() {
        let route = warp::any().map(|| vec![0xAC; 4]);
        let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
        let server = tokio::spawn(server);
        let worker = worker_for(format!("http://{addr}/piece"));
        let (response, mut meta) = worker.send_get().await.unwrap();
        meta.content_range_start = Some(0);
        meta.content_range_end = Some(3);
        meta.content_range_total = Some(5);
        let scheduler = build_scheduler(4, 4);
        let (tx, rx) = mpsc::channel(4);
        let writer = spawn_writer_ack(rx);
        let downloaded = Arc::new(AtomicU64::new(0));
        let (_stop, stop_rx) = watch::channel(StopSignal::Running);
        let error = worker_loop(
            0,
            failure_test_worker_config(worker),
            scheduler.clone(),
            tx,
            downloaded.clone(),
            stop_rx,
            Arc::new(MemoryBudget::new(64)),
            SpeedLimit::new(0),
            Some((response, meta, 0)),
            4,
            LogLevel::Off,
            0,
        )
        .await
        .unwrap_err();
        writer.await.unwrap();
        server.abort();
        assert!(
            matches!(error, DownloadError::ResumeMismatch(_)),
            "{error:?}"
        );
        assert_eq!(downloaded.load(Ordering::Relaxed), 0);
        let mut scheduler = scheduler.lock();
        assert_eq!(scheduler.remaining_count(), 1);
        assert!(scheduler.control_hints().inflight_piece_ids.is_empty());
        assert_eq!(scheduler.assign().unwrap().start, 0);
    }

    #[tokio::test]
    async fn writer_discard_failure_rolls_back_received_bytes_and_reclaims_piece() {
        let route = warp::any().map(|| vec![0xAC; 2]);
        let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
        let server = tokio::spawn(server);
        let worker = worker_for(format!("http://{addr}/piece"));
        let (response, mut meta) = worker.send_get().await.unwrap();
        meta.content_length = Some(4);
        meta.content_range_start = Some(0);
        meta.content_range_end = Some(3);
        meta.content_range_total = Some(4);
        let scheduler = build_scheduler(4, 4);
        let (tx, mut rx) = mpsc::channel(4);
        let writer = tokio::spawn(async move {
            assert!(matches!(
                rx.recv().await,
                Some(WriterCommand::BeginLease { .. })
            ));
            let mut received = Vec::new();
            loop {
                match rx.recv().await {
                    Some(WriterCommand::Data { data, offset, .. }) => {
                        assert_eq!(offset, received.len() as u64);
                        received.extend_from_slice(&data);
                    }
                    Some(WriterCommand::DiscardLease { ack, .. }) => {
                        assert_eq!(received, vec![0xAC; 2]);
                        // Model a writer failing after receiving the discard command.
                        drop(ack);
                        break;
                    }
                    _ => panic!("expected prefix data then discard"),
                }
            }
        });
        let downloaded = Arc::new(AtomicU64::new(0));
        let (_stop, stop_rx) = watch::channel(StopSignal::Running);
        let error = worker_loop(
            0,
            failure_test_worker_config(worker),
            scheduler.clone(),
            tx,
            downloaded.clone(),
            stop_rx,
            Arc::new(MemoryBudget::new(64)),
            SpeedLimit::new(0),
            Some((response, meta, 0)),
            4,
            LogLevel::Off,
            0,
        )
        .await
        .unwrap_err();
        writer.await.unwrap();
        server.abort();
        assert!(matches!(error, DownloadError::ChannelClosed), "{error:?}");
        assert_eq!(
            downloaded.load(Ordering::Relaxed),
            0,
            "failed prefix must be rolled back"
        );
        let mut scheduler = scheduler.lock();
        assert_eq!(scheduler.remaining_count(), 1);
        assert!(scheduler.control_hints().inflight_piece_ids.is_empty());
        assert_eq!(scheduler.assign().unwrap().start, 0);
    }

    #[tokio::test]
    async fn failed_checkpoint_save_keeps_tracker_pending_for_next_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-directory");
        tokio::fs::write(&blocker, b"preserve").await.unwrap();
        let control = blocker.join("state.bytehaul");
        let scheduler = build_scheduler(512, 256);
        complete_one_piece(&scheduler);
        let spec = DownloadSpec::new("http://example.invalid/piece").autosave_sync_every(1);
        let meta = response_meta();
        let ctx = MultiControlSaveContext {
            spec: &spec,
            meta: &meta,
            request_url: &spec.url,
            scheduler: &scheduler,
            total_size: 512,
            control_path: &control,
            log_level: LogLevel::Off,
            download_id: 0,
        };
        let mut tracker = ControlSaveTracker::new(0);
        persist_multi_control_snapshot(ControlSaveReason::Autosave, None, &mut tracker, &ctx).await;
        assert_eq!(tracker.last_saved_downloaded_bytes(), 0);
        assert_eq!(tokio::fs::read(&blocker).await.unwrap(), b"preserve");
        tokio::fs::remove_file(&blocker).await.unwrap();
        tokio::fs::create_dir(&blocker).await.unwrap();
        persist_multi_control_snapshot(ControlSaveReason::Autosave, None, &mut tracker, &ctx).await;
        assert_eq!(tracker.last_saved_downloaded_bytes(), 256);
        assert_eq!(
            ControlSnapshot::load(&control)
                .await
                .unwrap()
                .completed_bitset,
            vec![1]
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn multi_writer_failure_does_not_replace_existing_checkpoint() {
        let (addr, server) = spawn_flaky_range_server(0, None);
        let server = tokio::spawn(server);
        let dir = tempfile::tempdir().unwrap();
        let control = dir.path().join("previous.bytehaul");
        let spec = DownloadSpec::new(format!("http://{addr}/piece"))
            .resume(true)
            .piece_size(256)
            .max_connections(4)
            .file_allocation(crate::config::FileAllocation::None);
        let previous = ControlSnapshot {
            url: spec.url.clone(),
            total_size: 256,
            piece_size: 256,
            piece_count: 1,
            completed_bitset: vec![0],
            downloaded_bytes: 0,
            etag: None,
            last_modified: None,
        };
        previous.save(&control).await.unwrap();
        let original = tokio::fs::read(&control).await.unwrap();
        let (progress, _) = watch::channel(ProgressSnapshot::default());
        let (_stop, stop_rx) = watch::channel(StopSignal::Running);
        let client = crate::network::ClientNetworkConfig::default()
            .build_client()
            .unwrap();
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            run_multi_worker(
                client,
                &spec,
                &spec.url,
                Path::new("/dev/full"),
                &response_meta(),
                256,
                PieceMap::new(256, 256),
                None,
                &progress,
                stop_rx,
                &control,
                SpeedLimit::new(0),
                LogLevel::Off,
                0,
            ),
        )
        .await
        .unwrap()
        .unwrap_err();
        server.abort();
        assert!(matches!(error, DownloadError::Io(_)), "{error:?}");
        assert_eq!(progress.borrow().state, DownloadState::Failed);
        assert_eq!(tokio::fs::read(&control).await.unwrap(), original);
        assert_eq!(
            ControlSnapshot::load(&control)
                .await
                .unwrap()
                .downloaded_bytes,
            0
        );
    }

    #[tokio::test]
    async fn test_run_multi_worker_dynamic_split_requests_are_concurrent() {
        async fn run_case(
            path_segment: &'static str,
            min_segment_size: u64,
            expected_ranges: &[(u64, u64)],
        ) {
            let (arrivals_tx, mut arrivals_rx) = mpsc::unbounded_channel();
            let (addr, server) = spawn_gated_range_server(path_segment, arrivals_tx);
            let server = tokio::spawn(server);

            let dir = tempfile::tempdir().unwrap();
            let output_path = dir.path().join(format!("{path_segment}.bin"));
            let control_path = ControlSnapshot::control_path(&output_path);
            let meta = ResponseMeta {
                content_length: Some(1024),
                content_range_start: Some(0),
                content_range_end: Some(1023),
                content_range_total: Some(1024),
                accept_ranges: true,
                etag: Some("\"split-concurrency\"".into()),
                last_modified: Some("Thu, 01 Jan 2026 00:00:00 GMT".into()),
                content_disposition: None,
                content_encoding: None,
            };
            let spec = DownloadSpec::new(format!("http://{addr}/{path_segment}"))
                .piece_size(1024)
                .min_segment_size(min_segment_size)
                .min_split_size(1)
                .max_connections(4)
                .max_retries(0)
                .file_allocation(crate::config::FileAllocation::None)
                .output_path(output_path.clone());
            let piece_map = PieceMap::new(1024, 1024);
            let (progress_tx, _progress_rx) = watch::channel(ProgressSnapshot::default());
            let (_cancel_tx, cancel_rx) = watch::channel(StopSignal::Running);
            let client = crate::network::ClientNetworkConfig::default()
                .build_client()
                .unwrap();

            let download = run_multi_worker(
                client,
                &spec,
                &spec.url,
                &output_path,
                &meta,
                1024,
                piece_map,
                None,
                &progress_tx,
                cancel_rx,
                &control_path,
                SpeedLimit::new(0),
                LogLevel::Off,
                10,
            );
            let release_responses = async {
                let mut ranges = Vec::new();
                let mut releases = Vec::new();
                for _ in expected_ranges {
                    let (start, end, release) = arrivals_rx.recv().await.unwrap();
                    ranges.push((start, end));
                    releases.push(release);
                }
                ranges.sort_unstable();
                assert_eq!(ranges, expected_ranges);
                for release in releases {
                    release.send(()).unwrap();
                }
            };
            // This only bounds a broken scheduler/serial request deadlock. It is
            // not a speed comparison between machines or transfer strategies.
            let completed = tokio::time::timeout(Duration::from_secs(30), async {
                tokio::join!(download, release_responses).0.unwrap();
            })
            .await;
            server.abort();
            completed.expect("all expected ranges must arrive before any response is released");

            assert!(
                arrivals_rx.try_recv().is_err(),
                "unexpected extra range request"
            );
            let downloaded = std::fs::read(&output_path).unwrap();
            let expected: Vec<u8> = (0..1024).map(|offset| (offset % 251) as u8).collect();
            assert_eq!(downloaded, expected);
            let progress = progress_tx.borrow();
            assert_eq!(progress.state, DownloadState::Completed);
            assert_eq!(progress.downloaded, 1024);
            assert_eq!(progress.total_size, Some(1024));
        }

        run_case("unsplit", 1024, &[(0, 1023)]).await;
        run_case(
            "split",
            256,
            &[(0, 255), (256, 511), (512, 767), (768, 1023)],
        )
        .await;
    }
}
