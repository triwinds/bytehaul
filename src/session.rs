use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::stream::FuturesUnordered;
use futures::StreamExt;
use tokio::sync::{mpsc, oneshot, watch, Semaphore};

use crate::checksum::verify_checksum;
use crate::config::{DownloadSpec, LogLevel};
use crate::error::DownloadError;
use crate::eta::EtaEstimator;
use crate::filename::{detect_filename, sanitize_relative_path};
use crate::http::request::build_range_request;
use crate::http::response::ResponseMeta;
use crate::http::worker::HttpWorker;
use crate::progress::{DownloadState, ProgressSnapshot};
use crate::rate_limiter::SpeedLimit;
use crate::scheduler::{Scheduler, SchedulerState};
use crate::storage::control::ControlSnapshot;
use crate::storage::file::{create_output_file, open_existing_file};
use crate::storage::piece_map::PieceMap;
use crate::storage::segment::Segment;
use crate::storage::writer::{WriterCommand, WriterTask};

const CONTROL_SAVE_INTERVAL: Duration = Duration::from_secs(5);
const ETA_ALPHA: f64 = 0.3;
const SINGLE_ETA_SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StopSignal {
    Running,
    Cancel,
    Pause,
}

impl StopSignal {
    fn is_stop_requested(self) -> bool {
        !matches!(self, Self::Running)
    }
}

fn stop_signal_error(signal: StopSignal) -> Option<DownloadError> {
    match signal {
        StopSignal::Running => None,
        StopSignal::Cancel => Some(DownloadError::Cancelled),
        StopSignal::Pause => Some(DownloadError::Paused),
    }
}

fn stop_signal_state(signal: StopSignal) -> Option<DownloadState> {
    match signal {
        StopSignal::Running => None,
        StopSignal::Cancel => Some(DownloadState::Cancelled),
        StopSignal::Pause => Some(DownloadState::Paused),
    }
}

fn stop_signal_label(signal: StopSignal) -> &'static str {
    match signal {
        StopSignal::Running => "running",
        StopSignal::Cancel => "cancelled",
        StopSignal::Pause => "paused",
    }
}

/// Retry an async operation with exponential backoff.
async fn retry_with_backoff<F, Fut, T>(
    max_retries: u32,
    base_delay: Duration,
    max_delay: Duration,
    cancel_rx: &mut watch::Receiver<StopSignal>,
    mut op: F,
) -> Result<T, DownloadError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, DownloadError>>,
{
    let mut attempt = 0u32;
    loop {
        match op().await {
            Ok(val) => return Ok(val),
            Err(e) => {
                if !e.is_retryable() || attempt >= max_retries {
                    return Err(e);
                }
                attempt += 1;
                let backoff = if let Some(retry_secs) = e.retry_after_secs() {
                    Duration::from_secs(retry_secs)
                } else {
                    base_delay
                        .saturating_mul(1u32 << attempt.min(10))
                        .min(max_delay)
                };
                tokio::select! {
                    biased;
                    result = cancel_rx.changed() => {
                        if result.is_ok() {
                            if let Some(error) = stop_signal_error(*cancel_rx.borrow_and_update()) {
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

// ──────────────────────────────────────────────────────────────
//  Entry point
// ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
enum FreshResponseSource {
    RangeProbe,
    FallbackGet,
}

fn resolve_output_dir(spec: &DownloadSpec) -> Result<PathBuf, DownloadError> {
    let cwd = std::env::current_dir()?;
    let dir = spec.output_dir.clone().unwrap_or_else(|| cwd.clone());
    if dir.is_absolute() {
        Ok(dir)
    } else {
        Ok(cwd.join(dir))
    }
}

fn resolve_static_output_path(spec: &DownloadSpec) -> Result<Option<PathBuf>, DownloadError> {
    let Some(output_path) = &spec.output_path else {
        return Ok(None);
    };
    if output_path.is_absolute() {
        if spec.output_dir.is_some() {
            return Err(DownloadError::Other(
                "output_path must be relative when output_dir is set".into(),
            ));
        }
        return Ok(Some(output_path.clone()));
    }
    let relative = sanitize_relative_path(output_path).ok_or_else(|| {
        DownloadError::Other(
            "output_path must be a relative path without root prefixes or parent traversal"
                .into(),
        )
    })?;
    Ok(Some(resolve_output_dir(spec)?.join(relative)))
}

fn resolve_auto_output_path(
    spec: &DownloadSpec,
    meta: &ResponseMeta,
) -> Result<PathBuf, DownloadError> {
    Ok(resolve_output_dir(spec)?.join(detect_filename(
        meta.content_disposition.as_deref(),
        &spec.url,
    )))
}

async fn try_resume_download(
    client: reqwest::Client,
    worker: &HttpWorker,
    spec: &DownloadSpec,
    output_path: &Path,
    progress_tx: &watch::Sender<ProgressSnapshot>,
    cancel_rx: &mut watch::Receiver<StopSignal>,
    speed_limit: SpeedLimit,
    log_level: LogLevel,
    download_id: u64,
) -> Result<Option<PathBuf>, DownloadError> {
    let control_path = ControlSnapshot::control_path(output_path);
    let resume_ctrl = if spec.resume {
        match ControlSnapshot::load(&control_path).await {
            Ok(ctrl) => {
                log_debug!(
                    log_level,
                    download_id,
                    downloaded_bytes = ctrl.downloaded_bytes,
                    total_size = ctrl.total_size,
                    piece_count = ctrl.piece_count,
                    "control file loaded"
                );
                Some(ctrl)
            }
            Err(_) => None,
        }
    } else {
        None
    };

    if let Some(ctrl) = resume_ctrl {
        let is_multi = ctrl.piece_count > 1 && spec.max_connections > 1;

        if is_multi {
            let piece_map = PieceMap::from_bitset(
                ctrl.total_size,
                ctrl.piece_size,
                &ctrl.completed_bitset,
                ctrl.piece_count,
            );
            if let Some(probe_piece) = piece_map.next_missing_excluding(&HashSet::new()) {
                let (start, end) = piece_map.piece_range(probe_piece);
                let probe_result = retry_with_backoff(
                    spec.max_retries,
                    spec.retry_base_delay,
                    spec.retry_max_delay,
                    cancel_rx,
                    || worker.send_range(start, end - 1),
                )
                .await;
                if let Ok((resp, meta)) = probe_result {
                    if resp.status().as_u16() == 206
                        && range_response_allowed(&meta)
                        && validate_metadata(&meta, &ctrl)
                    {
                        log_info!(
                            log_level,
                            download_id,
                            strategy = "resumed multi",
                            total_size = ctrl.total_size,
                            completed_pieces = piece_map.completed_count(),
                            "download strategy selected"
                        );
                        run_multi_worker(
                            client,
                            spec,
                            output_path,
                            &meta,
                            ctrl.total_size,
                            piece_map,
                            Some((resp, probe_piece)),
                            progress_tx,
                            cancel_rx.clone(),
                            &control_path,
                            speed_limit,
                            log_level,
                            download_id,
                        )
                        .await?;
                        return Ok(Some(output_path.to_path_buf()));
                    }
                }
            }
        } else if ctrl.downloaded_bytes > 0 && ctrl.downloaded_bytes < ctrl.total_size {
            let probe_result = retry_with_backoff(
                spec.max_retries,
                spec.retry_base_delay,
                spec.retry_max_delay,
                cancel_rx,
                || worker.send_range(ctrl.downloaded_bytes, ctrl.total_size - 1),
            )
            .await;
            if let Ok((resp, meta)) = probe_result {
                if resp.status().as_u16() == 206
                    && range_response_allowed(&meta)
                    && validate_metadata(&meta, &ctrl)
                {
                    log_info!(
                        log_level,
                        download_id,
                        strategy = "resumed single",
                        start_offset = ctrl.downloaded_bytes,
                        total_size = ctrl.total_size,
                        "download strategy selected"
                    );
                    run_single_connection(
                        resp,
                        &meta,
                        spec,
                        output_path,
                        ctrl.downloaded_bytes,
                        progress_tx,
                        cancel_rx.clone(),
                        &control_path,
                        Some(ctrl.total_size),
                        speed_limit,
                        log_level,
                        download_id,
                    )
                    .await?;
                    return Ok(Some(output_path.to_path_buf()));
                }
            }
        }

        log_warn!(log_level, download_id, "control file discarded, restarting download");
        let _ = ControlSnapshot::delete(&control_path).await;
    }

    Ok(None)
}

#[allow(clippy::too_many_arguments)]
async fn run_fresh_from_response(
    client: reqwest::Client,
    spec: &DownloadSpec,
    output_path: &Path,
    response: reqwest::Response,
    meta: ResponseMeta,
    source: FreshResponseSource,
    progress_tx: &watch::Sender<ProgressSnapshot>,
    cancel_rx: watch::Receiver<StopSignal>,
    speed_limit: SpeedLimit,
    log_level: LogLevel,
    download_id: u64,
) -> Result<PathBuf, DownloadError> {
    let control_path = ControlSnapshot::control_path(output_path);

    if spec.max_connections > 1
        && matches!(source, FreshResponseSource::RangeProbe)
        && response.status().as_u16() == 206
        && range_response_allowed(&meta)
    {
        if let Some(total_size) = meta.content_range_total {
            if total_size > spec.min_split_size {
                log_info!(
                    log_level,
                    download_id,
                    strategy = "fresh multi",
                    total_size,
                    max_connections = spec.max_connections,
                    piece_size = spec.piece_size,
                    "download strategy selected"
                );
                let piece_map = PieceMap::new(total_size, spec.piece_size);
                run_multi_worker(
                    client,
                    spec,
                    output_path,
                    &meta,
                    total_size,
                    piece_map,
                    Some((response, 0)),
                    progress_tx,
                    cancel_rx,
                    &control_path,
                    speed_limit,
                    log_level,
                    download_id,
                )
                .await?;
                return Ok(output_path.to_path_buf());
            }
        }

        let total = meta.content_range_total;
        log_info!(
            log_level,
            download_id,
            strategy = "fresh single",
            total_size = total,
            reason = "file below min_split_size",
            "download strategy selected"
        );
        run_single_connection(
            response,
            &meta,
            spec,
            output_path,
            0,
            progress_tx,
            cancel_rx,
            &control_path,
            total,
            speed_limit,
            log_level,
            download_id,
        )
        .await?;
        return Ok(output_path.to_path_buf());
    }

    let reason = match source {
        FreshResponseSource::RangeProbe if response.status().as_u16() != 206 => {
            "range not supported (200 response)"
        }
        FreshResponseSource::RangeProbe => "file below min_split_size",
        FreshResponseSource::FallbackGet => "fallback GET (single connection or range probe failed)",
    };
    log_info!(
        log_level,
        download_id,
        strategy = "fresh single",
        reason,
        "download strategy selected"
    );
    let total = if response.status().as_u16() == 206 {
        meta.content_range_total
    } else {
        meta.content_length
    };
    run_single_connection(
        response,
        &meta,
        spec,
        output_path,
        0,
        progress_tx,
        cancel_rx,
        &control_path,
        total,
        speed_limit,
        log_level,
        download_id,
    )
    .await?;
    Ok(output_path.to_path_buf())
}

pub(crate) async fn run_download(
    client: reqwest::Client,
    spec: DownloadSpec,
    log_level: LogLevel,
    download_id: u64,
    progress_tx: watch::Sender<ProgressSnapshot>,
    cancel_rx: watch::Receiver<StopSignal>,
) -> Result<(), DownloadError> {
    let checksum = spec.checksum.clone();
    let result = run_download_inner(client, spec, log_level, download_id, &progress_tx, cancel_rx).await;

    let output_path = match result {
        Ok(output_path) => output_path,
        Err(e) => {
            log_error!(log_level, download_id, error = %e, "download failed");
            return Err(e);
        }
    };

    // Post-download checksum verification
    if let Some(ref expected) = checksum {
        log_info!(log_level, download_id, algorithm = "sha256", "checksum verification started");
        match verify_checksum(&output_path, expected).await {
            Ok(()) => {
                log_info!(log_level, download_id, "checksum verification passed");
            }
            Err(e) => {
                log_error!(log_level, download_id, error = %e, "checksum verification failed");
                return Err(e);
            }
        }
    }

    log_info!(log_level, download_id, output = %output_path.display(), "download completed successfully");
    Ok(())
}

async fn run_download_inner(
    client: reqwest::Client,
    spec: DownloadSpec,
    log_level: LogLevel,
    download_id: u64,
    progress_tx: &watch::Sender<ProgressSnapshot>,
    cancel_rx: watch::Receiver<StopSignal>,
) -> Result<PathBuf, DownloadError> {
    let worker = HttpWorker::new(client.clone(), &spec);
    let mut cancel_rx = cancel_rx;
    let speed_limit = SpeedLimit::new(spec.max_download_speed);

    if let Some(output_path) = resolve_static_output_path(&spec)? {
        if let Some(resumed_path) = try_resume_download(
            client.clone(),
            &worker,
            &spec,
            &output_path,
            progress_tx,
            &mut cancel_rx,
            speed_limit.clone(),
            log_level,
            download_id,
        )
        .await?
        {
            return Ok(resumed_path);
        }

        return if spec.max_connections > 1 {
            let piece_end = spec.piece_size.saturating_sub(1);
            if let Ok((resp, meta)) = worker.send_range(0, piece_end).await {
                run_fresh_from_response(
                    client,
                    &spec,
                    &output_path,
                    resp,
                    meta,
                    FreshResponseSource::RangeProbe,
                    progress_tx,
                    cancel_rx,
                    speed_limit,
                    log_level,
                    download_id,
                )
                .await
            } else {
                let (resp, meta) = retry_with_backoff(
                    spec.max_retries,
                    spec.retry_base_delay,
                    spec.retry_max_delay,
                    &mut cancel_rx,
                    || worker.send_get(),
                )
                .await?;
                run_fresh_from_response(
                    client,
                    &spec,
                    &output_path,
                    resp,
                    meta,
                    FreshResponseSource::FallbackGet,
                    progress_tx,
                    cancel_rx,
                    speed_limit,
                    log_level,
                    download_id,
                )
                .await
            }
        } else {
            let (resp, meta) = retry_with_backoff(
                spec.max_retries,
                spec.retry_base_delay,
                spec.retry_max_delay,
                &mut cancel_rx,
                || worker.send_get(),
            )
            .await?;
            run_fresh_from_response(
                client,
                &spec,
                &output_path,
                resp,
                meta,
                FreshResponseSource::FallbackGet,
                progress_tx,
                cancel_rx,
                speed_limit,
                log_level,
                download_id,
            )
            .await
        };
    }

    let (initial_response, initial_meta, source) = if spec.max_connections > 1 {
        let piece_end = spec.piece_size.saturating_sub(1);
        match worker.send_range(0, piece_end).await {
            Ok((resp, meta)) => (resp, meta, FreshResponseSource::RangeProbe),
            Err(_) => {
                let (resp, meta) = retry_with_backoff(
                    spec.max_retries,
                    spec.retry_base_delay,
                    spec.retry_max_delay,
                    &mut cancel_rx,
                    || worker.send_get(),
                )
                .await?;
                (resp, meta, FreshResponseSource::FallbackGet)
            }
        }
    } else {
        let (resp, meta) = retry_with_backoff(
            spec.max_retries,
            spec.retry_base_delay,
            spec.retry_max_delay,
            &mut cancel_rx,
            || worker.send_get(),
        )
        .await?;
        (resp, meta, FreshResponseSource::FallbackGet)
    };

    let output_path = resolve_auto_output_path(&spec, &initial_meta)?;
    if let Some(resumed_path) = try_resume_download(
        client.clone(),
        &worker,
        &spec,
        &output_path,
        progress_tx,
        &mut cancel_rx,
        speed_limit.clone(),
        log_level,
        download_id,
    )
    .await?
    {
        return Ok(resumed_path);
    }

    run_fresh_from_response(
        client,
        &spec,
        &output_path,
        initial_response,
        initial_meta,
        source,
        progress_tx,
        cancel_rx,
        speed_limit,
        log_level,
        download_id,
    )
    .await
}

// ──────────────────────────────────────────────────────────────
//  Single-connection download  (M1/M2 path)
// ──────────────────────────────────────────────────────────────

async fn run_single_connection(
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
    )
    .await;

    drop(write_tx);
    let writer_result = writer_handle
        .await
        .map_err(|e| DownloadError::Other(format!("writer panicked: {e}")))?;

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
) -> Result<(), DownloadError> {
    let mut stream = response.bytes_stream();
    let mut downloaded: u64 = start_offset;
    let start_time = Instant::now();
    let mut eta_estimator = EtaEstimator::new(ETA_ALPHA);
    let mut last_sample_time = start_time;
    let mut bytes_since_last_sample = 0u64;
    let mut cancel_rx = cancel_rx;
    let mut save_ticker = tokio::time::interval(CONTROL_SAVE_INTERVAL);
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
                            .map_err(|_| DownloadError::Other("budget semaphore closed".into()))?;

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

// ──────────────────────────────────────────────────────────────
//  Multi-worker download  (M3 path)
// ──────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn run_multi_worker(
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

    for worker_id in 0..num_workers {
        let first = if worker_id == 0 {
            probe_response.take()
        } else {
            None
        };
        let handle = tokio::spawn(worker_loop(
            worker_id,
            client.clone(),
            spec.url.clone(),
            spec.headers.clone(),
            spec.read_timeout,
            spec.max_retries,
            spec.retry_base_delay,
            spec.retry_max_delay,
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
    let mut save_ticker = tokio::time::interval(CONTROL_SAVE_INTERVAL);
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
                            download_error = Some(DownloadError::Other(
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
        .map_err(|e| DownloadError::Other(format!("writer panicked: {e}")))?;

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
        return Err(DownloadError::Other("download incomplete".into()));
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

#[allow(clippy::too_many_arguments)]
async fn worker_loop(
    worker_id: usize,
    client: reqwest::Client,
    url: String,
    headers: std::collections::HashMap<String, String>,
    timeout: Duration,
    max_retries: u32,
    retry_base_delay: Duration,
    retry_max_delay: Duration,
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
                        &url,
                        &headers,
                        timeout,
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
                    &url,
                    &headers,
                    timeout,
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
                    if !e.is_retryable() || attempt >= max_retries {
                        log_warn!(log_level, download_id = download_id, worker_id = worker_id,
                            piece_id = segment.piece_id, error = %e,
                            "segment failed, reclaiming (non-retryable or max retries exceeded)");
                        scheduler.lock().reclaim(segment.piece_id);
                        return Err(e);
                    }

                    attempt += 1;

                    // Compute backoff delay
                    let backoff = if let Some(retry_secs) = e.retry_after_secs() {
                        Duration::from_secs(retry_secs)
                    } else {
                        let exp = retry_base_delay.saturating_mul(1u32 << attempt.min(10));
                        exp.min(retry_max_delay)
                    };

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
                            .map_err(|_| DownloadError::Other("budget semaphore closed".into()))?;

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

// ──────────────────────────────────────────────────────────────
//  Helpers
// ──────────────────────────────────────────────────────────────

fn validate_metadata(meta: &ResponseMeta, ctrl: &ControlSnapshot) -> bool {
    if let Some(total) = meta.content_range_total {
        if total != ctrl.total_size {
            return false;
        }
    }
    if let (Some(expected), Some(actual)) = (&ctrl.etag, &meta.etag) {
        if expected != actual {
            return false;
        }
    }
    if let (Some(expected), Some(actual)) = (&ctrl.last_modified, &meta.last_modified) {
        if expected != actual {
            return false;
        }
    }
    true
}

fn range_response_allowed(meta: &ResponseMeta) -> bool {
    !matches!(
        meta.content_encoding.as_deref(),
        Some(encoding) if !encoding.eq_ignore_ascii_case("identity")
    )
}

async fn flush_piece_and_wait(
    write_tx: &mpsc::Sender<WriterCommand>,
    piece_id: usize,
) -> Result<(), DownloadError> {
    let (ack_tx, ack_rx) = oneshot::channel();
    write_tx
        .send(WriterCommand::FlushPiece {
            piece_id,
            ack: ack_tx,
        })
        .await
        .map_err(|_| DownloadError::ChannelClosed)?;
    ack_rx.await.map_err(|_| DownloadError::ChannelClosed)
}

async fn flush_all_and_wait(
    write_tx: &mpsc::Sender<WriterCommand>,
    sync_data: bool,
) -> Result<u64, DownloadError> {
    let (ack_tx, ack_rx) = oneshot::channel();
    write_tx
        .send(WriterCommand::FlushAll {
            sync_data,
            ack: ack_tx,
        })
        .await
        .map_err(|_| DownloadError::ChannelClosed)?;
    ack_rx.await.map_err(|_| DownloadError::ChannelClosed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_range_response_allowed_no_encoding() {
        let meta = ResponseMeta {
            content_length: None,
            content_range_start: None,
            content_range_end: None,
            content_range_total: None,
            accept_ranges: false,
            etag: None,
            last_modified: None,
            content_disposition: None,
            content_encoding: None,
        };
        assert!(range_response_allowed(&meta));
    }

    #[test]
    fn test_range_response_allowed_identity() {
        let meta = ResponseMeta {
            content_length: None,
            content_range_start: None,
            content_range_end: None,
            content_range_total: None,
            accept_ranges: false,
            etag: None,
            last_modified: None,
            content_disposition: None,
            content_encoding: Some("identity".into()),
        };
        assert!(range_response_allowed(&meta));
    }

    #[test]
    fn test_range_response_disallowed_gzip() {
        let meta = ResponseMeta {
            content_length: None,
            content_range_start: None,
            content_range_end: None,
            content_range_total: None,
            accept_ranges: false,
            etag: None,
            last_modified: None,
            content_disposition: None,
            content_encoding: Some("gzip".into()),
        };
        assert!(!range_response_allowed(&meta));
    }

    #[test]
    fn test_validate_metadata_matching() {
        let meta = ResponseMeta {
            content_length: None,
            content_range_start: None,
            content_range_end: None,
            content_range_total: Some(1000),
            accept_ranges: true,
            etag: Some("\"abc\"".into()),
            last_modified: Some("Thu, 01 Jan 2026 00:00:00 GMT".into()),
            content_disposition: None,
            content_encoding: None,
        };
        let ctrl = ControlSnapshot {
            url: "https://example.com".into(),
            total_size: 1000,
            piece_size: 1000,
            piece_count: 1,
            completed_bitset: vec![0],
            downloaded_bytes: 0,
            etag: Some("\"abc\"".into()),
            last_modified: Some("Thu, 01 Jan 2026 00:00:00 GMT".into()),
        };
        assert!(validate_metadata(&meta, &ctrl));
    }

    #[test]
    fn test_validate_metadata_size_mismatch() {
        let meta = ResponseMeta {
            content_length: None,
            content_range_start: None,
            content_range_end: None,
            content_range_total: Some(2000),
            accept_ranges: true,
            etag: None,
            last_modified: None,
            content_disposition: None,
            content_encoding: None,
        };
        let ctrl = ControlSnapshot {
            url: "https://example.com".into(),
            total_size: 1000,
            piece_size: 1000,
            piece_count: 1,
            completed_bitset: vec![0],
            downloaded_bytes: 0,
            etag: None,
            last_modified: None,
        };
        assert!(!validate_metadata(&meta, &ctrl));
    }

    #[test]
    fn test_validate_metadata_etag_mismatch() {
        let meta = ResponseMeta {
            content_length: None,
            content_range_start: None,
            content_range_end: None,
            content_range_total: Some(1000),
            accept_ranges: true,
            etag: Some("\"new\"".into()),
            last_modified: None,
            content_disposition: None,
            content_encoding: None,
        };
        let ctrl = ControlSnapshot {
            url: "https://example.com".into(),
            total_size: 1000,
            piece_size: 1000,
            piece_count: 1,
            completed_bitset: vec![0],
            downloaded_bytes: 0,
            etag: Some("\"old\"".into()),
            last_modified: None,
        };
        assert!(!validate_metadata(&meta, &ctrl));
    }

    #[test]
    fn test_validate_metadata_last_modified_mismatch() {
        let meta = ResponseMeta {
            content_length: None,
            content_range_start: None,
            content_range_end: None,
            content_range_total: Some(1000),
            accept_ranges: true,
            etag: None,
            last_modified: Some("Fri, 02 Jan 2026".into()),
            content_disposition: None,
            content_encoding: None,
        };
        let ctrl = ControlSnapshot {
            url: "https://example.com".into(),
            total_size: 1000,
            piece_size: 1000,
            piece_count: 1,
            completed_bitset: vec![0],
            downloaded_bytes: 0,
            etag: None,
            last_modified: Some("Thu, 01 Jan 2026".into()),
        };
        assert!(!validate_metadata(&meta, &ctrl));
    }

    #[test]
    fn test_validate_metadata_no_total_in_response() {
        let meta = ResponseMeta {
            content_length: None,
            content_range_start: None,
            content_range_end: None,
            content_range_total: None,
            accept_ranges: true,
            etag: None,
            last_modified: None,
            content_disposition: None,
            content_encoding: None,
        };
        let ctrl = ControlSnapshot {
            url: "https://example.com".into(),
            total_size: 1000,
            piece_size: 1000,
            piece_count: 1,
            completed_bitset: vec![0],
            downloaded_bytes: 0,
            etag: None,
            last_modified: None,
        };
        assert!(validate_metadata(&meta, &ctrl));
    }

    #[tokio::test]
    async fn test_retry_with_backoff_immediate_success() {
        let (_, mut cancel_rx) = watch::channel(StopSignal::Running);
        let result: Result<i32, DownloadError> = retry_with_backoff(
            3,
            Duration::from_millis(10),
            Duration::from_millis(100),
            &mut cancel_rx,
            || async { Ok(42) },
        )
        .await;
        assert_eq!(result.unwrap(), 42);
    }

    #[tokio::test]
    async fn test_retry_with_backoff_non_retryable() {
        let (_, mut cancel_rx) = watch::channel(StopSignal::Running);
        let result: Result<i32, DownloadError> = retry_with_backoff(
            3,
            Duration::from_millis(10),
            Duration::from_millis(100),
            &mut cancel_rx,
            || async { Err(DownloadError::Cancelled) },
        )
        .await;
        assert!(matches!(result, Err(DownloadError::Cancelled)));
    }

    #[tokio::test]
    async fn test_retry_with_backoff_retries_then_succeeds() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let (_, mut cancel_rx) = watch::channel(StopSignal::Running);
        let attempts = Arc::new(AtomicU32::new(0));
        let attempts_clone = attempts.clone();

        let result: Result<i32, DownloadError> = retry_with_backoff(
            3,
            Duration::from_millis(1),
            Duration::from_millis(10),
            &mut cancel_rx,
            move || {
                let attempts = attempts_clone.clone();
                async move {
                    let n = attempts.fetch_add(1, Ordering::Relaxed);
                    if n < 2 {
                        Err(DownloadError::HttpStatus {
                            status: 503,
                            message: "Service Unavailable".into(),
                        })
                    } else {
                        Ok(42)
                    }
                }
            },
        )
        .await;
        assert_eq!(result.unwrap(), 42);
        assert_eq!(attempts.load(Ordering::Relaxed), 3);
    }

    #[tokio::test]
    async fn test_retry_with_backoff_exhausts_retries() {
        let (_, mut cancel_rx) = watch::channel(StopSignal::Running);
        let result: Result<i32, DownloadError> = retry_with_backoff(
            2,
            Duration::from_millis(1),
            Duration::from_millis(10),
            &mut cancel_rx,
            || async {
                Err(DownloadError::HttpStatus {
                    status: 503,
                    message: "Service Unavailable".into(),
                })
            },
        )
        .await;
        assert!(matches!(
            result,
            Err(DownloadError::HttpStatus { status: 503, .. })
        ));
    }

    #[tokio::test]
    async fn test_retry_with_backoff_with_retry_after_hint() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let (_, mut cancel_rx) = watch::channel(StopSignal::Running);
        let attempts = Arc::new(AtomicU32::new(0));
        let attempts_clone = attempts.clone();

        let result: Result<i32, DownloadError> = retry_with_backoff(
            3,
            Duration::from_millis(1),
            Duration::from_millis(10),
            &mut cancel_rx,
            move || {
                let attempts = attempts_clone.clone();
                async move {
                    let n = attempts.fetch_add(1, Ordering::Relaxed);
                    if n < 1 {
                        Err(DownloadError::HttpStatus {
                            status: 429,
                            message: "retry-after:1".into(),
                        })
                    } else {
                        Ok(99)
                    }
                }
            },
        )
        .await;
        assert_eq!(result.unwrap(), 99);
    }

    #[tokio::test]
    async fn test_retry_with_backoff_cancelled() {
        let (cancel_tx, mut cancel_rx) = watch::channel(StopSignal::Running);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            let _ = cancel_tx.send(StopSignal::Cancel);
        });

        let result: Result<i32, DownloadError> = retry_with_backoff(
            10,
            Duration::from_secs(10),
            Duration::from_secs(60),
            &mut cancel_rx,
            || async {
                Err(DownloadError::HttpStatus {
                    status: 503,
                    message: "retry-after:60".into(),
                })
            },
        )
        .await;
        assert!(matches!(result, Err(DownloadError::Cancelled)));
    }

    #[tokio::test]
    async fn test_retry_with_backoff_paused() {
        let (cancel_tx, mut cancel_rx) = watch::channel(StopSignal::Running);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            let _ = cancel_tx.send(StopSignal::Pause);
        });

        let result: Result<i32, DownloadError> = retry_with_backoff(
            10,
            Duration::from_secs(10),
            Duration::from_secs(60),
            &mut cancel_rx,
            || async {
                Err(DownloadError::HttpStatus {
                    status: 503,
                    message: "retry-after:60".into(),
                })
            },
        )
        .await;
        assert!(matches!(result, Err(DownloadError::Paused)));
    }

    #[tokio::test]
    async fn test_flush_piece_and_wait_closed_channel() {
        let (tx, rx) = mpsc::channel::<WriterCommand>(1);
        drop(rx);
        let result = flush_piece_and_wait(&tx, 0).await;
        assert!(matches!(result, Err(DownloadError::ChannelClosed)));
    }

    #[tokio::test]
    async fn test_flush_all_and_wait_closed_channel() {
        let (tx, rx) = mpsc::channel::<WriterCommand>(1);
        drop(rx);
        let result = flush_all_and_wait(&tx, false).await;
        assert!(matches!(result, Err(DownloadError::ChannelClosed)));
    }

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
