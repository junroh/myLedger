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
- **Reclaiming a day's blocks and proposing its voids are two jobs, and only one is the leader's.**
  `reclaim` hands back the blocks of *any* segment the index has no entry in — no clock, no cursor, no
  retention, because a segment with no entries holds only dead records whether its day ran out or its
  holds all resolved early. Every node runs it for itself, which is what keeps a follower's store from
  growing while the leader's shrinks. `propose_expiry` walks an expired day and offers voids, and that
  needs the leader's clock. Once consensus is real the leadership gate goes on the second one only.
- **A void nobody took is offered again, and that is now true rather than claimed.** The engine keeps the
  slice it handed over — bounded by `expiry_blocks_per_round` times the records on a block, not by the
  size of a day — and re-offers whatever the index still points at. Four comments claimed the sweep
  re-offered a declined void; none did, because the re-walk was gated on the day's live count moving, so
  a declined void that was the last of its day left that day unfinished for ever — and with it every
  later day, since deletion is strictly ordered. Storage then grew without bound and the index never
  filled to stop it, because ongoing traffic churns its own slots.
- **The sequencer says when it has room** (`set_wants_expiry`), and the sweep offers nothing while it has
  not. Retrying without that pause cost 780,000 declines in a five-second run and tripled p99.9: the
  sweep is the only thing that retries a void, so a full backlog turned into a re-offer every round.
  Rule 12's pause, for the one backlog whose filler is the ledger rather than a client.
- **The calendar stops before two live days can share a segment.** A segment's number is its day modulo
  63, so a sweep far enough behind meets its own target as the day being written — one block range over
  two days, one count for both, and a day that can never finish. `open_day` refuses instead; records keep
  going into the open segment, which dates them later than they are. Late, self-releasing, and rule 20:
  the ceiling was the address format's, enforced by whichever structure misbehaved first.
- **A new leader resumes from the counts, not from its clock.** The cursor is leader-local and volatile
  on purpose — which day has run out is a judgment from the leader's own clock, never a log entry — so a
  new leader has none. Deriving it from the clock as `today - lifetime` abandons every day the old leader
  had not finished. The counts are the recovery because they are a function of the log: a segment with
  entries whose day has already expired is a day somebody left unfinished.
- **The slice sizes itself from the day, and no longer waits on a policy.** The requirement was always
  `drain rate >= a day's survivors / a day`; a design day is 2.9M blocks against 86,400 seconds, so two
  blocks a round leaves three orders of headroom and the binding constraint is a single round rather than
  the rate. That round is tens of microseconds at any survivor density, so there is nothing left for a
  policy to trade — which is why this stopped being an open question rather than getting an answer.
- **The expiry void is its own kind, and it is judged rather than applied.** `TransferKind::VoidExpiry`
  beside `VoidClient`: same money, same effect, same branch, and different in everything around it — no
  ack leaves for it, idempotency records nothing, and a refusal tells no one. It is judged because a
  client void or a settle may be in flight for the same
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

### The store has a block interface, not a disk one — and that is the next piece of work

`BlockStore` is addressed the way the engine thinks (`write(addr, bytes)`, `read(addr, into)`,
`free_segment`), and `MemBlockStore` is the whole of it. That was right while there was nothing below it, and
it is the reason five of the design's storage questions cannot even be asked: what it does *not* have is
anything a filesystem does. No file, no offset, no `fsync`, no notion of a write that has been accepted but
not made durable, and no way for a read to fail.

**So the shape of the change is not "add a disk" but "make the interface a disk's, and back it with memory
today."** The same move `stubkit` exists for: a seam the real thing slots into without the engine above
changing, and a stand-in that is honest about being one. Blocks are already the right unit — four kilobytes,
written once, never rewritten, freed a whole segment at a time — so what has to arrive is the vocabulary
underneath them.

What it unblocks, which is most of why it is next:

- **The speed contract.** `≤5ms worst case` is a claim about a device, and `StoreModel` prices one without
  being one. `SE-OQ-6` cannot be answered from memory.
- **`SE-OQ-4`**, the read backend — io_uring against a thread pool — is a choice between implementations of
  exactly this interface, and there is nowhere to put either.
- **Where a snapshot goes.** It is a byte stream today with no destination; a file is the destination, and the
  question in the decisions list below is waiting on there being one.
- **Durability at all.** Nothing here can yet distinguish "written" from "durable", which is the distinction
  a checkpoint's coverage and a log's truncation both rest on.
- **`SE-OQ-5`**, compression, is a property of what is written to a file rather than of a block in a map.

Not started. The design's §3.4 and §4.7 are the inputs, and the decisions list has the three questions a
snapshot's destination waits on.

### The negative answer the design asked for is refused

Refused rather than deferred, which is why it sits here with a reason instead of on a list. The engine's
design wanted a negative answer that tells "resolved or expired" from "never existed". Under the
retention promise that is unimplementable: answering "expired" needs per-hold state kept past
retention, and a tombstone is exactly the data the promise says is deleted. Design notes §14 has the
argument and what serves the need instead — telling the client when the void happens, which needs a
push channel the ledger does not have.

### Recovery is not real

The **snapshot's format and its round trip are built** — `SnapshotWriter` and `SnapshotReader`, chunked
because 42.7GB cannot be held or written in one piece, refusing a stream whose bucket count is not this
table's because a bucket's position in it *is* its position in the table. Four tests: the round trip answers
every carried hold the same, a hold still in the writeback buffer is deliberately not carried, a differently
sized table refuses the stream, and junk or an unknown version is refused rather than interpreted.

**Coverage is built too**, and it turned out checkable without replay. A commit's log position now reaches
the engine on the command that carries its effect, each buffered block remembers the position it began at,
and coverage is the oldest one's minus one — everything up to it is **durable**. The test asserts the
claim directly: coverage lags what has been applied by exactly what the buffer holds, it advances as the
buffer flushes, and no hold from after it is carried.

**And durable is not the same as written**, which is the correction the store's new interface forced. A block
handed to the store is written; a `sync` is what makes it durable, and only what a crash would still find may
be carried. So coverage stops at the oldest block no sync has covered — ahead of the block being filled and
the buffer behind it — and a second test says so: a restore taken before the sync answers none of the holds,
one taken after answers them. The worker syncs at the end of its round, which covers every block that round
sealed, and that cadence is now measured rather than assumed — see the closed decision below.

**Replay is built, and the whole chain is asserted**: a snapshot covering an earlier position, plus every
effect after it, lands on the same answers as never having stopped. `PendingEngine::replay` is a mode of its
own rather than a flag, because the one effect that is not idempotent — a `Create` arriving again — can only
be told from a fingerprint clash by reading a record, and the path that applies committed decisions in order
reads nothing (§11).

The group totals turned out to be the hard part, and the fix is a number rather than the membership index
§4.8 asks for. They are accumulated, and a snapshot takes them at its own instant while its coverage is
earlier — so replay counted every member created in between a second time, and a group of 303 came back as
456. The snapshot now carries the position the totals reflect beside its coverage, and replay skips a
`Create`'s increment at or below it. It is an argument to `replay` rather than state the engine keeps, so a
caller cannot forget to supply it.

**The stable read is built too**, and it is copy-on-write rather than a second copy of the table. The kick
cascade is what it is for and not the effects: an entry displaced between buckets mid-dump appears twice in
the stream — one `remove` then clears one slot and the other survives, a resolved hold alive again with its
money reserved for good — or nowhere, and no replay restores it because a relocation is in no log. So the one
method that writes a slot copies a bucket the snapshot has not reached, and each copy is dropped as it is
read. The engine owns the progress rather than the writer borrowing it, because a paced dump spans many worker
rounds and a borrow that long would forbid applying anything for the whole of it.

The test writes between chunks until the table relocates, then asserts the two failures away: everything at
or below coverage is carried, and the holds that answer equal the table's entries — the second is what a
relocation written twice would break.

What is left is **where it goes**: nothing writes a snapshot anywhere, nothing calls `replay` outside its
tests, and the throttle that would pace it is a decision rather than code (`status.md`'s list). §15 has the
interval arithmetic and the one number it waits on.

Nothing writes one anywhere yet, and **design notes §15 is the design for the rest** — reasoning with no code
behind it, which is why it says so at the top.

The sentence that used to be here was wrong in a way worth keeping: it said the flush window is an hour
because unflushed records have to fit in a checkpoint, so the number could not be justified without one.
They do not have to fit in it. What is unflushed is in the log, so a snapshot leaves it out and recovery
replays it — and the hour's justification becomes *an hour is what recovery replays*, which is arithmetic
on the log's size rather than a claim needing a device. The replay **rate** is measured now, and it says the
hour is nowhere near the binding term: a whole day of log replays in 12–18 seconds of engine time against 67
seconds to read it, so what bounds recovery is the log's bandwidth. An hour of window is a rounding error
inside that.

### Smaller, and each with a reason

- **The load-factor alarm is reporting only.** The index reports its load against the target and
  its worst cascade, but nothing warns or refuses before an insert fails. The channel it would warn
  over now exists; what is missing is the threshold and what a warning should make the node do —
  a question about operations rather than about the code, listed under *Decisions waiting on someone*.
- **Consensus is an echo**, the idem map has no expiry, and the log has no compaction. All three
  grow with a run, which the tools say out loud.
- **A long run's worst tail belongs to the idem stand-in, not to the ledger.** A ten-second
  `void-heavy` run at 100k/s shows p99.9 between 1.7ms and 24ms across repeats, with single maxima up
  to 114ms; a five-second run of the same thing never does. What grows with the run is the idem map,
  which has no expiry and so rehashes as it crosses each power of two — on the thread every request
  passes through. Reserving four million entries in it turns eight repeats of p99.9 1.7–24ms into
  1.7–4.3ms and removes every maximum above 11ms, which is what establishes the cause. The reservation
  was a diagnostic and was reverted: the real engine expires by rotating generations sized from a
  declared window, and pre-sizing a stand-in to an arbitrary number would hide a growth nobody has
  bounded rather than bound it. What is left is knowing which component a tail belongs to, and that no
  latency gate on a long run is measuring this ledger. Reproduce with
  `ledgerfio run --workload void-heavy --duration 10s --rate 100k --resolve-after 900000 --repeat 8`.
### Where the documents and the code have diverged

Places where the design names a mechanism this code does not have. Kept as a section of its own
because neither is a gap to fill in passing — each was a deliberate substitution, and a substitution that
nobody writes down is read as an implementation of the thing it replaced. Both now have their reason and
neither is outstanding work.

- **The timing wheel is one counter per day, not four levels.** The design detects expiry with a
  hierarchical wheel (~2GB, day / hour / minute / second, only the imminent day loaded in detail). Built
  here is the day level and only that: sixty-four counts, because every hold in a day shares that day's
  computed deadline and nothing can express a per-hold one. The design's own lazy-load note is why this
  is not a shortfall: its 2GB is the imminent day's *detail*, and even that is unnecessary here because the
  walk is resumable — only the offered-and-unlanded slice has to be kept. Counts, block ranges and that slice
  come to about eight kilobytes. Design notes §14 has the arithmetic and the two ways it was got wrong first.
- **There is no High Water Mark on the day, and the design asks for one.** Design §5.3 defends the
  *judgment* of when a day has expired with three things: an infrastructure that slews rather than steps
  the clock, `CLOCK_MONOTONIC` for ticks, and an HWM so the day only ever advances. `DaySource::WallClock`
  reads the clock raw and has none of them.
  What that costs is not a divergence between nodes, and getting that backwards is easy: an expiry void
  goes through consensus, so every node applies the same release at the same log position. A leader whose
  clock is a day fast makes the whole ledger delete a day early, uniformly and durably, and the judge
  cannot catch it — a record carries no timestamp, so nothing downstream knows the hold's age. `grace_days`
  absorbs it up to the grace and no further. So this is the one place expiry can produce a wrong answer
  rather than a late one, and it is unbuilt.
- **Nothing marks the sweep as the leader's work.** The engine has no notion of leadership, so
  `propose_expiry` runs in the worker loop regardless. On a real cluster a follower would offer voids it
  cannot propose. `reclaim` is the opposite and must keep running everywhere — see the built entry above.
  The split exists in the code; the gate does not, because there is nothing yet to gate on.
- **There is no group offset chain, and no membership index, and coverage is why.** Design §4.5 writes a
  group's legs into one flush batch with each head carrying the next leg's offset, so a lookup can walk the
  chain; §4.8 adds a `group_index` with `is_group_member` and `group_intact` to enumerate a group's members
  while it is undecided. Neither is built, and neither is needed here for the same shape of reason as the
  epoch below: a different mechanism removed the need. The sequencer checks a resolution's coverage by
  **count** — every record carries `budget_members` and `budget_remaining`, and the engine aggregates them —
  so nothing ever has to enumerate who the members are, and a structure for walking to them answers a
  question no one asks. What would bring them back is the general case §7 already names: a budget group
  spanning submissions, which needs a client-supplied durable id and full-coverage checking against a
  membership rather than against a count.

  This one was worse than unrecorded. Design notes §12 cited "a group's offset chain" as one of the things
  that rest on an address being stable — a reason resting on a structure that does not exist. It now names
  the four that do.
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
ordering in the idem stub and both fakes, and the simulator now creates holds on the unconstrained
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

- **When does the day get a High Water Mark, and what decides the clock policy around it?**
  *Default:* the day is whatever the leader's wall clock says, read raw. A leader a day fast makes the
  whole ledger delete a day early — uniformly, durably, and undetectably, because a record carries no
  timestamp for anything downstream to check against.
  *Stops being safe:* the first time two nodes' clocks can disagree by more than `grace_days`, which is
  the first real cluster. It is half an infrastructure decision (slew, not step) and half an engine one,
  which is why it is a question rather than a task.

- **What marks the sweep as the leader's work, once there is a leader?**
  *Default:* nothing. `propose_expiry` runs wherever the worker runs, and `reclaim` should — the two are
  already separate calls for exactly this reason.
  *Stops being safe:* the day consensus is real. A follower offering voids it cannot propose is waste
  rather than corruption, but it is waste that grows with the cluster.

- **Should the sweep stop offering a void whose lane is quarantined?**
  *Default:* it keeps offering. `set_wants_expiry` covers a full backlog and a sealed apply path, but a
  quarantined lane refuses each void at `prepare` while the backlog stays roomy, so the slice is re-offered
  until the quarantine lifts. `ledgersim check --seeds 32` shows 77,000 refusals for it — 1.2 a tick against
  the 64 a tick expiry may use, and every one of them counted.
  *Stops being safe:* if a quarantine can last long enough for that to matter, or if the share of a tick's
  expiry budget it burns ever competes with a day finishing. The fix needs the engine to know about lanes,
  which is a layer it does not cross today — so this is a question about where the knowledge belongs, not a
  missing line.

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

- **When does the idem engine get its rotating generations?**
  *Default:* a map that only grows, which owns the worst tail of any long run (see above).
  *Stops being safe:* it already is not, for measurement — no latency gate on a run longer than a few
  seconds is measuring this ledger rather than the stand-in. It becomes a correctness-adjacent question
  when a node has to run for a day.

- **Can `ledgerfio` price a store read at all?**
  *Default:* it cannot, and it does not say so. `--store-read` and `--store-iops` exist and are wired, but
  `engine reads store=0` in every configuration tried — `--resolve-after 900000`, `--overlay-limit 10000`,
  `--residency 1` included. Nothing falls out of a 24-hour residency window in a five-second run, so there is
  nothing for a store read to fetch. `ledgersim check` does reach the path (87,494 store reads over
  sixty-four seeds), so this is about the tool that can put a latency on it, not about the path.
  *Stops being safe:* it already is not, for anyone reading a report. A run that sets `--store-read` and shows
  a tail is showing something else, and `SE-OQ-6` cannot be approached from a tool that never issues the read
  being priced. What is missing is either a residency window a short run can empty or a way to make one.

- **What throttle paces the snapshot's write?**
  *Default:* none — nothing writes one anywhere, so nothing paces it. The unit is settled (a declared number
  of buckets per round, like every other background path) and so is the reason it is needed whatever disk the
  snapshot lands on. What is not chosen is the number, and it is the number that sizes the copy-on-write side
  buffer: a longer dump shadows more buckets.
  *Stops being safe:* the first snapshot written on a node serving traffic.

- **Who records the apply index on each side, and how is it restored?**
  *Default:* nobody. The seam is open — `ApplyIndex` names it, a commit carries its batch's log position,
  and the reactor records the last one it applied — but neither component keeps it and nothing restores it.
  Two tests hold the seam open so it cannot rot.
  *Stops being safe:* at the first snapshot of either component. The engine is behind a queue and cannot be
  asked synchronously, so the shape of the recording follows from the snapshot's shape, which is why this is
  a question rather than a half-written method.

- **Is applying a committed effect twice safe on the *account* side?**
  *Default:* unknown. The pending engine's replay-idempotency is established (§15), and it has to hold for
  the account component too the moment both checkpoint at different points — recovery then replays from
  the earlier one and the later component sees effects it already applied. The reactor already compares
  the two views' apply counts every tick and seals on a mismatch, so the invariant is live; what is not
  established is that it survives a restart.
  *Stops being safe:* the first time two components are snapshotted independently, which is the first
  checkpoint of either.

- **Does the snapshot share a disk with the Raft log?**
  *Default:* the design puts both on Disk 1 (§2.2) and nothing here has chosen. It changes only what the
  snapshot's throttled write competes with — log commits or the engine's own reads — and both are on a
  critical path, so the throttle is required either way (§15). What it does change is the arithmetic: on a
  shared disk the snapshot's share is measured against the log's own write rate, which is one more argument
  for a long interval.
  *Stops being safe:* at provisioning, since it is a volume decision. It is here so the throttle is not
  mistaken for something the layout could make unnecessary.

- **Where is a snapshot written, and is cold start local or from a peer?**
  *Default:* nowhere, so the question is open in both halves. §15 argues the serialisation is the whole
  mechanism and the disk cadence is a policy on top of it: a node that always fetches from a peer needs a
  healthy peer, and a cluster that loses power together needs either a local copy or a log long enough to
  replay from nothing.
  *Stops being safe:* whenever a deployment has to survive losing every node at once — which is a
  question about the operation rather than about the code.

- **Does the client get told when a hold is voided for outliving its retention?**
  *Default:* no. The negative answer the design asked for is refused for a stated reason (above), and
  nothing replaces it.
  *Stops being safe:* whenever a client's correctness depends on distinguishing "expired" from "never
  existed". It needs a push channel the ledger does not have, so this is a protocol decision, not an
  engine one.

- **Which of the design's storage questions are still untouched, and is that acceptable?**
  *Default:* untouched. `SE-OQ-3` (a group spilling across blocks and what it costs in IO),
  `SE-OQ-4` (io_uring against a thread pool), `SE-OQ-5` (compression), `SE-OQ-6` (the ≤5ms worst case
  verified against a real device) and `SE-OQ-8` (provisioning down on the cache hit rate) all need a disk
  under the block store, and there is none — `StoreModel` prices a device's latency and IOPS but is not
  one.
  *Stops being safe:* at the point a real device goes in, when all five become live at once. That is worth
  knowing in advance rather than discovering as five surprises.

### Closed, and kept so a reader can tell a settled question from an open one

A question that is answered leaves this list by being **moved down here with what answered it**, not by being
deleted. That is not tidiness: twice in one session an entry was removed at the moment it was fixed, and both
times the finding behind it went with it — including the one fact that made a declared bound worth declaring.
An entry that vanishes reads as a question nobody ever asked.

- **What should a node do once expiry has stopped working altogether?** It cannot get there any more. A
  declined expiry void is now retried from the slice the engine keeps rather than by a re-walk gated on
  progress, so one dropped notice no longer stops deletion for the life of the node; `set_wants_expiry` paces
  the retry so it is not a storm; and the calendar stops before a sweep far enough behind can reuse a day's
  segment. The three together are what closed it — none of them alone would have.
- **Can the sweep fall far enough behind to reuse a day's segment?** No: `open_day` refuses to advance
  first. What made this worth a declared bound rather than an accepted risk is in design notes §14 — the
  wrap was late rather than early only by a coincidence of three unrelated details.
- **How often must a snapshot be written, and does it need deltas?** A long interval, and no. The replay
  rate is measured — 16–25M effects a second, so a design day's 300M costs 12–18s of engine time against 67s
  to read the 34GB of log it is in. Recovery is bounded by the log's bandwidth by about six to one, so a
  snapshot a day old recovers in a minute or two and deltas would buy seconds off a read they do not shorten.
  `cargo bench -p ledger-pending --bench snapshot`, and design notes §15 has the table.
- **What sizes the expiry throttle's slice?** The day does. The requirement is met three orders over and the
  binding constraint is a single round, which is bounded by declaration rather than by density.
- **How often should the engine make its blocks durable?** Every worker round, and there is nothing to trade.
  Measured: at 1M tx/s a 500µs `sync` costs 2–9% of throughput and the knee is around 2ms; at the design's own
  rate — 1,736 arrivals a second, so about thirty-four blocks sealed — a 500µs `fsync` is 1.7% of one thread,
  against a real NVMe's 50–500µs. Group commit is why it is cheap: one sync covers every block the round
  sealed, so a slower device is covered by fewer syncs and the curve bends instead of falling.
  **What the same measurement found instead is a requirement on the *write*.** A block write is per block and
  nothing amortises it: at 1M tx/s this workload seals eighteen thousand blocks a second, so 100µs of write is
  1.8s of thread per second and throughput halves. A 4KB `O_DIRECT` write is 10–20µs, so it fits with less
  headroom than the read path has. Design notes §16 has both curves and the command.
- **What was weighed and rejected in §3 and §7?** Nothing, in both. Neither was a decision: what is in the
  code is what was intended, and the implementation reached something else and was corrected. §7 merged a
  linked chain with a budget group, which the design has apart and always did. §3 kept arriving at other
  shapes for the chain scratch and the lane gates. What nobody remembers is the sequence of wrong versions,
  which is not a set of alternatives and does not belong in a `Weighed` line.

  **Worth keeping as a warning about the format itself.** Four lines that ask what was weighed will invite an
  answer even where the honest one is "this was not a choice" — and a plausible reconstruction is worse than a
  blank. Both headers now say so outright.
- **Does the index need a `min_live_seg_id` epoch?** No, and a test says why: a day's blocks go back only
  once the index has no entry in it, so the dead slots are gone before the blocks are.
- **`SE-OQ-2`** — the index bench measures what cannot be placed at each load factor and cascade cap.
- **`SE-OQ-7`** — answered by the budget group index existing and dying with the group it tracks.

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
