use std::collections::VecDeque;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
struct ProgressSample {
    at: Instant,
    downloaded: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct EtaEstimator {
    samples: VecDeque<ProgressSample>,
    window: Duration,
    min_span: Duration,
}

impl EtaEstimator {
    pub(crate) fn new(window: Duration, min_span: Duration) -> Self {
        Self {
            samples: VecDeque::new(),
            window,
            min_span,
        }
    }

    pub(crate) fn record(&mut self, downloaded: u64, now: Instant) {
        if let Some(last) = self.samples.back().copied() {
            if downloaded < last.downloaded {
                self.samples.clear();
            } else if now <= last.at {
                if let Some(last) = self.samples.back_mut() {
                    last.downloaded = downloaded.max(last.downloaded);
                }
                return;
            }
        }

        self.samples.push_back(ProgressSample { at: now, downloaded });
        self.trim(now);
    }

    pub(crate) fn speed_bytes_per_sec(&self) -> Option<f64> {
        let last = self.samples.back()?;
        let (baseline_bytes, baseline_time) = self.window_start()?;
        let span = last.at.duration_since(baseline_time);
        if span < self.min_span {
            return None;
        }

        let delta_secs = span.as_secs_f64();
        if delta_secs <= 0.0 {
            return None;
        }

        let delta_bytes = (last.downloaded as f64 - baseline_bytes).max(0.0);
        Some(delta_bytes / delta_secs)
    }

    pub(crate) fn estimate(&self, remaining_bytes: u64) -> Option<f64> {
        let speed = self.speed_bytes_per_sec()?;
        if speed < 1.0 {
            return None;
        }
        Some(remaining_bytes as f64 / speed)
    }

    fn trim(&mut self, now: Instant) {
        let cutoff = now.checked_sub(self.window).unwrap_or(now);
        while self.samples.len() >= 2 {
            let second = self.samples.get(1).expect("length checked");
            if second.at <= cutoff {
                self.samples.pop_front();
            } else {
                break;
            }
        }
    }

    fn window_start(&self) -> Option<(f64, Instant)> {
        let last = self.samples.back()?;
        let cutoff = last.at.checked_sub(self.window).unwrap_or(last.at);
        let mut previous = None;

        for sample in self.samples.iter().copied() {
            if sample.at <= cutoff {
                previous = Some(sample);
                continue;
            }

            return match previous {
                Some(before) if before.at < cutoff => {
                    let total_span = sample.at.duration_since(before.at).as_secs_f64();
                    let elapsed = cutoff.duration_since(before.at).as_secs_f64();
                    let delta_bytes = sample.downloaded.saturating_sub(before.downloaded) as f64;
                    Some((before.downloaded as f64 + delta_bytes * (elapsed / total_span), cutoff))
                }
                Some(before) => Some((before.downloaded as f64, before.at)),
                None => Some((sample.downloaded as f64, sample.at)),
            };
        }

        previous.map(|sample| (sample.downloaded as f64, sample.at))
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::EtaEstimator;

    #[test]
    fn test_eta_estimator_constant_throughput() {
        let mut estimator = EtaEstimator::new(Duration::from_secs(5), Duration::from_secs(1));
        let start = Instant::now();

        estimator.record(0, start);
        estimator.record(1_000, start + Duration::from_secs(1));
        estimator.record(2_000, start + Duration::from_secs(2));
        estimator.record(3_000, start + Duration::from_secs(3));

        let speed = estimator.speed_bytes_per_sec().unwrap();
        let eta = estimator.estimate(5_000).unwrap();
        assert!((speed - 1_000.0).abs() < 0.01);
        assert!((eta - 5.0).abs() < 0.01);
    }

    #[test]
    fn test_eta_estimator_uses_recent_window() {
        let mut estimator = EtaEstimator::new(Duration::from_secs(5), Duration::from_secs(1));
        let start = Instant::now();

        estimator.record(0, start);
        estimator.record(10_000, start + Duration::from_secs(1));
        estimator.record(10_000, start + Duration::from_secs(6));

        assert_eq!(estimator.speed_bytes_per_sec().unwrap(), 0.0);
        assert_eq!(estimator.estimate(5_000), None);
    }

    #[test]
    fn test_eta_estimator_requires_minimum_span() {
        let mut estimator = EtaEstimator::new(Duration::from_secs(5), Duration::from_secs(1));
        let start = Instant::now();

        estimator.record(0, start);
        estimator.record(1_000, start + Duration::from_millis(500));

        assert_eq!(estimator.speed_bytes_per_sec(), None);
        assert_eq!(estimator.estimate(5_000), None);
    }

    #[test]
    fn test_eta_estimator_rises_when_speed_drops() {
        let mut estimator = EtaEstimator::new(Duration::from_secs(5), Duration::from_secs(1));
        let start = Instant::now();

        estimator.record(0, start);
        estimator.record(2_000, start + Duration::from_secs(1));
        estimator.record(4_000, start + Duration::from_secs(2));
        let fast_eta = estimator.estimate(5_000).unwrap();

        estimator.record(4_500, start + Duration::from_secs(7));
        let slower_eta = estimator.estimate(5_000).unwrap();

        assert!(slower_eta > fast_eta);
    }

    #[test]
    fn test_eta_estimator_returns_none_for_low_speed() {
        let mut estimator = EtaEstimator::new(Duration::from_secs(5), Duration::from_secs(1));
        let start = Instant::now();

        estimator.record(0, start);
        estimator.record(0, start + Duration::from_secs(2));
        assert_eq!(estimator.estimate(1_000), None);

        estimator.record(1, start + Duration::from_secs(4));
        assert_eq!(estimator.estimate(1_000), None);
    }

    #[test]
    fn test_eta_estimator_updates_last_sample_when_time_does_not_advance() {
        let mut estimator = EtaEstimator::new(Duration::from_secs(5), Duration::from_secs(1));
        let start = Instant::now();

        estimator.record(100, start);
        estimator.record(150, start);
        estimator.record(250, start + Duration::from_secs(2));

        let speed = estimator.speed_bytes_per_sec().unwrap();
        assert!((speed - 50.0).abs() < 0.01);
    }

    #[test]
    fn test_eta_estimator_resets_when_downloaded_regresses() {
        let mut estimator = EtaEstimator::new(Duration::from_secs(5), Duration::from_secs(1));
        let start = Instant::now();

        estimator.record(100, start);
        estimator.record(40, start + Duration::from_secs(1));
        assert_eq!(estimator.speed_bytes_per_sec(), None);

        estimator.record(140, start + Duration::from_secs(3));
        let speed = estimator.speed_bytes_per_sec().unwrap();
        assert!((speed - 50.0).abs() < 0.01);
    }

    #[test]
    fn test_eta_estimator_returns_none_for_zero_span_when_min_span_is_zero() {
        let mut estimator = EtaEstimator::new(Duration::ZERO, Duration::ZERO);
        let start = Instant::now();

        estimator.record(100, start);

        assert_eq!(estimator.speed_bytes_per_sec(), None);
    }

    #[test]
    fn test_eta_estimator_interpolates_window_start() {
        let mut estimator = EtaEstimator::new(Duration::from_secs(5), Duration::from_secs(1));
        let start = Instant::now();

        estimator.record(0, start);
        estimator.record(10_000, start + Duration::from_secs(10));

        let speed = estimator.speed_bytes_per_sec().unwrap();
        assert!((speed - 1_000.0).abs() < 0.01);
    }
}
