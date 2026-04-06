mod multi;
mod resume;
pub(crate) mod retry;
mod single;

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};

use crate::checksum::verify_checksum;
use crate::config::{DownloadSpec, LogLevel};
use crate::error::DownloadError;
use crate::filename::{detect_filename, sanitize_relative_path};
use crate::http::response::ResponseMeta;
use crate::http::worker::HttpWorker;
use crate::progress::{DownloadState, ProgressSnapshot};
use crate::rate_limiter::SpeedLimit;
use crate::storage::control::ControlSnapshot;
use crate::storage::writer::WriterCommand;

use self::multi::run_multi_worker;
use self::resume::try_resume_download;
use self::retry::retry_with_backoff;
use self::single::run_single_connection;

const SPEED_ESTIMATE_WINDOW: Duration = Duration::from_secs(5);
const MIN_SPEED_SAMPLE_SPAN: Duration = Duration::from_secs(1);
const MULTI_PROGRESS_INTERVAL: Duration = crate::progress::PROGRESS_REPORT_INTERVAL;

/// Attempt a range probe first; on failure fall back to a plain GET with retry.
async fn probe_or_fallback_get(
    worker: &HttpWorker,
    spec: &DownloadSpec,
    cancel_rx: &mut watch::Receiver<StopSignal>,
) -> Result<(reqwest::Response, ResponseMeta, FreshResponseSource), DownloadError> {
    if spec.max_connections > 1 {
        let piece_end = spec.piece_size.saturating_sub(1);
        if let Ok((resp, meta)) = worker.send_range(0, piece_end).await {
            return Ok((resp, meta, FreshResponseSource::RangeProbe));
        }
    }
    let (resp, meta) = retry_with_backoff(
        spec.max_retries,
        spec.retry_base_delay,
        spec.retry_max_delay,
        spec.max_retry_elapsed,
        cancel_rx,
        || worker.send_get(),
    )
    .await?;
    Ok((resp, meta, FreshResponseSource::FallbackGet))
}

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
            return Err(DownloadError::InvalidConfig(
                "output_path must be relative when output_dir is set".into(),
            ));
        }
        return Ok(Some(output_path.clone()));
    }
    let relative = sanitize_relative_path(output_path).ok_or_else(|| {
        DownloadError::InvalidConfig(
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
                let piece_map = crate::storage::piece_map::PieceMap::new(total_size, spec.piece_size);
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
            if !matches!(e, DownloadError::Cancelled | DownloadError::Paused) {
                progress_tx.send_modify(|progress| {
                    progress.state = DownloadState::Failed;
                    progress.eta_secs = None;
                });
            }
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

        return {
            let (resp, meta, source) = probe_or_fallback_get(&worker, &spec, &mut cancel_rx).await?;
            run_fresh_from_response(
                client,
                &spec,
                &output_path,
                resp,
                meta,
                source,
                progress_tx,
                cancel_rx,
                speed_limit,
                log_level,
                download_id,
            )
            .await
        };
    }

    let (initial_response, initial_meta, source) =
        probe_or_fallback_get(&worker, &spec, &mut cancel_rx).await?;

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

    #[test]
    fn test_stop_signal_is_stop_requested() {
        assert!(!StopSignal::Running.is_stop_requested());
        assert!(StopSignal::Cancel.is_stop_requested());
        assert!(StopSignal::Pause.is_stop_requested());
    }

    #[test]
    fn test_stop_signal_error() {
        assert!(stop_signal_error(StopSignal::Running).is_none());
        assert!(matches!(
            stop_signal_error(StopSignal::Cancel),
            Some(DownloadError::Cancelled)
        ));
        assert!(matches!(
            stop_signal_error(StopSignal::Pause),
            Some(DownloadError::Paused)
        ));
    }

    #[test]
    fn test_stop_signal_state() {
        assert!(stop_signal_state(StopSignal::Running).is_none());
        assert_eq!(
            stop_signal_state(StopSignal::Cancel),
            Some(DownloadState::Cancelled)
        );
        assert_eq!(
            stop_signal_state(StopSignal::Pause),
            Some(DownloadState::Paused)
        );
    }

    #[test]
    fn test_stop_signal_label() {
        assert_eq!(stop_signal_label(StopSignal::Running), "running");
        assert_eq!(stop_signal_label(StopSignal::Cancel), "cancelled");
        assert_eq!(stop_signal_label(StopSignal::Pause), "paused");
    }

    #[test]
    fn test_resolve_output_dir_uses_cwd_when_none() {
        let spec = DownloadSpec::new("http://example.com/file")
            .output_path(PathBuf::from("file.bin"));
        let dir = resolve_output_dir(&spec).unwrap();
        assert!(dir.is_absolute());
    }

    #[test]
    fn test_resolve_output_dir_absolute() {
        let abs_dir = std::env::temp_dir().join("bytehaul_test_resolve");
        let mut spec = DownloadSpec::new("http://example.com/file");
        spec.output_dir = Some(abs_dir.clone());
        let dir = resolve_output_dir(&spec).unwrap();
        assert_eq!(dir, abs_dir);
    }

    #[test]
    fn test_resolve_static_output_path_none() {
        let spec = DownloadSpec::new("http://example.com/file");
        let result = resolve_static_output_path(&spec).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_resolve_static_output_path_absolute() {
        let abs_path = std::env::temp_dir().join("bytehaul_test_abs.bin");
        let spec = DownloadSpec::new("http://example.com/file")
            .output_path(abs_path.clone());
        let result = resolve_static_output_path(&spec).unwrap();
        assert_eq!(result, Some(abs_path));
    }

    #[test]
    fn test_resolve_static_output_path_absolute_with_output_dir_is_error() {
        let abs_path = std::env::temp_dir().join("bytehaul_test_abs.bin");
        let mut spec = DownloadSpec::new("http://example.com/file")
            .output_path(abs_path);
        spec.output_dir = Some(std::env::temp_dir());
        let result = resolve_static_output_path(&spec);
        assert!(matches!(result, Err(DownloadError::InvalidConfig(_))));
    }

    #[test]
    fn test_resolve_static_output_path_relative() {
        let spec = DownloadSpec::new("http://example.com/file")
            .output_path(PathBuf::from("data.bin"));
        let result = resolve_static_output_path(&spec).unwrap().unwrap();
        assert!(result.is_absolute());
        assert!(result.ends_with("data.bin"));
    }
}
