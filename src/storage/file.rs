use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

use crate::config::FileAllocation;
use crate::error::DownloadError;

/// Open an existing file for writing (without truncation) for resume.
pub(crate) async fn open_existing_file(path: &Path) -> Result<tokio::fs::File, DownloadError> {
    tracing::debug!(path = %path.display(), "opening existing file for resume");
    let file = tokio::fs::OpenOptions::new().write(true).open(path).await?;
    Ok(file)
}

/// Create (or truncate) the output file, optionally pre-allocating space.
pub(crate) async fn create_output_file(
    path: &Path,
    total_size: Option<u64>,
    allocation: FileAllocation,
) -> Result<tokio::fs::File, DownloadError> {
    tracing::debug!(path = %path.display(), total_size = total_size, allocation = ?allocation, "creating output file");
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let path = path.to_path_buf();
    let std_file = tokio::task::spawn_blocking(move || -> Result<std::fs::File, DownloadError> {
        let file = std::fs::File::create(&path)?;

        if let (FileAllocation::Prealloc, Some(size)) = (allocation, total_size) {
            if size > 0 {
                preallocate_sync(&file, size)?;
            }
        }

        Ok(file)
    })
    .await
    .map_err(|e| DownloadError::Other(format!("spawn_blocking join error: {e}")))??;

    Ok(tokio::fs::File::from_std(std_file))
}

/// Write zeros to fill the file to the requested size, then seek back to start.
fn preallocate_sync(file: &std::fs::File, size: u64) -> Result<(), std::io::Error> {
    let mut writer = std::io::BufWriter::new(file.try_clone()?);
    let chunk = vec![0u8; 256 * 1024]; // 256 KiB
    let mut remaining = size;

    while remaining > 0 {
        let n = remaining.min(chunk.len() as u64) as usize;
        writer.write_all(&chunk[..n])?;
        remaining -= n as u64;
    }

    writer.flush()?;
    drop(writer);

    let mut f = file.try_clone()?;
    f.sync_all()?;
    f.seek(SeekFrom::Start(0))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_create_output_file_no_prealloc() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.bin");
        let _file = create_output_file(&path, Some(1000), FileAllocation::None)
            .await
            .unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.len(), 0); // No pre-allocation
    }

    #[tokio::test]
    async fn test_create_output_file_prealloc() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_prealloc.bin");
        let _file = create_output_file(&path, Some(1000), FileAllocation::Prealloc)
            .await
            .unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.len(), 1000);
    }

    #[tokio::test]
    async fn test_create_output_file_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("dir").join("test.bin");
        let _file = create_output_file(&path, None, FileAllocation::None)
            .await
            .unwrap();
        assert!(path.exists());
    }

    #[tokio::test]
    async fn test_open_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("existing.bin");
        std::fs::write(&path, b"hello").unwrap();
        let _file = open_existing_file(&path).await.unwrap();
    }

    #[tokio::test]
    async fn test_open_existing_file_not_found() {
        let result = open_existing_file(std::path::Path::new("/nonexistent/file")).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_create_output_file_prealloc_zero_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("zero.bin");
        let _file = create_output_file(&path, Some(0), FileAllocation::Prealloc)
            .await
            .unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.len(), 0);
    }

    #[tokio::test]
    async fn test_create_output_file_no_total_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nosize.bin");
        let _file = create_output_file(&path, None, FileAllocation::Prealloc)
            .await
            .unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.len(), 0);
    }
}
