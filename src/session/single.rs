use std::path::Path;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, watch};

use super::flow::MemoryBudget;

use super::{
    flush_all_and_wait, stop_signal_error, stop_signal_state, ControlSaveReason,
    ControlSaveTracker, StopSignal, MIN_SPEED_SAMPLE_SPAN, SPEED_ESTIMATE_WINDOW,
};
use crate::config::{DownloadSpec, LogLevel};
use crate::error::{DownloadError, TransportError, TransportErrorKind};
use crate::eta::EtaEstimator;
use crate::http::response::ResponseMeta;
use crate::http::worker::HttpWorker;
use crate::http::{next_data_chunk, HttpResponse};
use crate::progress::{
    DownloadState, ProgressReporter, ProgressSnapshot, ProgressUpdate, PROGRESS_REPORT_BYTES,
    PROGRESS_REPORT_INTERVAL,
};
use crate::rate_limiter::SpeedLimit;
use crate::storage::control::ControlSnapshot;
use crate::storage::file::{create_output_file, open_existing_file};
use crate::storage::writer::{FlushAllStats, WriterCommand, WriterTask};

use super::range_validate::{
    validate_range_response, ExpectedRange, RangeValidationDecision, RangeValidationMode,
};
use super::retry::{sleep_with_backoff, RetryDecision, RetryState};

#[cfg(test)]
#[derive(Debug, Clone, Copy)]
struct SingleStreamSummary {
    downloaded: u64,
    speed_bytes_per_sec: f64,
}

struct SingleControlSaveContext<'a> {
    control_path: &'a Path,
    snap_template: &'a ControlSnapshot,
    autosave_sync_every: u32,
    log_level: LogLevel,
    download_id: u64,
}

#[derive(Debug)]
enum SingleAttemptOutcome {
    Complete {
        final_offset: u64,
        speed_bytes_per_sec: f64,
    },
    Failed {
        error: DownloadError,
        received_in_attempt: u64,
    },
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_single_connection(
    response: HttpResponse,
    meta: &ResponseMeta,
    request_url: &str,
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
    let client = crate::network::ClientNetworkConfig::default().build_client()?;
    let worker = HttpWorker::new(client, spec);
    run_single_with_retry(
        worker,
        response,
        meta.clone(),
        request_url,
        spec,
        output_path,
        start_offset,
        progress_tx,
        cancel_rx,
        control_path,
        total_size,
        speed_limit,
        log_level,
        download_id,
    )
    .await
}

struct SingleWriterRuntime {
    write_tx: Option<mpsc::Sender<WriterCommand>>,
    writer_handle: Option<tokio::task::JoinHandle<Result<(), DownloadError>>>,
    written_bytes: Arc<AtomicU64>,
    budget: Arc<MemoryBudget>,
    file_total_size: Option<u64>,
}

impl SingleWriterRuntime {
    async fn start(
        output_path: &Path,
        start_offset: u64,
        spec: &DownloadSpec,
        total_size: Option<u64>,
    ) -> Result<Self, DownloadError> {
        let mut runtime = Self {
            write_tx: None,
            writer_handle: None,
            written_bytes: Arc::new(AtomicU64::new(start_offset)),
            budget: Arc::new(MemoryBudget::new(spec.memory_budget)),
            file_total_size: total_size,
        };
        runtime
            .start_writer(output_path, start_offset, spec, total_size)
            .await?;
        Ok(runtime)
    }

    async fn start_writer(
        &mut self,
        output_path: &Path,
        start_offset: u64,
        spec: &DownloadSpec,
        total_size: Option<u64>,
    ) -> Result<(), DownloadError> {
        let file = if start_offset > 0 {
            open_existing_file(output_path).await?
        } else {
            create_output_file(output_path, total_size, spec.file_allocation).await?
        };

        self.budget = Arc::new(MemoryBudget::new(spec.memory_budget));
        self.written_bytes = Arc::new(AtomicU64::new(start_offset));
        self.file_total_size = total_size;
        let (write_tx, write_rx) = mpsc::channel::<WriterCommand>(spec.channel_buffer);
        let written_bytes = self.written_bytes.clone();
        let budget = self.budget.clone();
        self.writer_handle = Some(tokio::spawn(
            WriterTask::new(
                write_rx,
                file,
                written_bytes,
                budget.semaphore.clone(),
                budget.watermark,
            )
            .run(),
        ));
        self.write_tx = Some(write_tx);
        Ok(())
    }

    fn write_tx(&self) -> Result<&mpsc::Sender<WriterCommand>, DownloadError> {
        self.write_tx.as_ref().ok_or(DownloadError::ChannelClosed)
    }

    fn file_total_size(&self) -> Option<u64> {
        self.file_total_size
    }

    async fn flush(&mut self) -> Result<FlushAllStats, DownloadError> {
        let write_tx = self.write_tx()?.clone();
        match flush_all_and_wait(&write_tx, true).await {
            Ok(stats) => Ok(stats),
            Err(flush_error) => {
                // A failed send/ack usually means the writer task has already
                // stopped. Join it before returning so storage failures are
                // propagated and the task cannot outlive the transfer.
                match self.close().await {
                    Ok(()) => Err(flush_error),
                    Err(writer_error) => Err(writer_error),
                }
            }
        }
    }

    async fn close(&mut self) -> Result<(), DownloadError> {
        self.write_tx.take();
        if let Some(handle) = self.writer_handle.take() {
            handle
                .await
                .map_err(|e| DownloadError::TaskFailed(format!("writer panicked: {e}")))??;
        }
        Ok(())
    }

    async fn reset(
        &mut self,
        output_path: &Path,
        spec: &DownloadSpec,
        total_size: Option<u64>,
    ) -> Result<(), DownloadError> {
        self.close().await?;
        self.start_writer(output_path, 0, spec, total_size).await
    }
}

/// Run a single transfer with one retry budget spanning body, Range requests,
/// response validation, and safe from-zero restarts.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_single_with_retry(
    worker: HttpWorker,
    response: HttpResponse,
    meta: ResponseMeta,
    request_url: &str,
    spec: &DownloadSpec,
    output_path: &Path,
    start_offset: u64,
    progress_tx: &watch::Sender<ProgressSnapshot>,
    cancel_rx: watch::Receiver<StopSignal>,
    control_path: &Path,
    initial_total_size: Option<u64>,
    speed_limit: SpeedLimit,
    log_level: LogLevel,
    download_id: u64,
) -> Result<(), DownloadError> {
    let mut offset = start_offset;
    let mut total_size = initial_total_size;
    let mut baseline = meta.clone();
    let mut pending_response = Some((response, meta));
    let mut cancel_rx = cancel_rx;
    let mut retry_state = RetryState::new(
        spec.max_retries,
        spec.retry_base_delay,
        spec.retry_max_delay,
        spec.max_retry_elapsed,
    );
    let mut writer = SingleWriterRuntime::start(output_path, offset, spec, total_size).await?;
    let mut use_control = spec.resume && total_size.is_some();
    let mut control_save_tracker = ControlSaveTracker::new(offset);
    let mut snap_template = single_snapshot_template(request_url, total_size, offset, &baseline);

    loop {
        let (response, response_meta) = if let Some(response) = pending_response.take() {
            response
        } else {
            let request_result = if offset > 0 {
                if let Some(total) = total_size {
                    worker.send_range(offset, total.saturating_sub(1)).await
                } else {
                    worker.send_get().await
                }
            } else {
                worker.send_get().await
            };

            match request_result {
                Ok(response) => response,
                Err(error) => match retry_state.decide(error) {
                    RetryDecision::Stop(error) => {
                        mark_single_terminal_progress(progress_tx, DownloadState::Failed);
                        writer.close().await?;
                        return Err(error);
                    }
                    RetryDecision::Retry {
                        error,
                        retry_count,
                        backoff,
                        elapsed,
                    } => {
                        log_single_retry(
                            log_level,
                            download_id,
                            retry_count,
                            retry_state.max_retries(),
                            &error,
                            offset,
                            0,
                            backoff,
                            elapsed,
                            false,
                            None,
                        );
                        if let Err(stop_error) = sleep_with_backoff(backoff, &mut cancel_rx).await {
                            mark_single_error_progress(progress_tx, &stop_error);
                            writer.close().await?;
                            return Err(stop_error);
                        }
                        continue;
                    }
                },
            }
        };

        if offset > 0 {
            let validation = validate_range_response(
                response.status().as_u16(),
                response
                    .headers()
                    .get("retry-after")
                    .and_then(|value| value.to_str().ok()),
                &response_meta,
                RangeValidationMode::ResumeProbe,
                ExpectedRange {
                    start: offset,
                    end_inclusive: total_size
                        .ok_or_else(|| {
                            DownloadError::ResumeMismatch(
                                "missing total size for Range resume".into(),
                            )
                        })?
                        .saturating_sub(1),
                    total_size,
                },
            );
            let validation_error = match validation {
                Ok(RangeValidationDecision::Accept)
                    if metadata_matches_baseline(&response_meta, &baseline, total_size) =>
                {
                    None
                }
                Ok(RangeValidationDecision::Accept) => Some(DownloadError::ResumeMismatch(
                    "resume response metadata does not match the original object".into(),
                )),
                Ok(RangeValidationDecision::FallbackToSingle(_)) => {
                    Some(DownloadError::ResumeMismatch(
                        "resume response unexpectedly requested a single-connection fallback"
                            .into(),
                    ))
                }
                Err(error) => Some(error),
            };

            if let Some(error) = validation_error {
                // A 200 response to a Range request is a complete body (the
                // server ignored Range) and can be safely consumed after the
                // output has been reset. Other validation failures must drop
                // the response and fetch a fresh baseline.
                let reusable_response = if response.status().as_u16() == 200 {
                    Some((response, response_meta.clone()))
                } else {
                    drop(response);
                    None
                };
                match retry_state.decide_restart(error) {
                    RetryDecision::Stop(error) => {
                        mark_single_terminal_progress(progress_tx, DownloadState::Failed);
                        writer.close().await?;
                        return Err(error);
                    }
                    RetryDecision::Retry {
                        error,
                        retry_count,
                        backoff,
                        elapsed,
                    } => {
                        log_single_retry(
                            log_level,
                            download_id,
                            retry_count,
                            retry_state.max_retries(),
                            &error,
                            offset,
                            0,
                            backoff,
                            elapsed,
                            true,
                            Some("range_or_metadata_mismatch"),
                        );
                        if let Err(stop_error) = sleep_with_backoff(backoff, &mut cancel_rx).await {
                            mark_single_error_progress(progress_tx, &stop_error);
                            writer.close().await?;
                            return Err(stop_error);
                        }
                        // The replacement object's size is not known until a
                        // fresh GET arrives; do not preallocate using the old
                        // object's size or a smaller replacement could leave
                        // stale trailing bytes in the output file.
                        writer.reset(output_path, spec, None).await?;
                        ControlSnapshot::delete(control_path).await?;
                        offset = 0;
                        total_size = None;
                        use_control = false;
                        baseline = response_meta;
                        snap_template =
                            single_snapshot_template(request_url, total_size, 0, &baseline);
                        control_save_tracker = ControlSaveTracker::new(0);
                        progress_tx.send_modify(|progress| {
                            progress.total_size = None;
                            progress.downloaded = 0;
                            progress.speed_bytes_per_sec = 0.0;
                            progress.eta_secs = None;
                            progress.state = DownloadState::Downloading;
                        });
                        pending_response = reusable_response;
                        continue;
                    }
                }
            }
        } else {
            if let Some(discovered_total) =
                super::single_response_total_size(response.status().as_u16(), &response_meta)
            {
                total_size = Some(discovered_total);
                use_control = spec.resume;
            }
            baseline = response_meta.clone();
            snap_template = single_snapshot_template(request_url, total_size, 0, &baseline);
            if matches!(
                spec.file_allocation,
                crate::config::FileAllocation::Prealloc
            ) && writer.file_total_size() != total_size
            {
                // A restart may have had to recreate the file before the new
                // response revealed its size. Recreate once more with the
                // discovered size so pre-allocation remains effective.
                writer.reset(output_path, spec, total_size).await?;
            }
        }

        let control_ctx = SingleControlSaveContext {
            control_path,
            snap_template: &snap_template,
            autosave_sync_every: spec.autosave_sync_every,
            log_level,
            download_id,
        };
        let control = if use_control {
            Some((control_path, &snap_template))
        } else {
            None
        };
        let outcome = stream_single_attempt(
            response,
            spec.read_timeout,
            writer.write_tx()?,
            progress_tx,
            cancel_rx.clone(),
            total_size,
            offset,
            control,
            writer.budget.clone(),
            &speed_limit,
            spec.control_save_interval,
            &mut control_save_tracker,
            spec.autosave_sync_every,
            log_level,
            download_id,
        )
        .await;

        match outcome {
            SingleAttemptOutcome::Complete {
                final_offset,
                speed_bytes_per_sec,
            } => {
                let stats = writer.flush().await?;
                if stats.written_bytes != final_offset
                    || total_size.is_some_and(|total| final_offset != total)
                {
                    let error = DownloadError::Internal(format!(
                        "single writer persisted {} bytes but attempt completed at {}",
                        stats.written_bytes, final_offset
                    ));
                    mark_single_terminal_progress(progress_tx, DownloadState::Failed);
                    writer.close().await?;
                    return Err(error);
                }
                writer.close().await?;
                if use_control {
                    ControlSnapshot::delete(control_path).await?;
                }
                progress_tx.send_modify(|progress| {
                    progress.downloaded = final_offset;
                    progress.speed_bytes_per_sec = speed_bytes_per_sec;
                    progress.state = DownloadState::Completed;
                    progress.eta_secs = Some(0.0);
                });
                log_debug!(
                    log_level,
                    download_id = download_id,
                    "single-connection download completed"
                );
                return Ok(());
            }
            SingleAttemptOutcome::Failed {
                error,
                received_in_attempt,
            } => {
                if matches!(error, DownloadError::Cancelled | DownloadError::Paused)
                    || !error.is_retryable()
                {
                    let state = match &error {
                        DownloadError::Cancelled => DownloadState::Cancelled,
                        DownloadError::Paused => DownloadState::Paused,
                        _ => DownloadState::Failed,
                    };
                    mark_single_terminal_progress(progress_tx, state);
                    writer.close().await?;
                    if use_control {
                        persist_single_control_snapshot(
                            ControlSaveReason::Terminal,
                            writer.written_bytes.load(Ordering::Acquire),
                            None,
                            &mut control_save_tracker,
                            &control_ctx,
                        )
                        .await;
                    }
                    return Err(error);
                }

                let stats = writer.flush().await?;
                offset = stats.written_bytes;
                progress_tx.send_modify(|progress| {
                    progress.downloaded = offset;
                    progress.speed_bytes_per_sec = 0.0;
                    progress.eta_secs = None;
                    progress.state = DownloadState::Downloading;
                });
                if use_control {
                    save_single_control_snapshot_at(
                        offset,
                        &mut control_save_tracker,
                        &control_ctx,
                    )
                    .await;
                }

                match retry_state.decide(error) {
                    RetryDecision::Stop(error) => {
                        mark_single_terminal_progress(progress_tx, DownloadState::Failed);
                        writer.close().await?;
                        return Err(error);
                    }
                    RetryDecision::Retry {
                        error,
                        retry_count,
                        backoff,
                        elapsed,
                    } => {
                        // A transport error after the writer has already
                        // reached the advertised end cannot form a valid
                        // non-empty Range (start would be greater than end).
                        // Restarting is the only safe way to re-establish an
                        // EOF boundary in that case.
                        let restart_from_zero = total_size.is_none_or(|total| offset >= total);
                        let restart_reason = if total_size.is_none() {
                            "unknown_total"
                        } else {
                            "body_error_at_or_after_total"
                        };
                        log_single_retry(
                            log_level,
                            download_id,
                            retry_count,
                            retry_state.max_retries(),
                            &error,
                            offset,
                            received_in_attempt,
                            backoff,
                            elapsed,
                            restart_from_zero,
                            restart_from_zero.then_some(restart_reason),
                        );
                        if let Err(stop_error) = sleep_with_backoff(backoff, &mut cancel_rx).await {
                            mark_single_error_progress(progress_tx, &stop_error);
                            writer.close().await?;
                            return Err(stop_error);
                        }
                        if restart_from_zero {
                            writer.reset(output_path, spec, None).await?;
                            ControlSnapshot::delete(control_path).await?;
                            offset = 0;
                            total_size = None;
                            use_control = false;
                            control_save_tracker = ControlSaveTracker::new(0);
                            progress_tx.send_modify(|progress| {
                                progress.total_size = None;
                                progress.downloaded = 0;
                                progress.speed_bytes_per_sec = 0.0;
                                progress.eta_secs = None;
                                progress.state = DownloadState::Downloading;
                            });
                        }
                    }
                }
            }
        }
    }
}

fn single_snapshot_template(
    request_url: &str,
    total_size: Option<u64>,
    downloaded_bytes: u64,
    meta: &ResponseMeta,
) -> ControlSnapshot {
    let total = total_size.unwrap_or(0);
    ControlSnapshot {
        url: request_url.to_string(),
        total_size: total,
        piece_size: total,
        piece_count: 1,
        completed_bitset: vec![0],
        downloaded_bytes,
        etag: meta.etag.clone(),
        last_modified: meta.last_modified.clone(),
    }
}

fn metadata_matches_baseline(
    meta: &ResponseMeta,
    baseline: &ResponseMeta,
    total_size: Option<u64>,
) -> bool {
    baseline
        .etag
        .as_ref()
        .is_none_or(|etag| meta.etag.as_ref() == Some(etag))
        && baseline
            .last_modified
            .as_ref()
            .is_none_or(|value| meta.last_modified.as_ref() == Some(value))
        && meta
            .content_range_total
            .is_none_or(|total| total_size == Some(total))
}

#[allow(clippy::too_many_arguments)]
fn log_single_retry(
    log_level: LogLevel,
    download_id: u64,
    attempt: u32,
    max_retries: u32,
    error: &DownloadError,
    resume_offset: u64,
    received_in_attempt: u64,
    backoff: Duration,
    elapsed: Duration,
    restart_from_zero: bool,
    restart_reason: Option<&str>,
) {
    log_warn!(
        log_level,
        download_id = download_id,
        attempt,
        max_retries,
        error = %error,
        resume_offset,
        received_in_attempt,
        backoff_ms = backoff.as_millis() as u64,
        restart_from_zero,
        restart_reason = restart_reason.unwrap_or(""),
        elapsed_ms = elapsed.as_millis() as u64,
        "single-connection transfer retry"
    );
}

fn mark_single_terminal_progress(
    progress_tx: &watch::Sender<ProgressSnapshot>,
    state: DownloadState,
) {
    progress_tx.send_modify(|progress| {
        progress.state = state;
        progress.eta_secs = None;
    });
}

fn mark_single_error_progress(
    progress_tx: &watch::Sender<ProgressSnapshot>,
    error: &DownloadError,
) {
    let state = match error {
        DownloadError::Cancelled => DownloadState::Cancelled,
        DownloadError::Paused => DownloadState::Paused,
        _ => DownloadState::Failed,
    };
    mark_single_terminal_progress(progress_tx, state);
}

/// Stream a single HTTP response body to the writer channel.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
async fn stream_single(
    response: HttpResponse,
    read_timeout: Duration,
    write_tx: &mpsc::Sender<WriterCommand>,
    progress_tx: &watch::Sender<ProgressSnapshot>,
    cancel_rx: watch::Receiver<StopSignal>,
    total_size: Option<u64>,
    start_offset: u64,
    control: Option<(&Path, &ControlSnapshot)>,
    budget: Arc<MemoryBudget>,
    speed_limit: &SpeedLimit,
    control_save_interval: Duration,
    control_save_tracker: &mut ControlSaveTracker,
    autosave_sync_every: u32,
    log_level: LogLevel,
    download_id: u64,
) -> Result<SingleStreamSummary, DownloadError> {
    match stream_single_attempt(
        response,
        read_timeout,
        write_tx,
        progress_tx,
        cancel_rx,
        total_size,
        start_offset,
        control,
        budget,
        speed_limit,
        control_save_interval,
        control_save_tracker,
        autosave_sync_every,
        log_level,
        download_id,
    )
    .await
    {
        SingleAttemptOutcome::Complete {
            final_offset,
            speed_bytes_per_sec,
        } => Ok(SingleStreamSummary {
            downloaded: final_offset,
            speed_bytes_per_sec,
        }),
        SingleAttemptOutcome::Failed { error, .. } => Err(error),
    }
}

/// Stream a single HTTP response body and preserve attempt-local byte counts.
#[allow(clippy::too_many_arguments)]
async fn stream_single_attempt(
    response: HttpResponse,
    read_timeout: Duration,
    write_tx: &mpsc::Sender<WriterCommand>,
    progress_tx: &watch::Sender<ProgressSnapshot>,
    cancel_rx: watch::Receiver<StopSignal>,
    total_size: Option<u64>,
    start_offset: u64,
    control: Option<(&Path, &ControlSnapshot)>,
    budget: Arc<MemoryBudget>,
    speed_limit: &SpeedLimit,
    control_save_interval: Duration,
    control_save_tracker: &mut ControlSaveTracker,
    autosave_sync_every: u32,
    log_level: LogLevel,
    download_id: u64,
) -> SingleAttemptOutcome {
    let mut body = response.into_body();
    let mut downloaded: u64 = start_offset;
    let mut received_in_attempt = 0u64;
    let expected_len = total_size.map(|total| total.saturating_sub(start_offset));
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
                                control_save_tracker,
                                &SingleControlSaveContext {
                                    control_path: cp,
                                    snap_template: tmpl,
                                    autosave_sync_every,
                                    log_level,
                                    download_id,
                                },
                            )
                            .await;
                        }
                        return SingleAttemptOutcome::Failed {
                            error,
                            received_in_attempt,
                        };
                    }
                }
            }

            _ = save_ticker.tick(), if control.is_some() => {
                if let Some((cp, tmpl)) = &control {
                    persist_single_control_snapshot(
                        ControlSaveReason::Autosave,
                        downloaded,
                        Some(write_tx),
                        control_save_tracker,
                        &SingleControlSaveContext {
                            control_path: cp,
                            snap_template: tmpl,
                            autosave_sync_every,
                            log_level,
                            download_id,
                        },
                    )
                    .await;
                }
            }

            chunk = next_data_chunk(&mut body, read_timeout) => {
                match chunk {
                    Ok(Some(data)) => {
                        let len = data.len();
                        if expected_len.is_some_and(|expected| {
                            received_in_attempt.saturating_add(len as u64) > expected
                        }) {
                            let error = DownloadError::Transport(TransportError::new(
                                TransportErrorKind::Body,
                                std::io::Error::new(
                                    std::io::ErrorKind::InvalidData,
                                    "server sent more bytes than expected for single connection",
                                ),
                            ));
                            progress_reporter.force_report(
                                progress_tx,
                                ProgressUpdate::new(downloaded, last_speed, last_eta_secs)
                                    .with_state(DownloadState::Failed),
                                Instant::now(),
                            );
                            return SingleAttemptOutcome::Failed {
                                error,
                                received_in_attempt,
                            };
                        }
                        if let Err(error) = budget.forward(
                            data, downloaded, None, write_tx, &mut cancel_rx, speed_limit,
                            |sent| {
                                downloaded += sent;
                                received_in_attempt += sent;
                            },
                        ).await {
                            progress_reporter.force_report(
                                progress_tx,
                                ProgressUpdate::new(downloaded, last_speed, last_eta_secs)
                                    .with_state(match error {
                                        DownloadError::Cancelled => DownloadState::Cancelled,
                                        DownloadError::Paused => DownloadState::Paused,
                                        _ => DownloadState::Failed,
                                    }),
                                Instant::now(),
                            );
                            return SingleAttemptOutcome::Failed { error, received_in_attempt };
                        }
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
                    Ok(None) => break,
                    Err(error) => {
                        progress_reporter.force_report(
                            progress_tx,
                            ProgressUpdate::new(downloaded, last_speed, last_eta_secs)
                                .with_state(DownloadState::Failed),
                            Instant::now(),
                        );
                        return SingleAttemptOutcome::Failed {
                            error,
                            received_in_attempt,
                        };
                    }
                }
            }
        }
    }
    if let Some(expected) = expected_len {
        if received_in_attempt != expected {
            let error = DownloadError::Transport(TransportError::new(
                TransportErrorKind::Body,
                std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    format!(
                        "single connection body ended after {received_in_attempt} bytes, expected {expected}"
                    ),
                ),
            ));
            progress_reporter.force_report(
                progress_tx,
                ProgressUpdate::new(downloaded, last_speed, last_eta_secs)
                    .with_state(DownloadState::Failed),
                Instant::now(),
            );
            return SingleAttemptOutcome::Failed {
                error,
                received_in_attempt,
            };
        }
    }
    SingleAttemptOutcome::Complete {
        final_offset: downloaded,
        speed_bytes_per_sec: last_speed,
    }
}

async fn persist_single_control_snapshot(
    reason: ControlSaveReason,
    current_downloaded: u64,
    write_tx: Option<&mpsc::Sender<WriterCommand>>,
    control_save_tracker: &mut ControlSaveTracker,
    ctx: &SingleControlSaveContext<'_>,
) {
    if !control_save_tracker.should_save(reason, current_downloaded, ctx.autosave_sync_every) {
        if matches!(reason, ControlSaveReason::Autosave)
            && current_downloaded > control_save_tracker.last_saved_downloaded_bytes()
        {
            log_debug!(
                ctx.log_level,
                download_id = ctx.download_id,
                checkpoint = reason.label(),
                pending_prefix_bytes = current_downloaded,
                pending_autosaves = control_save_tracker.pending_autosaves(),
                autosave_sync_every = ctx.autosave_sync_every,
                "control snapshot deferred"
            );
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
    let persisted_prefix_bytes =
        flush_stats.map_or(current_downloaded, |stats| stats.written_bytes);
    if persisted_prefix_bytes <= control_save_tracker.last_saved_downloaded_bytes() {
        return;
    }

    let mut snapshot = ctx.snap_template.clone();
    snapshot.downloaded_bytes = persisted_prefix_bytes;
    let save_started = Instant::now();
    match snapshot.save(ctx.control_path).await {
        Ok(()) => {
            control_save_tracker.mark_saved(persisted_prefix_bytes);
            log_debug!(
                ctx.log_level,
                download_id = ctx.download_id,
                checkpoint = reason.label(),
                persisted_prefix_bytes = persisted_prefix_bytes,
                flush_all_ms = flush_stats
                    .map(|stats| stats.flush_elapsed.as_millis() as u64)
                    .unwrap_or(0),
                sync_data_ms = flush_stats
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

async fn save_single_control_snapshot_at(
    persisted_prefix_bytes: u64,
    control_save_tracker: &mut ControlSaveTracker,
    ctx: &SingleControlSaveContext<'_>,
) {
    if persisted_prefix_bytes <= control_save_tracker.last_saved_downloaded_bytes() {
        return;
    }

    let mut snapshot = ctx.snap_template.clone();
    snapshot.downloaded_bytes = persisted_prefix_bytes;
    match snapshot.save(ctx.control_path).await {
        Ok(()) => {
            control_save_tracker.mark_saved(persisted_prefix_bytes);
            log_debug!(
                ctx.log_level,
                download_id = ctx.download_id,
                checkpoint = "retry-barrier",
                persisted_prefix_bytes,
                "control snapshot saved after retry barrier"
            );
        }
        Err(error) => {
            log_warn!(
                ctx.log_level,
                download_id = ctx.download_id,
                checkpoint = "retry-barrier",
                error = %error,
                "control snapshot save failed after retry barrier"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpListener};
    use std::thread;

    fn snapshot_template_with(total_size: u64, downloaded_bytes: u64) -> ControlSnapshot {
        let mut snapshot = snapshot_template();
        snapshot.total_size = total_size;
        snapshot.piece_size = total_size;
        snapshot.downloaded_bytes = downloaded_bytes;
        snapshot
    }

    fn single_response_meta(total_size: u64) -> ResponseMeta {
        ResponseMeta {
            content_length: Some(total_size),
            content_range_start: None,
            content_range_end: None,
            content_range_total: None,
            accept_ranges: true,
            etag: Some("\"single\"".into()),
            last_modified: Some("Thu, 01 Jan 2026 00:00:00 GMT".into()),
            content_disposition: None,
            content_encoding: None,
        }
    }

    fn test_spec(url: &str) -> DownloadSpec {
        let mut spec = DownloadSpec::new(url.to_string());
        spec.resume = true;
        spec.memory_budget = 1024;
        spec.channel_buffer = 4;
        spec.control_save_interval = Duration::from_millis(5);
        spec.autosave_sync_every = 1;
        spec
    }

    fn spawn_single_response_server(
        declared_len: usize,
        body: Vec<u8>,
        body_delay: Duration,
    ) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();

        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request);

            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {declared_len}\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
            if body_delay > Duration::ZERO {
                thread::sleep(body_delay);
            }
            stream.write_all(&body).unwrap();
            let _ = stream.shutdown(Shutdown::Both);
        });

        (format!("http://{addr}/file.bin"), handle)
    }

    async fn get_response(url: &str) -> HttpResponse {
        let client = crate::network::ClientNetworkConfig::default()
            .build_client()
            .unwrap();
        let req = crate::http::request::build_get_request(url, &std::collections::HashMap::new());
        tokio::time::timeout(Duration::from_secs(5), client.request(req))
            .await
            .unwrap()
            .unwrap()
    }

    fn spawn_ack_writer(
        mut write_rx: mpsc::Receiver<WriterCommand>,
        written_bytes: Arc<AtomicU64>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            while let Some(command) = write_rx.recv().await {
                match command {
                    WriterCommand::BeginLease { .. } => {}
                    WriterCommand::Data { offset, data, .. } => {
                        written_bytes.store(offset + data.len() as u64, Ordering::Release);
                    }
                    WriterCommand::FlushLease { ack, .. } => {
                        let _ = ack.send(());
                    }
                    WriterCommand::DiscardLease { ack, .. } => {
                        let _ = ack.send(0);
                    }
                    WriterCommand::FlushAll { ack, .. } => {
                        let _ = ack.send(crate::storage::writer::FlushAllStats {
                            written_bytes: written_bytes.load(Ordering::Acquire),
                            flush_elapsed: Duration::ZERO,
                            sync_elapsed: Some(Duration::ZERO),
                        });
                    }
                }
            }
        })
    }

    async fn join_server(handle: thread::JoinHandle<()>) {
        tokio::time::timeout(
            Duration::from_secs(5),
            tokio::task::spawn_blocking(move || handle.join().unwrap()),
        )
        .await
        .unwrap()
        .unwrap();
    }

    async fn join_writer(handle: tokio::task::JoinHandle<()>) {
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .unwrap()
            .unwrap();
    }

    fn snapshot_template() -> ControlSnapshot {
        ControlSnapshot {
            url: "https://example.com/single.bin".into(),
            total_size: 1024,
            piece_size: 1024,
            piece_count: 1,
            completed_bitset: vec![0],
            downloaded_bytes: 0,
            etag: Some("\"single\"".into()),
            last_modified: Some("Thu, 01 Jan 2026 00:00:00 GMT".into()),
        }
    }

    #[tokio::test]
    async fn test_run_single_connection_persists_control_on_stream_error() {
        let (url, server) = spawn_single_response_server(8, b"fail".to_vec(), Duration::ZERO);
        let response = get_response(&url).await;
        let meta = single_response_meta(8);
        let spec = test_spec(&url).max_retries(0);
        let dir = tempfile::tempdir().unwrap();
        let output_path = dir.path().join("single-error.bin");
        let control_path = dir.path().join("single-error.bytehaul");
        let (progress_tx, _) = watch::channel(ProgressSnapshot::default());
        let (_cancel_tx, cancel_rx) = watch::channel(StopSignal::Running);

        let err = tokio::time::timeout(
            Duration::from_secs(5),
            run_single_connection(
                response,
                &meta,
                &url,
                &spec,
                &output_path,
                0,
                &progress_tx,
                cancel_rx,
                &control_path,
                Some(8),
                SpeedLimit::new(0),
                LogLevel::Off,
                11,
            ),
        )
        .await
        .unwrap()
        .unwrap_err();

        join_server(server).await;

        assert!(matches!(err, DownloadError::Transport(_)));
        let loaded = ControlSnapshot::load(&control_path).await.unwrap();
        assert_eq!(loaded.downloaded_bytes, 4);
        assert_eq!(progress_tx.borrow().state, DownloadState::Failed);
    }

    #[tokio::test]
    async fn test_run_single_connection_completes_and_clears_control_file() {
        let (url, server) = spawn_single_response_server(4, b"done".to_vec(), Duration::ZERO);
        let response = get_response(&url).await;
        let meta = single_response_meta(4);
        let spec = test_spec(&url);
        let dir = tempfile::tempdir().unwrap();
        let output_path = dir.path().join("single-success.bin");
        let control_path = dir.path().join("single-success.bytehaul");
        snapshot_template_with(4, 1)
            .save(&control_path)
            .await
            .unwrap();
        let (progress_tx, _) = watch::channel(ProgressSnapshot::default());
        let (_cancel_tx, cancel_rx) = watch::channel(StopSignal::Running);

        tokio::time::timeout(
            Duration::from_secs(5),
            run_single_connection(
                response,
                &meta,
                &url,
                &spec,
                &output_path,
                0,
                &progress_tx,
                cancel_rx,
                &control_path,
                Some(4),
                SpeedLimit::new(0),
                LogLevel::Off,
                12,
            ),
        )
        .await
        .unwrap()
        .unwrap();

        join_server(server).await;

        assert!(!control_path.exists());
        assert_eq!(tokio::fs::read(&output_path).await.unwrap(), b"done");
        let snapshot = progress_tx.borrow().clone();
        assert_eq!(snapshot.downloaded, 4);
        assert_eq!(snapshot.state, DownloadState::Completed);
        assert_eq!(snapshot.eta_secs, Some(0.0));
    }

    #[tokio::test]
    async fn test_stream_single_returns_channel_closed_when_writer_receiver_dropped() {
        let (url, server) = spawn_single_response_server(4, b"data".to_vec(), Duration::ZERO);
        let response = get_response(&url).await;
        let (write_tx, write_rx) = mpsc::channel(1);
        drop(write_rx);
        let (progress_tx, _) = watch::channel(ProgressSnapshot::default());
        let (_cancel_tx, cancel_rx) = watch::channel(StopSignal::Running);
        let mut tracker = ControlSaveTracker::new(0);

        let err = tokio::time::timeout(
            Duration::from_secs(5),
            stream_single(
                response,
                Duration::from_secs(5),
                &write_tx,
                &progress_tx,
                cancel_rx,
                Some(4),
                0,
                None,
                Arc::new(MemoryBudget::new(16)),
                &SpeedLimit::new(0),
                Duration::from_secs(60),
                &mut tracker,
                1,
                LogLevel::Off,
                13,
            ),
        )
        .await
        .unwrap()
        .unwrap_err();

        join_server(server).await;

        assert!(matches!(err, DownloadError::ChannelClosed));
    }

    #[tokio::test]
    async fn test_stream_single_pauses_and_saves_control_snapshot() {
        let (url, server) =
            spawn_single_response_server(4, b"data".to_vec(), Duration::from_millis(50));
        let response = get_response(&url).await;
        let dir = tempfile::tempdir().unwrap();
        let control_path = dir.path().join("single-pause.bytehaul");
        let snapshot = snapshot_template_with(5, 0);
        let written_bytes = Arc::new(AtomicU64::new(1));
        let (write_tx, write_rx) = mpsc::channel(4);
        let writer = spawn_ack_writer(write_rx, written_bytes);
        let (progress_tx, _) = watch::channel(ProgressSnapshot::default());
        let (cancel_tx, cancel_rx) = watch::channel(StopSignal::Running);
        let mut tracker = ControlSaveTracker::new(0);

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let _ = cancel_tx.send(StopSignal::Pause);
        });

        let err = tokio::time::timeout(
            Duration::from_secs(5),
            stream_single(
                response,
                Duration::from_secs(5),
                &write_tx,
                &progress_tx,
                cancel_rx,
                Some(5),
                1,
                Some((&control_path, &snapshot)),
                Arc::new(MemoryBudget::new(16)),
                &SpeedLimit::new(0),
                Duration::from_secs(60),
                &mut tracker,
                1,
                LogLevel::Off,
                14,
            ),
        )
        .await
        .unwrap()
        .unwrap_err();

        drop(write_tx);
        join_writer(writer).await;
        join_server(server).await;

        assert!(matches!(err, DownloadError::Paused));
        let loaded = ControlSnapshot::load(&control_path).await.unwrap();
        assert_eq!(loaded.downloaded_bytes, 1);
        assert_eq!(progress_tx.borrow().state, DownloadState::Paused);
    }

    #[tokio::test]
    async fn test_stream_single_autosaves_existing_progress_before_body_arrives() {
        let (url, server) =
            spawn_single_response_server(4, b"data".to_vec(), Duration::from_millis(50));
        let response = get_response(&url).await;
        let dir = tempfile::tempdir().unwrap();
        let control_path = dir.path().join("single-autosave.bytehaul");
        let snapshot = snapshot_template_with(5, 0);
        let written_bytes = Arc::new(AtomicU64::new(1));
        let (write_tx, write_rx) = mpsc::channel(4);
        let writer = spawn_ack_writer(write_rx, written_bytes);
        let (progress_tx, _) = watch::channel(ProgressSnapshot::default());
        let (_cancel_tx, cancel_rx) = watch::channel(StopSignal::Running);
        let mut tracker = ControlSaveTracker::new(0);

        let summary = tokio::time::timeout(
            Duration::from_secs(5),
            stream_single(
                response,
                Duration::from_secs(5),
                &write_tx,
                &progress_tx,
                cancel_rx,
                Some(5),
                1,
                Some((&control_path, &snapshot)),
                Arc::new(MemoryBudget::new(16)),
                &SpeedLimit::new(0),
                Duration::from_millis(5),
                &mut tracker,
                1,
                LogLevel::Off,
                15,
            ),
        )
        .await
        .unwrap()
        .unwrap();

        drop(write_tx);
        join_writer(writer).await;
        join_server(server).await;

        let loaded = ControlSnapshot::load(&control_path).await.unwrap();
        assert_eq!(loaded.downloaded_bytes, 1);
        assert_eq!(summary.downloaded, 5);
        assert!(summary.speed_bytes_per_sec >= 0.0);
    }

    #[tokio::test]
    async fn test_persist_single_control_snapshot_defers_then_saves_autosave() {
        let dir = tempfile::tempdir().unwrap();
        let control_path = dir.path().join("single.bytehaul");
        let snapshot = snapshot_template();
        let mut tracker = ControlSaveTracker::new(0);
        let ctx = SingleControlSaveContext {
            control_path: &control_path,
            snap_template: &snapshot,
            autosave_sync_every: 2,
            log_level: LogLevel::Off,
            download_id: 1,
        };

        persist_single_control_snapshot(ControlSaveReason::Autosave, 256, None, &mut tracker, &ctx)
            .await;

        assert!(!control_path.exists());
        assert_eq!(tracker.last_saved_downloaded_bytes(), 0);
        assert_eq!(tracker.pending_autosaves(), 1);

        persist_single_control_snapshot(ControlSaveReason::Autosave, 512, None, &mut tracker, &ctx)
            .await;

        let loaded = ControlSnapshot::load(&control_path).await.unwrap();
        assert_eq!(loaded.downloaded_bytes, 512);
        assert_eq!(tracker.last_saved_downloaded_bytes(), 512);
        assert_eq!(tracker.pending_autosaves(), 0);
    }

    #[tokio::test]
    async fn test_persist_single_control_snapshot_returns_on_flush_failure() {
        let dir = tempfile::tempdir().unwrap();
        let control_path = dir.path().join("single-failed.bytehaul");
        let snapshot = snapshot_template();
        let mut tracker = ControlSaveTracker::new(0);
        let ctx = SingleControlSaveContext {
            control_path: &control_path,
            snap_template: &snapshot,
            autosave_sync_every: 1,
            log_level: LogLevel::Off,
            download_id: 2,
        };
        let (write_tx, write_rx) = mpsc::channel(1);
        drop(write_rx);

        persist_single_control_snapshot(
            ControlSaveReason::Terminal,
            256,
            Some(&write_tx),
            &mut tracker,
            &ctx,
        )
        .await;

        assert!(!control_path.exists());
        assert_eq!(tracker.last_saved_downloaded_bytes(), 0);
    }

    #[tokio::test]
    async fn test_persist_single_control_snapshot_skips_when_downloaded_does_not_advance() {
        let dir = tempfile::tempdir().unwrap();
        let control_path = dir.path().join("single-stable.bytehaul");
        let snapshot = snapshot_template();
        let mut tracker = ControlSaveTracker::new(0);
        let ctx = SingleControlSaveContext {
            control_path: &control_path,
            snap_template: &snapshot,
            autosave_sync_every: 1,
            log_level: LogLevel::Off,
            download_id: 3,
        };

        persist_single_control_snapshot(ControlSaveReason::Terminal, 256, None, &mut tracker, &ctx)
            .await;
        persist_single_control_snapshot(ControlSaveReason::Terminal, 256, None, &mut tracker, &ctx)
            .await;

        let loaded = ControlSnapshot::load(&control_path).await.unwrap();
        assert_eq!(loaded.downloaded_bytes, 256);
        assert_eq!(tracker.last_saved_downloaded_bytes(), 256);
    }

    #[tokio::test]
    async fn test_persist_single_control_snapshot_ignores_save_failure() {
        let dir = tempfile::tempdir().unwrap();
        let control_path = dir.path().join("missing").join("single.bytehaul");
        let snapshot = snapshot_template();
        let mut tracker = ControlSaveTracker::new(0);
        let ctx = SingleControlSaveContext {
            control_path: &control_path,
            snap_template: &snapshot,
            autosave_sync_every: 1,
            log_level: LogLevel::Off,
            download_id: 4,
        };

        persist_single_control_snapshot(ControlSaveReason::Terminal, 256, None, &mut tracker, &ctx)
            .await;

        assert!(!control_path.exists());
        assert_eq!(tracker.last_saved_downloaded_bytes(), 0);
    }

    #[tokio::test]
    async fn test_persist_single_control_snapshot_returns_when_downloaded_is_not_new() {
        let dir = tempfile::tempdir().unwrap();
        let control_path = dir.path().join("single-unchanged.bytehaul");
        let snapshot = snapshot_template();
        let mut tracker = ControlSaveTracker::new(256);
        let ctx = SingleControlSaveContext {
            control_path: &control_path,
            snap_template: &snapshot,
            autosave_sync_every: 1,
            log_level: LogLevel::Off,
            download_id: 5,
        };

        persist_single_control_snapshot(ControlSaveReason::Terminal, 256, None, &mut tracker, &ctx)
            .await;

        assert!(!control_path.exists());
        assert_eq!(tracker.last_saved_downloaded_bytes(), 256);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn test_run_single_connection_reports_writer_failure() {
        let (url, server) = spawn_single_response_server(4, b"done".to_vec(), Duration::ZERO);
        let response = get_response(&url).await;
        let meta = single_response_meta(4);
        let spec = test_spec(&url)
            .resume(true)
            .file_allocation(crate::config::FileAllocation::None);
        let dir = tempfile::tempdir().unwrap();
        let control_path = dir.path().join("single-writer-failure.bytehaul");
        let (progress_tx, _) = watch::channel(ProgressSnapshot::default());
        let (_cancel_tx, cancel_rx) = watch::channel(StopSignal::Running);

        let err = run_single_connection(
            response,
            &meta,
            &url,
            &spec,
            std::path::Path::new("/dev/full"),
            0,
            &progress_tx,
            cancel_rx,
            &control_path,
            Some(4),
            SpeedLimit::new(0),
            LogLevel::Off,
            15,
        )
        .await
        .unwrap_err();

        join_server(server).await;

        assert!(matches!(err, DownloadError::Io(_)), "got: {err:?}");
        let snapshot = progress_tx.borrow().clone();
        assert_eq!(snapshot.state, DownloadState::Failed);
        assert_eq!(snapshot.downloaded, 4);
        let loaded = ControlSnapshot::load(&control_path).await.unwrap();
        assert_eq!(loaded.downloaded_bytes, 4);
    }
}
