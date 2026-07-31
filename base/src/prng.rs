/// A deterministic integer generator: a run that cannot be repeated cannot be compared.
#[derive(Debug, Clone, Copy)]
pub struct Prng {
    state: u64,
}

impl Prng {
    /// The seed is mixed once, so neighbouring seeds do not start in neighbouring states — a sweep
    /// over 1, 2, 3 has to explore three different runs. The state is never zero, which is the one
    /// state xorshift cannot leave.
    pub const fn new(seed: u64) -> Self {
        let mixed = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0x5DEE_CE66_D5AA_1234;
        Self { state: mixed | 1 }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state ^= self.state << 13;
        self.state ^= self.state >> 7;
        self.state ^= self.state << 17;
        self.state
    }

    /// In `[0, 1)`, from the 53 bits a double can hold exactly.
    pub fn next_float(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// An exponential draw, which is the shape of a device's tail and of the gaps between arrivals
    /// nobody coordinates. A fixed latency has no tail at all, so a model built on one answers every
    /// question about a p99.9 with the p50.
    pub fn exponential_nanos(&mut self, mean_nanos: u64) -> u64 {
        if mean_nanos == 0 {
            return 0;
        }
        // 1 - next_float() so the draw is in (0, 1] and the logarithm is finite.
        let draw = -(1.0 - self.next_float()).ln() * mean_nanos as f64;
        draw as u64
    }
}

#[cfg(test)]
mod tests {
    use super::Prng;

    #[test]
    fn floats_stay_in_the_unit_interval() {
        let mut prng = Prng::new(7);
        for _ in 0..10_000 {
            let value = prng.next_float();
            assert!((0.0..1.0).contains(&value), "{value} is outside [0, 1)");
        }
    }

    /// The mean is the parameter, and the tail reaches several times it — the property a fixed latency
    /// does not have and the reason this exists.
    #[test]
    fn an_exponential_draw_has_the_mean_it_was_asked_for_and_a_tail() {
        let mut prng = Prng::new(11);
        let draws: Vec<u64> = (0..100_000)
            .map(|_| prng.exponential_nanos(1_000))
            .collect();
        let mean = draws.iter().sum::<u64>() as f64 / draws.len() as f64;
        assert!(
            (900.0..1_100.0).contains(&mean),
            "mean was {mean}, asked for 1000"
        );
        assert!(
            draws.iter().any(|&draw| draw > 5_000),
            "no draw reached five times the mean"
        );
    }
}
