# Status

What is built and verified, and what is not. Written to be read before picking up work: the design
reasoning lives in `design-notes.md`, the terms in `glossary.md`, and how to run things in `tools.md`.

Verified at the time of writing with **`make verify`** — the tests in debug, the release build, every
`ledgerfio` workload, `ledgerfio layout`, and all three `ledgersim` modes. It is a target rather than a
list here because a list here is what drifted: a workload that aborted on the reactor thread went two
commits unnoticed, since `cargo test` runs for milliseconds and reaching that defect took a second of
release build. What `make verify` still does not cover is `cargo bench -p ledger-pending` and `ledgerd`
draining on SIGTERM, both run by hand.

## Broken

**`hold_expiry::a_hold_the_client_resolved_is_never_resolved_twice` fails about one run in fifty.** Found
by `make verify` on its first run, which is the argument for the target. Present at `3bdda52`, before the
change that gave the ledger's own resolutions an idempotency dependency, so that change did not
introduce it.

What a captured failure says, with the lane state the harness now prints:

```
lane AccountId(10) last_seq=145 in_flight=38 awaits_pending=false quarantined=false
judged: 143  committed: 105  proposed_batches: 8  commit_failures: 0
holds_expired: 101  expiry_refused: 11  pending_removes: 52
```

Thirty-eight judged effects were never committed, which is exactly the lane's `in_flight`, and eight
proposals were made against a `batching.in_flight` of eight. So consensus is holding every proposal it
is allowed to hold and answering none, and nothing else can be proposed behind them. Not a lane stuck on
a component — no pending reply is outstanding and the lane is not quarantined — and not a refused
commit. A leak in what counts as a proposal in flight would look exactly like this, and so would an echo
that loses one.

**A second shape, seen before the negative-column check below and not since.** The same test reached a
pending column of **-80** against an expected 5: sixty-seven voids judged good against fifty-one written
holds, sixteen too many, each releasing five. The ledger released reservations it did not hold. Whether
that and the stall above are one defect or two is not established — the check turns the write into a
seal, so the next occurrence will stop at the cause instead of drifting past it.

## Built

### The sequencer

The five stages, the slot pool, lane state, linked chains, budget groups, the batcher and the
outbox. Contract-1 detection with quarantine and its recovery, fail-stop on enough lanes lost, and
the apply-path seal for a commit that cannot be applied. Both accounting identities are asserted by
every test that moves money and by every simulator seed.

**Order exemption is one clause:** a request whose debit account is unconstrained keeps no place in
its lane — no seq, no continuity check, no fence, and nothing queues behind it. Resolutions
included. What that gives up in contract-1 coverage is replaced by a data check: the reply carries
how many committed decisions the engine had applied, the request carries how many it was dispatched
behind, and fewer means the engine answered from state older than its own queue (`stale_answers`,
quarantine). Design notes §1.

### The pending engine

Real internals, on its own thread, reached through a two-part port: an inline half that answers from
the sequencer's own decisions and cannot refuse, and a queued half that carries writes, lookups and
fences in one order.

- **Index.** Cuckoo (2,4), eight-byte slot (16-bit fingerprint | ambiguity bit | 47-bit address),
  cascade cap 128, load target 0.90, and it never grows. Correctness is detection, not probability:
  a shared fingerprint is found at insert and marked, and only marked slots read a record. Design
  notes §11.
- **Records on blocks.** 4KB blocks, 80-byte records, little-endian by declaration, append-only —
  a changed remainder is a new version at a new address. Design notes §12.
- **Two memory windows, and they are not the same window.** The writeback buffer holds what is not
  written yet (the flush window, an hour: a recovery bound), and residency holds what is written and
  still worth keeping in memory (a day: a latency bound). Residency holds only survivors, because
  what is resident has been compacted.
- **Every size is derived from declared business inputs** (`PendingCapacity`), never set beside
  them, and an incoherent declaration is refused at startup with exit 2 rather than becoming a
  window nobody meant.
- **Compaction.** A block leaves the buffer carrying only what the index still points at. A record
  is alive exactly when the index points at it, so compaction reads nothing.
- **The apply path reads nothing.** A committed decision carries what the engine would otherwise
  read back. One exception, which is a fallback rather than a hole: a resolution judged inside the
  chain that created the hold has no record, sends zero, and the engine reads the version it
  appended moments ago.
- **The orderer.** Places reserved at dequeue and filled at completion, so reads that finish in the
  device's order still leave in the lane's. Order-wait and delivery-wait are reported separately.
- **The overlay holds no record.** Only what the sequencer has decided and not handed over, bounded
  by requests in flight. The record belongs to the engine and the reply carries it to the slot of
  the request that asked (rule 18). Design notes §2's correction.
- **The engine can speak first.** A third direction on the port (`notices`), with a channel of its
  own so news the engine sends is neither behind a reply nor in front of one. Two notices exist: a
  committed hold the index could not take seals the apply path (rule 19, where it used to be
  detect-and-report), and a hold that outlived its retention is proposed for release. Design notes
  §13.

### Retention, and the expiry that makes it true

A segment is a day, its number is the day modulo the segments an address has room for, and a day that
runs out is emptied by releasing whatever survived it. Design notes §14.

- **Deletion is never early, and that is the edge that matters.** Late costs space; early refuses a
  resolution still entitled to arrive, which is a wrong answer. `grace_days` (default 1) is the one
  number that buys away every source of early deletion — a segment's own coarseness, a wall clock
  jumping forward, a sweep that has not run — and it costs exactly that many days of capacity, which
  `declared_maximum` is sized for.
- **The deadline is computed, not stored.** A record carries no timestamp and stays 80 bytes: expiry
  is `segment's day + retention + grace`, read from the current configuration. So a configuration
  changed and restarted applies to records already written, which is what a retention promise needs.
- **Expiry is what makes the index's declared maximum true.** Without it a hold never leaves the
  index, and a long-running node eventually passes the maximum it was sized for and seals.
- **A day's blocks go back once nothing points into it.** A whole pass of the index finding nothing
  in the expiring segment is the one moment they are known to be dead, and handing them back is the
  only way the store shrinks — records are written once and never rewritten. Without it a run's total
  would be holds created rather than holds alive, which is the figure the capacity estimate rests on.
- **The void is judged, not applied.** A settle the client submitted may be in flight for the same
  hold, and only the judge sees both. Its id is derived from the hold, so two leaders propose the
  same one and the second is a duplicate — which is why the top bit of a transaction id is reserved
  and clients are refused it.
- **The engine is told the day rather than reading a clock.** One reading per day, wall time because
  retention is a calendar promise that outlives a restart, and injectable (`DaySource`) because a
  window measured in days is one no test could otherwise reach.

### The tools

`ledgerfio` drives the ledger — six workloads, rates, sweeps, an SLO gate, layout, and a store model
with latency and an IOPS ceiling. `ledgersim` runs the real reactor on a virtual clock in three
modes: `check` (invariants under fault injection), `capacity` and `require`. `ledgerd` assembles a
node and drains on a signal.

Both tools can now reach the read path for honest reasons: `--resolve-after` gives a hold an age, so
a resolution reads a record at a declared age rather than one written moments ago. `check` draws
narrow windows for three seeds in four, so the fetch path runs while faults are on, and the sweep
test asserts the store was reached.

## Not built

### The expiry throttle has no policy

The throttle is built, and it is a throttle rather than a rate limiter on purpose. It paces in three
places, none of which refuses anything: a bounded slice per round (`expiry_per_round`), nothing more
offered while the last slice is unanswered (`PendingWorker::sweep_expiry`), and no second walk of the
index until the index has moved (`Sweep::waiting_at`). That is the only shape that can be correct
here — a refused void has nobody to hear the refusal, and a void nobody hears is a pending column
reserved for good. A rate limiter belongs at the client edge, where a refusal reaches a client and
becomes an error to retry.

What is missing is the policy that sizes the slice: today it is a constant, where it should follow
what the sequencer is being asked to do. Falling behind deletes late, which is the safe direction, so
this is a capacity question rather than a correctness one. The requirement is derived rather than
guessed:

```
drain rate >= a day's survivors / a day
```

Sizing it needs a measurement nobody has taken: what a day's worth of voids does to the tail while
clients are being served. The seam the policy needs is already there — `expiring(want, into)` takes
its bound per call — so what waits is the number, not the code.

### The negative answer the design asked for is refused

Refused rather than deferred, which is why it sits here with a reason instead of on a list. The engine's
design wanted a negative answer that tells "resolved or expired" from "never existed". Under the
retention promise that is unimplementable: answering "expired" needs per-hold state kept past
retention, and a tombstone is exactly the data the promise says is deleted. Design notes §14 has the
argument and what serves the need instead — telling the client when the void happens, which needs a
push channel the ledger does not have.

### Recovery is not real

There is no checkpoint. The flush window is an hour *because* unflushed records are memory-only and
have to be in one, so the reason for that number cannot be verified yet. What is verified is that
the split exists and that residency keeps IO off resolutions inside it.

### Smaller, and each with a reason

- **The load-factor alarm is reporting only.** The index reports its load against the target and
  its worst cascade, but nothing warns or refuses before an insert fails. The channel it would warn
  over now exists; what is missing is the threshold and what a warning should make the node do,
  which is a question about operations rather than about the code.
- **Consensus is an echo**, the dedup map has no expiry, and the log has no compaction. All three
  grow with a run, which the tools say out loud.

Four earlier entries are closed: the in-chain fallback read is now exercised by the `linked`
workload (half its chains create a hold and resolve it inside the chain), the stub orderer's
metrics struct is `FakeOrderWait` so `OrderWait` names one thing, unordered replies skip lane
ordering in the dedup stub and both fakes, and the simulator now creates holds on the unconstrained
account so `check` exercises the order exemption itself — the sweep test asserts it did.

## Where the numbers live

No measured number is written into these documents as a fact to be trusted; each one names the
command that reproduces it. The three that decide the most:

- `ledgerfio run --workload partial-settle --duration 10s` — where reads land across the two
  windows, and what compaction saves.
- `ledgerfio run --workload hold-settle --resolve-after 900000 --external-ratio 30` — what order
  exemption is worth, as lane depth.
- `ledgersim check --seeds 64` — every invariant under fault injection, including the store path.
