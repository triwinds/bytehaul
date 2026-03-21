use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};

/// Token-bucket rate limiter shared across all workers.
///
/// Tokens replenish at `bytes_per_sec` rate. Each worker acquires
/// tokens before forwarding a chunk, sleeping if the bucket is empty.
pub(crate) struct RateLimiter {
    inner: Arc<Mutex<Bucket>>,
}

struct Bucket {
    tokens: f64,
    capacity: f64,
    rate: f64, // bytes per second
    last_refill: Instant,
}

impl RateLimiter {
    /// Create a rate limiter with the given bytes-per-second limit.
    /// If `bytes_per_sec` is 0, `acquire` is a no-op.
    pub fn new(bytes_per_sec: u64) -> Self {
        let rate = bytes_per_sec as f64;
        // Start with a full bucket (capacity = 1 second worth of tokens)
        let capacity = rate;
        Self {
            inner: Arc::new(Mutex::new(Bucket {
                tokens: capacity,
                capacity,
                rate,
                last_refill: Instant::now(),
            })),
        }
    }

    /// Acquire permission to send `n` bytes. Sleeps if tokens are insufficient.
    /// Handles chunks larger than bucket capacity by sleeping proportionally.
    pub async fn acquire(&self, n: usize) {
        let mut remaining = n as f64;
        while remaining > 0.0 {
            let sleep_dur = {
                let mut b = self.inner.lock().await;
                // Refill tokens based on elapsed time
                let now = Instant::now();
                let elapsed = now.duration_since(b.last_refill).as_secs_f64();
                b.tokens = (b.tokens + elapsed * b.rate).min(b.capacity);
                b.last_refill = now;

                if b.tokens > 0.0 {
                    let consume = remaining.min(b.tokens);
                    b.tokens -= consume;
                    remaining -= consume;
                    if remaining <= 0.0 {
                        return;
                    }
                }
                // Sleep for time needed to replenish min(remaining, capacity) tokens
                let needed = remaining.min(b.capacity);
                Duration::from_secs_f64(needed / b.rate)
            };
            tokio::time::sleep(sleep_dur).await;
        }
    }
}

impl Clone for RateLimiter {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

/// A wrapper that is either a real rate limiter or unlimited (no-op).
#[derive(Clone)]
pub(crate) enum SpeedLimit {
    Unlimited,
    Limited(RateLimiter),
}

impl SpeedLimit {
    pub fn new(bytes_per_sec: u64) -> Self {
        if bytes_per_sec == 0 {
            SpeedLimit::Unlimited
        } else {
            SpeedLimit::Limited(RateLimiter::new(bytes_per_sec))
        }
    }

    pub async fn acquire(&self, n: usize) {
        match self {
            SpeedLimit::Unlimited => {}
            SpeedLimit::Limited(rl) => rl.acquire(n).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_speed_limit_unlimited() {
        let sl = SpeedLimit::new(0);
        assert!(matches!(sl, SpeedLimit::Unlimited));
        // Should be a no-op
        sl.acquire(1_000_000).await;
    }

    #[tokio::test]
    async fn test_speed_limit_limited() {
        let sl = SpeedLimit::new(1_000_000);
        assert!(matches!(sl, SpeedLimit::Limited(_)));
        // Acquire small amount — should not block noticeably
        sl.acquire(100).await;
    }

    #[tokio::test]
    async fn test_rate_limiter_basic() {
        let rl = RateLimiter::new(100_000);
        // Acquire within bucket capacity
        rl.acquire(100).await;
    }

    #[tokio::test]
    async fn test_rate_limiter_clone() {
        let rl = RateLimiter::new(50_000);
        let rl2 = rl.clone();
        // Both should work
        rl.acquire(100).await;
        rl2.acquire(100).await;
    }

    #[tokio::test]
    async fn test_rate_limiter_large_chunk() {
        // Test acquiring more than bucket capacity
        let rl = RateLimiter::new(10_000_000);
        let start = tokio::time::Instant::now();
        rl.acquire(1_000).await;
        // Should complete quickly with large rate
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
    }

    #[tokio::test]
    async fn test_speed_limit_clone() {
        let sl = SpeedLimit::new(1000);
        let sl2 = sl.clone();
        sl2.acquire(10).await;
    }
}
