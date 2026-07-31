use std::time::Duration;

use ledger_pending::PendingCapacity;
use ledger_stubkit::LatencyRange;

use crate::workload::WorkloadKind;

/// One run of the load driver. Three groups: what to send, how hard to send it, and how the
/// external components should behave while it happens.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// Which mix of transfer kinds to generate.
    pub workload: WorkloadKind,
    /// Accounts the workload spreads over. This is the working set: it decides whether account
    /// state fits in cache, and how often two requests contend for the same lane.
    pub accounts: u64,
    /// How unevenly accounts are picked. 1 is uniform; higher concentrates traffic on fewer
    /// accounts, which is what puts several requests on one lane and makes fences appear.
    pub skew: f64,
    /// Fraction of transfers debiting the unconstrained clearing account instead of a user. That
    /// account needs no balance check and so keeps no place in its lane: this is the knob that shows
    /// what order exemption is worth, because without it every resolution on that one account queues
    /// behind the others.
    pub external_ratio: f64,
    /// How old a hold is when it is resolved, in holds created since. This is what decides which of the
    /// engine's windows a resolution lands in — and with it whether a read costs an IO, so it is the knob
    /// the whole read path is measured against.
    pub resolve_after: usize,
    /// What the business declares, and from which the engine derives every size it has. Held as the
    /// engine's own type rather than as loose fields, so the defaults are its defaults and the sizing
    /// rule stays in one place.
    pub capacity: PendingCapacity,
    /// The most the index may occupy. A declaration whose worst case needs more is refused at the start.
    pub index_budget: u64,
    /// Length of the measured phase. Funding the accounts happens first and is not measured.
    pub duration: Duration,
    /// Target submissions per second, or 0 to submit as fast as the ledger accepts. Use a rate
    /// to measure latency at a given load; use 0 to find the ceiling.
    pub rate: u64,
    /// Requests the client keeps unanswered. With a rate of 0 this is what sets the queue depth,
    /// so it sets latency: roughly in_flight divided by throughput.
    pub in_flight: u64,
    /// Effects that make a consensus batch full (sequencer `batching.size`).
    pub batch_size: usize,
    /// Ceiling on one consensus batch (sequencer `batching.max`).
    pub batch_max: usize,
    /// Judged effects allowed to wait for consensus before intake pauses. None follows `batch_max`,
    /// which is the smallest value the sequencer accepts — so raising the batch ceiling alone cannot
    /// produce a combination it refuses.
    pub batch_queued: Option<usize>,
    /// Transfers per client submission. Only affects how the client publishes; a linked chain is
    /// always submitted whole regardless.
    pub client_batch: usize,
    /// Depth of the queues between client and reactor.
    pub client_queue: usize,
    /// How long a partial batch waits before being proposed anyway.
    pub batch_linger: Duration,
    /// Consensus proposals allowed in flight at once.
    pub raft_in_flight: usize,
    /// Simulated consensus round trip. This is the latency floor for any committed transfer.
    pub raft_round_trip: LatencyRange,
    /// Simulated hold lookup latency. Only settle and void pay it, plus whatever shares their
    /// lane while they are outstanding.
    /// What a block read costs the pending engine, base and tail. This is the disk the engine does not
    /// have: everything above the block store is real code doing real work, so an invented delay belongs
    /// only where the missing device would be. Zero is the exact store, which every other answer is
    /// measured against.
    pub store_read: LatencyRange,
    /// Reads a second the store can serve, zero for no ceiling.
    pub store_iops: u64,
    /// Holds the engine's overlay may keep before idle ones are evicted. Small enough and a resolution
    /// has to ask the engine, which is the only way a run reaches the fetch path at all.
    pub overlay_limit: usize,
    /// Simulated dedup latency. Every request pays it.
    pub idem_latency: LatencyRange,
    /// Make the pending engine return every nth lane reply out of order, to prove the sequencer
    /// detects a contract-1 violation. 0 leaves it well behaved.
    pub violate_order_every: u32,
    /// Make consensus refuse every nth batch, to exercise rollback. 0 never fails.
    pub raft_fail_every: u64,
    /// Seed for the workload and for the simulated latencies, so a run repeats exactly.
    pub seed: u64,
    /// Core to bind the reactor to (Linux only; elsewhere a performance-class hint).
    pub pin: Option<usize>,
    /// Repeat the whole run this many times and report the median throughput.
    pub repeat: usize,
    /// Print the report as one JSON line instead of text.
    pub json: bool,
    /// Print the sequencer's log events (gaps, quarantine, fail-stop, pauses) to stderr.
    pub log: bool,
    /// Time each reactor stage, to see which one is the bottleneck. Costs a clock read per stage
    /// per tick, so the throughput of a profiled run is not the throughput of a plain one.
    pub profile: bool,
    /// Fail the run when end-to-end p99.9 is worse than this.
    pub slo_p999: Option<Duration>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            workload: WorkloadKind::SinglePhase,
            accounts: 100_000,
            skew: 1.0,
            external_ratio: 0.0,
            resolve_after: 0,
            capacity: PendingCapacity::default(),
            index_budget: 1 << 30,
            duration: Duration::from_secs(3),
            rate: 0,
            in_flight: 20_000,
            batch_size: 1_000,
            batch_max: 10_000,
            batch_queued: None,
            client_batch: 64,
            client_queue: 1 << 16,
            batch_linger: Duration::from_micros(200),
            raft_in_flight: 8,
            raft_round_trip: LatencyRange::new(
                Duration::from_micros(900),
                Duration::from_micros(1_400),
            ),
            // Zero: the default run measures the engine as built, and the store it was built on is
            // memory. Asking for a device's timing is what the knob is for.
            store_read: LatencyRange::fixed(Duration::ZERO),
            store_iops: 0,
            overlay_limit: 1 << 20,
            idem_latency: LatencyRange::new(Duration::from_micros(1), Duration::from_micros(5)),
            violate_order_every: 0,
            raft_fail_every: 0,
            seed: 0x5eed_1234,
            pin: None,
            repeat: 1,
            json: false,
            log: false,
            profile: false,
            slo_p999: None,
        }
    }
}

pub enum Command {
    Run(Options),
    /// The same run repeated with one option taking each value in turn.
    Sweep { base: Options, knob: String, values: Vec<String> },
    Layout,
    Help,
}

pub struct Cli {
    args: Vec<String>,
}

impl Cli {
    pub fn new(args: Vec<String>) -> Self {
        Self { args }
    }

    pub fn parse(self) -> Result<Command, String> {
        let mut args = self.args.into_iter();
        let command = args.next().unwrap_or_else(|| "help".to_owned());
        match command.as_str() {
            "layout" => Ok(Command::Layout),
            "help" | "-h" | "--help" => Ok(Command::Help),
            "run" => Self::parse_run(args.collect()),
            other => Err(format!("unknown command `{other}`")),
        }
    }

    fn parse_run(args: Vec<String>) -> Result<Command, String> {
        let mut options = Options::default();
        let mut sweep = None;
        let mut parser = lexopt::Parser::from_args(args);
        while let Some(arg) = parser.next().map_err(|err| err.to_string())? {
            let lexopt::Arg::Long(key) = arg else {
                return Err(format!("unknown option `{arg:?}`"));
            };
            let key = key.to_owned();
            if key == "sweep" {
                sweep = Some(Self::sweep_spec(&Self::value(&mut parser, &key)?)?);
                continue;
            }
            if Self::set_switch(&mut options, &key) {
                continue;
            }
            let value = Self::value(&mut parser, &key)?;
            Self::apply(&mut options, &key, &value)?;
        }
        match sweep {
            Some((knob, values)) => Ok(Command::Sweep { base: options, knob, values }),
            None => Ok(Command::Run(options)),
        }
    }

    fn value(parser: &mut lexopt::Parser, key: &str) -> Result<String, String> {
        parser
            .value()
            .map_err(|_| format!("--{key} needs a value"))?
            .into_string()
            .map_err(|_| format!("--{key} needs text"))
    }

    /// An option with no value. Returns false when the key is not one of them, so the caller reads
    /// a value for it.
    fn set_switch(options: &mut Options, key: &str) -> bool {
        match key {
            "json" => options.json = true,
            "log" => options.log = true,
            "cpu" => options.profile = true,
            _ => return false,
        }
        true
    }

    /// Every option that takes a value, in one place, so a sweep sets one of them the same way the
    /// command line does.
    pub fn apply(options: &mut Options, key: &str, value: &str) -> Result<(), String> {
        match key {
            "workload" => {
                options.workload =
                    WorkloadKind::parse(value).ok_or_else(|| format!("unknown workload `{value}`"))?
            }
            "accounts" => options.accounts = Self::count(value)?,
            "skew" => options.skew = Self::ratio(value)?.max(1.0),
            "external-ratio" => options.external_ratio = Self::ratio(value)?,
            "resolve-after" => options.resolve_after = Self::count(value)? as usize,
            "daily-arrivals" => options.capacity.daily_arrivals = Self::count(value)?,
            "retention-days" => options.capacity.retention_days = Self::count(value)?,
            "survivor-share" => options.capacity.worst_survivor_share = Self::ratio(value)?,
            "flush-survivors" => options.capacity.survives_flush_window = Self::ratio(value)?,
            "flush-window" => options.capacity.flush_window_hours = Self::count(value)?,
            "residency" => options.capacity.residency_hours = Self::count(value)?,
            "index-budget" => options.index_budget = Self::count(value)?,
            "duration" => options.duration = Self::duration(value)?,
            "rate" => options.rate = Self::count(value)?,
            "in-flight" => options.in_flight = Self::count(value)?,
            "batch-size" => options.batch_size = Self::count(value)? as usize,
            "batch-max" => options.batch_max = Self::count(value)? as usize,
            "batch-queued" => options.batch_queued = Some(Self::count(value)? as usize),
            "client-batch" => options.client_batch = (Self::count(value)? as usize).max(1),
            "client-queue" => options.client_queue = (Self::count(value)? as usize).max(2),
            "batch-linger" => options.batch_linger = Self::duration(value)?,
            "raft-in-flight" => options.raft_in_flight = Self::count(value)? as usize,
            "raft-rtt" => options.raft_round_trip = Self::latency(value)?,
            "store-read" => options.store_read = Self::latency(value)?,
            "store-iops" => options.store_iops = Self::count(value)?,
            "overlay-limit" => options.overlay_limit = Self::count(value)? as usize,
            "idem-latency" => options.idem_latency = Self::latency(value)?,
            "violate-order-every" => options.violate_order_every = Self::count(value)? as u32,
            "raft-fail-every" => options.raft_fail_every = Self::count(value)?,
            "seed" => options.seed = Self::count(value)?,
            "pin" => options.pin = Some(Self::count(value)? as usize),
            "repeat" => options.repeat = (Self::count(value)? as usize).max(1),
            "slo-p999" => options.slo_p999 = Some(Self::duration(value)?),
            other => return Err(format!("unknown option `--{other}`")),
        }
        Ok(())
    }

    /// `knob=v1,v2`, where the knob is any option that takes a value.
    fn sweep_spec(spec: &str) -> Result<(String, Vec<String>), String> {
        let (knob, values) = spec.split_once('=').ok_or("--sweep needs knob=v1,v2")?;
        let knob = knob.trim_start_matches('-').to_owned();
        let values: Vec<String> = values.split(',').map(str::to_owned).collect();
        if knob.is_empty() || values.iter().any(String::is_empty) {
            return Err(format!("bad sweep `{spec}`"));
        }
        Self::apply(&mut Options::default(), &knob, &values[0])?;
        Ok((knob, values))
    }

    /// Accepts `0.3`, `30%`, `1.4`.
    fn ratio(text: &str) -> Result<f64, String> {
        match text.strip_suffix('%') {
            Some(digits) => digits.parse::<f64>().map(|value| value / 100.0),
            None => text.parse::<f64>(),
        }
        .map_err(|_| format!("bad ratio `{text}`"))
    }

    /// Accepts `750us`, `5ms`, `2s`, `1m`.
    fn duration(text: &str) -> Result<Duration, String> {
        let (digits, unit) = text.split_at(
            text.find(|c: char| !c.is_ascii_digit() && c != '_')
                .unwrap_or(text.len()),
        );
        let amount: u64 = digits
            .replace('_', "")
            .parse()
            .map_err(|_| format!("bad duration `{text}`"))?;
        match unit {
            "ns" => Ok(Duration::from_nanos(amount)),
            "us" => Ok(Duration::from_micros(amount)),
            "ms" => Ok(Duration::from_millis(amount)),
            "" | "s" => Ok(Duration::from_secs(amount)),
            "m" => Ok(Duration::from_secs(amount * 60)),
            other => Err(format!("unknown time unit `{other}`")),
        }
    }

    /// Accepts `1000`, `1_000`, `100k`, `2m`.
    fn count(text: &str) -> Result<u64, String> {
        let cleaned = text.replace('_', "");
        let (digits, multiplier) = match cleaned.strip_suffix('k') {
            Some(digits) => (digits.to_owned(), 1_000),
            None => match cleaned.strip_suffix('m') {
                Some(digits) => (digits.to_owned(), 1_000_000),
                None => (cleaned.clone(), 1),
            },
        };
        let parsed = if let Some(hex) = digits.strip_prefix("0x") {
            u64::from_str_radix(hex, 16)
        } else {
            digits.parse()
        };
        parsed
            .map(|value| value * multiplier)
            .map_err(|_| format!("bad number `{text}`"))
    }

    /// Latency in microseconds, either `500` or `100:800`.
    fn latency(text: &str) -> Result<LatencyRange, String> {
        let (min, max) = text.split_once(':').unwrap_or((text, text));
        let min = Self::count(min)?;
        let max = Self::count(max)?;
        if max < min {
            return Err(format!("bad latency range `{text}`"));
        }
        Ok(LatencyRange::new(
            Duration::from_micros(min),
            Duration::from_micros(max),
        ))
    }
}
