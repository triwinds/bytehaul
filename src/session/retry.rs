use std::time::{Duration, Instant};

use tokio::sync::watch;

use super::{stop_signal_error, StopSignal};
use crate::error::DownloadError;

/// A retry policy's state for one logical operation.
///
/// The state deliberately contains no I/O or sleeping.  Callers can therefore
/// use the same accounting for response probes, segment transfers, and a
/// complete single-connection transfer while keeping resource cleanup in the
/// caller that owns it.
#[derive(Debug)]
pub(crate) struct RetryState {
    retries_started: u32,
    started_at: Instant,
    max_retries: u32,
    base_delay: Duration,
    max_delay: Duration,
    max_retry_elapsed: Option<Duration>,
}

/// The result of applying a retry policy to one failed operation.
#[derive(Debug)]
pub(crate) enum RetryDecision {
    Stop(DownloadError),
    Retry {
        error: DownloadError,
        retry_count: u32,
        backoff: Duration,
        elapsed: Duration,
    },
}

impl RetryState {
    pub(crate) fn new(
        max_retries: u32,
        base_delay: Duration,
        max_delay: Duration,
        max_retry_elapsed: Option<Duration>,
    ) -> Self {
        Self {
            retries_started: 0,
            started_at: Instant::now(),
            max_retries,
            base_delay,
            max_delay,
            max_retry_elapsed,
        }
    }

    /// Apply the normal retry policy to an error.
    pub(crate) fn decide(&mut self, error: DownloadError) -> RetryDecision {
        self.decide_inner(error, false)
    }

    /// Apply retry count and elapsed-budget checks for a safe restart.
    ///
    /// A response with a mismatched validator or an ignored Range is not
    /// globally retryable (`DownloadError::is_retryable` must remain strict),
    /// but the single-transfer orchestrator still has to charge the restart
    /// to its existing retry scope.  This method is that explicit, local
    /// escape hatch; it does not alter error classification anywhere else.
    pub(crate) fn decide_restart(&mut self, error: DownloadError) -> RetryDecision {
        self.decide_inner(error, true)
    }

    pub(crate) fn max_retries(&self) -> u32 {
        self.max_retries
    }

    fn decide_inner(&mut self, error: DownloadError, force_retry: bool) -> RetryDecision {
        if (!force_retry && !error.is_retryable()) || self.retries_started >= self.max_retries {
            return RetryDecision::Stop(error);
        }

        let elapsed = self.started_at.elapsed();
        if let Some(limit) = self.max_retry_elapsed {
            if elapsed >= limit {
                return RetryDecision::Stop(DownloadError::RetryBudgetExceeded { elapsed, limit });
            }
        }

        let retry_count = self.retries_started.saturating_add(1);
        let backoff = retry_backoff(&error, retry_count, self.base_delay, self.max_delay);

        let elapsed = self.started_at.elapsed();
        if let Some(limit) = self.max_retry_elapsed {
            if elapsed.saturating_add(backoff) > limit {
                return RetryDecision::Stop(DownloadError::RetryBudgetExceeded { elapsed, limit });
            }
        }

        self.retries_started = retry_count;
        RetryDecision::Retry {
            error,
            retry_count,
            backoff,
            elapsed,
        }
    }
}

fn retry_backoff(
    error: &DownloadError,
    retry_count: u32,
    base_delay: Duration,
    max_delay: Duration,
) -> Duration {
    if let Some(retry_secs) = error.retry_after_secs() {
        return Duration::from_secs(retry_secs);
    }

    let raw = base_delay
        .saturating_mul(1u32 << retry_count.min(10))
        .min(max_delay);
    // Equal jitter: retain half the deterministic delay and randomize the
    // remaining half.  This gives every retry a non-zero floor while avoiding
    // a synchronized retry wave.
    let half = raw / 2;
    let max_jitter = half.as_nanos().min(u64::MAX as u128) as u64;
    let jitter = Duration::from_nanos(fastrand::u64(0..=max_jitter));
    half + jitter
}

/// Sleep for a retry delay while observing pause/cancel signals.
pub(crate) async fn sleep_with_backoff(
    backoff: Duration,
    cancel_rx: &mut watch::Receiver<StopSignal>,
) -> Result<(), DownloadError> {
    if let Some(error) = stop_signal_error(*cancel_rx.borrow()) {
        return Err(error);
    }

    let sleep = tokio::time::sleep(backoff);
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            biased;
            result = cancel_rx.changed() => {
                match result {
                    Ok(()) => {
                        if let Some(error) = stop_signal_error(*cancel_rx.borrow_and_update()) {
                            return Err(error);
                        }
                    }
                    // A dropped signal sender means no future stop request;
                    // finish the already selected backoff normally.
                    Err(_) => {
                        sleep.as_mut().await;
                        return Ok(());
                    }
                }
            }
            _ = &mut sleep => return Ok(()),
        }
    }
}

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
    let mut retry_state = RetryState::new(max_retries, base_delay, max_delay, max_retry_elapsed);
    loop {
        if let Some(error) = stop_signal_error(*cancel_rx.borrow()) {
            return Err(error);
        }
        let operation = op();
        tokio::pin!(operation);
        let result = loop {
            tokio::select! {
                biased;
                changed = cancel_rx.changed() => {
                    if changed.is_err() {
                        break operation.await;
                    }
                    if let Some(error) = stop_signal_error(*cancel_rx.borrow_and_update()) {
                        return Err(error);
                    }
                }
                result = &mut operation => break result,
            }
        };
        match result {
            Ok(val) => return Ok(val),
            Err(e) => match retry_state.decide(e) {
                RetryDecision::Stop(error) => return Err(error),
                RetryDecision::Retry { backoff, .. } => {
                    sleep_with_backoff(backoff, cancel_rx).await?;
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stop_interrupts_pending_operation() {
        for signal in [StopSignal::Pause, StopSignal::Cancel] {
            let (tx, mut rx) = watch::channel(StopSignal::Running);
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let mut started_tx = Some(started_tx);
            let operation =
                retry_with_backoff(0, Duration::ZERO, Duration::ZERO, None, &mut rx, || {
                    started_tx.take().unwrap().send(()).unwrap();
                    std::future::pending::<Result<(), DownloadError>>()
                });
            let stop = async {
                started_rx.await.unwrap();
                tx.send(signal).unwrap();
            };
            let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
                tokio::join!(operation, stop)
            })
            .await
            .expect("stop did not interrupt pending operation");
            assert!(matches!(
                (signal, result),
                (StopSignal::Pause, Err(DownloadError::Paused))
                    | (StopSignal::Cancel, Err(DownloadError::Cancelled))
            ));
        }
    }

    #[tokio::test]
    async fn already_stopped_does_not_start_operation() {
        for signal in [StopSignal::Pause, StopSignal::Cancel] {
            let (_tx, mut rx) = watch::channel(signal);
            let result: Result<(), DownloadError> =
                retry_with_backoff(0, Duration::ZERO, Duration::ZERO, None, &mut rx, || {
                    panic!("operation started after stop request");
                    #[allow(unreachable_code)]
                    std::future::ready(Ok(()))
                })
                .await;
            assert!(matches!(
                (signal, result),
                (StopSignal::Pause, Err(DownloadError::Paused))
                    | (StopSignal::Cancel, Err(DownloadError::Cancelled))
            ));
        }
    }

    #[tokio::test]
    async fn closed_sender_allows_pending_operation_to_finish() {
        let (tx, mut rx) = watch::channel(StopSignal::Running);
        drop(tx);
        let result =
            retry_with_backoff(0, Duration::ZERO, Duration::ZERO, None, &mut rx, || async {
                tokio::task::yield_now().await;
                Ok(42)
            })
            .await;
        assert_eq!(result.unwrap(), 42);
    }

    fn retryable_error() -> DownloadError {
        DownloadError::HttpStatus {
            status: 503,
            message: "Service Unavailable".into(),
        }
    }

    #[test]
    fn test_retry_state_zero_retries_returns_original_error() {
        let mut state =
            RetryState::new(0, Duration::from_millis(1), Duration::from_millis(5), None);
        assert!(matches!(
            state.decide(retryable_error()),
            RetryDecision::Stop(DownloadError::HttpStatus { status: 503, .. })
        ));
    }

    #[test]
    fn test_retry_state_counts_additional_retries() {
        let mut state = RetryState::new(2, Duration::ZERO, Duration::ZERO, None);
        assert!(matches!(
            state.decide(retryable_error()),
            RetryDecision::Retry { retry_count: 1, .. }
        ));
        assert!(matches!(
            state.decide(retryable_error()),
            RetryDecision::Retry { retry_count: 2, .. }
        ));
        assert!(matches!(
            state.decide(retryable_error()),
            RetryDecision::Stop(DownloadError::HttpStatus { status: 503, .. })
        ));
    }

    #[test]
    fn test_retry_state_restart_uses_same_budget() {
        let mut state = RetryState::new(1, Duration::ZERO, Duration::ZERO, None);
        let mismatch = || DownloadError::ResumeMismatch("changed object".into());
        assert!(matches!(
            state.decide_restart(mismatch()),
            RetryDecision::Retry { retry_count: 1, .. }
        ));
        assert!(matches!(
            state.decide_restart(mismatch()),
            RetryDecision::Stop(DownloadError::ResumeMismatch(_))
        ));
    }

    #[test]
    fn test_retry_backoff_equal_jitter_is_within_bounds() {
        let backoff = retry_backoff(
            &retryable_error(),
            1,
            Duration::from_millis(10),
            Duration::from_millis(100),
        );
        assert!((Duration::from_millis(10)..=Duration::from_millis(20)).contains(&backoff));
    }

    #[test]
    fn test_retry_after_overrides_backoff() {
        let error = DownloadError::HttpStatus {
            status: 503,
            message: "retry-after:7".into(),
        };
        assert_eq!(
            retry_backoff(
                &error,
                1,
                Duration::from_millis(1),
                Duration::from_millis(2)
            ),
            Duration::from_secs(7)
        );
    }

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
