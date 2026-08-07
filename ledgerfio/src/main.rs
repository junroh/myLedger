mod cli;
mod client;
mod clock;
mod histogram;
mod report;
mod runner;
mod workload;

use cli::{Cli, Command};
use ledger_pending::{BLOCK_BYTES, RECORDS_PER_BLOCK};
use report::RunReport;
use runner::Runner;
use workload::WorkloadKind;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match Cli::new(args).parse() {
        Ok(Command::Run(options)) => run_workload(options),
        Ok(Command::Sweep { base, knob, values }) => sweep(base, &knob, &values),
        Ok(Command::Layout { json }) => print_layout(json),
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

/// Every crate's sizing units, in the order a reader meets them. Gathered here for the same reason
/// `HOT_TYPES` is: each crate declares what it owns, and the tool is what puts them side by side.
fn sizing() -> Vec<(&'static str, &'static ledger_base::SizedPart)> {
    [
        ("sequencer", ledger_sequencer::SIZING),
        ("accounts", ledger_account::SIZING),
        ("idem", ledger_idempotency::SIZING),
        ("pending", ledger_pending::SIZING),
        ("consensus", ledger_raft::SIZING),
    ]
    .into_iter()
    .flat_map(|(owner, parts)| parts.iter().map(move |part| (owner, part)))
    .collect()
}

fn print_layout(json: bool) {
    if json {
        print_sizing_json();
        return;
    }
    // Per-line packing is printed for both line sizes the claims are checked against: it is the same
    // struct either way, but not the same number of misses.
    println!(
        "{:<14} {:>6} {:>6} {:>9} {:>8}  line fit",
        "type", "size", "align", "per 128B", "per 64B"
    );
    let hot_types = ledger_base::HOT_TYPES
        .iter()
        .chain(ledger_sequencer::HOT_TYPES)
        .chain(ledger_pending::HOT_TYPES);
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
    println!();
    println!(
        "{:<28} {:<10} {:>10}  unit",
        "sizing part", "owner", "bytes/unit"
    );
    for (owner, part) in sizing() {
        println!(
            "{:<28} {:<10} {:>10}  {}",
            part.name,
            owner,
            part.bytes,
            part.unit.name()
        );
    }
    println!(
        "a bucket count is next_pow2(entries * 8 / 7), so a hash table's cost is a staircase; \
         {RECORDS_PER_BLOCK} records fit a {BLOCK_BYTES}B block"
    );
}

/// The same table as a document, for a sizing model that would otherwise hard-code these numbers and
/// go quietly wrong when a struct changes. `bucket_rule` and `records_per_block` are here because a
/// consumer cannot derive them from a unit cost, and getting either wrong is a whole factor rather
/// than a rounding.
fn print_sizing_json() {
    let parts: Vec<serde_json::Value> = sizing()
        .into_iter()
        .map(|(owner, part)| {
            serde_json::json!({
                "name": part.name,
                "owner": owner,
                "unit": part.unit.name(),
                "bytes": part.bytes,
            })
        })
        .collect();
    let document = serde_json::json!({
        "cache_line": ledger_base::CACHE_LINE,
        "bucket_rule": "next_power_of_two(entries * 8 / 7)",
        "records_per_block": RECORDS_PER_BLOCK,
        "block_bytes": BLOCK_BYTES,
        "parts": parts,
    });
    println!("{document}");
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
    println!(
        "  --accounts <n>              (100k) working set: cache residency and lane contention"
    );
    println!("  --seed <n>                  (0x5eed1234) makes a run repeat exactly");
    println!();
    println!("how hard to send it:");
    println!("  --duration <5s|500ms>       (3s) measured phase; funding is not measured");
    println!("  --rate <tx/s>               (0 = as fast as accepted; set a rate to measure latency at load)");
    println!(
        "  --in-flight <n>             (20k) unanswered requests; with rate 0 this sets latency"
    );
    println!(
        "  --client-batch <n>          (64) transfers per submission; a chain always goes whole"
    );
    println!("  --client-queue <n>          (65536) queue depth between client and reactor");
    println!("  --repeat <n>                (1) run n times, report median throughput");
    println!();
    println!("sequencer tuning:");
    println!("  --batch-size <n>            (1000) effects that make a consensus batch full");
    println!("  --batch-max <n>             (10k) ceiling on one batch; cut at a chain boundary");
    println!("  --batch-queued <n>          (--batch-max) judged effects waiting for consensus before intake pauses");
    println!("  --batch-linger <200us>      (200us) how long a partial batch waits: the low-load latency floor");
    println!("  --raft-in-flight <n>        (8) proposals outstanding at once");
    println!(
        "  --pin <cpu>                 (unset) bind the reactor core (Linux; elsewhere a QoS hint)"
    );
    println!();
    println!("external components (simulated, us or min:max):");
    println!("  --raft-rtt <us>             (900:1400) consensus round trip: the latency floor");
    println!("  --store-read <us>           (0) what a block read costs the engine: the disk it has not got");
    println!(
        "  --store-write <us>          (0) what sealing a block costs; synchronous, so it holds the engine's thread"
    );
    println!(
        "  --store-sync <us>           (0) what making the sealed blocks durable costs; holds the thread too"
    );
    println!(
        "  --store-iops <n>            (0) reads a second that store can serve, 0 for no ceiling"
    );
    println!(
        "  --store-read-depth <n>      (128) reads the volume holds at once; past it reads are refused"
    );
    println!("  --store-read-cache <n>      (64) blocks kept from answered reads; 0 for none");
    println!(
        "  --store-write-depth <n>     (128) writes and barriers its lane holds; its own arithmetic"
    );
    println!(
        "  --store-fault-every <n>     (0) refuse every nth store call, so the seal is exercised"
    );
    println!(
        "  --store-corrupt-every <n>   (0) flip a bit in every nth block read: a device that answers wrongly"
    );
    println!("  --store-dir <path>          (unset) put segment files here; unset is memory");
    println!("  --store-read-threads <n>    (0) threads issuing the store's preads; 0 reads synchronously");
    println!(
        "  --store-write-lane <0|1>    (1) pwrite and fsync on a thread of their own; 0 is the baseline"
    );
    println!(
        "  --snapshot-dir <path>       (unset) put snapshots here; its own volume, so its own flag"
    );
    println!(
        "  --snapshot-every <n>        (0) log positions between snapshots — a distance, so no clock"
    );
    println!(
        "  --snapshot-bytes <n>        (4k) bytes of the stream one worker round writes; whole blocks"
    );
    println!("  --snapshot-shadow <n>       (2M) buckets the stable read may hold before a dump is dropped");
    println!("  --overlay-limit <n>         (1M) ceiling on the sequencer's own hold decisions; in flight bounds it");
    println!("  --idem-latency <us>         (1:5) idem; every request pays it");
    println!("  --violate-order-every <n>   (0) return every nth lane reply out of order");
    println!("  --raft-fail-every <n>       (0) refuse every nth batch");
    println!();
    println!("workload shape:");
    println!(
        "  --skew <f>                  (1.0 uniform) higher concentrates traffic on few accounts"
    );
    println!(
        "  --external-ratio <0.3|30%>  (0) share of debits on the unconstrained clearing account"
    );
    println!(
        "  --resolve-after <n>         (0) resolve a hold once n more exist: its age, and so which"
    );
    println!(
        "                              engine window answers it — 0 resolves each one at once"
    );
    println!();
    println!("what the engine is sized for (it derives every window from these):");
    println!("  --daily-arrivals <n>        (1m) transfers a day; scales both memory windows");
    println!("  --retention-days <n>        (2) how long a hold may live: with the share below, the index");
    println!(
        "  --grace-days <n>            (1) slack before deletion, so it is never early: costs this"
    );
    println!(
        "                              many days of capacity and buys away every early-deletion cause"
    );
    println!(
        "  --survivor-share <50%>      (50%) still unresolved when retention ends: sizes the index"
    );
    println!(
        "  --flush-survivors <50%>     (50%) still unresolved when their block is carried_on: sizes"
    );
    println!(
        "                              residency, and `died in buffer` measures the same thing"
    );
    println!(
        "  --flush-window <hours>      (1) how long a record may go unwritten: a recovery bound"
    );
    println!(
        "  --residency <hours>         (24) how long it stays readable in memory: a latency bound"
    );
    println!(
        "  --index-budget <bytes>      (1073741824) refuse a declaration needing a larger index"
    );
    println!(
        "  --expiry-days <n>           (0) move the engine's calendar this many days over the run,"
    );
    println!(
        "                              evenly. 0 leaves it on the wall clock, where a run of seconds"
    );
    println!(
        "                              never crosses a day and the expiry sweep is unreachable. Past"
    );
    println!(
        "                              retention+grace to reach the expiry of this run's own holds"
    );
    println!(
        "  --expiry-blocks <n>         (2) blocks of an expiring day one sweep round reads. Bounds the"
    );
    println!(
        "                              work and the voids both, at 51 records a block: see `sweep`"
    );
    println!();
    println!("measuring:");
    println!("  --sweep <knob=v1,v2>        run once per value and print one row each");
    println!(
        "  --cpu                       time each reactor stage; changes the throughput it reports"
    );
    println!("  --slo-p999 <50ms>           fail the run (exit 1) when end-to-end p99.9 is worse");
    println!();
    println!("output:");
    println!("  --json                      one JSON line instead of text");
    println!("  --log                       print the sequencer log events to stderr");
}
