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
