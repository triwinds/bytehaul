use sha2::{Digest, Sha256, Sha512};

use crate::config::Checksum;
use crate::error::DownloadError;

/// Verify the checksum of a file on disk against the expected value.
///
/// `stop` is polled between hash chunks so a long verification still observes a
/// pause or cancel request; `DownloadError::Cancelled` / `Paused` returned from
/// it aborts the verification with that outcome. Pass `|| None` when no stop
/// request can exist.
pub(crate) async fn verify_checksum(
    path: &std::path::Path,
    expected: &Checksum,
    stop: impl Fn() -> Option<DownloadError>,
) -> Result<(), DownloadError> {
    let algorithm = match expected {
        Checksum::Sha256(_) => "sha256",
        Checksum::Sha1(_) => "sha1",
        Checksum::Md5(_) => "md5",
        Checksum::Sha512(_) => "sha512",
    };
    tracing::debug!(path = %path.display(), algorithm, "starting checksum verification");
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(DownloadError::Io)?;

    match expected {
        Checksum::Sha256(expected_hex) => {
            hash_and_compare::<Sha256>(&mut file, expected_hex, &stop).await
        }
        Checksum::Sha512(expected_hex) => {
            hash_and_compare::<Sha512>(&mut file, expected_hex, &stop).await
        }
        Checksum::Sha1(expected_hex) => {
            hash_and_compare::<sha1::Sha1>(&mut file, expected_hex, &stop).await
        }
        Checksum::Md5(expected_hex) => {
            hash_and_compare::<md5::Md5>(&mut file, expected_hex, &stop).await
        }
    }
}

async fn hash_and_compare<D: Digest + Default>(
    file: &mut tokio::fs::File,
    expected_hex: &str,
    stop: &impl Fn() -> Option<DownloadError>,
) -> Result<(), DownloadError> {
    use tokio::io::AsyncReadExt;
    let mut hasher = D::default();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        if let Some(error) = stop() {
            return Err(error);
        }
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
            expected: expected_hex.to_string(),
            actual: actual_hex,
        });
    }
    Ok(())
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
        verify_checksum(&path, &expected, || None).await.unwrap();
    }

    #[tokio::test]
    async fn test_verify_checksum_fail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.bin");
        tokio::fs::write(&path, b"hello world").await.unwrap();

        let expected = Checksum::Sha256(
            "0000000000000000000000000000000000000000000000000000000000000000".into(),
        );
        let result = verify_checksum(&path, &expected, || None).await;
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
        verify_checksum(&path, &expected, || None).await.unwrap();
    }

    #[tokio::test]
    async fn test_verify_checksum_file_not_found() {
        let result = verify_checksum(
            std::path::Path::new("/nonexistent/file"),
            &Checksum::Sha256("abc".into()),
            || None,
        )
        .await;
        assert!(matches!(result, Err(DownloadError::Io(_))));
    }

    #[tokio::test]
    async fn test_verify_checksum_sha1() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_sha1.bin");
        tokio::fs::write(&path, b"hello world").await.unwrap();
        // SHA-1 of "hello world"
        let expected = Checksum::Sha1("2aae6c35c94fcfb415dbe95f408b9ce91ee846ed".into());
        verify_checksum(&path, &expected, || None).await.unwrap();
    }

    #[tokio::test]
    async fn test_verify_checksum_md5() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_md5.bin");
        tokio::fs::write(&path, b"hello world").await.unwrap();
        // MD5 of "hello world"
        let expected = Checksum::Md5("5eb63bbbe01eeed093cb22bb8f5acdc3".into());
        verify_checksum(&path, &expected, || None).await.unwrap();
    }

    #[tokio::test]
    async fn test_verify_checksum_sha512() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_sha512.bin");
        tokio::fs::write(&path, b"hello world").await.unwrap();
        // SHA-512 of "hello world"
        let expected = Checksum::Sha512(
            "309ecc489c12d6eb4cc40f50c902f2b4d0ed77ee511a7c7a9bcd3ca86d4cd86f989dd35bc5ff499670da34255b45b0cfd830e81f605dcf7dc5542e93ae9cd76f".into(),
        );
        verify_checksum(&path, &expected, || None).await.unwrap();
    }

    #[tokio::test]
    async fn test_verify_checksum_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.bin");
        tokio::fs::write(&path, b"").await.unwrap();

        let expected = Checksum::Sha256(
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
        );
        verify_checksum(&path, &expected, || None).await.unwrap();
    }

    #[tokio::test]
    async fn test_verify_checksum_observes_stop_between_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.bin");
        // Larger than one 64 KiB hash chunk, so the stop hook is polled again
        // before the file is fully read.
        tokio::fs::write(&path, vec![0x5Au8; 256 * 1024])
            .await
            .unwrap();

        let expected = Checksum::Sha256("00".repeat(32));
        let polls = std::cell::Cell::new(0u32);
        let result = verify_checksum(&path, &expected, || {
            polls.set(polls.get() + 1);
            (polls.get() >= 3).then_some(DownloadError::Paused)
        })
        .await;

        assert!(matches!(result, Err(DownloadError::Paused)));
        assert!(
            polls.get() >= 3,
            "verification must poll the stop check while hashing, not only once"
        );
    }
}
