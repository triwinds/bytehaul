use bytehaul::{DownloadError, DownloadSpec, Downloader, FileAllocation};
use futures::StreamExt;
use warp::Filter;

#[tokio::test]
async fn test_auto_filename_uses_content_disposition() {
    let content = b"content disposition body".to_vec();
    let route = warp::path("attachment").map(move || {
        warp::http::Response::builder()
            .status(200)
            .header("content-length", content.len().to_string())
            .header(
                "content-disposition",
                "attachment; filename=server-name.bin",
            )
            .body(content.clone())
            .unwrap()
    });
    let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let downloader = Downloader::builder().build().unwrap();
    let mut spec = DownloadSpec::new(format!("http://{addr}/attachment")).output_dir(dir.path());
    spec.file_allocation = FileAllocation::None;

    let handle = downloader.download(spec);
    handle.wait().await.unwrap();

    let output_path = dir.path().join("server-name.bin");
    assert_eq!(std::fs::read(&output_path).unwrap(), b"content disposition body");
}

#[tokio::test]
async fn test_auto_filename_falls_back_to_url_path() {
    let content = b"url fallback body".to_vec();
    let route = warp::path!("files" / "report.bin").map(move || {
        warp::http::Response::builder()
            .status(200)
            .header("content-length", content.len().to_string())
            .body(content.clone())
            .unwrap()
    });
    let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let downloader = Downloader::builder().build().unwrap();
    let mut spec = DownloadSpec::new(format!("http://{addr}/files/report.bin")).output_dir(dir.path());
    spec.file_allocation = FileAllocation::None;

    let handle = downloader.download(spec);
    handle.wait().await.unwrap();

    let output_path = dir.path().join("report.bin");
    assert_eq!(std::fs::read(&output_path).unwrap(), b"url fallback body");
}

#[tokio::test]
async fn test_output_dir_and_relative_output_path_are_combined() {
    let content = b"nested output path body".to_vec();
    let route = warp::path("nested-output").map(move || {
        warp::http::Response::builder()
            .status(200)
            .header("content-length", content.len().to_string())
            .body(content.clone())
            .unwrap()
    });
    let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("nested").join("custom.bin");
    let downloader = Downloader::builder().build().unwrap();
    let mut spec = DownloadSpec::new(format!("http://{addr}/nested-output"))
        .output_dir(dir.path())
        .output_path("nested/custom.bin");
    spec.file_allocation = FileAllocation::None;

    let handle = downloader.download(spec);
    handle.wait().await.unwrap();

    assert_eq!(std::fs::read(&output_path).unwrap(), b"nested output path body");
}

#[tokio::test]
async fn test_auto_filename_resume_after_pause() {
    let content: Vec<u8> = (0..2_000_000u32).map(|index| (index % 251) as u8).collect();
    let expected = content.clone();

    let data = std::sync::Arc::new(content);
    let route = warp::path("resume-auto")
        .and(warp::header::optional::<String>("range"))
        .map(move |range_header: Option<String>| {
            let data = data.clone();
            let total = data.len();

            match range_header {
                Some(range) => {
                    let range = range.trim_start_matches("bytes=");
                    let parts: Vec<&str> = range.split('-').collect();
                    let start: u64 = parts[0].parse().unwrap_or(0);
                    let end: u64 = if parts.len() > 1 && !parts[1].is_empty() {
                        parts[1]
                            .parse::<u64>()
                            .unwrap_or(total as u64 - 1)
                            .min(total as u64 - 1)
                    } else {
                        total as u64 - 1
                    };
                    let slice = data[start as usize..=end as usize].to_vec();
                    warp::http::Response::builder()
                        .status(206)
                        .header("content-length", slice.len().to_string())
                        .header("content-range", format!("bytes {}-{}/{}", start, end, total))
                        .header("accept-ranges", "bytes")
                        .header(
                            "content-disposition",
                            "attachment; filename=resume-auto.bin",
                        )
                        .body(warp::hyper::Body::from(slice))
                        .unwrap()
                }
                None => {
                    let chunks: Vec<Result<Vec<u8>, std::convert::Infallible>> = data
                        .chunks(16 * 1024)
                        .map(|chunk| Ok(chunk.to_vec()))
                        .collect();
                    let stream = futures::stream::iter(chunks).then(
                        |chunk: Result<Vec<u8>, std::convert::Infallible>| async move {
                            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
                            chunk
                        },
                    );
                    let body = warp::hyper::Body::wrap_stream(stream);
                    warp::http::Response::builder()
                        .status(200)
                        .header("content-length", total.to_string())
                        .header("accept-ranges", "bytes")
                        .header(
                            "content-disposition",
                            "attachment; filename=resume-auto.bin",
                        )
                        .body(body)
                        .unwrap()
                }
            }
        });
    let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("resume-auto.bin");
    let control_path = output_path.with_file_name("resume-auto.bin.bytehaul");
    let downloader = Downloader::builder().build().unwrap();
    let mut spec = DownloadSpec::new(format!("http://{addr}/resume-auto")).output_dir(dir.path());
    spec.file_allocation = FileAllocation::None;
    spec.max_connections = 1;

    let handle = downloader.download(spec.clone());
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    handle.pause();

    assert!(matches!(handle.wait().await, Err(DownloadError::Paused)));
    assert!(output_path.exists(), "resolved output file should exist after pause");
    assert!(control_path.exists(), "control file should use the resolved filename");

    let resumed = downloader.download(spec);
    resumed.wait().await.unwrap();

    assert_eq!(std::fs::read(&output_path).unwrap(), expected);
    assert!(!control_path.exists(), "control file should be removed after success");
}

#[tokio::test]
async fn test_auto_filename_defaults_to_download() {
    let content = b"default filename body".to_vec();
    let route = warp::path::end().map(move || {
        warp::http::Response::builder()
            .status(200)
            .header("content-length", content.len().to_string())
            .body(content.clone())
            .unwrap()
    });
    let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let downloader = Downloader::builder().build().unwrap();
    let mut spec = DownloadSpec::new(format!("http://{addr}/")).output_dir(dir.path());
    spec.file_allocation = FileAllocation::None;

    let handle = downloader.download(spec);
    handle.wait().await.unwrap();

    let output_path = dir.path().join("download");
    assert_eq!(std::fs::read(&output_path).unwrap(), b"default filename body");
}