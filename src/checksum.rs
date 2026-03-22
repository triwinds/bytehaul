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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hex_encode() {
        assert_eq!(hex_encode(&[0x00, 0xff, 0x0a]), "00ff0a");
        assert_eq!(hex_encode(&[]), "");
        assert_eq!(hex_encode(&[0xde, 0xad, 0xbe, 0xef]), "deadbeef");
    }

    #[tokio::test]
    async fn test_verify_checksum_pass() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.bin");
        tokio::fs::write(&path, b"hello world").await.unwrap();

        // SHA-256 of "hello world"
        let expected = Checksum::Sha256(
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9".into(),
        );
        verify_checksum(&path, &expected).await.unwrap();
    }

    #[tokio::test]
    async fn test_verify_checksum_fail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.bin");
        tokio::fs::write(&path, b"hello world").await.unwrap();

        let expected = Checksum::Sha256(
            "0000000000000000000000000000000000000000000000000000000000000000".into(),
        );
        let result = verify_checksum(&path, &expected).await;
        assert!(matches!(
            result,
            Err(DownloadError::ChecksumMismatch { .. })
        ));
    }

    #[tokio::test]
    async fn test_verify_checksum_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.bin");
        tokio::fs::write(&path, b"hello world").await.unwrap();

        // Upper case hash should also match
        let expected = Checksum::Sha256(
            "B94D27B9934D3E08A52E52D7DA7DABFAC484EFE37A5380EE9088F7ACE2EFCDE9".into(),
        );
        verify_checksum(&path, &expected).await.unwrap();
    }

    #[tokio::test]
    async fn test_verify_checksum_file_not_found() {
        let result = verify_checksum(
            std::path::Path::new("/nonexistent/file"),
            &Checksum::Sha256("abc".into()),
        )
        .await;
        assert!(matches!(result, Err(DownloadError::Io(_))));
    }
}
