use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
use crate::http::{next_data_chunk, HttpResponse};
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

struct SingleControlSaveContext<'a> {
    control_path: &'a Path,
    snap_template: &'a ControlSnapshot,
    autosave_sync_every: u32,
    log_level: LogLevel,
    download_id: u64,
}

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
        url: request_url.to_string(),
        total_size: ts,
        piece_size: ts,
        piece_count: 1,
        completed_bitset: vec![0],
        downloaded_bytes: start_offset,
        etag: meta.etag.clone(),
        last_modified: meta.last_modified.clone(),
    };
    let mut control_save_tracker = ControlSaveTracker::new(start_offset);
    let control_save_ctx = SingleControlSaveContext {
        control_path,
        snap_template: &snap_template,
        autosave_sync_every: spec.autosave_sync_every,
        log_level,
        download_id,
    };

    let stream_result = stream_single(
        response,
        spec.read_timeout,
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
                &mut control_save_tracker,
                &control_save_ctx,
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
                &mut control_save_tracker,
                &control_save_ctx,
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
    response: HttpResponse,
    read_timeout: Duration,
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
    let mut body = response.into_body();
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
                    Ok(None) => break,
                    Err(error) => {
                        progress_reporter.force_report(
                            progress_tx,
                            ProgressUpdate::new(downloaded, last_speed, last_eta_secs)
                                .with_state(DownloadState::Failed),
                            Instant::now(),
                        );
                        return Err(error);
                    }
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
    control_save_tracker: &mut ControlSaveTracker,
    ctx: &SingleControlSaveContext<'_>,
) {
    if !control_save_tracker.should_save(reason, downloaded, ctx.autosave_sync_every) {
        if matches!(reason, ControlSaveReason::Autosave)
            && downloaded > control_save_tracker.last_saved_downloaded_bytes()
        {
            log_debug!(ctx.log_level, download_id = ctx.download_id,
                checkpoint = reason.label(), downloaded_bytes = downloaded,
                pending_autosaves = control_save_tracker.pending_autosaves(),
                autosave_sync_every = ctx.autosave_sync_every,
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
    let persisted_downloaded = flush_stats.map_or(downloaded, |stats| stats.written_bytes);
    if persisted_downloaded <= control_save_tracker.last_saved_downloaded_bytes() {
        return;
    }

    let mut snapshot = ctx.snap_template.clone();
    snapshot.downloaded_bytes = persisted_downloaded;
    let save_started = Instant::now();
    match snapshot.save(ctx.control_path).await {
        Ok(()) => {
            control_save_tracker.mark_saved(persisted_downloaded);
            log_debug!(ctx.log_level, download_id = ctx.download_id,
                checkpoint = reason.label(), downloaded_bytes = persisted_downloaded,
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
        let req = crate::http::request::build_get_request(
            url,
            &std::collections::HashMap::new(),
        );
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
                    WriterCommand::Data { offset, data, .. } => {
                        written_bytes.store(offset + data.len() as u64, Ordering::Release);
                    }
                    WriterCommand::FlushPiece { ack, .. } => {
                        let _ = ack.send(());
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
        let spec = test_spec(&url);
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
        snapshot_template_with(4, 1).save(&control_path).await.unwrap();
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
                Arc::new(Semaphore::new(16)),
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
                Arc::new(Semaphore::new(16)),
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
                Arc::new(Semaphore::new(16)),
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

        persist_single_control_snapshot(
            ControlSaveReason::Autosave,
            256,
            None,
            &mut tracker,
            &ctx,
        )
        .await;

        assert!(!control_path.exists());
        assert_eq!(tracker.last_saved_downloaded_bytes(), 0);
        assert_eq!(tracker.pending_autosaves(), 1);

        persist_single_control_snapshot(
            ControlSaveReason::Autosave,
            512,
            None,
            &mut tracker,
            &ctx,
        )
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

        persist_single_control_snapshot(
            ControlSaveReason::Terminal,
            256,
            None,
            &mut tracker,
            &ctx,
        )
        .await;
        persist_single_control_snapshot(
            ControlSaveReason::Terminal,
            256,
            None,
            &mut tracker,
            &ctx,
        )
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

        persist_single_control_snapshot(
            ControlSaveReason::Terminal,
            256,
            None,
            &mut tracker,
            &ctx,
        )
        .await;

        assert!(!control_path.exists());
        assert_eq!(tracker.last_saved_downloaded_bytes(), 0);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn test_run_single_connection_reports_writer_failure() {
        let (url, server) = spawn_single_response_server(4, b"done".to_vec(), Duration::ZERO);
        let response = get_response(&url).await;
        let meta = single_response_meta(4);
        let spec = test_spec(&url).resume(true).file_allocation(crate::config::FileAllocation::None);
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
