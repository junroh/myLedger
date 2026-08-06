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
- **A day's blocks go back once nothing points into it**, and the index says when in constant time.
  `live_per_segment` is one count per day, maintained by the one method that writes a slot — the
  design's day-wheel counts, and the whole of the wheel that day-granular deadlines need. Handing the
  blocks back is the only way the store shrinks, records being written once, so without it a run's total
  is holds created rather than holds alive — the figure the capacity estimate rests on.
- **A day's survivors are found in that day's own blocks**, read sequentially and checked against the
  index, a declared number of blocks per round. It replaced searching the index for addresses in the
  expiring segment, which bounded the voids a round collected and not the slots it walked: a day
  thinning towards empty scanned most of the table per round and the round that ended it scanned all of
  it, 2.2s at the design's size, on the thread that answers lookups. Rule 20, and design notes §14 has
  both measurements.
- **The slice sizes itself from the day, and no longer waits on a policy.** The requirement was always
  `drain rate >= a day's survivors / a day`; a design day is 2.9M blocks against 86,400 seconds, so two
  blocks a round leaves three orders of headroom and the binding constraint is a single round rather than
  the rate. That round is tens of microseconds at any survivor density, so there is nothing left for a
  policy to trade — which is why this stopped being an open question rather than getting an answer.
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
  over now exists; what is missing is the threshold and what a warning should make the node do —
  a question about operations rather than about the code, listed under *Decisions waiting on someone*.
- **Consensus is an echo**, the dedup map has no expiry, and the log has no compaction. All three
  grow with a run, which the tools say out loud.
- **A long run's worst tail belongs to the dedup stand-in, not to the ledger.** A ten-second
  `void-heavy` run at 100k/s shows p99.9 between 1.7ms and 24ms across repeats, with single maxima up
  to 114ms; a five-second run of the same thing never does. What grows with the run is the dedup map,
  which has no expiry and so rehashes as it crosses each power of two — on the thread every request
  passes through. Reserving four million entries in it turns eight repeats of p99.9 1.7–24ms into
  1.7–4.3ms and removes every maximum above 11ms, which is what establishes the cause. The reservation
  was a diagnostic and was reverted: the real engine expires by rotating generations sized from a
  declared window, and pre-sizing a stand-in to an arbitrary number would hide a growth nobody has
  bounded rather than bound it. What is left is knowing which component a tail belongs to, and that no
  latency gate on a long run is measuring this ledger. Reproduce with
  `ledgerfio run --workload void-heavy --duration 10s --rate 100k --resolve-after 900000 --repeat 8`.
- **The sweep falling far enough behind can reuse a day's segment, and only the seal stops it.** A
  segment's number is its day modulo 63, so day *D*'s segment comes round again at day *D + 63*. If the
  sweep is more than `63 - lifetime` days behind, that day arrives before *D* was ever emptied, and the
  two days share both a segment count and a block range. The result is late, not early: the count never
  reaches zero, so the day is never freed and the store stops shrinking — and the walk reads block
  numbers that belong to another day, whose records fail the index's address check and offer no void.
  Verified by driving the engine there by hand. Unreachable on a real node because the index is sized
  for `lifetime` days of survivors and seals a few days behind, long before sixty; `validate` refuses a
  lifetime that needs more segments than exist, but nothing refuses a sweep this far behind. So the
  bound is real and undeclared, which is rule 20's shape — recorded rather than fixed, because the fix
  is a question about what a node should do when expiry has stopped working at all, which is the first
  entry under *Decisions waiting on someone*.

### Where the documents and the code have diverged

Two places where the design names a mechanism this code does not have. Kept as a section of its own
because neither is a gap to fill in passing — each was a deliberate substitution, and a substitution that
nobody writes down is read as an implementation of the thing it replaced. Both now have their reason and
neither is outstanding work.

- **The timing wheel is one counter per day, not four levels.** The design detects expiry with a
  hierarchical wheel (~2GB, day / hour / minute / second, only the imminent day loaded in detail). Built
  here is the day level and only that: sixty-four counts, because every hold in a day shares that day's
  computed deadline and nothing can express a per-hold one. The design's own lazy-load note is why this
  is not a shortfall — a wheel holding the holds would be 38GB, so it was never meant to hold them.
  Design notes §14.
- **There is no `min_live_seg_id`, and it is verified that there needs to be none.** Design §3.1 and
  §4.6 give the index an epoch, and there they need one: `apply_expire` unlinks a segment on a *time*
  condition, so slots addressing it survive the unlink and two jobs have to cope — a lookup answers Dead
  by comparing the segment against the epoch, and an insert treats such a slot as empty so a 90%-full
  table does not kick the dead around.

  Here the free condition is different in kind, and that is the whole of it: `free_segment` has exactly
  one caller, inside the branch where `live_in_segment` is zero. The dead slots are gone *before* the
  blocks are, so a lookup finds no slot rather than a stale one and an insert sees a genuinely empty
  slot. Both jobs have nothing to act on. It is absent because the ordering makes it unnecessary, not
  because it was skipped, and `a_freed_day_leaves_no_slot_behind_so_the_index_needs_no_epoch` is what
  says so — change the free condition back to a time and that test fails.

  So this is no longer a divergence, and it stays in this section only because the design names the
  mechanism: the epoch is not missing, it has nothing to do.

Four earlier entries are closed: the in-chain fallback read is now exercised by the `linked`
workload (half its chains create a hold and resolve it inside the chain), the stub orderer's
metrics struct is `FakeOrderWait` so `OrderWait` names one thing, unordered replies skip lane
ordering in the dedup stub and both fakes, and the simulator now creates holds on the unconstrained
account so `check` exercises the order exemption itself — the sweep test asserts it did.

## Decisions waiting on someone

Not gaps in the code — questions the code cannot answer for itself, each of which it is currently
answering by default. They were scattered through the prose above until they were collected here, which is
why this section exists: a decision written into a paragraph about something else is one nobody revisits,
and a default nobody chose reads exactly like a decision that was made.

Closed decisions are not here. Those live in `design-notes.md`, whose sections each open with what was
tried, what broke, what was weighed and what was chosen — the same shape as an entry below, with the
question already answered.

Every entry says the same three things: **the question**, the **default** the code takes while it goes
unanswered, and **when that default stops being safe**. The source design's own open questions are tagged
`SE-OQ-n` where one matches, because until now those numbers appeared nowhere in this repository.

- **What should a node do once expiry has stopped working altogether?**
  *Default:* nothing — it falls further behind for ever, silently, and the store stops shrinking. The
  segment-reuse entry above has the mechanism.
  *Stops being safe:* when a deployment can reach that state without the index sealing first. Today two
  independently chosen sizes make the seal come first, which is rule 20's shape rather than a guarantee.

- **What threshold should the load-factor alarm fire at, and what should the node do when it fires?**
  *Default:* it reports and nothing acts. Inserts succeed until one cannot be placed, and that seals.
  *Stops being safe:* as soon as an operator is expected to react before the seal rather than after it.
  Half the input exists: `cargo bench -p ledger-pending --bench index` measures what cannot be placed at
  each load factor and cascade cap — at the 0.90 target and a cap of 128 it is zero in 7.5 million, and it
  is 149 per million if the cap drops to 32. That is `SE-OQ-2`, answered. What is missing is the operational
  half.

- **Where does `expiry_blocks_per_round` come from in a deployment?**
  *Default:* a constant, two. It has three orders of headroom against a design day, so nothing is wrong
  today.
  *Stops being safe:* if a day's blocks ever grow faster than the rounds available to read them —
  a much larger `daily_arrivals`, or a store whose reads are slow enough that a round no longer fits
  beside the lookups.

- **When does the dedup engine get its rotating generations?**
  *Default:* a map that only grows, which owns the worst tail of any long run (see above).
  *Stops being safe:* it already is not, for measurement — no latency gate on a run longer than a few
  seconds is measuring this ledger rather than the stand-in. It becomes a correctness-adjacent question
  when a node has to run for a day.

- **How is the flush window's hour justified without a checkpoint?**
  *Default:* an hour, chosen because unflushed records are memory-only and have to fit in a checkpoint
  that does not exist.
  *Stops being safe:* the number cannot be wrong yet, because nothing depends on it. It has to be
  re-derived the moment recovery is real. Related: `SE-OQ-1`, the split between the two windows and what
  the hit rate buys.

- **Does the client get told when a hold is voided for outliving its retention?**
  *Default:* no. The negative answer the design asked for is refused for a stated reason (above), and
  nothing replaces it.
  *Stops being safe:* whenever a client's correctness depends on distinguishing "expired" from "never
  existed". It needs a push channel the ledger does not have, so this is a protocol decision, not an
  engine one.

- **What was weighed and rejected in §3 and §7, and does either deserve revisiting?**
  *Default:* unknown. `design-notes.md` now opens every section with what was tried, what broke, what was
  weighed and what was chosen, and two sections answer the third with **not recorded** — the chain scratch
  with lane gates, and separating a chain from a budget group. Both look right and both are load-bearing;
  what is missing is the evidence that anything else was considered.
  *Stops being safe:* the moment either is questioned. A decision whose alternatives were never written
  down can only be defended by whoever made it, and that is the position this file exists to avoid.

- **Which of the design's storage questions are still untouched, and is that acceptable?**
  *Default:* untouched. `SE-OQ-3` (a group spilling across blocks and what it costs in IO),
  `SE-OQ-4` (io_uring against a thread pool), `SE-OQ-5` (compression), `SE-OQ-6` (the ≤5ms worst case
  verified against a real device) and `SE-OQ-8` (provisioning down on the cache hit rate) all need a disk
  under the block store, and there is none — `StoreModel` prices a device's latency and IOPS but is not
  one.
  *Stops being safe:* at the point a real device goes in, when all five become live at once. That is worth
  knowing in advance rather than discovering as five surprises.

**Closed, and kept here so a reader can tell a settled question from an open one.** The expiry throttle's
slice no longer needs a policy — the requirement is met three orders over and the binding constraint is a
single round, which is bounded by declaration. `min_live_seg_id` needs no epoch here, and a test says why.
`SE-OQ-2` has its measurement. `SE-OQ-7` is answered by the budget group index existing and dying with the
group it tracks.

## Where the numbers live

No measured number is written into these documents as a fact to be trusted; each one names the
command that reproduces it. The five that decide the most:

- `ledgerfio run --workload partial-settle --duration 10s` — where reads land across the two
  windows, and what compaction saves.
- `ledgerfio run --workload hold-settle --resolve-after 900000 --external-ratio 30` — what order
  exemption is worth, as lane depth.
- `ledgersim check --seeds 64` — every invariant under fault injection, including the store path.
- `cargo bench -p ledger-pending --bench sweep -- --repeat 5 --pin 2` — what emptying one day costs,
  driven through the real engine. Three rows, and the middle one is the one a policy needs: records read
  per void against how much of the day is still alive, because a day's blocks hold its dead as well as
  its living. The third says a round stays bounded as the blocks it reads grow, which is the property the
  index scan did not have at any setting.
- `ledgerfio run --workload void-heavy --rate 100k --daily-arrivals 150m --index-budget 4100m
  --expiry-days 60` — expiry as a client sees it. Sixty days forces a sweep per day; it is the run that
  showed the old index scan as a 108ms tail and shows nothing now.
