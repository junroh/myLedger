use hdrhistogram::Histogram as Hdr;

/// Latency distribution. Quantiles come from an HDR histogram at three significant figures, so a
/// reported quantile is within 0.1% of the truth and never below it. The count, sum and max are kept
/// alongside it because an exact mean costs three fields.
pub struct Histogram {
    quantiles: Hdr<u64>,
    count: u64,
    sum_nanos: u128,
    max_nanos: u64,
}

impl Histogram {
    /// Anything slower than this is recorded as this. A minute of latency is already a failed run.
    const CEILING_NANOS: u64 = 60_000_000_000;
    const SIGNIFICANT_FIGURES: u8 = 3;

    pub fn new() -> Self {
        Self {
            quantiles: Hdr::new_with_bounds(1, Self::CEILING_NANOS, Self::SIGNIFICANT_FIGURES)
                .expect("histogram bounds"),
            count: 0,
            sum_nanos: 0,
            max_nanos: 0,
        }
    }

    /// Saturating, so a pathological sample is clamped rather than dropped or grown into.
    pub fn record(&mut self, nanos: u64) {
        self.quantiles.saturating_record(nanos);
        self.count += 1;
        self.sum_nanos += u128::from(nanos);
        self.max_nanos = self.max_nanos.max(nanos);
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    pub fn mean_nanos(&self) -> u64 {
        if self.count == 0 {
            return 0;
        }
        (self.sum_nanos / u128::from(self.count)) as u64
    }

    pub fn max_nanos(&self) -> u64 {
        self.max_nanos
    }

    pub fn percentile_nanos(&self, quantile: f64) -> u64 {
        if self.count == 0 {
            return 0;
        }
        self.quantiles.value_at_quantile(quantile).min(self.max_nanos)
    }
}

impl Default for Histogram {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What every reported latency depends on. A quantile is the top of the bucket the value fell
    /// in: never below the truth, and within the precision the histogram was built with.
    #[test]
    fn a_quantile_is_never_below_the_truth_and_within_precision() {
        let mut histogram = Histogram::new();
        for value in 1..=10_000u64 {
            histogram.record(value * 1_000); // 1us .. 10ms
        }

        assert_eq!(histogram.count(), 10_000);
        assert_eq!(histogram.max_nanos(), 10_000_000);
        assert_eq!(histogram.mean_nanos(), 5_000_500, "the mean is exact, not bucketed");

        for (quantile, truth) in [(0.5, 5_000_000u64), (0.9, 9_000_000), (0.999, 9_990_000)] {
            let reported = histogram.percentile_nanos(quantile);
            assert!(reported >= truth, "{quantile} under-reported: {reported} < {truth}");
            let error = (reported - truth) as f64 / truth as f64;
            assert!(error < 0.001, "{quantile} off by {:.3}%", error * 100.0);
        }
    }

    /// Small values are counted exactly, because the first buckets are one nanosecond wide. A
    /// quantile is the smallest value at or below which that share of the samples falls — of 0..15,
    /// half fall at or below 7. An empty histogram answers zero rather than dividing by nothing.
    #[test]
    fn small_values_are_exact_and_an_empty_histogram_is_zero() {
        let empty = Histogram::new();
        assert_eq!((empty.count(), empty.mean_nanos(), empty.percentile_nanos(0.99)), (0, 0, 0));

        let mut histogram = Histogram::new();
        for value in 0..16u64 {
            histogram.record(value);
        }
        assert_eq!(histogram.percentile_nanos(0.5), 7);
        assert_eq!(histogram.percentile_nanos(1.0), 15);
        assert_eq!(histogram.max_nanos(), 15);
    }
}
