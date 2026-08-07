mod fakes;
mod report;
mod sim;
mod workload;

use crate::sim::{capacity, check, Costs, Plan, Prediction};

/// Two questions, two modes. `check` asks whether any interleaving breaks an invariant; `capacity`
/// asks what the ledger would do against components this slow, on a core that costs this much. The
/// first owes nothing to this machine; the second is an estimate, and says what it was calibrated
/// from.
fn main() {
    let command = std::env::args().nth(1).unwrap_or_else(|| "help".to_owned());
    match command.as_str() {
        "check" => run_check(),
        "capacity" => run_capacity(),
        "require" => run_require(),
        "help" | "-h" | "--help" => help(),
        other => {
            eprintln!("ledgersim: unknown command `{other}`");
            std::process::exit(2);
        }
    }
}

fn run_check() {
    let mut seeds = 64u64;
    let mut steps = 2_000u64;
    let mut one = None;
    let mut verbose = false;
    parse(|name, parser| match name {
        "seeds" => number(name, &text(parser, name)?).map(|value| seeds = value),
        "steps" => number(name, &text(parser, name)?).map(|value| steps = value),
        "seed" => number(name, &text(parser, name)?).map(|value| one = Some(value)),
        "verbose" => {
            verbose = true;
            Ok(())
        }
        other => Err(format!("unknown option `--{other}`")),
    });

    if let Some(seed) = one {
        match check(seed, steps) {
            Ok(report) => report::seed(&report),
            Err(failure) => report::failure(&failure),
        }
        return;
    }
    let mut halted = 0;
    let mut reached = report::Coverage::default();
    // A soak of thousands of seeds takes minutes; a tool that says nothing for minutes looks stuck.
    // The clock is only ever read here, never by the simulation.
    let started = std::time::Instant::now();
    for seed in 1..=seeds {
        if !verbose && seed % 16 == 0 {
            let rate = seed as f64 / started.elapsed().as_secs_f64();
            eprint!("\rledgersim: {seed}/{seeds} seeds ({rate:.1}/s)");
        }
        match check(seed, steps) {
            Ok(report) => {
                halted += u64::from(report.halted);
                reached.add(&report);
                if verbose {
                    report::seed(&report);
                }
            }
            Err(failure) => report::failure(&failure),
        }
    }
    if !verbose {
        eprintln!();
    }
    println!(
        "ledgersim: {seeds} seeds x {steps} steps held every invariant in {:.1}s, {halted} halted on a fault",
        started.elapsed().as_secs_f64()
    );
    reached.print();
}

fn default_plan() -> Plan {
    Plan {
        duration_nanos: 1_000_000_000,
        rate: 0,
        read_queue_depth: 20_000,
        accounts: 100_000,
        costs: Costs::default(),
        // The pending engine as a black box: what it answers a command in, its tail, and how many a
        // second it can answer. Declared inputs — nothing here measures a component that does not exist.
        pending_nanos: 1_000_000,
        pending_tail_nanos: 200_000,
        // No limit by default: what the pending engine can sustain is its own design's business, and a
        // number carried over from a device's specification would be a guess dressed as an input. Set
        // it to check a candidate.
        pending_rate: 0,
        idem_nanos: 3_000,
        // A consensus round trip across a region, which is what the deployment this is sized for has.
        raft_nanos: 10_000_000,
        linger_nanos: 200_000,
        // Declared, like the device's numbers: a third of the round trip, the shape a quorum has.
        raft_tail_nanos: 3_000_000,
        skew: 1.0,
        poisson: true,
        batches_in_flight: 8,
        cost_percent: 100,
        flush_blocks: ledger_pending::DEFAULT_FLUSH_BLOCKS,
        resident_blocks: ledger_pending::DEFAULT_RESIDENT_BLOCKS,
        resolve_after: 0,
        // No day passes unless one is asked for, so a capacity number stays comparable with every one
        // taken before expiry existed. `--day-ms` with `--expiry-blocks` is what turns the sweep on.
        day_nanos: 0,
        lifetime_days: 1,
        expiry_blocks_per_round: 0,
        // What share of resolutions the pending engine answers from memory. Declared: how many entries
        // that takes is that component's own question.
    }
}

/// What the command line adds to a plan: a tail the run is judged against. `capacity` turns it into a
/// pass or fail, `require` solves for the component that would hold it, and neither has its own copy
/// of what a knob means.
#[derive(Clone, Copy)]
struct Options {
    plan: Plan,
    slo_nanos: Option<u64>,
}

/// Every option that takes a value, in one place, so `--sweep` sets one of them exactly the way the
/// command line does and the two modes cannot drift apart on what a knob means.
fn apply(options: &mut Options, key: &str, value: &str) -> Result<(), String> {
    let micros = |text: &str| number(key, text).map(|value| value * 1_000);
    let plan = &mut options.plan;
    match key {
        "duration-ms" => plan.duration_nanos = number(key, value)? * 1_000_000,
        "rate" => plan.rate = number(key, value)?,
        // Queue depth, which `fio` calls iodepth: requests outstanding, not a ledger setting.
        "qd" => plan.read_queue_depth = number(key, value)?.max(1),
        "accounts" => plan.accounts = number(key, value)?.max(2),
        "pending-us" => plan.pending_nanos = micros(value)?,
        "pending-tail-us" => plan.pending_tail_nanos = micros(value)?,
        "pending-rate" => plan.pending_rate = number(key, value)?,
        "idem-us" => plan.idem_nanos = micros(value)?,
        "raft-us" => plan.raft_nanos = micros(value)?,
        "raft-tail-us" => plan.raft_tail_nanos = micros(value)?,
        "linger-us" => plan.linger_nanos = micros(value)?,
        "skew" => plan.skew = number(key, value)? as f64,
        "batches-in-flight" => plan.batches_in_flight = (number(key, value)? as usize).max(1),
        "arrivals" => {
            plan.poisson = match value {
                "poisson" => true,
                "smooth" => false,
                _ => return Err("--arrivals takes poisson or smooth".to_owned()),
            }
        }
        "cost-intake" => plan.costs.intake_ns = number(key, value)? as f64,
        "cost-judge" => plan.costs.judge_ns = number(key, value)? as f64,
        "cost-propose" => plan.costs.propose_ns = number(key, value)? as f64,
        "cost-apply" => plan.costs.apply_ns = number(key, value)? as f64,
        "cost-scale" => plan.cost_percent = number(key, value)?.max(1),
        "flush-blocks" => plan.flush_blocks = number(key, value)?.max(1) as usize,
        "resident-blocks" => plan.resident_blocks = number(key, value)? as usize,
        "resolve-after" => plan.resolve_after = number(key, value)? as usize,
        // Retention on the virtual clock. A day of zero leaves expiry out of the run entirely, which is
        // the default; a short day is what lets a run of seconds cross a window measured in days.
        "day-ms" => plan.day_nanos = number(key, value)? * 1_000_000,
        "lifetime-days" => plan.lifetime_days = number(key, value)?.max(1),
        "expiry-blocks" => plan.expiry_blocks_per_round = number(key, value)? as usize,
        "slo-p999-us" => options.slo_nanos = Some(micros(value)?),
        other => return Err(format!("unknown option `--{other}`")),
    }
    Ok(())
}

/// Whether the run held what was asked of it. The tail is only half the test: it is drawn from what
/// came back, so a component slow enough that the client's queue depth never turned over reports a
/// *better* tail from fewer samples. That has to be refused before the quantile is believed.
fn held(options: &Options, prediction: &Prediction) -> bool {
    if !prediction.queue_depth_turned_over() {
        return false;
    }
    match options.slo_nanos {
        Some(target) => prediction.latency_us[2] * 1_000.0 <= target as f64,
        None => true,
    }
}

fn run_capacity() {
    let mut options = Options {
        plan: default_plan(),
        slo_nanos: None,
    };
    let mut sweep = None;
    parse(|name, parser| {
        let value = text(parser, name)?;
        if name == "sweep" {
            sweep = Some(sweep_spec(&value)?);
            return Ok(());
        }
        apply(&mut options, name, &value)
    });

    match sweep {
        Some((knob, values)) => sweep_capacity(options, &knob, &values),
        None => predict_once(options),
    }
}

fn predict_once(options: Options) {
    match capacity(options.plan) {
        Ok(prediction) => {
            let verdict = options.slo_nanos.map(|target_nanos| report::Verdict {
                target_nanos,
                held: held(&options, &prediction),
            });
            report::prediction(&options.plan, &prediction, verdict);
            if verdict.is_some_and(|verdict| !verdict.held) {
                std::process::exit(1);
            }
        }
        Err(failure) => report::failure(&failure),
    }
}

/// One run per value, so a knob's effect reads down a column instead of across shell history.
fn sweep_capacity(base: Options, knob: &str, values: &[String]) {
    report::sweep_header(knob);
    let mut failed = false;
    for value in values {
        let mut options = base;
        if let Err(err) = apply(&mut options, knob, value) {
            eprintln!("ledgersim: {err}");
            std::process::exit(2);
        }
        match capacity(options.plan) {
            Ok(prediction) => {
                let ok = held(&options, &prediction);
                failed |= !ok;
                report::sweep_row(value, &prediction, ok);
            }
            Err(failure) => report::failure(&failure),
        }
    }
    if failed {
        std::process::exit(1);
    }
}

/// `knob=v1,v2`, where the knob is any option that takes a value. The first value is applied to a
/// throwaway plan here, so a misspelled knob fails before the first run rather than after it.
fn sweep_spec(spec: &str) -> Result<(String, Vec<String>), String> {
    let (knob, values) = spec.split_once('=').ok_or("--sweep needs knob=v1,v2")?;
    let knob = knob.trim_start_matches('-').to_owned();
    let values: Vec<String> = values.split(',').map(str::to_owned).collect();
    if knob.is_empty() || values.iter().any(String::is_empty) {
        return Err(format!("bad sweep `{spec}`"));
    }
    apply(
        &mut Options {
            plan: default_plan(),
            slo_nanos: None,
        },
        &knob,
        &values[0],
    )?;
    Ok((knob, values))
}

/// How slow a component is even asked about. A budget beyond this says the component is not what the
/// design has to worry about, so the search says so rather than pretending to have found a number.
const SEARCHED_NANOS: u64 = 100_000_000;

/// The inverse of `capacity`, and the question a design actually asks: not "what happens at 5ms" but
/// "how slow may the pending engine be and still hold this rate at this tail". Solved by bisection on
/// the forward model, because the forward model is the thing that has the real reactor in it.
fn run_require() {
    let mut options = Options {
        plan: Plan {
            duration_nanos: 500_000_000,
            ..default_plan()
        },
        slo_nanos: Some(5_000_000),
    };
    let mut solve = Solve::Pending;
    parse(|name, parser| {
        let value = text(parser, name)?;
        if name == "solve" {
            solve = match value.as_str() {
                "pending" => Solve::Pending,
                "raft" => Solve::Raft,
                "idem" => Solve::Idem,
                _ => return Err("--solve takes pending, raft or idem".to_owned()),
            };
            return Ok(());
        }
        apply(&mut options, name, &value)
    });
    let plan = options.plan;
    let slo_nanos = options.slo_nanos.expect("require always has a target");
    if plan.rate == 0 {
        eprintln!(
            "ledgersim: require needs --rate, since it solves for a component's budget at a rate"
        );
        std::process::exit(2);
    }
    println!(
        "ledgersim require: how slow may {} be at {} tx/s with p99.9 <= {}us?",
        solve.name(),
        plan.rate,
        slo_nanos / 1_000
    );
    println!("  inputs         {}", solve.inputs(&plan));
    // An instant pending engine is the best case: if that misses the target, no budget exists and
    // something other than the pending engine is what has to change.
    let probe = |nanos: u64| {
        let mut attempt = plan;
        solve.set(&mut attempt, nanos);
        capacity(attempt).map(|prediction| (attempt, prediction))
    };
    // Held the tail on a queue depth that actually turned over, and kept up with the offered rate.
    // Committed is not the test — duplicates and refusals are part of this traffic, so committed is
    // always below what was offered.
    let met = |plan: &Plan, prediction: &Prediction| {
        held(
            &Options {
                plan: *plan,
                slo_nanos: Some(slo_nanos),
            },
            prediction,
        ) && prediction.offered() >= plan.rate as f64 * 0.95
    };
    let (best_plan, best) = match probe(0) {
        Err(failure) => report::failure(&failure),
        Ok((plan, prediction)) if !met(&plan, &prediction) => {
            println!(
                "  no budget      with {} taking no time at all, p99.9 is still {:.0}us at {:.0} \
                 submissions/s — what has to change is not this component",
                solve.name(),
                prediction.latency_us[2],
                prediction.offered()
            );
            report::evidence(&plan, &prediction);
            return;
        }
        Ok(found) => found,
    };
    // Bisect on the unknown's latency. More latency is never better for the tail, so the largest value
    // that still holds is the budget.
    let mut low = (0, best_plan, best);
    let mut high = SEARCHED_NANOS;
    for _ in 0..10 {
        let middle = (low.0 + high) / 2;
        if middle == low.0 {
            break;
        }
        match probe(middle) {
            Err(failure) => report::failure(&failure),
            Ok((attempt, prediction)) => {
                if met(&attempt, &prediction) {
                    low = (middle, attempt, prediction);
                } else {
                    high = middle;
                }
            }
        }
    }
    // The bracket, not just its lower end: a budget quoted without the value that failed hides how
    // finely it was searched.
    if high < SEARCHED_NANOS {
        println!(
            "  budget         {} may take up to {:.1}ms, and {:.1}ms was already too slow",
            solve.name(),
            low.0 as f64 / 1e6,
            high as f64 / 1e6
        );
    } else {
        println!(
            "  budget         {} may take up to {:.1}ms, which is where the search stopped — nothing \
             slower was tried, so this is a floor on the budget rather than the budget",
            solve.name(),
            low.0 as f64 / 1e6
        );
    }
    // The three values together are the requirement: any two of them fix the third, and the one that
    // decides whether a design can work at all is the concurrency.
    if matches!(solve, Solve::Pending) {
        let commands = report::rate_of(low.2.pending_commands, &low.2);
        println!(
            "  requires       answering within {:.1}ms, sustaining {:.0} commands/s, and holding {:.0} \
             of them in flight",
            low.0 as f64 / 1e6,
            commands,
            commands * low.0 as f64 / 1e9
        );
    }
    report::evidence(&low.1, &low.2);
}

/// Which component's latency is the unknown. Every other one is an input, because a budget for two
/// unknowns at once is a curve, not an answer.
#[derive(Clone, Copy)]
enum Solve {
    Pending,
    Raft,
    Idem,
}

impl Solve {
    fn name(self) -> &'static str {
        match self {
            Self::Pending => "the pending engine",
            Self::Raft => "a consensus round trip",
            Self::Idem => "a idem answer",
        }
    }

    fn set(self, plan: &mut Plan, nanos: u64) {
        match self {
            Self::Pending => plan.pending_nanos = nanos,
            Self::Raft => plan.raft_nanos = nanos,
            Self::Idem => plan.idem_nanos = nanos,
        }
    }

    /// Every input, with the unknown left as a question mark. A budget quoted without these is not
    /// quotable at all: it is an answer about one component at one operating point.
    fn inputs(self, plan: &Plan) -> String {
        let latency = |mine: bool, nanos: u64| {
            if mine {
                "?".to_owned()
            } else {
                format!("{}us", nanos / 1_000)
            }
        };
        format!(
            "pending engine {} at {}/s, consensus {}, idem {}, client qd {}",
            latency(matches!(self, Self::Pending), plan.pending_nanos),
            plan.pending_rate,
            latency(matches!(self, Self::Raft), plan.raft_nanos),
            latency(matches!(self, Self::Idem), plan.idem_nanos),
            plan.read_queue_depth
        )
    }
}

/// One argument walk for both modes: the mode decides what each option means. The name is copied
/// out first, because an argument borrows the parser it came from.
fn parse(mut apply: impl FnMut(&str, &mut lexopt::Parser) -> Result<(), String>) {
    let mut parser = lexopt::Parser::from_args(std::env::args_os().skip(2));
    loop {
        let name = match parser.next() {
            Ok(Some(lexopt::Arg::Long(name))) => name.to_owned(),
            Ok(Some(other)) => {
                eprintln!("ledgersim: unknown option `{other:?}`");
                std::process::exit(2);
            }
            Ok(None) => return,
            Err(err) => {
                eprintln!("ledgersim: {err}");
                std::process::exit(2);
            }
        };
        if name == "help" {
            help();
            return;
        }
        if let Err(err) = apply(&name, &mut parser) {
            eprintln!("ledgersim: {err}");
            std::process::exit(2);
        }
    }
}

fn text(parser: &mut lexopt::Parser, key: &str) -> Result<String, String> {
    parser
        .value()
        .map_err(|_| format!("--{key} needs a value"))?
        .into_string()
        .map_err(|_| format!("--{key} needs text"))
}

fn number(key: &str, value: &str) -> Result<u64, String> {
    value.parse().map_err(|_| format!("--{key} needs a number"))
}

fn help() {
    println!("ledgersim <check|capacity|require> [options]");
    println!();
    println!("check — does any interleaving break an invariant?");
    println!("  --seeds <n>         how many seeds to run (64)");
    println!("  --steps <n>         virtual steps per seed (2000)");
    println!("  --seed <n>          run one seed, to reproduce a failure");
    println!("  --verbose           print what each seed did");
    println!();
    println!("require — how slow may a component be and still hold a rate at a tail?");
    println!("  --rate <per-s>      offered load to hold (required)");
    println!("  --slo-p999-us <n>   the tail it has to hold (5000)");
    println!("  --solve <component> which latency is the unknown: pending, raft or idem (pending)");
    println!(
        "  --pending-us <n>    the pending engine's answer time, when solving for another (1000)"
    );
    println!("  --pending-rate <n>  a candidate ceiling to check, 0 for no limit (0)");
    println!("  --raft-us <n>       consensus round trip (10000)");
    println!("  --qd <n>            the client's queue depth (20000)");
    println!("  --accounts <n>      working set (100000)");
    println!("  --duration-ms <n>   virtual time per probe (500)");
    println!();
    println!("capacity — what would it do against components this slow?");
    println!("  --duration-ms <n>   virtual time to measure (1000)");
    println!("  --rate <per-s>      offered load, or 0 to saturate (0)");
    println!(
        "  --qd <n>            the client's queue depth: requests it leaves outstanding (20000)"
    );
    println!("  --accounts <n>      working set (100000)");
    println!("  --pending-us <n>    what the pending engine answers a command in (1000)");
    println!("  --pending-tail-us <n> mean of its tail, which is what reorders answers (200)");
    println!("  --pending-rate <n>  commands a second it can answer, 0 for no limit (0)");
    println!("  --raft-tail-us <n>  mean of the consensus tail (3000)");
    println!("  --skew <n>          account concentration, 1 is uniform (1)");
    println!("  --arrivals <kind>   poisson or smooth (poisson)");
    println!("  --batches-in-flight <n> proposals consensus may have outstanding (8)");
    println!("  --idem-us <n>       idem (3)");
    println!("  --raft-us <n>       consensus round trip (10000)");
    println!("  --linger-us <n>     how long a partial batch waits (200)");
    println!("  --cost-intake <ns>  per request, from `ledgerfio run --cpu` (181)");
    println!("  --cost-judge <ns>   per effect (93)");
    println!("  --cost-propose <ns> per effect (5)");
    println!("  --cost-apply <ns>   per effect (135)");
    println!("  --cost-scale <pct>  every stage scaled, for a core this much slower (100)");
    println!("  --resolve-after <n> holds committed since, before one is resolved: its age (0)");
    println!("  --flush-blocks <n>  the engine's unwritten window, in blocks (1024)");
    println!("  --resident-blocks <n>  written-and-still-readable window, in blocks (4096)");
    println!(
        "                      the last three decide the share of resolutions that cost an IO"
    );
    println!("  --slo-p999-us <n>   the tail to hold; fails the run (exit 1) when it is worse");
    println!("  --day-ms <n>        how long a day is on the virtual clock; 0 passes none (0)");
    println!("  --lifetime-days <n> retention plus grace: how long a hold lives (1)");
    println!("  --expiry-blocks <n>     blocks of an expiring day the sweep reads per round; 0 is off (0)");
    println!(
        "                      the last three put expiry against the traffic: how far the sweep \n\
         \x20                     falls behind is reported in days"
    );
    println!("  --sweep <knob=a,b>  run once per value of any option above, one row each");
}

/// A bounded sweep, so a broken invariant fails the build rather than waiting for someone to run the
/// tool. The tool itself is for the long soak.
#[cfg(test)]
mod tests {
    #[test]
    fn a_short_sweep_of_seeds_holds_every_invariant() {
        let mut committed = 0;
        let mut served = 0;
        let mut overflows = 0;
        let mut sealed_for_overflow = 0;
        let mut sealed_for_store = 0;
        let mut store_reads = 0;
        let mut store_faults = 0;
        let mut store_corruptions = 0;
        let mut exempt_lookups = 0;
        let mut expiries = 0;
        let mut stale = 0;
        // Thirty-two rather than sixteen, because about a third of seeds are *meant* to stop serving —
        // that is what a fault that quarantines enough lanes or seals the apply path does — and a
        // sixteen-seed sample of a one-in-three event swings wide enough to fail on the draw rather than
        // on the ledger. Measured: at sixteen seeds the halted count moved between three and nine without
        // anything about the ledger changing, while at sixty-four it sat at seventeen either way.
        const SEEDS: u64 = 32;
        for seed in 1..=SEEDS {
            match super::check(seed, 200) {
                Ok(report) => {
                    committed += report.metrics.committed;
                    served += u64::from(!report.halted);
                    overflows += report.overflowed;
                    store_reads += report.store_reads;
                    store_faults += report.store_faults;
                    store_corruptions += report.store_corruptions;
                    // The same rule as the overflow above, for the other two ways this node can be told its
                    // records are unreachable: a store that refused or answered wrongly has to have stopped
                    // it. Per seed, because one seed sealing would cover for another that did not.
                    if report.store_faults + report.store_corruptions > 0 {
                        assert!(
                            report.metrics.store_failures > 0 && report.halted,
                            "seed {seed} met {} store faults and {} corrupt blocks and kept serving",
                            report.store_faults,
                            report.store_corruptions
                        );
                        sealed_for_store += 1;
                    }
                    exempt_lookups += report.exempt_lookups;
                    expiries += report.expiries_offered;
                    stale += report.metrics.stale_answers;
                    // Rule 19 as a test: an index that could not take a committed hold has to have
                    // stopped this node, not been counted and stepped over. Checked per seed, because
                    // one seed sealing would otherwise cover for another that did not.
                    if report.overflowed > 0 {
                        assert!(
                            report.metrics.holds_not_stored > 0 && report.halted,
                            "seed {seed} overflowed its index {} times and kept serving",
                            report.overflowed
                        );
                        sealed_for_overflow += 1;
                    }
                }
                Err(failure) => panic!(
                    "seed {} broke {:?} at step {} with {:?}",
                    failure.seed, failure.broken, failure.step, failure.faults
                ),
            }
        }
        // Without this the sweep could pass by exercising nothing at all.
        assert!(
            served >= SEEDS / 2,
            "only {served} of {SEEDS} seeds kept serving"
        );
        // Not "this never happens" any more, but "the ledger answers it when it does". A sweep where no
        // seed overflowed would be reporting that the seal holds about a path it never entered.
        // A device that refuses and one that answers wrongly are different causes with one reaction, and a
        // sweep that met neither would be reporting that the seal holds about a path it never entered.
        assert!(
            sealed_for_store > 0,
            "no seed met a store that refused or lied, so the fault path and its seal are untested here \
             ({store_faults} refusals, {store_corruptions} corrupt blocks seen)"
        );
        assert!(
            sealed_for_overflow > 0,
            "no seed outgrew its index, so the notice channel and its seal are untested here \
             ({overflows} overflows seen)"
        );
        assert!(
            committed > 1_000,
            "the sweep only committed {committed} effects"
        );
        // The fetch path — the candidate walk and the fingerprint confirmation — only runs on a store
        // read. Without this the sweep would report that every invariant held about a path it never
        // entered, which is what it did until the windows were made narrow enough to leave memory.
        assert!(
            store_reads > 0,
            "no seed reached the store, so the fetch path is untested"
        );
        // An exempt resolution's reply keeps no place in its lane, so it exercises the exemption
        // itself — the path where the data check is all that stands in for the order check.
        assert!(
            exempt_lookups > 0,
            "no seed resolved a hold on an unconstrained account, so the order exemption is untested"
        );
        // Retention is what makes the index's declared maximum true rather than assumed, and the void it
        // produces moves money — so a sweep that crossed no day would be reporting that the identities
        // hold about a path it never entered.
        assert!(
            expiries > 0,
            "no seed outlived its retention, so expiry and the void it proposes are untested"
        );
        // Not asserted here: sixteen seeds never draw the stale-answer fault, and raising its odds to
        // make them would shift every other fault's draw to buy one assertion. The mechanism is asserted
        // deterministically in the sequencer's `lane_ordering` tests, and a full `check --seeds 64` run
        // reports how often it fired — which is what the coverage line is for.
        let _ = stale;
    }

    /// The division of labour, stated as a test. A device with a tail finishes reads out of the order
    /// they were asked for; the component puts each lane back in order, so the sequencer sees none of
    /// it. Take the ordering away and the same run shows the sequencer detecting gaps — which is the
    /// other half of the contract, and the reason the reordering lives in the component.
    #[test]
    fn a_device_tail_is_reordered_by_the_component_and_detected_when_it_is_not() {
        // A tail twice the base latency, and nothing kept in memory, so every resolution is a command.
        let timings = crate::fakes::Timings {
            pending_nanos: 2_000,
            pending_tail_nanos: 4_000,
            pending_rate: 0,
            resident_holds: 0,
            // Wide windows: this test is about a device's tail reaching the lane, not about the store.
            flush_blocks: ledger_pending::DEFAULT_FLUSH_BLOCKS,
            resident_blocks: ledger_pending::DEFAULT_RESIDENT_BLOCKS,
            idem_nanos: 1_000,
            raft_nanos: 2_000,
            raft_tail_nanos: 1_000,
            // No day passes: this test is about a device's tail, and a background sweep would add
            // traffic that has nothing to do with what it is measuring.
            day_nanos: 0,
            lifetime_days: 1,
            expiry_blocks_per_round: 0,
            store: ledger_pending::StoreModel::default(),
        };
        let faults = |violate| crate::fakes::Faults {
            violate_order_every: violate,
            inbox_depth: 256,
            ..crate::fakes::Faults::default()
        };
        let run = super::sim::Run {
            steps: 400,
            accounts: 12,
            batch_size: 4,
            batches_in_flight: 4,
            burst: 16,
        };
        let ordered = super::sim::explore(1, timings, faults(0), run).expect("no invariant broke");
        assert_eq!(
            ordered.metrics.seq_gaps, 0,
            "the component reordered the tail, so the sequencer must see no gap"
        );
        assert!(
            ordered.metrics.pending_lookups > 0,
            "no lookup was made at all"
        );

        let unordered =
            super::sim::explore(1, timings, faults(2), run).expect("no invariant broke");
        assert!(
            unordered.metrics.seq_gaps > 0,
            "with the ordering taken away the sequencer has to detect a gap"
        );
    }

    /// A rate is a promise the harness makes, and it was quietly broken once: arrivals counted from
    /// `now` instead of from the previous arrival lost every gap a clock jump covered, so a run asked
    /// for 500k a second offered 17k.
    #[test]
    fn the_offered_rate_is_the_one_that_was_asked_for() {
        let target = 200_000;
        let mut plan = super::default_plan();
        plan.duration_nanos = 200_000_000;
        plan.rate = target;
        plan.accounts = 1_000;
        let prediction = super::capacity(plan).expect("no invariant broke");
        let offered = prediction.offered();
        assert!(
            (offered - target as f64).abs() < target as f64 * 0.05,
            "asked for {target}/s, offered {offered:.0}/s"
        );
    }

    /// A node is allowed to stop serving — that is what a sealed apply path is for. The harness has to
    /// report it, which means every loop that waits for the ledger to make progress needs the ledger's
    /// terminal state as an exit. This one used to spin ten million times and then panic.
    #[test]
    fn a_node_that_stops_serving_during_setup_is_reported_rather_than_waited_for() {
        let timings = crate::fakes::Timings {
            pending_nanos: 1_000,
            pending_tail_nanos: 0,
            pending_rate: 0,
            resident_holds: 1 << 16,
            flush_blocks: ledger_pending::DEFAULT_FLUSH_BLOCKS,
            resident_blocks: ledger_pending::DEFAULT_RESIDENT_BLOCKS,
            idem_nanos: 1_000,
            // A round trip long enough that several batches are outstanding together, which is what
            // lets a pair of them be answered in the wrong order.
            raft_nanos: 8_000,
            raft_tail_nanos: 0,
            day_nanos: 0,
            lifetime_days: 1,
            expiry_blocks_per_round: 0,
            store: ledger_pending::StoreModel::default(),
        };
        let faults = crate::fakes::Faults {
            reorder_every: 2,
            inbox_depth: 256,
            ..crate::fakes::Faults::default()
        };
        // One request per batch, so several are outstanding together, and enough accounts that funding
        // is still in flight when the reordered commit lands — the moment the harness used to spin at.
        let run = super::sim::Run {
            steps: 200,
            accounts: 2_000,
            batch_size: 1,
            batches_in_flight: 8,
            burst: 8,
        };
        let report = super::sim::explore(1, timings, faults, run).expect("no invariant broke");
        assert!(
            report.halted,
            "the reordered commit should have stopped the node"
        );
        assert!(
            !report.funded,
            "funding cannot finish on a node that stopped serving"
        );
    }

    /// Small and quick, and everything the capacity tests do not vary stated once.
    fn quick_plan() -> super::Plan {
        super::Plan {
            duration_nanos: 20_000_000,
            read_queue_depth: 4_096,
            accounts: 64,
            pending_nanos: 100_000,
            pending_tail_nanos: 0,
            pending_rate: 1_000_000,
            raft_nanos: 500_000,
            raft_tail_nanos: 0,
            ..super::default_plan()
        }
    }

    /// A prediction has to come out of a run that also held every invariant, and it has to be a
    /// number rather than a division by zero.
    #[test]
    fn a_capacity_run_predicts_something() {
        let prediction = super::capacity(quick_plan()).expect("no invariant broke");
        assert!(prediction.committed > 0, "nothing committed");
        assert!(prediction.throughput() > 0.0);
        assert!(prediction.latency_us[0] > 0.0, "p50 has to be positive");
    }

    /// A prediction has to be about the ledger and not about this tool: nothing refused for want of a
    /// slot, every account funded before the traffic starts, and a client that resolves holds it was
    /// told committed. Each of those, when it was wrong, turned the run into a measurement of its own
    /// harness refusing work.
    #[test]
    fn a_capacity_run_measures_the_ledger_and_not_the_harness() {
        // A round trip long enough that a request holds its slot for many ticks, which is what used
        // to make the ledger refuse the client's queue depth as overload.
        let plan = super::Plan {
            raft_nanos: 5_000_000,
            ..quick_plan()
        };
        let prediction = super::capacity(plan).expect("no invariant broke");
        assert_eq!(
            prediction.overloaded, 0,
            "the ledger was refused load it was never sized for"
        );
        let per_commit = prediction.metrics.admitted as f64 / prediction.committed.max(1) as f64;
        assert!(
            per_commit < 3.0,
            "{per_commit:.2} admitted per committed effect: the run is measuring refusals"
        );
    }

    /// A submission reaches the ledger whole. A client queue depth wider than the intake queue is what
    /// that queue fill; a chain whose second leg was dropped there is terminated by the next unlinked
    /// request instead, and the ledger judges two unrelated transfers as one atomic unit. This traffic's
    /// chains are self-funding — the first leg pays for the second — so a chain refused is a chain the
    /// client never sent.
    #[test]
    fn a_chain_reaches_the_ledger_whole_or_waits() {
        let plan = super::Plan {
            read_queue_depth: 20_000,
            ..quick_plan()
        };
        let prediction = super::capacity(plan).expect("no invariant broke");
        let metrics = prediction.metrics;
        assert!(
            metrics.linked_chains_judged > 0,
            "no chain was offered at all"
        );
        assert_eq!(
            metrics.linked_chains_rejected, 0,
            "{} of {} chains were refused, so they were not the chains the client sent",
            metrics.linked_chains_rejected, metrics.linked_chains_judged
        );
        assert_eq!(
            metrics.linked_chains_aborted, 0,
            "a chain was left unterminated"
        );
    }

    /// A tail is drawn from what came back, so a component slow enough that the client's queue depth
    /// never turned over reports a *better* tail from fewer samples, and would pass any target.
    /// The guard is what refuses it, and this is the shape that used to get through: a `require` search
    /// answered "99.9ms is fine" from a run where nothing committed at all.
    #[test]
    fn a_tail_from_a_queue_depth_that_never_turned_over_is_refused() {
        let plan = super::Plan {
            duration_nanos: 60_000_000,
            accounts: 1_000,
            // Longer than the whole measurement, so every request is still in flight when it ends.
            idem_nanos: 99_000_000,
            ..quick_plan()
        };
        let options = super::Options {
            plan,
            slo_nanos: Some(9_000_000_000),
        };
        let prediction = super::capacity(plan).expect("no invariant broke");
        assert_eq!(
            prediction.latency_us[2], 0.0,
            "the run has to be the empty-histogram shape"
        );
        assert!(
            prediction.answered <= prediction.outstanding,
            "the depth turned over after all"
        );
        assert!(
            !super::held(&options, &prediction),
            "a p99.9 of 0 from {} answers and {} still outstanding passed a 9s target",
            prediction.answered,
            prediction.outstanding
        );
    }

    /// Every counter in a prediction is measured over the same stretch. Funding is a fixed cost that
    /// does not grow with the duration, so a whole-run counter divided by a measured one answered
    /// differently at two durations — which read as the ledger changing, not the arithmetic.
    #[test]
    fn the_same_plan_answers_the_same_at_two_durations() {
        let per_commit = |duration_nanos| {
            // Enough accounts that funding is a visible share of a short measurement: with a handful,
            // a whole-run counter and a measured one agree by accident.
            let plan = super::Plan {
                duration_nanos,
                accounts: 50_000,
                ..quick_plan()
            };
            let prediction = super::capacity(plan).expect("no invariant broke");
            prediction.metrics.admitted as f64 / prediction.committed.max(1) as f64
        };
        let short = per_commit(20_000_000);
        let long = per_commit(80_000_000);
        assert!(
            (short - long).abs() < 0.2,
            "{short:.2} admitted per committed effect over 20ms but {long:.2} over 80ms"
        );
    }
}
