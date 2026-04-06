use std::time::{Duration, Instant};

use tokio::sync::watch;

use super::{stop_signal_error, StopSignal};
use crate::error::DownloadError;

/// Retry an async operation with exponential backoff.
pub(crate) async fn retry_with_backoff<F, Fut, T>(
    max_retries: u32,
    base_delay: Duration,
    max_delay: Duration,
    max_retry_elapsed: Option<Duration>,
    cancel_rx: &mut watch::Receiver<StopSignal>,
    mut op: F,
) -> Result<T, DownloadError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, DownloadError>>,
{
    let started_at = Instant::now();
    for attempt in 0u32.. {
        match op().await {
            Ok(val) => return Ok(val),
            Err(e) => {
                if !e.is_retryable() || attempt >= max_retries {
                    return Err(e);
                }

                if let Some(limit) = max_retry_elapsed {
                    let elapsed = started_at.elapsed();
                    if elapsed >= limit {
                        return Err(DownloadError::RetryBudgetExceeded { elapsed, limit });
                    }
                }

                let retry_count = attempt.saturating_add(1);
                let backoff = if let Some(retry_secs) = e.retry_after_secs() {
                    Duration::from_secs(retry_secs)
                } else {
                    let raw = base_delay
                        .saturating_mul(1u32 << retry_count.min(10))
                        .min(max_delay);
                    // Equal jitter: half deterministic + half random to avoid
                    // thundering herd while keeping a minimum delay floor.
                    let half = raw / 2;
                    let jitter = Duration::from_nanos(
                        fastrand::u64(0..=half.as_nanos().min(u64::MAX as u128) as u64),
                    );
                    half + jitter
                };

                if let Some(limit) = max_retry_elapsed {
                    let elapsed = started_at.elapsed();
                    if elapsed.saturating_add(backoff) > limit {
                        return Err(DownloadError::RetryBudgetExceeded { elapsed, limit });
                    }
                }

                tokio::select! {
                    result = cancel_rx.changed() => {
                        if result.is_ok() {
                            if let Some(error) = stop_signal_error(*cancel_rx.borrow_and_update()) {
                                return Err(error);
                            }
                        }
                    }
                    _ = tokio::time::sleep(backoff) => {}
                }
            }
        }
    }

    unreachable!("infinite retry loop should always return from success or error branches")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_retry_with_backoff_immediate_success() {
        let (_, mut cancel_rx) = watch::channel(StopSignal::Running);
        let result: Result<i32, DownloadError> = retry_with_backoff(
            3,
            Duration::from_millis(10),
            Duration::from_millis(100),
            None,
            &mut cancel_rx,
            || async { Ok(42) },
        )
        .await;
        assert_eq!(result.unwrap(), 42);
    }

    #[tokio::test]
    async fn test_retry_with_backoff_immediate_success_with_live_sender() {
        let (_cancel_tx, mut cancel_rx) = watch::channel(StopSignal::Running);
        let result: Result<i32, DownloadError> = retry_with_backoff(
            3,
            Duration::from_millis(10),
            Duration::from_millis(100),
            None,
            &mut cancel_rx,
            || async { Ok(7) },
        )
        .await;
        assert_eq!(result.unwrap(), 7);
    }

    #[tokio::test]
    async fn test_retry_with_backoff_non_retryable() {
        let (_, mut cancel_rx) = watch::channel(StopSignal::Running);
        let result: Result<i32, DownloadError> = retry_with_backoff(
            3,
            Duration::from_millis(10),
            Duration::from_millis(100),
            None,
            &mut cancel_rx,
            || async { Err(DownloadError::Cancelled) },
        )
        .await;
        assert!(matches!(result, Err(DownloadError::Cancelled)));
    }

    #[tokio::test]
    async fn test_retry_with_backoff_retries_then_succeeds() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::Arc;
        let (_, mut cancel_rx) = watch::channel(StopSignal::Running);
        let attempts = Arc::new(AtomicU32::new(0));
        let attempts_clone = attempts.clone();

        let result: Result<i32, DownloadError> = retry_with_backoff(
            3,
            Duration::from_millis(1),
            Duration::from_millis(10),
            None,
            &mut cancel_rx,
            move || {
                let attempts = attempts_clone.clone();
                async move {
                    let n = attempts.fetch_add(1, Ordering::Relaxed);
                    if n < 2 {
                        Err(DownloadError::HttpStatus {
                            status: 503,
                            message: "Service Unavailable".into(),
                        })
                    } else {
                        Ok(42)
                    }
                }
            },
        )
        .await;
        assert_eq!(result.unwrap(), 42);
        assert_eq!(attempts.load(Ordering::Relaxed), 3);
    }

    #[tokio::test]
    async fn test_retry_with_backoff_exhausts_retries() {
        let (_, mut cancel_rx) = watch::channel(StopSignal::Running);
        let result: Result<i32, DownloadError> = retry_with_backoff(
            2,
            Duration::from_millis(1),
            Duration::from_millis(10),
            None,
            &mut cancel_rx,
            || async {
                Err(DownloadError::HttpStatus {
                    status: 503,
                    message: "Service Unavailable".into(),
                })
            },
        )
        .await;
        assert!(matches!(
            result,
            Err(DownloadError::HttpStatus { status: 503, .. })
        ));
    }

    #[tokio::test]
    async fn test_retry_with_backoff_with_retry_after_hint() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::Arc;
        let (_, mut cancel_rx) = watch::channel(StopSignal::Running);
        let attempts = Arc::new(AtomicU32::new(0));
        let attempts_clone = attempts.clone();

        let result: Result<i32, DownloadError> = retry_with_backoff(
            3,
            Duration::from_millis(1),
            Duration::from_millis(10),
            None,
            &mut cancel_rx,
            move || {
                let attempts = attempts_clone.clone();
                async move {
                    let n = attempts.fetch_add(1, Ordering::Relaxed);
                    if n < 1 {
                        Err(DownloadError::HttpStatus {
                            status: 429,
                            message: "retry-after:1".into(),
                        })
                    } else {
                        Ok(99)
                    }
                }
            },
        )
        .await;
        assert_eq!(result.unwrap(), 99);
    }

    #[tokio::test]
    async fn test_retry_with_backoff_cancelled() {
        let (cancel_tx, mut cancel_rx) = watch::channel(StopSignal::Running);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            let _ = cancel_tx.send(StopSignal::Cancel);
        });

        let result: Result<i32, DownloadError> = retry_with_backoff(
            10,
            Duration::from_secs(10),
            Duration::from_secs(60),
            None,
            &mut cancel_rx,
            || async {
                Err(DownloadError::HttpStatus {
                    status: 503,
                    message: "retry-after:60".into(),
                })
            },
        )
        .await;
        assert!(matches!(result, Err(DownloadError::Cancelled)));
    }

    #[tokio::test]
    async fn test_retry_with_backoff_paused() {
        let (cancel_tx, mut cancel_rx) = watch::channel(StopSignal::Running);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            let _ = cancel_tx.send(StopSignal::Pause);
        });

        let result: Result<i32, DownloadError> = retry_with_backoff(
            10,
            Duration::from_secs(10),
            Duration::from_secs(60),
            None,
            &mut cancel_rx,
            || async {
                Err(DownloadError::HttpStatus {
                    status: 503,
                    message: "retry-after:60".into(),
                })
            },
        )
        .await;
        assert!(matches!(result, Err(DownloadError::Paused)));
    }

    #[tokio::test]
    async fn test_retry_with_backoff_retry_budget_exhausted() {
        let (_, mut cancel_rx) = watch::channel(StopSignal::Running);
        let result: Result<i32, DownloadError> = retry_with_backoff(
            10,
            Duration::from_millis(25),
            Duration::from_millis(25),
            Some(Duration::from_millis(10)),
            &mut cancel_rx,
            || async {
                Err(DownloadError::HttpStatus {
                    status: 503,
                    message: "Service Unavailable".into(),
                })
            },
        )
        .await;
        assert!(matches!(
            result,
            Err(DownloadError::RetryBudgetExceeded { .. })
        ));
    }

    #[tokio::test]
    async fn test_retry_with_backoff_retries_with_live_sender() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::Arc;

        let (_cancel_tx, mut cancel_rx) = watch::channel(StopSignal::Running);
        let attempts = Arc::new(AtomicU32::new(0));
        let attempts_clone = attempts.clone();

        let result: Result<i32, DownloadError> = retry_with_backoff(
            2,
            Duration::from_millis(1),
            Duration::from_millis(5),
            None,
            &mut cancel_rx,
            move || {
                let attempts = attempts_clone.clone();
                async move {
                    let attempt = attempts.fetch_add(1, Ordering::Relaxed);
                    if attempt == 0 {
                        Err(DownloadError::HttpStatus {
                            status: 503,
                            message: "Service Unavailable".into(),
                        })
                    } else {
                        Ok(7)
                    }
                }
            },
        )
        .await;

        assert_eq!(result.unwrap(), 7);
        assert_eq!(attempts.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn test_retry_with_backoff_zero_retries_returns_retryable_error() {
        let (_cancel_tx, mut cancel_rx) = watch::channel(StopSignal::Running);

        let result: Result<i32, DownloadError> = retry_with_backoff(
            0,
            Duration::from_millis(1),
            Duration::from_millis(5),
            None,
            &mut cancel_rx,
            || async {
                Err(DownloadError::HttpStatus {
                    status: 503,
                    message: "Service Unavailable".into(),
                })
            },
        )
        .await;

        assert!(matches!(
            result,
            Err(DownloadError::HttpStatus { status: 503, .. })
        ));
    }

    #[tokio::test]
    async fn test_retry_with_backoff_immediate_budget_exhaustion() {
        let (_cancel_tx, mut cancel_rx) = watch::channel(StopSignal::Running);

        let result: Result<i32, DownloadError> = retry_with_backoff(
            2,
            Duration::from_millis(1),
            Duration::from_millis(5),
            Some(Duration::ZERO),
            &mut cancel_rx,
            || async {
                Err(DownloadError::HttpStatus {
                    status: 503,
                    message: "Service Unavailable".into(),
                })
            },
        )
        .await;

        assert!(matches!(
            result,
            Err(DownloadError::RetryBudgetExceeded { .. })
        ));
    }
}
