use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::DownloadError;

const MAGIC: u32 = 0x4259_4845; // "BYHE"
const VERSION: u32 = 1;
const HEADER_SIZE: usize = 16; // magic(4) + version(4) + payload_len(4) + checksum(4)

/// Control file snapshot for resume support.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ControlSnapshot {
    pub url: String,
    pub total_size: u64,
    pub piece_size: u64,
    pub piece_count: usize,
    pub completed_bitset: Vec<u8>,
    /// For single-connection resume: the byte offset written so far.
    pub downloaded_bytes: u64,
    pub etag: Option<String>,
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
        let snapshot = self.clone();
        let path = control_path.to_path_buf();
        tokio::task::spawn_blocking(move || save_sync(&snapshot, &path))
            .await
            .map_err(|e| DownloadError::Other(format!("spawn_blocking: {e}")))?
    }

    /// Load and validate a control snapshot from disk.
    pub async fn load(control_path: &Path) -> Result<Self, DownloadError> {
        let path = control_path.to_path_buf();
        tokio::task::spawn_blocking(move || load_sync(&path))
            .await
            .map_err(|e| DownloadError::Other(format!("spawn_blocking: {e}")))?
    }

    /// Delete the control file if it exists.
    pub async fn delete(control_path: &Path) -> Result<(), DownloadError> {
        match tokio::fs::remove_file(control_path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(DownloadError::Io(e)),
        }
    }
}

fn save_sync(snapshot: &ControlSnapshot, path: &Path) -> Result<(), DownloadError> {
    use std::io::Write;

    let payload = bincode::serialize(snapshot)
        .map_err(|e| DownloadError::Other(format!("serialize: {e}")))?;
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

fn load_sync(path: &Path) -> Result<ControlSnapshot, DownloadError> {
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
    if version != VERSION {
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

    bincode::deserialize(payload)
        .map_err(|e| DownloadError::ControlFileCorrupted(format!("deserialize: {e}")))
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
}
