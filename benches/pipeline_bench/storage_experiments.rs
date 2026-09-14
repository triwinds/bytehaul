//! P4 candidates, isolated from the production writer. These intentionally do
//! not implement leases or cancellation, and cannot alone justify a backend swap.
use super::{collect, millis, Config, Round, Scenario};
use bytes::{Bytes, BytesMut};
use std::io::{IoSlice, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

const SIZE: usize = 64 * 1024 * 1024;
const CHUNK: usize = 16 * 1024;
const BATCH: usize = 256 * 1024;

pub(super) fn names() -> Vec<String> {
    let mut names: Vec<String> = [
        "io/tokio_seek_16KiB",
        "io/tokio_sequential_16KiB",
        "io/tokio_batch_256KiB",
        "io/blocking_batch_256KiB",
        "cache/copy_then_write",
        "cache/retain_then_vectored_write",
        "allocation/grow",
        "allocation/zero_fill",
        "allocation/logical_length",
    ]
    .into_iter()
    .map(|s| format!("storage/{s}"))
    .collect();
    if cfg!(windows) {
        names.push("storage/allocation/windows_reserve".into());
    }
    names
}

pub(super) async fn run(config: &Config, dir: &Path) -> Vec<Scenario> {
    let mut results = Vec::new();
    if !names().iter().any(|name| config.selected(name)) {
        return results;
    }
    // Nonconstant bytes make offset errors observable. Input allocation is
    // outside measured rounds, exactly the same for every candidate.
    let source: Arc<Vec<u8>> = Arc::new((0..SIZE).map(|i| (i % 251) as u8).collect());
    for name in names() {
        let mode = name.strip_prefix("storage/").unwrap().to_owned();
        let source = source.clone();
        let path = dir.join(name.replace('/', "_"));
        results.push(
            collect(
                config,
                "storage",
                &name,
                "64 MiB file, including final sync",
                vec![
                    ("bytes", SIZE.to_string()),
                    ("chunk_bytes", CHUNK.to_string()),
                ],
                move |_| {
                    let source = source.clone();
                    let path = path.clone();
                    let mode = mode.clone();
                    async move {
                        let started = Instant::now();
                        let result = candidate(&mode, &path, source.clone()).await;
                        let elapsed = millis(started.elapsed());
                        match result {
                            Ok(round) => {
                                // Verification and cleanup are excluded from total,
                                // but included in the harness's round CPU and millis.
                                assert_eq!(std::fs::read(&path).unwrap(), *source);
                                std::fs::remove_file(&path).unwrap();
                                round.metric("total", elapsed).metric("verified", 1.0)
                            }
                            Err(error) => {
                                let _ = std::fs::remove_file(&path);
                                Round::new().metric("failed", 1.0).note(error.to_string())
                            }
                        }
                    }
                },
            )
            .await,
        );
    }
    results
}

async fn candidate(mode: &str, path: &Path, source: Arc<Vec<u8>>) -> std::io::Result<Round> {
    if mode.starts_with("allocation/") || mode.starts_with("cache/") {
        let mode = mode.to_owned();
        let path = path.to_owned();
        return tokio::task::spawn_blocking(move || sync_candidate(&mode, &path, &source))
            .await
            .map_err(std::io::Error::other)?;
    }
    let batch = if mode.ends_with("16KiB") {
        CHUNK
    } else {
        BATCH
    };
    let mut writes = 0;
    let mut seeks = 0;
    if mode == "io/blocking_batch_256KiB" {
        let mut file = std::fs::File::create(path)?;
        for offset in (0..SIZE).step_by(batch) {
            let source = source.clone();
            // One bounded pool job per batch, not a thread per download.
            file = tokio::task::spawn_blocking(move || {
                file.write_all(&source[offset..offset + batch])?;
                Ok::<_, std::io::Error>(file)
            })
            .await
            .map_err(std::io::Error::other)??;
            writes += 1;
        }
        tokio::task::spawn_blocking(move || file.sync_all())
            .await
            .map_err(std::io::Error::other)??;
    } else {
        let mut file = tokio::fs::File::create(path).await?;
        for (index, chunk) in source.chunks(batch).enumerate() {
            if mode == "io/tokio_seek_16KiB" {
                file.seek(SeekFrom::Start((index * batch) as u64)).await?;
                seeks += 1;
            }
            file.write_all(chunk).await?;
            writes += 1;
        }
        file.flush().await?;
        file.sync_all().await?;
    }
    Ok(Round::new()
        .metric("write_calls", writes as f64)
        .metric("seek_calls", seeks as f64))
}

fn sync_candidate(mode: &str, path: &Path, source: &[u8]) -> std::io::Result<Round> {
    let mut file = std::fs::File::create(path)?;
    let mut round = Round::new();
    if mode.starts_with("allocation/") {
        let started = Instant::now();
        match mode {
            "allocation/grow" => (),
            "allocation/zero_fill" => {
                let zeros = vec![0u8; BATCH];
                for _ in 0..SIZE / BATCH {
                    file.write_all(&zeros)?;
                }
                file.sync_all()?;
                file.rewind()?;
            }
            // A logical-length control, NOT a disk-space reservation claim.
            "allocation/logical_length" => {
                file.set_len(SIZE as u64)?;
                file.sync_all()?;
            }
            #[cfg(windows)]
            "allocation/windows_reserve" => {
                windows_reserve(&file, SIZE as i64)?;
                round = round.metric("length_after_reserve", file.metadata()?.len() as f64);
                file.set_len(SIZE as u64)?;
                file.sync_all()?;
            }
            _ => unreachable!(),
        }
        round = round.metric("allocation_ms", millis(started.elapsed()));
        for chunk in source.chunks(BATCH) {
            file.write_all(chunk)?;
        }
    } else {
        // Model already-received body chunks. Creating those chunks is common
        // to both candidates, and excluded from the assembly timing only.
        let chunks: Vec<_> = source.chunks(CHUNK).map(Bytes::copy_from_slice).collect();
        let started = Instant::now();
        let mut copied = Vec::new();
        let mut retained = Vec::new();
        for piece in chunks.chunks(1024 * 1024 / CHUNK) {
            if mode == "cache/copy_then_write" {
                let mut buffer = BytesMut::new();
                for chunk in piece {
                    buffer.extend_from_slice(chunk);
                }
                copied.push(buffer.freeze());
            } else {
                retained.push(piece.to_vec());
            }
        }
        // Release transport references in both modes before writing.
        drop(chunks);
        round = round
            .metric("assembly_ms", millis(started.elapsed()))
            .metric(
                "cache_copied_bytes",
                if copied.is_empty() { 0.0 } else { SIZE as f64 },
            );
        let mut writes = 0;
        for block in copied {
            file.write_all(&block)?;
            writes += 1;
        }
        for blocks in retained {
            let mut slices: Vec<_> = blocks.iter().map(|b| IoSlice::new(b)).collect();
            let mut remaining = slices.as_mut_slice();
            while !remaining.is_empty() {
                match file.write_vectored(remaining) {
                    Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
                    Ok(n) => {
                        writes += 1;
                        IoSlice::advance_slices(&mut remaining, n);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                }
            }
        }
        round = round.metric("write_calls", writes as f64);
    }
    file.sync_all()?;
    Ok(round)
}

/// Benchmark-only Windows candidate. FileAllocationInfo reserves space;
/// logical EOF is measured separately and then extended by the caller.
#[cfg(windows)]
fn windows_reserve(file: &std::fs::File, bytes: i64) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    #[link(name = "kernel32")]
    extern "system" {
        fn SetFileInformationByHandle(
            handle: *mut std::ffi::c_void,
            class: i32,
            info: *const std::ffi::c_void,
            size: u32,
        ) -> i32;
    }
    // FILE_ALLOCATION_INFO is a single LARGE_INTEGER (8-byte size/alignment).
    #[repr(C, align(8))]
    struct AllocationInfo {
        size: i64,
    }
    let info = AllocationInfo { size: bytes };
    let ok = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            5,
            (&info as *const AllocationInfo).cast(),
            std::mem::size_of_val(&info) as u32,
        )
    };
    if ok == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}
