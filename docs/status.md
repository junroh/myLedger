# Status

What is built and verified, and what is not. Written to be read before picking up work: the design
reasoning lives in `design-notes.md`, the terms in `glossary.md`, and how to run things in `tools.md`.

Verified at the time of writing with **`make verify`** — the tests in debug, the release build, every
`ledgerfio` workload, `ledgerfio layout`, and all three `ledgersim` modes. It is a target rather than a
list here because a list here is what drifted: a workload that aborted on the reactor thread went two
commits unnoticed, since `cargo test` runs for milliseconds and reaching that defect took a second of
release build. What `make verify` still does not cover is `cargo bench -p ledger-pending` and `ledgerd`
draining on SIGTERM, both run by hand.

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
- **A removal is remembered until the engine has applied it.** The sequencer erases what it knows the
  moment it hands a write over, and the engine clears its index a queue later; a lookup answered in
  between carries the hold as it was, remainder intact. That gap resolved holds twice and put the
  second resolution in the log. The marker now outlives the hand-over — stamped with the applies sent
  and retired when the engine reports it has got that far — and it is visible to all three readers: a
  request is told the hold is missing, an answer that crossed the removal is not believed, and the
  judge is told it is resolved. One of the three left open is enough to lose the money.
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

### The expiry sweep walks the whole index, and at scale that breaks the speed contract

This entry used to say the throttle was waiting on a number rather than on code. It has been measured,
and it was the wrong half. The rate is not the problem and never was:

| | measured | budget |
|---|---|---|
| one pass of the index | **2.2s** at the design's 5.33B slots | ≤5ms, the hard contract |
| a day's total sweep work | **4.4s** | a day, 86,400s |

So the drain rate the requirement asks for — a day's survivors per day — is met three orders over. What
is not bounded is one pass. `expiring(want, into)` bounds the voids a round *collects*, not the slots it
*visits*, and a day on its way to empty runs out of voids to collect long before it runs out of table:
with fewer survivors left than one round takes, a round walks everything. The pass that ends a day
always does. It runs on the engine's own thread ahead of `drain_commands`, so every lookup behind it
pays the whole pass.

This is rule 20 again, and it is the reason the entry is here rather than under a tuning knob: no value
of `expiry_per_round` can make a 2.2s uninterruptible call fit in 5ms, because that knob does not bound
the walk. The fix is the timing wheel — see the divergence below — which removes the walk instead of
slicing it. A slot ceiling per round would buy the contract back in a few lines, and is deliberately not
taken: it is code the wheel deletes.

Both numbers are reproducible, and one anchors the other: the bench predicts 112ms for a 268M-slot pass
and a run with clients being served shows 108ms, a 4% agreement. Commands under *Where the numbers live*.

Two smaller facts the same measurement turned up. A fresh node's first sweep walks a table nothing has
touched yet, so it pays page faults on top of the scan — 459ms at 268M slots against 112ms warm. And a
run whose declared windows are large never flushes a record out of the writeback buffer, so every index
entry still addresses the buffer segment and no sweep of a day can find anything: reaching a real expiry
in a run needs more records created than the flush window holds.

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

### Where the documents and the code have diverged

Two places where the design says one thing and this code does another. Kept as a section of its own
because neither is a gap to fill in passing — each was a substitution, and a substitution that nobody
writes down is read as an implementation of the thing it replaced.

- **There is no timing wheel.** The design detects expiry with a hierarchical wheel (~2GB, day / hour /
  minute / second, only the imminent day loaded in detail). This code walks the index instead, repeatedly,
  until a pass finds nothing. Two consequences follow from the substitution rather than from the design:
  the same void is offered again on every pass until the judge has taken it, and one pass costs 2.2s at
  the design's index — the entry above. **This is the next piece of work.**
- **There is no `min_live_seg_id`.** Design §3.1 and §4.6 give the index an epoch: a slot addressing a
  segment older than the oldest live one is dead, so a lookup answers Dead with no IO and an insert reuses
  the slot without a kick cascade. Here a slot is only ever cleared by the resolution that removes it, so
  an expired day's slots stay occupied until its voids are all judged. Building the wheel closes the first
  entry and leaves this one: they are separate mechanisms that the single index walk was standing in for
  at once.

Four earlier entries are closed: the in-chain fallback read is now exercised by the `linked`
workload (half its chains create a hold and resolve it inside the chain), the stub orderer's
metrics struct is `FakeOrderWait` so `OrderWait` names one thing, unordered replies skip lane
ordering in the dedup stub and both fakes, and the simulator now creates holds on the unconstrained
account so `check` exercises the order exemption itself — the sweep test asserts it did.

## Where the numbers live

No measured number is written into these documents as a fact to be trusted; each one names the
command that reproduces it. The five that decide the most:

- `ledgerfio run --workload partial-settle --duration 10s` — where reads land across the two
  windows, and what compaction saves.
- `ledgerfio run --workload hold-settle --resolve-after 900000 --external-ratio 30` — what order
  exemption is worth, as lane depth.
- `ledgersim check --seeds 64` — every invariant under fault injection, including the store path.
- `cargo bench -p ledger-pending --bench sweep -- --repeat 5 --pin 2` — what one pass of the index
  costs, and what a day of expiry walks. The size rows are the extrapolation's licence: flat cost per
  slot across a 64× range is what lets a 2GB table speak for a 37GB one, and a row that stops being flat
  withdraws the permission.
- `ledgerfio run --workload void-heavy --rate 100k --daily-arrivals 150m --index-budget 4100m
  --expiry-days 60` — the same pass as a client sees it. Sixty days forces one pass per day, because
  three passes in ten seconds are lost in a tail that 950,000 other requests also occupy.
