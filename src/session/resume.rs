use std::path::{Path, PathBuf};

use tokio::sync::watch;

use super::retry::retry_with_backoff;
use super::single::run_single_connection;
use super::multi::run_multi_worker;
use super::{range_response_allowed, validate_metadata, StopSignal};
use crate::config::{DownloadSpec, LogLevel};
use crate::error::DownloadError;
use crate::http::worker::HttpWorker;
use crate::network::BytehaulClient;
use crate::progress::ProgressSnapshot;
use crate::rate_limiter::SpeedLimit;
use crate::storage::control::ControlSnapshot;
use crate::storage::piece_map::PieceMap;

pub(crate) async fn validate_local_resume_state(
    output_path: &Path,
    ctrl: &ControlSnapshot,
    spec: &DownloadSpec,
) -> Result<(), DownloadError> {
    let metadata = tokio::fs::metadata(output_path)
        .await
        .map_err(map_resume_metadata_error)?;

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

        let min_len = piece_map
            .highest_completed_piece()
            .map(|piece_id| piece_map.piece_range(piece_id).1)
            .unwrap_or(0);

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

fn map_resume_metadata_error(error: std::io::Error) -> DownloadError {
    if error.kind() == std::io::ErrorKind::NotFound {
        DownloadError::ResumeMismatch("local download file is missing".into())
    } else {
        DownloadError::Io(error)
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn try_resume_download(
    client: BytehaulClient,
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
            if let Some(probe_piece) = piece_map.first_missing() {
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
                        let request_url = worker.final_url().await?;
                        run_multi_worker(
                            client,
                            spec,
                            &request_url,
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
                    let request_url = worker.final_url().await?;
                    run_single_connection(
                        resp,
                        &meta,
                        &request_url,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::FileAllocation;

    fn test_ctrl(
        total_size: u64,
        piece_size: u64,
        piece_count: usize,
        downloaded_bytes: u64,
        completed_bitset: Vec<u8>,
    ) -> ControlSnapshot {
        ControlSnapshot {
            url: "http://example.com/file".into(),
            total_size,
            piece_size,
            piece_count,
            completed_bitset,
            downloaded_bytes,
            etag: None,
            last_modified: None,
        }
    }

    fn test_spec(max_connections: u32, file_allocation: FileAllocation) -> DownloadSpec {
        DownloadSpec::new("http://example.com/file")
            .file_allocation(file_allocation)
            .max_connections(max_connections)
    }

    #[tokio::test]
    async fn test_validate_resume_file_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.bin");
        let ctrl = test_ctrl(1000, 1000, 1, 500, vec![0]);
        let spec = test_spec(1, FileAllocation::None);
        let err = validate_local_resume_state(&missing, &ctrl, &spec)
            .await
            .unwrap_err();
        assert!(matches!(err, DownloadError::ResumeMismatch(msg) if msg.contains("missing")));
    }

    #[tokio::test]
    async fn test_validate_resume_downloaded_exceeds_total() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.bin");
        std::fs::write(&path, vec![0u8; 500]).unwrap();
        let ctrl = test_ctrl(1000, 1000, 1, 2000, vec![0]);
        let spec = test_spec(1, FileAllocation::None);
        let err = validate_local_resume_state(&path, &ctrl, &spec)
            .await
            .unwrap_err();
        assert!(matches!(err, DownloadError::ResumeMismatch(msg) if msg.contains("more bytes")));
    }

    #[tokio::test]
    async fn test_validate_resume_single_no_prealloc_ok() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.bin");
        std::fs::write(&path, vec![0u8; 500]).unwrap();
        let ctrl = test_ctrl(1000, 1000, 1, 500, vec![0]);
        let spec = test_spec(1, FileAllocation::None);
        validate_local_resume_state(&path, &ctrl, &spec)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_validate_resume_single_no_prealloc_size_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.bin");
        std::fs::write(&path, vec![0u8; 300]).unwrap();
        let ctrl = test_ctrl(1000, 1000, 1, 500, vec![0]);
        let spec = test_spec(1, FileAllocation::None);
        let err = validate_local_resume_state(&path, &ctrl, &spec)
            .await
            .unwrap_err();
        assert!(matches!(err, DownloadError::ResumeMismatch(msg) if msg.contains("resume file size mismatch")));
    }

    #[tokio::test]
    async fn test_validate_resume_single_prealloc_ok() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.bin");
        std::fs::write(&path, vec![0u8; 1000]).unwrap();
        let ctrl = test_ctrl(1000, 1000, 1, 500, vec![0]);
        let spec = test_spec(1, FileAllocation::Prealloc);
        validate_local_resume_state(&path, &ctrl, &spec)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_validate_resume_single_prealloc_size_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.bin");
        std::fs::write(&path, vec![0u8; 800]).unwrap();
        let ctrl = test_ctrl(1000, 1000, 1, 500, vec![0]);
        let spec = test_spec(1, FileAllocation::Prealloc);
        let err = validate_local_resume_state(&path, &ctrl, &spec)
            .await
            .unwrap_err();
        assert!(matches!(err, DownloadError::ResumeMismatch(msg) if msg.contains("resume file size mismatch")));
    }

    #[tokio::test]
    async fn test_validate_resume_multi_prealloc_ok() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.bin");
        // 2 pieces of 500 bytes each, first piece complete
        std::fs::write(&path, vec![0u8; 1000]).unwrap();
        let ctrl = test_ctrl(1000, 500, 2, 500, vec![0b01]);
        let spec = test_spec(4, FileAllocation::Prealloc);
        validate_local_resume_state(&path, &ctrl, &spec)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_validate_resume_multi_prealloc_size_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.bin");
        std::fs::write(&path, vec![0u8; 800]).unwrap();
        let ctrl = test_ctrl(1000, 500, 2, 500, vec![0b01]);
        let spec = test_spec(4, FileAllocation::Prealloc);
        let err = validate_local_resume_state(&path, &ctrl, &spec)
            .await
            .unwrap_err();
        assert!(matches!(err, DownloadError::ResumeMismatch(msg) if msg.contains("preallocated")));
    }

    #[tokio::test]
    async fn test_validate_resume_multi_no_prealloc_ok() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.bin");
        // 2 pieces of 500 bytes, first piece (0) complete → file needs at least 500 bytes
        std::fs::write(&path, vec![0u8; 500]).unwrap();
        let ctrl = test_ctrl(1000, 500, 2, 500, vec![0b01]);
        let spec = test_spec(4, FileAllocation::None);
        validate_local_resume_state(&path, &ctrl, &spec)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_validate_resume_multi_no_prealloc_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.bin");
        // Second piece (id=1) is complete (range 500..1000), but file is only 200 bytes
        std::fs::write(&path, vec![0u8; 200]).unwrap();
        let ctrl = test_ctrl(1000, 500, 2, 500, vec![0b10]);
        let spec = test_spec(4, FileAllocation::None);
        let err = validate_local_resume_state(&path, &ctrl, &spec)
            .await
            .unwrap_err();
        assert!(matches!(err, DownloadError::ResumeMismatch(msg) if msg.contains("truncated")));
    }

    #[tokio::test]
    async fn test_validate_resume_multi_no_prealloc_too_large() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.bin");
        std::fs::write(&path, vec![0u8; 1500]).unwrap();
        let ctrl = test_ctrl(1000, 500, 2, 500, vec![0b01]);
        let spec = test_spec(4, FileAllocation::None);
        let err = validate_local_resume_state(&path, &ctrl, &spec)
            .await
            .unwrap_err();
        assert!(matches!(err, DownloadError::ResumeMismatch(msg) if msg.contains("larger")));
    }

    #[tokio::test]
    async fn test_validate_resume_multi_no_prealloc_downloaded_exceeds_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.bin");
        std::fs::write(&path, vec![0u8; 500]).unwrap();
        let ctrl = test_ctrl(1000, 500, 2, 800, vec![0b01]);
        let spec = test_spec(4, FileAllocation::None);
        let err = validate_local_resume_state(&path, &ctrl, &spec)
            .await
            .unwrap_err();
        assert!(matches!(err, DownloadError::ResumeMismatch(msg) if msg.contains("downloaded bytes")));
    }

    #[tokio::test]
    async fn test_validate_resume_multi_completed_exceeds_downloaded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.bin");
        std::fs::write(&path, vec![0u8; 1000]).unwrap();
        // Both pieces marked complete (1000 bytes) but downloaded_bytes only 100
        let ctrl = test_ctrl(1000, 500, 2, 100, vec![0b11]);
        let spec = test_spec(4, FileAllocation::None);
        let err = validate_local_resume_state(&path, &ctrl, &spec)
            .await
            .unwrap_err();
        assert!(matches!(err, DownloadError::ResumeMismatch(msg) if msg.contains("completed bytes")));
    }

    #[tokio::test]
    async fn test_validate_resume_metadata_io_error_is_not_mapped_to_missing_file() {
        let err = map_resume_metadata_error(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "resume metadata access denied",
        ));
        assert!(matches!(err, DownloadError::Io(_)));
    }
}
