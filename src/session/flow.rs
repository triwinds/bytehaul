use std::sync::Arc;

use bytes::Bytes;
use tokio::sync::{mpsc, watch, Semaphore};

use super::{stop_signal_error, StopSignal};
use crate::error::DownloadError;
use crate::rate_limiter::SpeedLimit;
use crate::storage::segment::LeaseKey;
use crate::storage::writer::WriterCommand;

/// Shared geometry for producer chunks and the writer's flush threshold.
/// Below the threshold there is always room for one maximum-sized chunk.
pub(super) struct MemoryBudget {
    pub semaphore: Arc<Semaphore>,
    pub(super) max_chunk: usize,
    pub watermark: usize,
}

impl MemoryBudget {
    pub fn new(bytes: usize) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(bytes)),
            max_chunk: bytes.div_ceil(2).min(u32::MAX as usize),
            watermark: (bytes / 2).max(1),
        }
    }

    /// Account only commands actually enqueued, including a partial body frame
    /// when a stop or writer failure interrupts a later chunk.
    #[allow(clippy::too_many_arguments)]
    pub async fn forward(
        &self,
        mut data: Bytes,
        mut offset: u64,
        lease_key: Option<LeaseKey>,
        write_tx: &mpsc::Sender<WriterCommand>,
        cancel_rx: &mut watch::Receiver<StopSignal>,
        speed_limit: &SpeedLimit,
        mut sent: impl FnMut(u64),
    ) -> Result<(), DownloadError> {
        while !data.is_empty() {
            let len = data.len().min(self.max_chunk);
            let send = async {
                speed_limit.acquire(len).await;
                let permit = self
                    .semaphore
                    .acquire_many(len as u32)
                    .await
                    .map_err(|_| DownloadError::Internal("budget semaphore closed".into()))?;
                // Reserving the channel slot keeps cancellation from losing a
                // command after accounting it or leaking its budget permits.
                let slot = write_tx
                    .reserve()
                    .await
                    .map_err(|_| DownloadError::ChannelClosed)?;
                slot.send(WriterCommand::Data {
                    offset,
                    data: data.split_to(len),
                    lease_key,
                });
                permit.forget();
                Ok::<(), DownloadError>(())
            };
            tokio::select! {
                biased;
                error = wait_for_stop(cancel_rx) => return Err(error),
                _ = write_tx.closed() => return Err(DownloadError::ChannelClosed),
                result = send => result?,
            }
            offset += len as u64;
            sent(len as u64);
        }
        Ok(())
    }
}

pub(super) async fn wait_for_stop(cancel_rx: &mut watch::Receiver<StopSignal>) -> DownloadError {
    loop {
        if let Some(error) = stop_signal_error(*cancel_rx.borrow_and_update()) {
            return error;
        }
        if cancel_rx.changed().await.is_err() {
            // Dropping the handle does not cancel a download.
            return std::future::pending().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::writer::WriterTask;
    use std::sync::atomic::AtomicU64;
    use std::time::Duration;

    #[tokio::test]
    async fn tiny_and_nondivisible_budgets_make_progress_across_leases() {
        for size in [1, 3, 100] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("data");
            let file = tokio::fs::File::create(&path).await.unwrap();
            let budget = Arc::new(MemoryBudget::new(size));
            let (tx, rx) = mpsc::channel(1);
            let writer = tokio::spawn(
                WriterTask::new(
                    rx,
                    file,
                    Arc::new(AtomicU64::new(0)),
                    budget.semaphore.clone(),
                    budget.watermark,
                )
                .run(),
            );
            let (stop_tx, stop_rx) = watch::channel(StopSignal::Running);
            let mut tasks = Vec::new();
            for id in 0..3 {
                let tx = tx.clone();
                let budget = budget.clone();
                let mut stop_rx = stop_rx.clone();
                tasks.push(tokio::spawn(async move {
                    let key = LeaseKey {
                        piece_id: id,
                        lease_id: id as u64,
                    };
                    tx.send(WriterCommand::BeginLease { lease_key: key })
                        .await
                        .unwrap();
                    let mut sent = 0;
                    for frame in 0..2 {
                        budget
                            .forward(
                                Bytes::from(vec![id as u8 + 1; 60]),
                                id as u64 * 120 + frame * 60,
                                Some(key),
                                &tx,
                                &mut stop_rx,
                                &SpeedLimit::Unlimited,
                                |len| sent += len,
                            )
                            .await
                            .unwrap();
                    }
                    assert_eq!(sent, 120);
                }));
            }
            tokio::time::timeout(Duration::from_secs(2), async {
                for task in tasks {
                    task.await.unwrap();
                }
                drop(tx);
                writer.await.unwrap().unwrap();
            })
            .await
            .expect("bounded chunks must allow the writer to flush");
            drop(stop_tx);
            let expected: Vec<_> = (1..=3).flat_map(|b| vec![b; 120]).collect();
            assert_eq!(std::fs::read(path).unwrap(), expected);
            assert_eq!(budget.semaphore.available_permits(), size);
        }
    }

    #[tokio::test]
    async fn budget_wait_stop_preserves_partial_send_accounting() {
        for signal in [StopSignal::Pause, StopSignal::Cancel] {
            let budget = MemoryBudget::new(1);
            let (tx, mut rx) = mpsc::channel(4);
            let (stop_tx, mut stop_rx) = watch::channel(StopSignal::Running);
            let mut sent = 0;
            {
                let forwarding = budget.forward(
                    Bytes::from_static(b"abc"),
                    7,
                    None,
                    &tx,
                    &mut stop_rx,
                    &SpeedLimit::Unlimited,
                    |n| sent += n,
                );
                tokio::pin!(forwarding);
                assert!(futures::poll!(&mut forwarding).is_pending());
                let WriterCommand::Data { offset, data, .. } = rx.try_recv().unwrap() else {
                    panic!()
                };
                assert_eq!(offset, 7);
                assert_eq!(data, &b"a"[..]);
                stop_tx.send(signal).unwrap();
                let err = forwarding.await.unwrap_err();
                assert!(matches!(
                    (signal, err),
                    (StopSignal::Pause, DownloadError::Paused)
                        | (StopSignal::Cancel, DownloadError::Cancelled)
                ));
            }
            assert_eq!(sent, 1);
            assert!(rx.try_recv().is_err());
            assert_eq!(budget.semaphore.available_permits(), 0);
        }
    }

    #[tokio::test]
    async fn blocked_rate_budget_and_channel_observe_writer_closure_and_stop() {
        for wait in ["rate", "budget", "channel"] {
            for stop in [true, false] {
                let budget = MemoryBudget::new(8);
                let (tx, mut rx) = mpsc::channel(1);
                let (stop_tx, mut stop_rx) = watch::channel(StopSignal::Running);
                let speed = if wait == "rate" {
                    SpeedLimit::new(1)
                } else {
                    SpeedLimit::Unlimited
                };
                if wait == "rate" {
                    speed.acquire(1).await;
                }
                let held = if wait == "budget" {
                    Some(budget.semaphore.acquire_many(8).await.unwrap())
                } else {
                    None
                };
                if wait == "channel" {
                    tx.send(WriterCommand::BeginLease {
                        lease_key: LeaseKey {
                            piece_id: 0,
                            lease_id: 0,
                        },
                    })
                    .await
                    .unwrap();
                }
                let mut sent = 0;
                {
                    let forwarding = budget.forward(
                        Bytes::from_static(b"abcd"),
                        0,
                        None,
                        &tx,
                        &mut stop_rx,
                        &speed,
                        |n| sent += n,
                    );
                    tokio::pin!(forwarding);
                    assert!(futures::poll!(&mut forwarding).is_pending());
                    if stop {
                        stop_tx.send(StopSignal::Pause).unwrap();
                    } else {
                        rx.close();
                    }
                    let err = tokio::time::timeout(Duration::from_secs(1), forwarding)
                        .await
                        .unwrap()
                        .unwrap_err();
                    assert!(matches!(
                        (stop, err),
                        (true, DownloadError::Paused) | (false, DownloadError::ChannelClosed)
                    ));
                }
                drop(held);
                assert_eq!(sent, 0);
                assert_eq!(budget.semaphore.available_permits(), 8);
            }
        }
    }
}
