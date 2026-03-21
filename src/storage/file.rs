use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

use crate::config::FileAllocation;
use crate::error::DownloadError;

/// Create (or truncate) the output file, optionally pre-allocating space.
pub(crate) async fn create_output_file(
    path: &Path,
    total_size: Option<u64>,
    allocation: FileAllocation,
) -> Result<tokio::fs::File, DownloadError> {
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
