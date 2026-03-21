use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

use crate::config::Checksum;
use crate::error::DownloadError;

/// Verify the checksum of a file on disk against the expected value.
pub(crate) async fn verify_checksum(
    path: &std::path::Path,
    expected: &Checksum,
) -> Result<(), DownloadError> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(DownloadError::Io)?;

    match expected {
        Checksum::Sha256(expected_hex) => {
            let mut hasher = Sha256::new();
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let n = file.read(&mut buf).await.map_err(DownloadError::Io)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
            }
            let result = hasher.finalize();
            let actual_hex = hex_encode(&result);
            if actual_hex != expected_hex.to_ascii_lowercase() {
                return Err(DownloadError::ChecksumMismatch {
                    expected: expected_hex.clone(),
                    actual: actual_hex,
                });
            }
            Ok(())
        }
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}
