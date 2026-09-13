//! Lifecycle regression tests (P0 of the simplification plan).
//!
//! These cover the four behaviour bugs found in review (B1–B4) plus the
//! invariants the lifecycle fix relies on:
//!
//! * exactly one public terminal state per task, derived from the result that
//!   `wait()` returns;
//! * `Completed` only after writer finalization, cleanup and verification, so a
//!   checksum mismatch cannot follow a `Completed` snapshot;
//! * a stop request ends a queued task without another download releasing its
//!   permit, and a task that never started creates no output or checkpoint;
//! * dropping a handle neither cancels a download nor wedges it.

use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytehaul::{
    Checksum, DownloadError, DownloadHandle, DownloadSpec, DownloadState, Downloader,
    FileAllocation,
};
use futures::StreamExt;
use warp::Filter;

fn is_terminal(state: DownloadState) -> bool {
    matches!(
        state,
        DownloadState::Completed
            | DownloadState::Failed
            | DownloadState::Cancelled
            | DownloadState::Paused
    )
}

/// The public terminal state a `wait()` result must correspond to.
fn expected_state(result: &Result<(), DownloadError>) -> DownloadState {
    match result {
        Ok(()) => DownloadState::Completed,
        Err(DownloadError::Cancelled) => DownloadState::Cancelled,
        Err(DownloadError::Paused) => DownloadState::Paused,
        Err(_) => DownloadState::Failed,
    }
}

/// Collect every state the progress callback observes for one task.
fn collect_states(handle: &DownloadHandle) -> Arc<Mutex<Vec<DownloadState>>> {
    let states = Arc::new(Mutex::new(Vec::new()));
    let collected = states.clone();
    handle.on_progress(move |snapshot| {
        collected.lock().unwrap().push(snapshot.state);
    });
    states
}

/// Wait (bounded, never a fixed sleep) for the callback's terminal state, then
/// assert that exactly one terminal state was ever published and that it is the
/// last one observed.
async fn assert_single_terminal_state(
    states: &Arc<Mutex<Vec<DownloadState>>>,
    expected: DownloadState,
) -> Vec<DownloadState> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let observed = states.lock().unwrap().clone();
        if observed.last().is_some_and(|state| is_terminal(*state)) {
            let terminal: Vec<_> = observed
                .iter()
                .filter(|state| is_terminal(**state))
                .copied()
                .collect();
            assert_eq!(
                terminal,
                vec![expected],
                "a task must publish exactly one terminal state: {observed:?}"
            );
            assert_eq!(
                observed.last().copied(),
                Some(expected),
                "the last published state must be the task's terminal state: {observed:?}"
            );
            return observed;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the task never published a terminal state: {observed:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn spawn_body_server(path: &'static str, body: Vec<u8>) -> (String, tokio::task::JoinHandle<()>) {
    let route = warp::path(path).map(move || {
        warp::http::Response::builder()
            .header("content-length", body.len().to_string())
            .body(body.clone())
            .unwrap()
    });
    let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
    let handle = tokio::spawn(server);
    (format!("http://{addr}/{path}"), handle)
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

// ──────────────────────────────────────────────────────────────
//  B1: verification failure must not report Completed
// ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn checksum_mismatch_ends_failed_without_a_premature_completed() {
    let body: Vec<u8> = (0..4096u32).map(|index| (index % 251) as u8).collect();
    let (url, server) = spawn_body_server("checksum", body.clone());
    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("checksum.bin");

    let downloader = Downloader::builder().build().unwrap();
    let spec = DownloadSpec::new(url)
        .output_path(output_path.clone())
        .file_allocation(FileAllocation::None)
        .resume(false)
        .checksum(Checksum::Sha256("00".repeat(32)));

    let handle = downloader.download(spec);
    let states = collect_states(&handle);
    let progress = handle.subscribe_progress();
    let result = handle.wait().await;

    assert!(
        matches!(result, Err(DownloadError::ChecksumMismatch { .. })),
        "got {result:?}"
    );
    assert_eq!(
        progress.borrow().state,
        DownloadState::Failed,
        "a failed verification must leave the final state Failed"
    );
    assert_eq!(progress.borrow().downloaded, body.len() as u64);
    // The bytes were transferred correctly; only the outcome differs.
    assert_eq!(std::fs::read(&output_path).unwrap(), body);
    assert_single_terminal_state(&states, DownloadState::Failed).await;

    server.abort();
}

#[tokio::test]
async fn matching_checksum_publishes_completed_once() {
    let body: Vec<u8> = (0..8192u32).map(|index| (index % 251) as u8).collect();
    let (url, server) = spawn_body_server("checksum-ok", body.clone());
    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("checksum-ok.bin");

    let downloader = Downloader::builder().build().unwrap();
    let spec = DownloadSpec::new(url)
        .output_path(output_path.clone())
        .file_allocation(FileAllocation::None)
        .resume(false)
        .checksum(Checksum::Sha256(sha256_hex(&body)));

    let handle = downloader.download(spec);
    let states = collect_states(&handle);
    let progress = handle.subscribe_progress();
    handle.wait().await.unwrap();

    assert_eq!(progress.borrow().state, DownloadState::Completed);
    assert_eq!(progress.borrow().downloaded, body.len() as u64);
    assert_eq!(progress.borrow().eta_secs, Some(0.0));
    assert_single_terminal_state(&states, DownloadState::Completed).await;

    server.abort();
}

// ──────────────────────────────────────────────────────────────
//  B2: a rejected configuration must end Failed, not stay Pending
// ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn invalid_config_ends_failed_and_creates_no_files() {
    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("invalid.bin");
    let downloader = Downloader::builder().build().unwrap();

    for spec in [
        DownloadSpec::new("")
            .output_path(output_path.clone())
            .resume(true),
        DownloadSpec::new("http://127.0.0.1:1/nonexistent")
            .output_path(output_path.clone())
            .resume(true)
            .max_connections(0),
    ] {
        let handle = downloader.download(spec);
        let states = collect_states(&handle);
        let progress = handle.subscribe_progress();
        let result = handle.wait().await;

        assert!(
            matches!(result, Err(DownloadError::InvalidConfig(_))),
            "got {result:?}"
        );
        assert_eq!(progress.borrow().state, DownloadState::Failed);
        assert_eq!(progress.borrow().downloaded, 0);
        assert_single_terminal_state(&states, DownloadState::Failed).await;

        assert!(!output_path.exists(), "a rejected config must not write");
        assert!(
            !dir.path().join("invalid.bin.bytehaul").exists(),
            "a task that never started must not create a checkpoint"
        );
    }
}

// ──────────────────────────────────────────────────────────────
//  B3: a queued task must stop on its own, without a released permit
// ──────────────────────────────────────────────────────────────

/// A server that sends the first body chunk immediately and withholds the rest
/// until the test releases it, so the first download keeps its permit while the
/// second one is queued.
fn spawn_held_body_server(
    release: Arc<tokio::sync::Notify>,
) -> (String, tokio::task::JoinHandle<()>) {
    let route = warp::path("held").map(move || {
        let release = release.clone();
        let stream = futures::stream::once(async move { Ok::<_, Infallible>(vec![0x5Au8; 1024]) })
            .chain(futures::stream::once(async move {
                release.notified().await;
                Ok::<_, Infallible>(vec![0x5Au8; 3072])
            }));
        warp::http::Response::builder()
            .header("content-length", "4096")
            .body(warp::hyper::Body::wrap_stream(stream))
            .unwrap()
    });
    let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
    let handle = tokio::spawn(server);
    (format!("http://{addr}/held"), handle)
}

/// Wait until the permit holder is actually transferring.
async fn wait_until_running(handle: &DownloadHandle) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if handle.progress().state == DownloadState::Downloading {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the first download never started: {:?}",
            handle.progress().state
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn queued_task_stops_while_the_first_download_keeps_its_permit() {
    for stop in ["cancel", "pause"] {
        let release_body = Arc::new(tokio::sync::Notify::new());
        let (url, server) = spawn_held_body_server(release_body.clone());
        let dir = tempfile::tempdir().unwrap();

        let downloader = Downloader::builder()
            .max_concurrent_downloads(1)
            .build()
            .unwrap();

        // First task: the permit holder. Its body is deliberately withheld, so
        // it keeps running while the second task is queued behind it.
        let holder_path = dir.path().join("holder.bin");
        let holder = downloader.download(
            DownloadSpec::new(url.clone())
                .output_path(holder_path.clone())
                .file_allocation(FileAllocation::None)
                .read_timeout(Duration::from_secs(30))
                .resume(false),
        );
        wait_until_running(&holder).await;

        // Second task: queued, never started.
        let queued_path = dir.path().join("queued.bin");
        let queued = downloader.download(
            DownloadSpec::new(url)
                .output_path(queued_path.clone())
                .resume(true),
        );
        let queued_states = collect_states(&queued);
        let queued_progress = queued.subscribe_progress();
        assert_eq!(
            queued.progress().state,
            DownloadState::Pending,
            "the second task must be waiting for the permit"
        );

        let expected = match stop {
            "cancel" => {
                queued.cancel();
                DownloadState::Cancelled
            }
            _ => {
                queued.pause();
                DownloadState::Paused
            }
        };

        // The permit is still held, so ending here proves the stop does not
        // depend on another download releasing it.
        let result = tokio::time::timeout(Duration::from_secs(5), queued.wait())
            .await
            .expect("a queued stop must not wait for another download to release a permit")
            .unwrap_err();
        assert_eq!(expected_state(&Err(result)), expected);
        assert_eq!(
            holder.progress().state,
            DownloadState::Downloading,
            "the permit holder must still be running"
        );
        assert!(holder_path.exists(), "the permit holder must keep running");
        assert_eq!(queued_progress.borrow().state, expected);
        assert_single_terminal_state(&queued_states, expected).await;

        assert!(
            !queued_path.exists(),
            "a stopped queued task must not create its output file"
        );
        assert!(
            !dir.path().join("queued.bin.bytehaul").exists(),
            "a stopped queued task must not create a checkpoint"
        );

        // Release the holder so the test can finish cleanly.
        holder.cancel();
        assert!(matches!(holder.wait().await, Err(DownloadError::Cancelled)));
        release_body.notify_one();
        server.abort();
    }
}

// ──────────────────────────────────────────────────────────────
//  B4: a zero concurrency limit is rejected at build time
// ──────────────────────────────────────────────────────────────

#[test]
fn zero_concurrency_limit_is_rejected_when_building() {
    let error = match Downloader::builder().max_concurrent_downloads(0).build() {
        Ok(_) => panic!("a zero concurrency limit must be rejected"),
        Err(error) => error,
    };
    assert!(
        matches!(error, DownloadError::InvalidConfig(ref message) if message.contains("max_concurrent_downloads")),
        "got {error:?}"
    );
}

// ──────────────────────────────────────────────────────────────
//  Result/state consistency under stop races, writer failure and handle drops
// ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn stopping_during_completion_keeps_result_and_state_consistent() {
    let body: Vec<u8> = (0..2048u32).map(|index| (index % 251) as u8).collect();
    let (url, server) = spawn_body_server("race", body);

    for round in 0..25 {
        let dir = tempfile::tempdir().unwrap();
        let downloader = Downloader::builder().build().unwrap();
        let handle = downloader.download(
            DownloadSpec::new(url.clone())
                .output_path(dir.path().join("race.bin"))
                .file_allocation(FileAllocation::None)
                .resume(false),
        );
        let states = collect_states(&handle);
        let progress = handle.subscribe_progress();
        // Cancel immediately: the stop and the completion race, and whichever
        // wins, the returned result and the published state must agree.
        handle.cancel();
        let result = handle.wait().await;

        let expected = expected_state(&result);
        assert_eq!(
            progress.borrow().state,
            expected,
            "round {round}: wait() returned {result:?} but the snapshot is {:?}",
            progress.borrow().state
        );
        assert_single_terminal_state(&states, expected).await;
    }

    server.abort();
}

#[tokio::test]
async fn writer_failure_ends_failed_and_creates_no_checkpoint() {
    let route = warp::path("writer").map(|| warp::http::Response::new(vec![0x11u8; 1024]));
    let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
    let server = tokio::spawn(server);

    let dir = tempfile::tempdir().unwrap();
    // An existing directory cannot be opened as the output file, so the writer
    // fails before any byte is written.
    let output_path = dir.path().join("locked.bin");
    std::fs::create_dir(&output_path).unwrap();
    let control_path = dir.path().join("locked.bin.bytehaul");
    let downloader = Downloader::builder().build().unwrap();
    let handle = downloader.download(
        DownloadSpec::new(format!("http://{addr}/writer"))
            .output_path(output_path)
            .resume(true),
    );
    let states = collect_states(&handle);
    let progress = handle.subscribe_progress();
    let result = handle.wait().await;

    assert!(result.is_err(), "a writer failure must not report success");
    assert_eq!(progress.borrow().state, DownloadState::Failed);
    assert_single_terminal_state(&states, DownloadState::Failed).await;
    assert!(
        !control_path.exists(),
        "a failed writer must not leave a checkpoint behind"
    );

    server.abort();
}

#[tokio::test]
async fn dropped_handle_neither_cancels_nor_wedges_a_running_download() {
    // Dropping a handle removes the last stop-signal sender. That must not
    // cancel the download, and the stop branch must not stay ready forever on
    // the closed channel, which would starve the body and stall the transfer.
    let body: Vec<u8> = (0..64 * 1024u32).map(|index| (index % 251) as u8).collect();
    let (url, server) = spawn_body_server("dropped", body.clone());
    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("dropped.bin");

    let downloader = Downloader::builder().build().unwrap();
    let handle = downloader.download(
        DownloadSpec::new(url)
            .output_path(output_path.clone())
            .file_allocation(FileAllocation::None)
            .resume(false),
    );
    let mut progress = handle.subscribe_progress();
    drop(handle);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if progress.borrow().state == DownloadState::Completed {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the download must proceed after its handle is dropped, saw {:?}",
            progress.borrow().state
        );
        progress.changed().await.unwrap();
    }

    assert_eq!(std::fs::read(&output_path).unwrap(), body);
    server.abort();
}

/// A multi-connection fixture: the same object served as a full body or as a
/// byte range, so the probe sees `206` plus `content-range` like a real server.
fn spawn_range_server(path: &'static str, data: Vec<u8>) -> (String, tokio::task::JoinHandle<()>) {
    let data = Arc::new(data);
    let route = warp::path(path)
        .and(warp::header::optional::<String>("range"))
        .map(move |range: Option<String>| {
            let total = data.len();
            let (start, end) = match range.as_deref() {
                Some(value) => {
                    let (start, end) = value
                        .strip_prefix("bytes=")
                        .unwrap()
                        .split_once('-')
                        .unwrap();
                    (
                        start.parse::<usize>().unwrap(),
                        end.parse::<usize>().unwrap().min(total - 1),
                    )
                }
                None => (0, total - 1),
            };
            let payload = data[start..=end].to_vec();
            let mut response = warp::http::Response::builder()
                .status(if range.is_some() { 206 } else { 200 })
                .header("content-length", end - start + 1)
                .header("etag", "\"lifecycle\"")
                .header("accept-ranges", "bytes");
            if range.is_some() {
                response = response.header("content-range", format!("bytes {start}-{end}/{total}"));
            }
            response
                .body(warp::hyper::Body::wrap_stream(futures::stream::once(
                    async move { Ok::<_, Infallible>(payload) },
                )))
                .unwrap()
        });
    let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
    let handle = tokio::spawn(server);
    (format!("http://{addr}/{path}"), handle)
}

#[tokio::test]
async fn multi_connection_download_publishes_one_completed_state() {
    let size = 6 * 1024 * 1024;
    let body: Vec<u8> = (0..size).map(|index| (index % 251) as u8).collect();
    let expected = body.clone();
    let (url, server) = spawn_range_server("multi", body);

    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("multi.bin");
    let downloader = Downloader::builder().build().unwrap();
    let handle = downloader.download(
        DownloadSpec::new(url)
            .output_path(output_path.clone())
            .file_allocation(FileAllocation::None)
            .resume(false)
            .max_connections(4)
            .min_split_size(1),
    );
    let states = collect_states(&handle);
    let progress = handle.subscribe_progress();
    handle.wait().await.unwrap();

    assert_eq!(progress.borrow().state, DownloadState::Completed);
    assert_eq!(progress.borrow().downloaded, size as u64);
    assert_eq!(std::fs::read(&output_path).unwrap(), expected);
    assert_single_terminal_state(&states, DownloadState::Completed).await;

    server.abort();
}
