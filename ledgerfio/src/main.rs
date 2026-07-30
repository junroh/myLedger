mod cli;
mod client;
mod clock;
mod histogram;
mod report;
mod runner;
mod workload;

use cli::{Cli, Command};
use report::RunReport;
use runner::Runner;
use workload::WorkloadKind;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match Cli::new(args).parse() {
        Ok(Command::Run(options)) => run_workload(options),
        Ok(Command::Sweep { base, knob, values }) => sweep(base, &knob, &values),
        Ok(Command::Layout) => print_layout(),
        Ok(Command::Help) => print_help(),
        Err(err) => {
            eprintln!("ledgerfio: {err}");
            std::process::exit(2);
        }
    }
}

/// One run per value, so a knob's effect is read down a column instead of across shell history.
fn sweep(base: cli::Options, knob: &str, values: &[String]) {
    let mut failed = false;
    let mut header = false;
    for value in values {
        if interrupted() {
            break;
        }
        let mut options = base;
        if let Err(err) = Cli::apply(&mut options, knob, value) {
            eprintln!("ledgerfio: {err}");
            std::process::exit(2);
        }
        let report = Runner::new(options).run();
        if options.json {
            report.print_json();
        } else {
            if !header {
                RunReport::print_row_header(knob);
                header = true;
            }
            report.print_row(value);
        }
        failed |= !report.passed();
    }
    if failed {
        std::process::exit(1);
    }
}

/// Repeats the same run and reports the median, because a single number is not comparable.
fn run_workload(options: cli::Options) {
    let mut throughputs = Vec::new();
    let mut failed = false;
    for _ in 0..options.repeat {
        if interrupted() {
            break;
        }
        let report = Runner::new(options).run();
        if options.json {
            report.print_json();
        } else {
            report.print_text();
        }
        throughputs.push(report.throughput());
        failed |= !report.passed();
    }
    if options.repeat > 1 {
        throughputs.sort_by(|left, right| left.total_cmp(right));
        println!(
            "median of {} runs: {:.0} tx/s (min {:.0}, max {:.0})",
            throughputs.len(),
            throughputs[throughputs.len() / 2],
            throughputs[0],
            throughputs[throughputs.len() - 1]
        );
    }
    if failed {
        std::process::exit(1);
    }
}

fn interrupted() -> bool {
    ledger_base::Signals::requested()
}

fn print_layout() {
    // Per-line packing is printed for both line sizes the claims are checked against: it is the same
    // struct either way, but not the same number of misses.
    println!(
        "{:<14} {:>6} {:>6} {:>9} {:>8}  line fit",
        "type", "size", "align", "per 128B", "per 64B"
    );
    let hot_types = ledger_base::HOT_TYPES.iter().chain(ledger_sequencer::HOT_TYPES);
    for layout in hot_types {
        println!(
            "{:<14} {:>6} {:>6} {:>9} {:>8}  {}",
            layout.name,
            layout.size,
            layout.align,
            layout.per_line_on(128),
            layout.per_line_on(64),
            layout.fit_name()
        );
    }
    println!(
        "cache line: {} bytes here; claims checked against {:?}",
        ledger_base::CACHE_LINE,
        ledger_base::SUPPORTED_LINES
    );
}

fn print_help() {
    let workloads: Vec<&str> = WorkloadKind::all().iter().map(|kind| kind.name()).collect();
    println!("ledgerfio <command> [options]");
    println!();
    println!("commands:");
    println!("  run       drive a workload against the ledger");
    println!("  layout    print hot struct layout against its budget");
    println!();
    println!("what to send:");
    println!("  --workload <{}>", workloads.join("|"));
    println!("                              (single-phase) mix of transfer kinds");
    println!("  --accounts <n>              (100k) working set: cache residency and lane contention");
    println!("  --seed <n>                  (0x5eed1234) makes a run repeat exactly");
    println!();
    println!("how hard to send it:");
    println!("  --duration <5s|500ms>       (3s) measured phase; funding is not measured");
    println!("  --rate <tx/s>               (0 = as fast as accepted; set a rate to measure latency at load)");
    println!("  --in-flight <n>             (20k) unanswered requests; with rate 0 this sets latency");
    println!("  --client-batch <n>          (64) transfers per submission; a chain always goes whole");
    println!("  --client-queue <n>          (65536) queue depth between client and reactor");
    println!("  --repeat <n>                (1) run n times, report median throughput");
    println!();
    println!("sequencer tuning:");
    println!("  --batch-size <n>            (1000) effects that make a consensus batch full");
    println!("  --batch-max <n>             (10k) ceiling on one batch; cut at a chain boundary");
    println!("  --batch-queued <n>          (--batch-max) judged effects waiting for consensus before intake pauses");
    println!("  --batch-linger <200us>      (200us) how long a partial batch waits: the low-load latency floor");
    println!("  --raft-in-flight <n>        (8) proposals outstanding at once");
    println!("  --pin <cpu>                 (unset) bind the reactor core (Linux; elsewhere a QoS hint)");
    println!();
    println!("external components (simulated, us or min:max):");
    println!("  --raft-rtt <us>             (900:1400) consensus round trip: the latency floor");
    println!("  --pending-latency <us>      (100:800) hold lookup; settle and void pay it, and their lane waits");
    println!("  --idem-latency <us>         (1:5) dedup; every request pays it");
    println!("  --violate-order-every <n>   (0) return every nth lane reply out of order");
    println!("  --raft-fail-every <n>       (0) refuse every nth batch");
    println!();
    println!("workload shape:");
    println!("  --skew <f>                  (1.0 uniform) higher concentrates traffic on few accounts");
    println!("  --external-ratio <0.3|30%>  (0) share of debits on the unconstrained clearing account");
    println!();
    println!("measuring:");
    println!("  --sweep <knob=v1,v2>        run once per value and print one row each");
    println!("  --cpu                       time each reactor stage; changes the throughput it reports");
    println!("  --slo-p999 <50ms>           fail the run (exit 1) when end-to-end p99.9 is worse");
    println!();
    println!("output:");
    println!("  --json                      one JSON line instead of text");
    println!("  --log                       print the sequencer log events to stderr");
}
