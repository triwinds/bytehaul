//! Compatibility of the multi-IP switch through the public API.
//!
//! `docs/multi-ip-connection-plan.zh-CN.md` §8 (M4) requires the switch to leave
//! the observable behaviour alone when an origin has one address: the same
//! bytes, the same resume, the same multi-connection download. A name is used
//! rather than the loopback literal so the policy path is the one under test -
//! an IP-literal URL never enters it.

use bytehaul::{DownloadSpec, Downloader, FileAllocation, LogLevel, SlowTransferMode};

use warp::Filter;

/// The body every route serves: large enough to need several ranges.
fn content() -> Vec<u8> {
    (0..400_000u32).map(|i| (i % 251) as u8).collect()
}

fn downloader(multi_ip: bool) -> Downloader {
    Downloader::builder()
        .multi_ip(multi_ip)
        .log_level(LogLevel::Off)
        .build()
        .unwrap()
}

#[tokio::test]
async fn a_policy_download_matches_the_plain_one() {
    let body = content();
    let route = warp::path("file").map({
        let body = body.clone();
        move || warp::http::Response::new(body.clone())
    });
    let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let url = format!("http://localhost:{}/file", addr.port());
    let mut downloaded = Vec::new();
    for multi_ip in [false, true] {
        let output_path = dir.path().join(format!("multi-ip-{multi_ip}.bin"));
        let spec = DownloadSpec::new(url.clone())
            .output_path(output_path.clone())
            .file_allocation(FileAllocation::None);
        downloader(multi_ip).download(spec).wait().await.unwrap();
        downloaded.push(std::fs::read(&output_path).unwrap());
    }

    assert_eq!(downloaded[0], body, "the plain path still works");
    assert_eq!(
        downloaded[1], body,
        "the policy path must download the same bytes"
    );
}

#[tokio::test]
async fn two_policy_downloads_share_one_origin() {
    let body = content();
    let route = warp::path("shared").map({
        let body = body.clone();
        move || warp::http::Response::new(body.clone())
    });
    let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let url = format!("http://localhost:{}/shared", addr.port());
    let downloader = downloader(true);
    let mut handles = Vec::new();
    for index in 0..2 {
        let output_path = dir.path().join(format!("shared-{index}.bin"));
        let spec = DownloadSpec::new(url.clone())
            .output_path(output_path.clone())
            .file_allocation(FileAllocation::None);
        handles.push((output_path, downloader.download(spec)));
    }

    for (output_path, handle) in handles {
        handle.wait().await.unwrap();
        assert_eq!(
            std::fs::read(&output_path).unwrap(),
            body,
            "concurrent tasks on one origin must not interfere"
        );
    }
}

#[tokio::test]
async fn a_policy_download_resumes_where_the_previous_one_stopped() {
    let body = content();
    let route = warp::path("resumable")
        .and(warp::header::optional::<String>("range"))
        .map({
            let body = body.clone();
            move |range: Option<String>| {
                let Some(range) = range else {
                    return warp::http::Response::builder()
                        .status(200)
                        .header("Accept-Ranges", "bytes")
                        .header("Content-Length", body.len().to_string())
                        .body(body.clone())
                        .unwrap();
                };
                let (start, _) = range
                    .trim_start_matches("bytes=")
                    .split_once('-')
                    .expect("range spec");
                let start: usize = start.trim().parse().unwrap();
                let slice = body[start..].to_vec();
                warp::http::Response::builder()
                    .status(206)
                    .header("Accept-Ranges", "bytes")
                    .header(
                        "Content-Range",
                        format!("bytes {start}-{}/{}", body.len() - 1, body.len()),
                    )
                    .header("Content-Length", slice.len().to_string())
                    .body(slice)
                    .unwrap()
            }
        });
    let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("resumed.bin");
    let url = format!("http://localhost:{}/resumable", addr.port());

    // A partial file on disk is what makes the next download a resume.
    let partial = &body[..100_000];
    std::fs::write(&output_path, partial).unwrap();

    let spec = DownloadSpec::new(url)
        .output_path(output_path.clone())
        .resume(true)
        .file_allocation(FileAllocation::None);
    downloader(true).download(spec).wait().await.unwrap();

    assert_eq!(std::fs::read(&output_path).unwrap(), body);
}

#[tokio::test]
async fn a_policy_download_survives_a_rate_limit() {
    let body = content();
    let route = warp::path("limited").map({
        let body = body.clone();
        move || warp::http::Response::new(body.clone())
    });
    let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
    tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("limited.bin");
    // A local rate limit is exactly the kind of backpressure that must not
    // become part of an address's measured rate (§7).
    let spec = DownloadSpec::new(format!("http://localhost:{}/limited", addr.port()))
        .output_path(output_path.clone())
        .max_download_speed(400_000)
        .slow_transfer_mode(SlowTransferMode::Disabled)
        .file_allocation(FileAllocation::None);

    downloader(true).download(spec).wait().await.unwrap();
    assert_eq!(std::fs::read(&output_path).unwrap(), body);
}
