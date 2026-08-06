# myLedger — Sequencer Implementation Rules

Design inputs: component architecture / sequencer design / implementation plan / pending engine
design (component and storage internals).

## Working agreement

**Ask before writing code.** Say what the change is, why, and what it touches — then wait for a yes.
This covers refactors, renames and fixes noticed along the way, not just new features. Reading the code,
measuring, and running the tools need no permission; changing a file does. When a question is being
discussed, the answer is the reasoning, not a commit.

**Explain simply and briefly.** Answer in plain Korean, short sentences. Lead with the finding in one
line, then the number that supports it, then the one decision I need. No layered clauses, no restating
the design's own prose, no tables where three lines would do. The code and the documents carry the
careful wording; a reply to me does not. If an explanation needs a paragraph to set up, it is too long.

## Agent working context

`CLAUDE.md` is the canonical source for agent guidance. Keep project rules, architecture constraints,
verification commands, and document conventions here; do not duplicate them in agent-specific files.
When agent guidance needs to change, update this file and keep `AGENTS.md` as a pointer only.

Before starting work, read this file and `docs/status.md`. Then read only the documents the task calls
for: `docs/glossary.md` for terminology, `docs/request-flow.md` for pipeline changes,
`docs/design-notes.md` for the relevant design decision, `docs/scenario-coverage.md` for supported
behaviour, and `docs/tools.md` before choosing a measurement or simulation. `docs/status.md` is the
source of truth for current work and outstanding gaps.

**A decision is written in one of two places, in the same shape.** Settled ones open a `design-notes.md`
section with four lines — what was **tried**, what **broke**, what was **weighed**, what was **chosen** —
and the prose below is the evidence. Unsettled ones are an entry under *Decisions waiting on someone* in
`docs/status.md`, with the question, the **default** the code takes meanwhile, and **when that default
stops being safe**. Write the four lines when a decision closes and **move** the entry to that section's
closed list with what answered it — deleting it takes the finding behind it with it, and an entry that
vanished reads as a question nobody asked. A decision left in prose is one nobody revisits, and a default
nobody chose reads exactly like a choice.

## Coding rules

1. **DRY.** A rule lives in exactly one place. Exception: the transfer kinds stay explicit branches in the S3-judge and S5-apply hot paths. Share the *delta rule*, not the branching — and the two voids are the worked example in both directions: `VoidClient` and `VoidExpiry` are separate kinds because three stages have to tell them apart, and they share one arm in the judge because they are the same movement of money.
2. **Self-explanatory code.** Names carry the meaning; comments explain *why* only. No banner comments, no restating code.
3. **One name per concept.** `docs/glossary.md` is the list, and it matches the design documents where they have a term. Notably: *pending* is the phase, the column and the component; a *hold* is one reservation; a *chain* is the atomicity unit of a submission; a *budget group* is a lifetime property of holds — "group" alone never means a chain.
4. **Only what is needed.** No speculative extension points, no abstraction for a future that is not here yet.
5. **Struct-oriented.** State is owned by a struct, behaviour lives in `impl` methods. Avoid free functions. Traits only at the external boundary (pending / idempotency / raft ports).
6. **Errors.** No `unwrap`/`expect`/`panic` on core paths — return `Result<_, LedgerError>`. A self-invariant is checked after every tick in constant time, asserts loudly in a debug build, and in a release build counts and seals the apply path — a node whose own numbers stop adding up must not apply more of them. The whole-ledger version (`audit`) walks every account and belongs between ticks, in a test or a simulation.
7. **Unsafe is quarantined.** Only `base::spsc` may use `unsafe`, and `base` denies it everywhere else.
8. **Units and types.** Money is an integer minor unit (`Amount`), identifiers are newtypes, time fields name their unit (`_nanos`).
9. **Crates mirror the design's components.** One crate per component, at the repository root, so the component view is the directory listing. Dependency direction is enforced by the compiler, not by discipline:

```
base/          contracts and foundation: model, client protocol, layout budget, hash, spsc,
               affinity, prng, ports/{account,pending,idempotency,raft}
account/       account component: durable four-column records, DRAM-resident, persists itself
sequencer/     reactor S1..S5, slots, lane state, hold view, linked groups
pending/       pending engine (memory tier today, disk tier later)
idempotency/   dedup engine (map today, rotating generations later)
raft/          consensus and log (echo today, five nodes later)
service/       assembles the node: owns the reactor thread, the log stream drain and the client endpoint
stubkit/       what a stand-in component needs and the ledger does not: latency ranges, the lane
               ordering a real component would do itself, the worker loop that services a queue
benchkit/      shared bench harness (repeat, median, placement header) — dev-dependency only
ledgerfio/     ledger load driver: workload mixes, rates, latency distribution
ledgersim/     simulator: the real reactor on a virtual clock — invariants under fault injection
               (check), capacity against components we do not have (capacity), and the budget a
               component gets for a rate and a tail (require)
```

Each crate benchmarks what it owns. `ledgerfio` is the fio-style tool for the ledger as a whole, not
a microbenchmark. Running the ledger is `service`'s job: it owns the reactor thread and hands out a
client endpoint, `stop_token` lets anyone ask it to stop, and `shutdown` waits for the drain. The
sequencer itself has no threads, transport or clock of its own, so tests and simulations drive
`tick()` by hand. No signal handling in any library — that belongs to whoever owns the process.

No implementation of a contract lives in `base`, and no way to violate one: the lane ordering that
makes contract 1 true — and the switch that breaks it for a test — belongs to the components, so it
sits in `stubkit`. The dependency graph is the status report: `account` and `sequencer` reach for
`base` alone, while the three simulated components also pull `stubkit`, and each will drop it as it
becomes real.

Port traits live in `base`, not in the sequencer: several crates implement them, and a
component must never depend on the sequencer to find its own interface. `cargo tree -p
ledger-sequencer -e normal` shows only `base` — every component reaches the sequencer through
a port.

**State follows ownership, not convenience.** The account component owns the durable
columns and its own persistence; the sequencer owns everything volatile about a request in
flight (lane seq, the propose-time overlay, the fence counter, quarantine). The account is
called inline because the judge cannot proceed without an answer, but it is still an
external component: it is not the sequencer's field to reach into. The client is not the
sequencer's either — `Reactor::new` hands back queue endpoints and `ledgerfio` wraps them.
10. **Performance first.** No heap allocation, locking, or repeated hashing on the hot path. External paths stay separated per character so a slow path cannot block a fast one.
11. **Grouped state, grouped config.** A stage owns its state in one struct (`Batcher`, `Outbox`, `Pipeline`, `Safety`, `PendingChannel`) instead of scattering fields across the reactor. `ReactorConfig` groups by concern (`capacity`, `batching`, `linked`, `holds`, `safety`) and `validate()` refuses combinations that would misbehave silently. Anything the transport or a rate limiter decides stays outside: the sequencer receives queue ends (`Transport`) and publishes `Backpressure`, it does not size client queues or cap rates.
12. **Queues are bounded.** Every internal backlog has a limit, and reaching it pauses intake so backpressure reaches the client. Nothing grows without bound because a peer is slow.
13. **Logging never touches the hot path.** The reactor writes fixed-size log events for state transitions only — never per request — and a separate thread formats and prints them.
14. **Visibility is expressed once.** Modules are private, so a crate's public surface is the `pub use` list in its `lib.rs` and nothing else. Inside a private module `pub` is already crate-only, which is why `pub(crate)` is noise; narrow a single item only where module privacy cannot reach it — the methods of a re-exported type, such as the reactor's stage methods (`pub(super)`). A `pub` with no user outside the crate is either dead code to delete or a missing test.

## Invariant discipline

Every integrity bug found in this code so far came from one of two places, and neither is inside a
component. Most were at a **seam**: two things that had to happen together were written as two
statements, and something failed or intervened between them. The rest only appeared under **load**:
something latent that every short run reached the edge of and no short run crossed. These six rules are
those bugs generalised.

16. **A pairing is one call, not two statements.** Both overlays are taken by `take_overlays` and
    given back by `give_back_overlays`; an effect reaches both of an account's sides through one
    call, or neither. Where a pair is deliberately asymmetric, the asymmetry gets its own name
    (`settle_overlays`: the lane's promise is released while the hold's remainder starts following
    the engine's write) rather than being left as two calls that look like they drifted apart.
17. **Nothing observable changes before the fallible part succeeds.** A counter bumped or a pin taken
    before a send that a full queue can refuse is counted twice when the dispatch is retried. One
    exception, and it is deliberate: intake issues the lane seq and marks the lane entered before
    dispatch can fail, because the seq must be kept — dropping it would leave a permanent gap.
18. **One value, one owner.** What a hold has left follows the write the engine is sent, and nothing
    else writes it. Where the hot path needs a local copy of a fact that lives elsewhere — a lane's
    quarantine flag beside safety's list of quarantined lanes — one call sets both and the invariant
    check proves they agree.

    **A judgment has an owner too, and this is the half that keeps being missed.** Three defects in
    one week were the same shape: something everything depended on that nothing owned. A lane's order
    came out of whichever component a request happened to travel through. A cascade's depth was
    bounded by whichever resource ran out first. Whether a hold still existed was decided separately
    by each of the three readers that asked. In each case the invariant held by coincidence of how
    the pieces behaved, and moving one piece broke it silently. Decide it once, in one place, and let
    the readers derive — `HoldOverlay::known` is that, and its comment says why the compiler could
    not have helped: the state already existed and only its lifetime changed.
19. **Detect and stop, never detect and continue.** A commit that answers the wrong batch, or a
    committed effect that cannot be applied, means this node's own bookkeeping no longer follows the
    log: the apply path is sealed, nothing more is applied or answered, and there is deliberately no
    operator action — the drain that never completes is the signal to replace the leader. A
    contract-1 violation is different in kind: an external component is broken, our state is intact,
    so the lane is quarantined and the rest keeps serving.
20. **No bound may be enforced by the stack.** Rule 12 bounds every queue; this is the same rule for a
    backlog that is not one. A cascade of gated chains was followed by nested call, so its real ceiling —
    the slot pool, 65,536 — was enforced by a stack that held about 1,400 of those frames, and the node
    aborted where a queue would have applied backpressure. Making the two agree is not the answer: a
    stack's capacity is its size over its frame size, frame size belongs to the optimiser (debug and
    release disagreed by eight times on how long the crash took), so a limit expressed that way is one
    nobody declared and no build can check. Depth stays constant and the ceiling lives on a structure
    sized from the declaration — `Cascade`, and the pending index's `MAX_HOPS` cascade cap, which is this
    rule already stated as a number.
21. **A component's queue is part of what it does.** Idempotency records transaction ids *and* returns a
    lane in seq order, and only the first is why anything calls it. A request that skipped the component
    to avoid the record gave up the order with it, and then nothing ordered it against the requests that
    had not skipped it — a seq gap of this node's own making, which the timing hid until a change widened
    the window. Where a request needs one and not the other, ask for one and not the other
    (`IdemAsk::Serialize`); do not skip the component.

## Layout discipline

Cache layout is a build-time contract, not a convention:

- Every watched type **declares how it sits against the cache line** and the build checks the claim: `LineFit::Inside` (fits in every supported line and is aligned so an array never crosses one), `LineFit::WholeLines` (starts on and occupies whole lines on the selected build target), or `LineFit::Straddles(reason)` — an exception that has to say why. Breaking a claim fails the build, and `ledgerfio layout` prints the claim next to the size.
- Prefer `Inside` for small hot state: it cannot straddle and costs a fraction of a whole line per value. `WholeLines` is for random-access state too big for one line, and for cross-thread isolation (`CachePadded`). Exceptions are measured before being taken — see design notes §5.
- `CACHE_LINE` and the `cache_aligned!` macro in `ledger-base` are the only place alignment is expressed. The workspace Cargo configuration selects 128 bytes for Apple Silicon; x86 and generic ARM64 select 64 bytes unless a verified ARM64 deployment changes that central target configuration. `repr(align(..))` accepts literals only, so it is written once inside the macro.
- `cache_aligned!` emits const assertions (`size_of <= CACHE_LINE`, `align_of == CACHE_LINE`). Adding a field that overflows the line **fails the build** instead of silently regressing throughput.
- Each crate lists the hot types it owns in its own `HOT_TYPES` with a size budget; `ledgerfio layout` prints them all.
- Field access stays local: hot structs keep private fields and expose semantic methods, so a layout change (e.g. AoS to SoA) touches its owning module only.

## Testing rules

- No test sprawl. Test invariants and contracts; the test name states the expectation.
- Two kinds, kept apart. A **unit test** lives next to the logic it covers (`#[cfg(test)] mod tests`), builds one struct, and starts no threads: seq and quarantine rules, batch cutting, budget rules, chain assembly, config validation. An **integration test** lives in `tests/`, drives `tick()` through the shared harness with the stub components, and asserts on what a client sees. One file per behaviour the file is named after.
- Required axes: accounting identities (including under refused commits and under commits answered out of order, where the rollback and pairing paths are the risk), seq-gap detection and quarantine with its recovery, four-kind delta correctness (partial settle included), duplicate detection, path separation (no head-of-line blocking), bounded queues refusing work rather than growing, allocation-free steady state.
- One check for what must always hold. `assert_consistent` on the test harness asserts both identities, that no column went negative and that no reservation survived; any test that moves money ends with it, instead of each test picking its own subset.
- **`cargo test` without `--release`, always.** Rule 6's self-invariants and every `debug_assert` are compiled out of a release build, so a release-only test run silently skips the layer that catches a broken invariant before it becomes a wrong answer. This is not hypothetical: the overlay's `unpin without a pin` assertion was firing on seven of `ledgersim`'s sixteen sweep seeds, unheard, because the sweep was only ever run in release — two pin-accounting defects sat behind it. Run the release build too, for the numbers; run the debug build to be told.
- Performance is verified two ways:
  - **Deterministic performance tests** — time-independent metrics such as zero heap allocations per request, or layout budgets.
  - **Benchmarks** — each crate's own `benches/` (shared harness in `benchkit`) plus the `ledgerfio` load driver. Benchmarks report numbers, they do not assert absolute thresholds.

## Local runs

`ledgerfio` drives the ledger the way `fio` drives storage: workload mixes, target rates, latency distribution. Every phase stays runnable locally with the workloads that exist at that point.

```
make verify                    # all of it: tests in debug, then every workload and mode in release
cargo test                     # debug, so rule 6's asserts and every debug_assert are live
cargo run --release -p ledgerfio -- run --workload hold-settle --duration 5s --accounts 100k
cargo run --release -p ledgerfio -- run --workload linked --repeat 3 --pin 2
cargo run --release -p ledgerfio -- run --workload hold-settle --skew 4 --cpu
cargo run --release -p ledgerfio -- run --sweep raft-rtt=200,1000,5000 --slo-p999 50ms
cargo run --release -p ledgerfio -- layout
# a run stops on SIGINT or SIGTERM: the driver stops submitting, the service drains, the report prints
cargo bench -p ledger-account   --bench columns  -- --repeat 5 --pin 2
cargo bench -p ledger-sequencer --bench pipeline -- --repeat 5 --pin 2
```

Every measurement reports the thread placement it got (`pinned`, `performance-qos`, or
`scheduler-default`) and benchmarks report median/min/max, because absolute numbers without
those two facts are not comparable.

Core utilisation and the per-stage split need `--cpu`, which times each stage: the share of
ticks that found work is not utilisation, because one tick can carry hundreds of requests. A
profiled run trades a few percent of throughput for that, so saturation and peak throughput are
read from different runs. `--slo-p999` turns a run into a check (exit 1 on failure), and
`--sweep knob=a,b,c` prints one row per value instead of one report per shell invocation.

15. **Do not rebuild what exists.** Write it here only when it is the performance contract or a design invariant, and say which:

- **Hand-rolled on purpose.** The cache padding and the layout claims: alignment is expressed in one macro at the target's own line size; `Inside` claims are checked against all of `SUPPORTED_LINES`, while deliberately padded `WholeLines` claims are checked against the selected target line. That preserves portable packed state without paying another target's padding cost — which is the part a padding wrapper does not give; the `Clock` seam, because a test that waits on real time is not a test; the fixed-size log event ring, because logging may not allocate on the reactor's thread; the small buffer pool; and the stand-in machinery in `stubkit`, which exists to be deleted.
- **Replaced by a crate.** Latency quantiles (`hdrhistogram`, after the hand-rolled buckets were found to over-report p99.9 in the load driver's own runs), argument parsing (`lexopt`), signal handling (`signal-hook`, which registers through `sigaction` — the raw `signal()` it replaced can reset its own handler after the first signal), the hasher (`rustc-hash`, which is what the hand-written one was a copy of), and the load driver's JSON line (`serde`, so a new field cannot land in the wrong place). Consensus, io_uring and any wire format go the same way when those paths become real.
- **Serialisation belongs to whoever has a wire.** `serde` is a dependency of the load driver, not of the crates it measures: the JSON shape is that tool's output format, not the ledger's.
- **Weighed and refused.** A hardware profile for the simulator (`ns = cycles/freq + misses × dram_latency`, the shape the Python model uses): measuring its own inputs refuted the form, because the stages' misses overlap and the formula prices each at full latency. What replaces it is the measured curve of cost against working set, plus `--cost-*` for another machine and `--cost-scale` for one nobody can run. Design notes §10 has the numbers and the conditions.
- **Weighed and refused.** A bench harness (`divan`, `criterion`): many transitive crates and a proc macro for what `benchkit` does in a short file, and neither knows about thread placement, which every number here has to report. What was actually missing was one statistic — the spread between repeats — and the samples were already there.
- **Under review, and the default answer is the crate.** The SPSC ring is the only `unsafe` module, and a widely used crate is the safer place for lock-free code. It stays only because it is measurably faster than `rtrb` on both single pushes and every batch size measured. The comparison lives in `base`'s spsc bench as a dev-dependency, so the claim is re-runnable rather than remembered — `cargo bench -p ledger-base --bench spsc`. Switch when the margin stops mattering or the ring needs changing: safety wins by default, and the numbers are what buy the exception. The `unsafe` is covered by tests for order, partial batches, wrap-around and dropping exactly once.
- **A swap needs its contract stated first.** Write the test that says what the current code guarantees — the histogram's quantile error, the ring's order and drop behaviour, that a stop request is not consumed by reading it — then replace, and the test decides whether the replacement is equivalent.

Tokio arrives only when real external I/O does, and never owns the reactor's logical scheduling.
