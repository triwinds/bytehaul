use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::DownloadError;

const MAGIC: u32 = 0x4259_4845; // "BYHE"
const VERSION_V1: u32 = 1;
const VERSION_V2: u32 = 2;
const VERSION: u32 = VERSION_V2;
const HEADER_SIZE: usize = 16; // magic(4) + version(4) + payload_len(4) + checksum(4)

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ControlHints {
    pub dirty_piece_ids: Vec<usize>,
    pub inflight_piece_ids: Vec<usize>,
    pub snapshot_seq: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ControlSnapshotV2 {
    snapshot: ControlSnapshot,
    hints: ControlHints,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum ControlPayload {
    V2(ControlSnapshotV2),
}

/// Control file snapshot for resume support.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlSnapshot {
    /// The original download URL.
    pub url: String,
    /// Total file size in bytes.
    pub total_size: u64,
    /// Piece size used for splitting.
    pub piece_size: u64,
    /// Total number of pieces.
    pub piece_count: usize,
    /// Bitset encoding which pieces have been completed.
    pub completed_bitset: Vec<u8>,
    /// Durable progress stored in the control file.
    ///
    /// For single-connection resume this is the persisted prefix length. For
    /// multi-connection resume it counts only bytes belonging to fully
    /// completed pieces.
    pub downloaded_bytes: u64,
    /// HTTP `ETag` header from the server, used to detect file changes.
    pub etag: Option<String>,
    /// HTTP `Last-Modified` header from the server.
    pub last_modified: Option<String>,
}

impl ControlSnapshot {
    /// Derive the control file path from the download output path.
    pub fn control_path(output_path: &Path) -> PathBuf {
        let mut p = output_path.as_os_str().to_os_string();
        p.push(".bytehaul");
        PathBuf::from(p)
    }

    /// Save the snapshot atomically: write to tmp → fsync → rename.
    pub async fn save(&self, control_path: &Path) -> Result<(), DownloadError> {
        self.save_with_hints(control_path, ControlHints::default()).await
    }

    pub(crate) async fn save_with_hints(
        &self,
        control_path: &Path,
        hints: ControlHints,
    ) -> Result<(), DownloadError> {
        tracing::debug!(path = %control_path.display(), control_downloaded_bytes = self.downloaded_bytes, "saving control file");
        let snapshot = self.clone();
        let path = control_path.to_path_buf();
        tokio::task::spawn_blocking(move || save_sync(&snapshot, hints, &path))
            .await
            .map_err(|e| DownloadError::TaskFailed(format!("spawn_blocking: {e}")))?
    }

    /// Load and validate a control snapshot from disk.
    pub async fn load(control_path: &Path) -> Result<Self, DownloadError> {
        Ok(Self::load_with_hints(control_path).await?.0)
    }

    pub(crate) async fn load_with_hints(
        control_path: &Path,
    ) -> Result<(Self, ControlHints), DownloadError> {
        tracing::debug!(path = %control_path.display(), "loading control file");
        let path = control_path.to_path_buf();
        tokio::task::spawn_blocking(move || load_sync_with_hints(&path))
            .await
            .map_err(|e| DownloadError::TaskFailed(format!("spawn_blocking: {e}")))?
    }

    /// Delete the control file if it exists.
    pub async fn delete(control_path: &Path) -> Result<(), DownloadError> {
        tracing::debug!(path = %control_path.display(), "deleting control file");
        match tokio::fs::remove_file(control_path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(DownloadError::Io(e)),
        }
    }
}

fn save_sync(
    snapshot: &ControlSnapshot,
    hints: ControlHints,
    path: &Path,
) -> Result<(), DownloadError> {
    use std::io::Write;

    let payload = bincode::serialize(&ControlPayload::V2(ControlSnapshotV2 {
        snapshot: snapshot.clone(),
        hints,
    }))
        .map_err(|e| DownloadError::Internal(format!("control file serialize failed: {e}")))?;
    let checksum = crc32fast::hash(&payload);

    let tmp_path = path.with_extension("bytehaul.tmp");
    let mut file = std::fs::File::create(&tmp_path)?;

    file.write_all(&MAGIC.to_le_bytes())?;
    file.write_all(&VERSION.to_le_bytes())?;
    file.write_all(&(payload.len() as u32).to_le_bytes())?;
    file.write_all(&checksum.to_le_bytes())?;
    file.write_all(&payload)?;

    file.flush()?;
    file.sync_all()?;
    drop(file);

    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

fn load_sync_with_hints(path: &Path) -> Result<(ControlSnapshot, ControlHints), DownloadError> {
    use std::io::Read;

    let mut file = std::fs::File::open(path)?;
    let mut data = Vec::new();
    file.read_to_end(&mut data)?;

    if data.len() < HEADER_SIZE {
        return Err(DownloadError::ControlFileCorrupted("too short".into()));
    }

    let magic = u32::from_le_bytes(data[0..4].try_into().unwrap());
    let version = u32::from_le_bytes(data[4..8].try_into().unwrap());
    let payload_len = u32::from_le_bytes(data[8..12].try_into().unwrap()) as usize;
    let checksum = u32::from_le_bytes(data[12..16].try_into().unwrap());

    if magic != MAGIC {
        return Err(DownloadError::ControlFileCorrupted("bad magic".into()));
    }
    if version != VERSION_V1 && version != VERSION_V2 {
        return Err(DownloadError::ControlFileCorrupted(format!(
            "unsupported version: {version}"
        )));
    }
    if data.len() < HEADER_SIZE + payload_len {
        return Err(DownloadError::ControlFileCorrupted(
            "truncated payload".into(),
        ));
    }

    let payload = &data[HEADER_SIZE..HEADER_SIZE + payload_len];
    let actual_checksum = crc32fast::hash(payload);
    if actual_checksum != checksum {
        return Err(DownloadError::ControlFileCorrupted(
            "checksum mismatch".into(),
        ));
    }

    match version {
        VERSION_V1 => {
            let snapshot: ControlSnapshot = bincode::deserialize(payload)
                .map_err(|e| DownloadError::ControlFileCorrupted(format!("deserialize: {e}")))?;
            Ok((snapshot, ControlHints::default()))
        }
        VERSION_V2 => {
            let payload: ControlPayload = bincode::deserialize(payload)
                .map_err(|e| DownloadError::ControlFileCorrupted(format!("deserialize: {e}")))?;
            match payload {
                ControlPayload::V2(snapshot) => Ok((snapshot.snapshot, snapshot.hints)),
            }
        }
        _ => unreachable!("unsupported versions are rejected above"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_save_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let ctrl_path = dir.path().join("test.bytehaul");

        let snapshot = ControlSnapshot {
            url: "https://example.com/file.bin".to_string(),
            total_size: 1_000_000,
            piece_size: 1_000_000,
            piece_count: 1,
            completed_bitset: vec![0],
            downloaded_bytes: 500_000,
            etag: Some("\"abc123\"".to_string()),
            last_modified: Some("Thu, 01 Jan 2026 00:00:00 GMT".to_string()),
        };

        snapshot.save(&ctrl_path).await.unwrap();
        let loaded = ControlSnapshot::load(&ctrl_path).await.unwrap();

        assert_eq!(loaded.url, snapshot.url);
        assert_eq!(loaded.total_size, snapshot.total_size);
        assert_eq!(loaded.downloaded_bytes, snapshot.downloaded_bytes);
        assert_eq!(loaded.etag, snapshot.etag);
        assert_eq!(loaded.last_modified, snapshot.last_modified);
    }

    #[tokio::test]
    async fn test_load_corrupted() {
        let dir = tempfile::tempdir().unwrap();
        let ctrl_path = dir.path().join("bad.bytehaul");
        std::fs::write(&ctrl_path, b"garbage data here").unwrap();

        let result = ControlSnapshot::load(&ctrl_path).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_delete_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let ctrl_path = dir.path().join("nope.bytehaul");
        ControlSnapshot::delete(&ctrl_path).await.unwrap();
    }

    #[tokio::test]
    async fn test_control_path_derivation() {
        let path = std::path::PathBuf::from("/tmp/myfile.bin");
        let ctrl = ControlSnapshot::control_path(&path);
        assert!(ctrl.to_str().unwrap().ends_with(".bytehaul"));
    }

    #[tokio::test]
    async fn test_load_bad_magic() {
        let dir = tempfile::tempdir().unwrap();
        let ctrl_path = dir.path().join("bad_magic.bytehaul");
        // Write a file with wrong magic but enough header bytes
        let mut data = vec![0u8; 20];
        data[0..4].copy_from_slice(&0xDEADBEEFu32.to_le_bytes());
        data[4..8].copy_from_slice(&VERSION_V1.to_le_bytes());
        data[8..12].copy_from_slice(&0u32.to_le_bytes());
        data[12..16].copy_from_slice(&0u32.to_le_bytes());
        std::fs::write(&ctrl_path, &data).unwrap();

        let result = ControlSnapshot::load(&ctrl_path).await;
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(err_msg.contains("bad magic"));
    }

    #[tokio::test]
    async fn test_load_bad_version() {
        let dir = tempfile::tempdir().unwrap();
        let ctrl_path = dir.path().join("bad_ver.bytehaul");
        let mut data = vec![0u8; 20];
        data[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        data[4..8].copy_from_slice(&99u32.to_le_bytes()); // wrong version
        data[8..12].copy_from_slice(&0u32.to_le_bytes());
        data[12..16].copy_from_slice(&0u32.to_le_bytes());
        std::fs::write(&ctrl_path, &data).unwrap();

        let result = ControlSnapshot::load(&ctrl_path).await;
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(err_msg.contains("unsupported version"));
    }

    #[tokio::test]
    async fn test_load_truncated_payload() {
        let dir = tempfile::tempdir().unwrap();
        let ctrl_path = dir.path().join("truncated.bytehaul");
        let mut data = vec![0u8; 16];
        data[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        data[4..8].copy_from_slice(&VERSION.to_le_bytes());
        data[8..12].copy_from_slice(&1000u32.to_le_bytes()); // claims 1000 bytes of payload
        data[12..16].copy_from_slice(&0u32.to_le_bytes());
        std::fs::write(&ctrl_path, &data).unwrap();

        let result = ControlSnapshot::load(&ctrl_path).await;
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(err_msg.contains("truncated"));
    }

    #[tokio::test]
    async fn test_load_checksum_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let ctrl_path = dir.path().join("bad_crc.bytehaul");

        // First save a valid snapshot
        let snapshot = ControlSnapshot {
            url: "https://example.com/file.bin".to_string(),
            total_size: 100,
            piece_size: 100,
            piece_count: 1,
            completed_bitset: vec![0],
            downloaded_bytes: 0,
            etag: None,
            last_modified: None,
        };
        snapshot.save(&ctrl_path).await.unwrap();

        // Corrupt the payload but keep the header intact
        let mut data = std::fs::read(&ctrl_path).unwrap();
        if data.len() > HEADER_SIZE + 2 {
            data[HEADER_SIZE + 1] ^= 0xFF; // flip a byte in payload
        }
        std::fs::write(&ctrl_path, &data).unwrap();

        let result = ControlSnapshot::load(&ctrl_path).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_save_load_roundtrip_with_hints() {
        let dir = tempfile::tempdir().unwrap();
        let ctrl_path = dir.path().join("test-v2.bytehaul");

        let snapshot = ControlSnapshot {
            url: "https://example.com/file.bin".to_string(),
            total_size: 1_000_000,
            piece_size: 256_000,
            piece_count: 4,
            completed_bitset: vec![0b0000_0011],
            downloaded_bytes: 512_000,
            etag: Some("\"abc123\"".to_string()),
            last_modified: Some("Thu, 01 Jan 2026 00:00:00 GMT".to_string()),
        };
        let hints = ControlHints {
            dirty_piece_ids: vec![2],
            inflight_piece_ids: vec![3],
            snapshot_seq: 7,
        };

        snapshot
            .save_with_hints(&ctrl_path, hints.clone())
            .await
            .unwrap();
        let (loaded, loaded_hints) = ControlSnapshot::load_with_hints(&ctrl_path).await.unwrap();

        assert_eq!(loaded.url, snapshot.url);
        assert_eq!(loaded.downloaded_bytes, snapshot.downloaded_bytes);
        assert_eq!(loaded_hints, hints);
    }

    #[tokio::test]
    async fn test_load_v1_snapshot_upgrades_with_empty_hints() {
        let dir = tempfile::tempdir().unwrap();
        let ctrl_path = dir.path().join("legacy-v1.bytehaul");

        let snapshot = ControlSnapshot {
            url: "https://example.com/file.bin".to_string(),
            total_size: 64,
            piece_size: 64,
            piece_count: 1,
            completed_bitset: vec![0],
            downloaded_bytes: 32,
            etag: None,
            last_modified: None,
        };

        let payload = bincode::serialize(&snapshot).unwrap();
        let checksum = crc32fast::hash(&payload);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&MAGIC.to_le_bytes());
        bytes.extend_from_slice(&VERSION_V1.to_le_bytes());
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&checksum.to_le_bytes());
        bytes.extend_from_slice(&payload);
        std::fs::write(&ctrl_path, bytes).unwrap();

        let (loaded, hints) = ControlSnapshot::load_with_hints(&ctrl_path).await.unwrap();
        assert_eq!(loaded.downloaded_bytes, snapshot.downloaded_bytes);
        assert!(hints.dirty_piece_ids.is_empty());
        assert!(hints.inflight_piece_ids.is_empty());
        assert_eq!(hints.snapshot_seq, 0);
    }

    #[tokio::test]
    async fn test_delete_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let ctrl_path = dir.path().join("to_delete.bytehaul");
        std::fs::write(&ctrl_path, b"some data").unwrap();
        assert!(ctrl_path.exists());
        ControlSnapshot::delete(&ctrl_path).await.unwrap();
        assert!(!ctrl_path.exists());
    }

    #[tokio::test]
    async fn test_load_too_short() {
        let dir = tempfile::tempdir().unwrap();
        let ctrl_path = dir.path().join("short.bytehaul");
        // Write less than HEADER_SIZE bytes
        std::fs::write(&ctrl_path, b"short").unwrap();

        let result = ControlSnapshot::load(&ctrl_path).await;
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(err_msg.contains("too short"));
    }

    #[tokio::test]
    async fn test_delete_directory_returns_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = ControlSnapshot::delete(dir.path()).await.unwrap_err();
        assert!(matches!(err, DownloadError::Io(_)));
    }
}
