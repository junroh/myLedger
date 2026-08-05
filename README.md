# myLedger

A high-performance financial ledger. The sequencer is built; the components around it are
in-memory stand-ins.

## What it does

Every transfer moves money between exactly two accounts, so double-entry holds by construction. A
transfer is one of four kinds:

- **single-phase** — posted immediately
- **hold** — reserves money without posting it
- **settle** — moves a hold's money to posted, in whole or in part
- **void** — releases what is left of a hold

Transfers can also be **linked**, which makes a run of them commit or roll back together, and a
hold can join a **shared budget group**, which must then be resolved as a whole.

The sequencer issues an order per account, judges every request against balances, and hands
batches to consensus. It keeps no durable state: the consensus log is the truth, and everything the
sequencer holds is one leader's work in progress.

## Components

| component | state |
|---|---|
| sequencer | built |
| account balances | in-memory, persistence not built |
| pending engine (holds) | in-memory tier; disk tier and expiry sweep not built |
| idempotency | in-memory map; the one-hour window is not expired yet |
| consensus | commits locally after a simulated round trip; no replication |
| rate limiter | not built |

The sequencer reaches all of them through ports, so each can be replaced without touching it. The
three that are simulated share one crate of stand-in machinery — latency, lane ordering, a worker
thread — which is how the dependency graph shows what is still a stub.

## Running it

The ledger process:

```
cargo run --release -p ledgerd -- --accounts 1000
```

It serves no clients yet — a network listener has nowhere to attach until it is written — so the
way to drive the ledger is the load tool, which starts a service of its own:

```
cargo run --release -p ledgerfio -- run --workload hold-settle --duration 5s --accounts 100k
cargo run --release -p ledgerfio -- run --workload hold-settle --skew 4 --cpu
cargo run --release -p ledgerfio -- run --sweep rate=1m,2m,3m --slo-p999 50ms
cargo run --release -p ledgerfio -- help
```

`--cpu` says which stage the core spends its time in, `--skew` concentrates traffic on a few
accounts, `--slo-p999` makes a run pass or fail, and `--sweep` repeats it once per value.

Both stop on SIGINT or SIGTERM: they stop submitting, let the ledger drain, and report.

The simulator asks three questions the load driver cannot. `check` runs the real reactor on a virtual
clock with the components faked — latency and its tail, refused commits, commits answered out of order,
aggressive eviction — and asks the ledger's own audit after every step; a seed is the whole story, so a
failure reproduces. `capacity` advances that same clock by what the work would have cost, to ask what
the ledger does against a disk-backed pending engine or a slower core. `require` runs that backwards:
given a rate and a tail, how slow may one component be.

```
cargo run --release -p ledgersim -- check --seeds 512 --steps 3000
cargo run --release -p ledgersim -- check --seed 137          # reproduce one failure
cargo run --release -p ledgersim -- capacity --pending-us 5000 --raft-us 2000
cargo run --release -p ledgersim -- capacity --pending-rate 64000 --qd 60000
cargo run --release -p ledgersim -- require --rate 300000 --pending-hit 0 --slo-p999-us 60000
cargo run --release -p ledgersim -- require --solve raft --rate 300000 --slo-p999-us 20000
```

Its per-stage costs come from `ledgerfio run --cpu`, so the logic is the ledger's and only the numbers are
estimated. Predictions say so on every line they print — including which limit the run found, since the
client's queue depth divided by the latency is a ceiling the ledger never sees, and depth has to cover
rate times latency. The components are black boxes with a latency, a tail and a rate: the rate matters as
much as the latency, because a component modelled as latency alone has unlimited parallelism.

Tests and benchmarks:

```
cargo test        # debug on purpose: the self-invariants and every debug_assert are live only here
cargo bench -p ledger-sequencer --bench pipeline -- --repeat 5
cargo bench -p ledger-account   --bench columns  -- --repeat 5
cargo run  --release -p ledgerfio -- layout
```

## Dependencies

The workspace uses only `std` today, which is a starting point rather than a goal. What is written
here is what carries the performance contract or a design invariant: the cache padding and layout
claims, the queue between threads, the injectable clock, the allocation-free log ring. Anything else
is a candidate for a crate that already does it: the hasher, latency quantiles, argument parsing,
signal handling and the load driver's JSON already come from one, and consensus, io_uring and a wire
format will. A replacement lands after a test states what the current code
guarantees, so the test can decide whether the replacement is equivalent.

## Documents

- `docs/status.md` — what is built and verified, and what is not, with why each gap is still open
- `docs/tools.md` — the five tools that measure and check this code, and which question each answers
- `docs/request-flow.md` — what happens to one transfer, stage by stage, and where each failure is caught
- `docs/glossary.md` — one name per concept, and how it maps to the code
- `docs/design-notes.md` — decisions that the code cannot explain on its own, with the measurements
  behind them
- `docs/scenario-coverage.md` — which usage scenarios work, and what is deliberately left out
- `CLAUDE.md` — the rules this code is written to
