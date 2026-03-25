#[derive(Debug, Clone)]
pub(crate) struct EtaEstimator {
    ewma_speed: Option<f64>,
    alpha: f64,
}

impl EtaEstimator {
    pub(crate) fn new(alpha: f64) -> Self {
        Self {
            ewma_speed: None,
            alpha,
        }
    }

    pub(crate) fn update_sample(&mut self, delta_bytes: u64, delta_secs: f64) {
        if delta_secs <= 0.0 {
            return;
        }

        let sample_speed = delta_bytes as f64 / delta_secs;
        self.ewma_speed = Some(match self.ewma_speed {
            Some(previous) => self.alpha * sample_speed + (1.0 - self.alpha) * previous,
            None => sample_speed,
        });
    }

    pub(crate) fn estimate(&self, remaining_bytes: u64) -> Option<f64> {
        let speed = self.ewma_speed?;
        if speed < 1.0 {
            return None;
        }
        Some(remaining_bytes as f64 / speed)
    }
}

#[cfg(test)]
mod tests {
    use super::EtaEstimator;

    #[test]
    fn test_eta_estimator_constant_throughput() {
        let mut estimator = EtaEstimator::new(0.3);

        estimator.update_sample(1_000, 1.0);
        estimator.update_sample(1_000, 1.0);
        estimator.update_sample(1_000, 1.0);

        let eta = estimator.estimate(5_000).unwrap();
        assert!((eta - 5.0).abs() < 0.01);
    }

    #[test]
    fn test_eta_estimator_rises_when_speed_drops() {
        let mut estimator = EtaEstimator::new(0.3);

        estimator.update_sample(1_000, 1.0);
        let fast_eta = estimator.estimate(5_000).unwrap();

        estimator.update_sample(100, 1.0);
        let slower_eta = estimator.estimate(5_000).unwrap();

        assert!(slower_eta > fast_eta);
    }

    #[test]
    fn test_eta_estimator_returns_none_for_low_speed() {
        let mut estimator = EtaEstimator::new(0.3);
        estimator.update_sample(0, 1.0);
        assert_eq!(estimator.estimate(1_000), None);

        estimator.update_sample(1, 2.0);
        assert_eq!(estimator.estimate(1_000), None);
    }

    #[test]
    fn test_eta_estimator_ignores_non_positive_delta_secs() {
        let mut estimator = EtaEstimator::new(0.3);
        estimator.update_sample(1_000, 0.0);
        assert_eq!(estimator.estimate(1_000), None);
    }
}