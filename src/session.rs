use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::stream::FuturesUnordered;
use futures::StreamExt;
use tokio::sync::{mpsc, watch};

use crate::config::DownloadSpec;
use crate::error::DownloadError;
use crate::http::request::build_range_request;
use crate::http::response::ResponseMeta;
use crate::http::worker::HttpWorker;
use crate::progress::{DownloadState, ProgressSnapshot};
use crate::scheduler::{Scheduler, SchedulerState};
use crate::storage::control::ControlSnapshot;
use crate::storage::file::{create_output_file, open_existing_file};
use crate::storage::piece_map::PieceMap;
use crate::storage::segment::Segment;
use crate::storage::writer::{WriteCommand, WriterTask};

const CONTROL_SAVE_INTERVAL: Duration = Duration::from_secs(5);

// ──────────────────────────────────────────────────────────────
//  Entry point
// ──────────────────────────────────────────────────────────────

pub(crate) async fn run_download(
    client: reqwest::Client,
    spec: DownloadSpec,
    progress_tx: watch::Sender<ProgressSnapshot>,
    cancel_rx: watch::Receiver<bool>,
) -> Result<(), DownloadError> {
    let control_path = ControlSnapshot::control_path(&spec.output_path);
    let worker = HttpWorker::new(client.clone(), &spec);

    // ── Phase 1: attempt resume ──────────────────────────────
    let resume_ctrl = if spec.resume {
        ControlSnapshot::load(&control_path).await.ok()
    } else {
        None
    };

    if let Some(ctrl) = resume_ctrl {
        let is_multi = ctrl.piece_count > 1 && spec.max_connections > 1;

        if is_multi {
            let piece_map = PieceMap::from_bitset(
                ctrl.total_size,
                ctrl.piece_size,
                &ctrl.completed_bitset,
                ctrl.piece_count,
            );
            if let Some(probe_piece) = piece_map.next_missing_excluding(&HashSet::new()) {
                let (start, end) = piece_map.piece_range(probe_piece);
                if let Ok((resp, meta)) = worker.send_range(start, end - 1).await {
                    if resp.status().as_u16() == 206 && validate_metadata(&meta, &ctrl) {
                        return run_multi_worker(
                            client,
                            &spec,
                            &meta,
                            ctrl.total_size,
                            piece_map,
                            Some((resp, probe_piece)),
                            &progress_tx,
                            cancel_rx,
                            &control_path,
                        )
                        .await;
                    }
                }
            }
        } else if ctrl.downloaded_bytes > 0 && ctrl.downloaded_bytes < ctrl.total_size {
            if let Ok((resp, meta)) =
                worker.send_range(ctrl.downloaded_bytes, ctrl.total_size - 1).await
            {
                if resp.status().as_u16() == 206 && validate_metadata(&meta, &ctrl) {
                    return run_single_connection(
                        resp,
                        &meta,
                        &spec,
                        ctrl.downloaded_bytes,
                        &progress_tx,
                        cancel_rx,
                        &control_path,
                        Some(ctrl.total_size),
                    )
                    .await;
                }
            }
        }
        // Resume failed — discard
        let _ = ControlSnapshot::delete(&control_path).await;
    }

    // ── Phase 2: fresh download ──────────────────────────────
    if spec.max_connections > 1 {
        let piece_end = spec.piece_size.saturating_sub(1);
        if let Ok((resp, meta)) = worker.send_range(0, piece_end).await {
            if resp.status().as_u16() == 206 {
                if let Some(total_size) = meta.content_range_total {
                    if total_size > spec.min_split_size {
                        let piece_map = PieceMap::new(total_size, spec.piece_size);
                        return run_multi_worker(
                            client,
                            &spec,
                            &meta,
                            total_size,
                            piece_map,
                            Some((resp, 0)),
                            &progress_tx,
                            cancel_rx,
                            &control_path,
                        )
                        .await;
                    }
                }
                // File too small for splitting — fall through to GET
                drop(resp);
            } else {
                // Got 200 — Range not supported; stream this response
                return run_single_connection(
                    resp,
                    &meta,
                    &spec,
                    0,
                    &progress_tx,
                    cancel_rx,
                    &control_path,
                    meta.content_length,
                )
                .await;
            }
        }
    }

    // ── Fallback: plain GET ──────────────────────────────────
    let (resp, meta) = worker.send_get().await?;
    run_single_connection(
        resp,
        &meta,
        &spec,
        0,
        &progress_tx,
        cancel_rx,
        &control_path,
        meta.content_length,
    )
    .await
}

// ──────────────────────────────────────────────────────────────
//  Single-connection download  (M1/M2 path)
// ──────────────────────────────────────────────────────────────

async fn run_single_connection(
    response: reqwest::Response,
    meta: &ResponseMeta,
    spec: &DownloadSpec,
    start_offset: u64,
    progress_tx: &watch::Sender<ProgressSnapshot>,
    cancel_rx: watch::Receiver<bool>,
    control_path: &Path,
    total_size: Option<u64>,
) -> Result<(), DownloadError> {
    let use_control = spec.resume && total_size.is_some();

    let file = if start_offset > 0 {
        open_existing_file(&spec.output_path).await?
    } else {
        create_output_file(&spec.output_path, total_size, spec.file_allocation).await?
    };

    let (write_tx, write_rx) = mpsc::channel::<WriteCommand>(spec.channel_buffer);
    let written_bytes = Arc::new(AtomicU64::new(start_offset));
    let writer_handle =
        tokio::spawn(WriterTask::new(write_rx, file, written_bytes.clone()).run());

    let ts = total_size.unwrap_or(0);
    let snap_template = ControlSnapshot {
        url: spec.url.clone(),
        total_size: ts,
        piece_size: ts,
        piece_count: 1,
        completed_bitset: vec![0],
        downloaded_bytes: start_offset,
        etag: meta.etag.clone(),
        last_modified: meta.last_modified.clone(),
    };

    let stream_result = stream_single(
        response,
        &write_tx,
        progress_tx,
        cancel_rx,
        total_size,
        start_offset,
        if use_control {
            Some((&written_bytes, control_path, &snap_template))
        } else {
            None
        },
    )
    .await;

    drop(write_tx);
    let writer_result = writer_handle
        .await
        .map_err(|e| DownloadError::Other(format!("writer panicked: {e}")))?;

    if let Err(ref e) = stream_result {
        if use_control {
            let w = written_bytes.load(Ordering::Acquire);
            let mut s = snap_template.clone();
            s.downloaded_bytes = w;
            let _ = s.save(control_path).await;
        }
        if !matches!(e, DownloadError::Cancelled) {
            progress_tx.send_modify(|p| p.state = DownloadState::Failed);
        }
        return stream_result;
    }
    writer_result?;

    if use_control {
        let _ = ControlSnapshot::delete(control_path).await;
    }
    progress_tx.send_modify(|p| p.state = DownloadState::Completed);
    Ok(())
}

/// Stream a single HTTP response body to the writer channel.
async fn stream_single(
    response: reqwest::Response,
    write_tx: &mpsc::Sender<WriteCommand>,
    progress_tx: &watch::Sender<ProgressSnapshot>,
    cancel_rx: watch::Receiver<bool>,
    total_size: Option<u64>,
    start_offset: u64,
    control: Option<(&Arc<AtomicU64>, &Path, &ControlSnapshot)>,
) -> Result<(), DownloadError> {
    let mut stream = response.bytes_stream();
    let mut downloaded: u64 = start_offset;
    let start_time = Instant::now();
    let mut cancel_rx = cancel_rx;
    let mut save_ticker = tokio::time::interval(CONTROL_SAVE_INTERVAL);
    save_ticker.tick().await;

    progress_tx.send_modify(|p| {
        p.total_size = total_size;
        p.downloaded = start_offset;
        p.state = DownloadState::Downloading;
        p.start_time = Some(start_time);
    });

    loop {
        tokio::select! {
            biased;

            result = cancel_rx.changed() => {
                if result.is_ok() && *cancel_rx.borrow_and_update() {
                    progress_tx.send_modify(|p| p.state = DownloadState::Cancelled);
                    return Err(DownloadError::Cancelled);
                }
            }

            _ = save_ticker.tick(), if control.is_some() => {
                if let Some((wb, cp, tmpl)) = &control {
                    let w = wb.load(Ordering::Acquire);
                    let mut s = (*tmpl).clone();
                    s.downloaded_bytes = w;
                    let _ = s.save(cp).await;
                }
            }

            chunk = stream.next() => {
                match chunk {
                    Some(Ok(data)) => {
                        let offset = downloaded;
                        downloaded += data.len() as u64;
                        if write_tx.send(WriteCommand { offset, data }).await.is_err() {
                            return Err(DownloadError::ChannelClosed);
                        }
                        let elapsed = start_time.elapsed().as_secs_f64();
                        let speed = if elapsed > 0.0 { downloaded as f64 / elapsed } else { 0.0 };
                        progress_tx.send_modify(|p| {
                            p.downloaded = downloaded;
                            p.speed_bytes_per_sec = speed;
                        });
                    }
                    Some(Err(e)) => {
                        progress_tx.send_modify(|p| p.state = DownloadState::Failed);
                        return Err(DownloadError::Http(e));
                    }
                    None => break,
                }
            }
        }
    }
    Ok(())
}

// ──────────────────────────────────────────────────────────────
//  Multi-worker download  (M3 path)
// ──────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn run_multi_worker(
    client: reqwest::Client,
    spec: &DownloadSpec,
    meta: &ResponseMeta,
    total_size: u64,
    piece_map: PieceMap,
    probe_response: Option<(reqwest::Response, usize)>,
    progress_tx: &watch::Sender<ProgressSnapshot>,
    cancel_rx: watch::Receiver<bool>,
    control_path: &Path,
) -> Result<(), DownloadError> {
    // File
    let file = if piece_map.completed_count() > 0 {
        open_existing_file(&spec.output_path).await?
    } else {
        create_output_file(&spec.output_path, Some(total_size), spec.file_allocation).await?
    };

    // Writer
    let (write_tx, write_rx) = mpsc::channel::<WriteCommand>(spec.channel_buffer);
    let written_bytes = Arc::new(AtomicU64::new(0));
    let writer_handle =
        tokio::spawn(WriterTask::new(write_rx, file, written_bytes.clone()).run());

    // Scheduler
    let initial_downloaded = piece_map.completed_bytes();
    let scheduler: Scheduler = Arc::new(parking_lot::Mutex::new(SchedulerState::new(piece_map)));

    // Shared progress counter
    let downloaded = Arc::new(AtomicU64::new(initial_downloaded));
    let start_time = Instant::now();

    progress_tx.send_modify(|p| {
        p.total_size = Some(total_size);
        p.downloaded = initial_downloaded;
        p.state = DownloadState::Downloading;
        p.start_time = Some(start_time);
    });

    // Spawn workers
    let remaining = scheduler.lock().remaining_count();
    let num_workers = (spec.max_connections as usize).min(remaining).max(1);

    let mut abort_handles = Vec::with_capacity(num_workers);
    let mut workers = FuturesUnordered::new();
    let mut probe_response = probe_response;

    for worker_id in 0..num_workers {
        let first = if worker_id == 0 {
            probe_response.take()
        } else {
            None
        };
        let handle = tokio::spawn(worker_loop(
            worker_id,
            client.clone(),
            spec.url.clone(),
            spec.headers.clone(),
            spec.read_timeout,
            scheduler.clone(),
            write_tx.clone(),
            downloaded.clone(),
            cancel_rx.clone(),
            first,
        ));
        abort_handles.push(handle.abort_handle());
        workers.push(handle);
    }

    // Drop our sender so writer can finish once workers are done
    drop(write_tx);

    // ── Monitor loop ──
    let mut cancel_rx = cancel_rx;
    let mut save_ticker = tokio::time::interval(CONTROL_SAVE_INTERVAL);
    save_ticker.tick().await;
    let mut progress_interval = tokio::time::interval(Duration::from_millis(200));
    progress_interval.tick().await;

    let mut download_error: Option<DownloadError> = None;

    loop {
        tokio::select! {
            biased;

            result = cancel_rx.changed() => {
                if result.is_ok() && *cancel_rx.borrow_and_update() {
                    for ah in &abort_handles { ah.abort(); }
                    progress_tx.send_modify(|p| p.state = DownloadState::Cancelled);
                    download_error = Some(DownloadError::Cancelled);
                    break;
                }
            }

            _ = save_ticker.tick(), if spec.resume => {
                save_multi_control(spec, meta, &scheduler, total_size, control_path).await;
            }

            _ = progress_interval.tick() => {
                let d = downloaded.load(Ordering::Relaxed);
                let elapsed = start_time.elapsed().as_secs_f64();
                let speed = if elapsed > 0.0 { d as f64 / elapsed } else { 0.0 };
                progress_tx.send_modify(|p| {
                    p.downloaded = d;
                    p.speed_bytes_per_sec = speed;
                });
            }

            result = workers.next() => {
                match result {
                    Some(Ok(Ok(()))) => {
                        if workers.is_empty() { break; }
                    }
                    Some(Ok(Err(e))) => {
                        if download_error.is_none() && !matches!(e, DownloadError::ChannelClosed) {
                            download_error = Some(e);
                        }
                        if workers.is_empty() { break; }
                    }
                    Some(Err(join_err)) => {
                        if !join_err.is_cancelled() {
                            download_error = Some(DownloadError::Other(
                                format!("worker panicked: {join_err}"),
                            ));
                        }
                        if workers.is_empty() { break; }
                    }
                    None => break,
                }
            }
        }
    }

    // Drain remaining workers on error
    if download_error.is_some() {
        for ah in &abort_handles {
            ah.abort();
        }
        while workers.next().await.is_some() {}
    }

    // Wait for writer
    let writer_result = writer_handle
        .await
        .map_err(|e| DownloadError::Other(format!("writer panicked: {e}")))?;

    if let Some(e) = download_error {
        if spec.resume {
            save_multi_control(spec, meta, &scheduler, total_size, control_path).await;
        }
        return Err(e);
    }
    writer_result?;

    if !scheduler.lock().all_done() {
        if spec.resume {
            save_multi_control(spec, meta, &scheduler, total_size, control_path).await;
        }
        return Err(DownloadError::Other("download incomplete".into()));
    }

    let _ = ControlSnapshot::delete(control_path).await;
    // Final progress update
    let d = downloaded.load(Ordering::Relaxed);
    let elapsed = start_time.elapsed().as_secs_f64();
    let speed = if elapsed > 0.0 { d as f64 / elapsed } else { 0.0 };
    progress_tx.send_modify(|p| {
        p.downloaded = d;
        p.speed_bytes_per_sec = speed;
        p.state = DownloadState::Completed;
    });
    Ok(())
}

async fn save_multi_control(
    spec: &DownloadSpec,
    meta: &ResponseMeta,
    scheduler: &Scheduler,
    total_size: u64,
    control_path: &Path,
) {
    let snap = {
        let sched = scheduler.lock();
        ControlSnapshot {
            url: spec.url.clone(),
            total_size,
            piece_size: sched.piece_size(),
            piece_count: sched.piece_count(),
            completed_bitset: sched.snapshot_bitset(),
            downloaded_bytes: sched.completed_bytes(),
            etag: meta.etag.clone(),
            last_modified: meta.last_modified.clone(),
        }
    };
    let _ = snap.save(control_path).await;
}

// ──────────────────────────────────────────────────────────────
//  Worker loop
// ──────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn worker_loop(
    _worker_id: usize,
    client: reqwest::Client,
    url: String,
    headers: std::collections::HashMap<String, String>,
    timeout: Duration,
    scheduler: Scheduler,
    write_tx: mpsc::Sender<WriteCommand>,
    downloaded: Arc<AtomicU64>,
    cancel_rx: watch::Receiver<bool>,
    first_response: Option<(reqwest::Response, usize)>,
) -> Result<(), DownloadError> {
    let mut cancel_rx = cancel_rx;
    let mut first_response = first_response;

    loop {
        if *cancel_rx.borrow() {
            return Err(DownloadError::Cancelled);
        }

        let segment = match scheduler.lock().assign() {
            Some(seg) => seg,
            None => return Ok(()),
        };

        let result = if let Some((resp, pid)) = first_response.take() {
            if pid == segment.piece_id {
                stream_segment(resp, &segment, &write_tx, &downloaded, &mut cancel_rx).await
            } else {
                download_segment(
                    &client, &url, &headers, timeout, &segment, &write_tx, &downloaded,
                    &mut cancel_rx,
                )
                .await
            }
        } else {
            download_segment(
                &client, &url, &headers, timeout, &segment, &write_tx, &downloaded,
                &mut cancel_rx,
            )
            .await
        };

        match result {
            Ok(()) => {
                scheduler.lock().complete(segment.piece_id);
            }
            Err(e) => {
                scheduler.lock().reclaim(segment.piece_id);
                return Err(e);
            }
        }
    }
}

/// Download a segment by sending a fresh Range request.
#[allow(clippy::too_many_arguments)]
async fn download_segment(
    client: &reqwest::Client,
    url: &str,
    headers: &std::collections::HashMap<String, String>,
    timeout: Duration,
    segment: &Segment,
    write_tx: &mpsc::Sender<WriteCommand>,
    downloaded: &Arc<AtomicU64>,
    cancel_rx: &mut watch::Receiver<bool>,
) -> Result<(), DownloadError> {
    let req = build_range_request(client, url, headers, timeout, segment.start, segment.end - 1);
    let response = req.send().await?;

    let status = response.status().as_u16();
    if status != 206 {
        return Err(DownloadError::HttpStatus {
            status,
            message: format!("expected 206, got {status}"),
        });
    }

    stream_segment(response, segment, write_tx, downloaded, cancel_rx).await
}

/// Stream an already-opened response into the writer channel.
async fn stream_segment(
    response: reqwest::Response,
    segment: &Segment,
    write_tx: &mpsc::Sender<WriteCommand>,
    downloaded: &Arc<AtomicU64>,
    cancel_rx: &mut watch::Receiver<bool>,
) -> Result<(), DownloadError> {
    let mut stream = response.bytes_stream();
    let mut offset = segment.start;

    loop {
        tokio::select! {
            biased;

            result = cancel_rx.changed() => {
                if result.is_ok() && *cancel_rx.borrow_and_update() {
                    return Err(DownloadError::Cancelled);
                }
            }

            chunk = stream.next() => {
                match chunk {
                    Some(Ok(data)) => {
                        let len = data.len() as u64;
                        if write_tx.send(WriteCommand { offset, data }).await.is_err() {
                            return Err(DownloadError::ChannelClosed);
                        }
                        offset += len;
                        downloaded.fetch_add(len, Ordering::Relaxed);
                    }
                    Some(Err(e)) => return Err(DownloadError::Http(e)),
                    None => break,
                }
            }
        }
    }
    Ok(())
}

// ──────────────────────────────────────────────────────────────
//  Helpers
// ──────────────────────────────────────────────────────────────

fn validate_metadata(meta: &ResponseMeta, ctrl: &ControlSnapshot) -> bool {
    if let (Some(expected), Some(actual)) = (&ctrl.etag, &meta.etag) {
        if expected != actual {
            return false;
        }
    }
    if let (Some(expected), Some(actual)) = (&ctrl.last_modified, &meta.last_modified) {
        if expected != actual {
            return false;
        }
    }
    true
}
