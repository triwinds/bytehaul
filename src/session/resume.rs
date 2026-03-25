use std::collections::HashSet;
use std::path::{Path, PathBuf};

use tokio::sync::watch;

use super::retry::retry_with_backoff;
use super::single::run_single_connection;
use super::multi::run_multi_worker;
use super::{range_response_allowed, validate_metadata, StopSignal};
use crate::config::{DownloadSpec, LogLevel};
use crate::error::DownloadError;
use crate::http::worker::HttpWorker;
use crate::progress::ProgressSnapshot;
use crate::rate_limiter::SpeedLimit;
use crate::storage::control::ControlSnapshot;
use crate::storage::piece_map::PieceMap;

pub(crate) async fn validate_local_resume_state(
    output_path: &Path,
    ctrl: &ControlSnapshot,
    spec: &DownloadSpec,
) -> Result<(), DownloadError> {
    let metadata = tokio::fs::metadata(output_path).await.map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            DownloadError::ResumeMismatch("local download file is missing".into())
        } else {
            DownloadError::Io(error)
        }
    })?;

    if ctrl.downloaded_bytes > ctrl.total_size {
        return Err(DownloadError::ResumeMismatch(
            "control file records more bytes than total_size".into(),
        ));
    }

    let actual_len = metadata.len();
    let is_multi = ctrl.piece_count > 1 && spec.max_connections > 1;
    let preallocated = matches!(spec.file_allocation, crate::config::FileAllocation::Prealloc);

    if is_multi {
        let piece_map = PieceMap::from_bitset(
            ctrl.total_size,
            ctrl.piece_size,
            &ctrl.completed_bitset,
            ctrl.piece_count,
        );
        let completed_bytes = piece_map.completed_bytes();
        if completed_bytes > ctrl.downloaded_bytes {
            return Err(DownloadError::ResumeMismatch(
                "control file marks more completed bytes than downloaded_bytes".into(),
            ));
        }

        if preallocated {
            if actual_len != ctrl.total_size {
                return Err(DownloadError::ResumeMismatch(format!(
                    "preallocated resume file size mismatch: expected {} bytes, found {}",
                    ctrl.total_size, actual_len
                )));
            }
            return Ok(());
        }

        let mut min_len = 0u64;
        for piece_id in (0..piece_map.piece_count()).rev() {
            if piece_map.is_complete(piece_id) {
                min_len = piece_map.piece_range(piece_id).1;
                break;
            }
        }

        if actual_len < min_len {
            return Err(DownloadError::ResumeMismatch(format!(
                "resume file is truncated: expected at least {} bytes from completed pieces, found {}",
                min_len, actual_len
            )));
        }
        if actual_len > ctrl.total_size {
            return Err(DownloadError::ResumeMismatch(format!(
                "resume file is larger than expected total size: expected at most {} bytes, found {}",
                ctrl.total_size, actual_len
            )));
        }
        if ctrl.downloaded_bytes > actual_len {
            return Err(DownloadError::ResumeMismatch(format!(
                "control file records {} downloaded bytes but local file only has {} bytes",
                ctrl.downloaded_bytes, actual_len
            )));
        }
        return Ok(());
    }

    let expected_len = if preallocated {
        ctrl.total_size
    } else {
        ctrl.downloaded_bytes
    };
    if actual_len != expected_len {
        return Err(DownloadError::ResumeMismatch(format!(
            "resume file size mismatch: expected {} bytes, found {}",
            expected_len, actual_len
        )));
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn try_resume_download(
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

        if let Err(error) = validate_local_resume_state(output_path, &ctrl, spec).await {
            log_warn!(
                log_level,
                download_id,
                error = %error,
                "local resume state rejected, restarting download"
            );
            let _ = ControlSnapshot::delete(&control_path).await;
            return Ok(None);
        }

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
                    spec.max_retry_elapsed,
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
                spec.max_retry_elapsed,
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
