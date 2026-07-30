use ledger_base::Prng;

/// A rate-limited server with a latency and a tail: a storage device, or a component sitting on top of
/// one. Both layers of the pending engine are one of these, which is the only way to keep them apart.
///
/// A read is admitted no sooner than `1 / iops` after the previous one, which is the throughput
/// ceiling; its own latency is then added on top and does not hold the gate, so about `iops × latency`
/// reads are in flight together. That is the parallelism, and it is also why completions arrive **out
/// of the order they were asked for**: each read draws its own tail. A model with a fixed latency
/// completes everything in order and hides the cost of putting a lane back in order.
///
/// Past the ceiling the admission queue grows without bound: the wait, not the device, becomes the
/// latency. Both behaviours belong to the same model because a real device has both.
pub struct Server {
    base_nanos: u64,
    tail_nanos: u64,
    /// The device's own time per read. Zero is a device with no rate limit, which is a baseline rather
    /// than a device.
    service_nanos: u64,
    free_at_nanos: u64,
    reads: u64,
    queued_nanos: u64,
}

/// What the server did, for a report that has to say which layer was the limit.
#[derive(Debug, Clone, Copy, Default)]
pub struct ServerStats {
    pub reads: u64,
    /// Time requests spent waiting for the server to be free rather than being served.
    pub queued_nanos: u64,
}

impl Server {
    pub fn new(base_nanos: u64, tail_nanos: u64, per_second: u64) -> Self {
        Self {
            base_nanos,
            tail_nanos,
            service_nanos: if per_second == 0 { 0 } else { 1_000_000_000 / per_second },
            free_at_nanos: 0,
            reads: 0,
            queued_nanos: 0,
        }
    }

    /// When a request handed over now would be answered.
    pub fn serve(&mut self, now: u64, prng: &mut Prng) -> u64 {
        self.reads += 1;
        let latency = self.base_nanos + prng.exponential_nanos(self.tail_nanos);
        if self.service_nanos == 0 {
            return now + latency;
        }
        let admitted = now.max(self.free_at_nanos);
        self.queued_nanos += admitted - now;
        self.free_at_nanos = admitted + self.service_nanos;
        admitted + latency
    }

    pub fn stats(&self) -> ServerStats {
        ServerStats { reads: self.reads, queued_nanos: self.queued_nanos }
    }

    /// Forget what has been served, for a caller that measures one stretch of a run and set up first.
    /// The rate gate keeps its state: a device does not become free because somebody started
    /// counting.
    pub fn reset_stats(&mut self) {
        self.reads = 0;
        self.queued_nanos = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::Server;
    use ledger_base::Prng;

    /// Reads asked for in one order complete in another, which is what a lane's order has to be put
    /// back together from.
    #[test]
    fn reads_with_a_tail_complete_out_of_the_order_they_were_asked_for() {
        let mut device = Server::new(1_000, 500, 0);
        let mut prng = Prng::new(3);
        let completions: Vec<u64> = (0..64).map(|_| device.serve(0, &mut prng)).collect();
        assert!(
            completions.windows(2).any(|pair| pair[1] < pair[0]),
            "every read completed in the order it was asked for, so there is no tail"
        );
    }

    /// Below the rate a read waits for nothing; the ceiling is a rate, not a latency.
    #[test]
    fn a_device_under_its_rate_makes_nobody_wait() {
        let mut device = Server::new(1_000, 0, 1_000_000);
        let mut prng = Prng::new(3);
        for step in 0..1_000 {
            let now = step * 10_000;
            assert_eq!(device.serve(now, &mut prng), now + 1_000);
        }
        assert_eq!(device.stats().queued_nanos, 0);
    }

    /// Asked for more than it can serve, the queue is what grows — the answer's latency stops being
    /// the device's own.
    #[test]
    fn a_device_asked_past_its_rate_queues_without_bound() {
        let mut device = Server::new(1_000, 0, 100_000);
        let mut prng = Prng::new(3);
        let mut last = 0;
        for _ in 0..1_000 {
            last = device.serve(0, &mut prng);
        }
        assert!(last > 9_000_000, "1000 reads of 10us each should reach 10ms, got {last}");
        assert!(device.stats().queued_nanos > 0);
    }
}
