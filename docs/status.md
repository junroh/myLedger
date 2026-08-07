# Status

What is built and verified, and what is not. Written to be read before picking up work: the design
reasoning lives in `design-notes.md`, the terms in `glossary.md`, and how to run things in `tools.md`.

Verified at the time of writing with **`make verify`** — the tests in debug, the release build, every
`ledgerfio` workload, two runs that write snapshots (one to a volume of its own and one to the blocks'),
`ledgerfio layout`, and all three `ledgersim` modes. It is a target rather than a
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

**A refusal carries its reason.** A client whose submission comes back sees a full queue, and that symptom
is the same whether the store is behind, consensus is behind, or the client is not collecting its own acks
— three causes wanting three different reactions. The sequencer publishes which backlog stopped intake
(`PauseCause`, on a transition only, so a steady state costs nothing), the client's endpoint carries a view
of it across the thread boundary, and `submit` returns the transfer *and* the cause. `PauseCause::None`
beside a refusal is its own answer: the client outran a sequencer that was still admitting. `ledgerfio`
counts refused submissions by cause — once per submission rather than once per retry, and only in the
measured phase, since the funding burst refuses thousands before the reactor has taken a tick — and prints
the line only when there were any. At the default client queue there usually are none: intake pauses tens
of thousands of times in a saturated run and the request ring still drains before the driver's next push.
At `--client-queue 64` the same run reports 68,881, of which 437 met intake actually stopped and the rest
met a ring that had not drained since it last was. **That gap is why the published cause is the last one
rather than the current one** — an instantaneous answer is `None` for most refusals at the ceiling, which
the first version of this did and which said nothing.

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
  own so news the engine sends is neither behind a reply nor in front of one. Three notices exist: a
  committed hold the index could not take seals the apply path (rule 19, where it used to be
  detect-and-report), a store that refused a call seals it too and is counted apart because the cause is a
  device rather than a table, and a hold that outlived its retention is proposed for release. Design notes
  §13, §16 and §17.

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
- **A void nobody took is offered again, and the engine is now *told* which one rather than guessing.** It
  keeps the slice it handed over — bounded by `expiry_blocks_per_round` times the records on a block, not
  by the size of a day. What it could see was a void landing: the hold stops existing. What it could not
  see was one being refused, so it re-offered the whole slice every round on the chance that some of it
  had been — and a re-offer is judged like any resolution, so every one is a lookup. **1,939,198 lookups
  to release 89,352 holds: twenty-two reads apiece.** `PendingCommand::ExpiryDeclined` is the missing
  half — an expiry void gets no ack because no client asked for it, but the engine has to hear a refusal
  or it cannot tell one from a void still in flight. Now: **90,000 lookups for 89,800 holds, one apiece**,
  and `ledgersim check` goes from 678,000 offers with 325,000 dropped to 142,000 with none dropped, at the
  same number admitted. Four comments claimed the sweep
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

**A test that has to see a request waiting holds the answer rather than timing it.** `AnswerGate` stops a
stand-in sending, says how many answers are queued, and lets a declared number through — so an interleaving
a test depends on is a state it waits for instead of a delay it hopes was long enough. It lives in
`stubkit` and both stand-ins that need it use it: `MemoryPending::replies` and `EchoRaft::commits`.

**Six tests were fixed by it and by one review, and the review is the part worth keeping.** Every test that
touches real time was read and sorted into three. A *deadline* (the harness's five seconds, `allocation`'s
thirty) is a failure bound rather than a subject, and safe. A latency whose waited-on state only the
reactor can move — `backpressure`'s slot pool, `linked_chains`' in-flight hold — is safe, because the
reactor is the test thread. What is **not** safe is a latency arranging an interleaving against a component
that answers on its own thread, and there were four of those:

- three in `lane_ordering`, on a five-millisecond pending latency. Two needed both replies queued before
  either left; the third needed the reordered one sent *alone*, because two answers handed over together
  are judged in whatever order the tick takes them — found by measuring, since the swap fired in the
  failing runs too.
- one in `pipeline_stages`, asserting `committed == 0` after a two-hundred-millisecond round trip, which is
  a claim about the clock. Consensus is held now and the test costs no time at all.

Two more were a different shape and are in the same family. `ledger_invariants` waited for a sixth batch
that a seal had already made impossible, and ends on the seal now. `hold_not_stored` waited for records to
be *written* and then expected a read to reach the device — but a written block is still in residency, and
on a busy machine all 512 lookups were answered from memory. It waits for a record to have left memory,
which is the state it meant.

**And then the four that were safe went too, because the argument for them was the fragile part.** A raft
round trip in `backpressure`, `lane_ordering`, `linked_chains` and `ledger_invariants` was defensible —
the state each waited on can only be moved by the reactor, which is the test thread — but that is a
sentence about how the pieces happen to fit, and the gate makes it unnecessary. Commits are held instead,
and no test configures a duration to arrange anything now.

One more was found in the same sweep and it was **failing open rather than failing loudly**: a unit test
asserted that no hold was offered early by asking for two hundred milliseconds. On a busy machine that is
a handful of loops rather than a handful of rounds, so it would have passed without checking. The bound is
rounds now — two records appended after the day changed prove two rounds ran, and the sweep runs at the
top of every one.

**What is left in a test is a deadline and an injected clock, and nothing else.** A deadline is a failure
bound: reaching it is the test failing, never the test deciding. `ManualClock` is what the linger test
uses, and it is the shape the rest now follow.

Measured after: every test binary in the workspace running at once, forty rounds, 1,000 executions, clean;
the previous round of fixes measured 1,500 the same way.

Both tools can now reach the read path for honest reasons: `--resolve-after` gives a hold an age, so
a resolution reads a record at a declared age rather than one written moments ago. `check` draws
narrow windows for three seeds in four, so the fetch path runs while faults are on, and the sweep
test asserts the store was reached.

### The store's interface is a disk's, backed by memory

`DurableStore` is a filesystem's vocabulary: an **object** is a file, brought into being by its first block
(a write that says it is creating), appended to at an offset, read at an offset, made durable by a **barrier**,
renamed, removed whole, and able to fail. Writes and barriers are submitted and answered for, the way reads already
were (§20). `MemoryStore` backs it today and `LatencyStore` prices a device in front of any backend. Design notes
§16.

- **An object is a day's blocks or one of the snapshot's two files**, in one namespace because they are on
  one disk. The day ↔ segment mapping stays in `RecordLog`: below the store there are objects, not days.
  While a file was named by `segment: u8` there was nowhere for a snapshot to be written that was not a
  day, which is why this type exists (§20).
- **One instance per volume, and every IO into that disk goes through it** — reads and writes, the blocks'
  and the snapshot's. That is what makes the store the one place a queue depth, an in-flight count and
  (later) a watchdog can live. Two writers keep queues of their own *above* it, with reactions of their
  own to a full one: the log stops applying so backpressure reaches the client, a dump waits. A completion
  queue is one queue, so `IoOwner` in the top bits of a handle says whose each answer is, and the log
  routes what is not its own to a mailbox the snapshot drains.
- **A volume counts what it did, and it is the only thing that can.** Every other IO number in this
  engine is a caller's tally of what it asked for, which answers "what did the drain do" and never "what
  is this disk doing" — and on a volume two writers share, neither caller can see the other's half.
  `VolumeStats` is the disk's own: reads queued, answered and how deep the queue got, reads done inline,
  writes and bytes, barriers, removes, renames, refusals by side, and faults. Counted by each backing,
  because only the backing knows whether a submit was taken; the rule for what counts as what is in one
  place. A run prints one line per volume, and the shared-volume case is where it earns itself — one
  directory for both reports a write queue that reached its full 128 and six refusals, neither of which
  any caller could see. It is also the accounting the watchdog needs (§20), which is why in-flight was a
  method nobody called.

- **What says two directories are one volume is a declaration, and only half of one exists.**
  `OpenBacking::same_volume` recognises the same directory, canonicalised — the case it cannot be wrong
  about. `st_dev` is refused for being wrong in both directions (§20). So two directories are two volumes
  today, and the second is opened exact: the `--store-*` knobs describe the blocks' device, and pricing a
  second disk with the first one's numbers is the same guess turned round.

- **The offset is a function of the address** — the block number times the block size — so nothing has to be
  restored to know where a block sits: a segment's file begins with a hole and its own blocks are one extent
  inside it, and the extent map a filesystem already keeps *is* the layout. The relative form needed the
  segment's first block, which is not derivable from the live slots, and a restore test proved it rather than
  arguing it.
- **Written is not durable**, and coverage follows the later one. A block a sync has not covered is not
  carried by a snapshot, because a restart could not read it.
- **A block carries a checksum**, in the sixteen bytes fifty-one records leave spare, so integrity costs no
  space. It is the only thing that can see a device which *answers* wrongly rather than refusing: double-entry
  cannot, because a corrupted remainder moves both sides by the same wrong amount. `--store-corrupt-every`
  produces one and the reaction is the seal.
- **A read can fail**, and both kinds do the same thing: `StoreFault::Missing` is this node's own record of
  where blocks are disagreeing with the store, `Device` is an `EIO` or an `ENOSPC`. Either seals the apply
  path through `PendingNotice::StoreFailed`, counted apart from `holds_not_stored` because one is a table
  sized too small and the other a device. `--store-fault-every` is what produces one, and without it the seal
  would be code nothing had run.
- **A device's cost is charged where it lands, and where that is has since become a choice.** A lookup's
  read occupies the device (`--store-read`, `--store-iops`, `--store-queue-depth`). A write and a sync
  (`--store-write`, `--store-sync`) occupy the *thread* — and that was unconditional when this was written.
  It is now what happens with `--store-write-lane 0`, which is still the default and still the baseline; with
  the lane on they occupy a thread of their own (§20). The apply-path read occupies the thread either way,
  and it is measured at zero.
- **Blocks are 4096-byte aligned buffers**, because direct IO wants the address aligned as well as the offset,
  and an unaligned buffer would cost a bounce copy per IO.
- **`FileStore` is the real backing**: one file per segment, `pwrite`/`pread`/`fsync`/`unlink`, and **no
  `unsafe`** — everything has a safe form in `std`'s unix extensions and only the *value* of `O_DIRECT` comes
  from `libc`. `--store-dir` asks for it. Its `submit`/`poll` reads synchronously, which is the placeholder
  `SE-OQ-4` replaces rather than an implementation of it.

What it unblocks, and what each is worth now:

- **`SE-OQ-4`**, the read backend, has its portable third: `FileStore` reads on N threads
  (`--store-read-threads`), each with its own SPSC pair, and zero is the synchronous baseline. Measured, the
  curve peaks at **two threads and 9% over synchronous**, then falls to 43% *below* it at sixteen — because what
  bounds the count is the cores this assembly has spare, not the reads it has to serve. The default is zero
  because that number is a deployment's and not a constant. **And the same measurement is the design's own
  argument for io_uring, as a number**: Little's law wants a hundred reads outstanding at 0.5ms and 200k/s,
  cores here allow two threads, and no thread pool closes a gap of fifty. io_uring and libaio are the same two
  methods again; the trait arrives with the second implementation.
- **`SE-OQ-6`**, the `≤5ms` worst case, has its tooling and not its answer. Against the model: 5ms reads
  sustain 100k tx/s at p99.9 9.7ms with a queue depth of 512. Against a real filesystem at 40,000 store reads
  a second: p99 5.4ms and p99.9 6.1ms at depth 2048, against p99 93.7ms at 128 — **twice now a "the device is
  too slow" number has been a queue too short.** What is still missing is a device: without `O_DIRECT` the
  reads come through the page cache and twenty megabytes of segment files fit in it, so those figures price
  the syscall path and the queue rather than a disk. That needs a Linux host.
- **`SE-OQ-5`**, compression, is narrowed rather than answered: block-level compression would break the offset
  rule, so it belongs inside a block, at the record.
- **Where a snapshot goes** was the one of the five this did not move, and the next piece of work moved it:
  a directory of its own, one file replaced by rename, paced by bytes a round (§19).

Two decisions came out of this one and are written as their own rather than left in its prose: **§17**, what a
broken store is and what this node does about it, and **§18**, how a read is issued and by how many threads.
What §16 names as unbuilt is a startup reconcile for files a previous life left behind — and three things that
all wait on a Linux host: `O_DIRECT`, `SE-OQ-6`'s answer against a device, and whether the read pool earns
anything.

## Not built

### The negative answer the design asked for is refused

Refused rather than deferred, which is why it sits here with a reason instead of on a list. The engine's
design wanted a negative answer that tells "resolved or expired" from "never existed". Under the
retention promise that is unimplementable: answering "expired" needs per-hold state kept past
retention, and a tombstone is exactly the data the promise says is deleted. Design notes §14 has the
argument and what serves the need instead — telling the client when the void happens, which needs a
push channel the ledger does not have.

### Recovery has a destination now, and not a start-up

Everything up to and including *where a snapshot goes* is built; what is missing is a node that begins from
one. The two halves are listed apart below because only the second is still a design question.

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

**And it goes somewhere, through the store.** A dump is written to the partial object, made durable by a
barrier, renamed over the current one, and the directory synced inside that rename — so a crash leaves the
previous snapshot or the new one and never a prefix of the new one wearing the current name. One name and
not a series, because an older snapshot is only restorable while the log still holds everything after its
coverage. Design notes §19 and §20.

- **The stream is padded to whole blocks (format version 4)**, because a block is what the store takes.
  The padding is the destination's rather than the format's — a follower receiving the same bytes over a
  wire has no blocks — so `SnapshotWriter` still hands out 32-byte records and the stage rounds the last
  chunk up. The reader stops at the header's count and refuses a tail that is anything but zeroes; the
  version bump is what stops an older reader taking the padding for records.
- **Nothing destructive happens while a write of the dump is outstanding**, which is rule 22 asked in
  advance this time rather than after four patches. A rename between "the last chunk returned" and "the
  stream is on the disk" publishes a prefix, and a removal with chunks queued leaves them landing in a file
  nothing will look at. So a dump has phases, the shadow still goes the moment a dump is given up on — it
  is memory, and a slow disk must not cost the apply path any — and the object goes when the last
  completion has. The test drives those two apart with a store that answers only when told, and reverting
  the wait fails it.

- **The cadence is a log distance, which is what leaves the engine with no clock for it.** What recovery
  costs is the effects it replays and what the log has to retain is the entries it keeps, both counted in log
  positions, so an interval measured in them needs neither a wall clock (which steps backwards) nor a
  monotonic one (which restarts at zero and so cannot express "since the last snapshot" across a restart).
  A node applying nothing writes none; a node at ten times the rate writes them ten times as often. The unit
  is a committed batch, because that is what `ApplyIndex` counts.
- **The throttle is 4096 bytes a round, and the tail is what picked it.** Per byte a larger chunk is cheaper
  — 64KB costs 0.11% of throughput for each MB/s it writes against 4KB's 0.28% — but a chunk is written
  inside one worker round, so it holds the thread every lookup passes through: while a dump runs the median
  goes 1.5ms at 4KB and 6.5ms at 64KB, against 1.3ms with no dump and a 5ms contract. A small chunk running
  more of the time costs the median a little; a large one running less of the time costs a percentile a lot.
  **Two readings of that were checked and both are wrong**: it is not the bigger chunk writing more bytes
  (volume rises 44% across the range while the median rises 313%), and it is not the syscall's own duration
  (64KB to a page cache is tens of microseconds against five milliseconds of movement) — it is queueing
  behind a worker that is the bottleneck at that rate. §19 has both curves and the command.
- **A round is what a dump gets, so it yields to traffic without being told to.** The same 4KB writes 558MB/s
  when the engine has rounds to spare and 35MB/s when it is saturated, because the worker's rounds go to
  commands first. A rate limit in bytes a second would have had to be told.
- **The shadow has a declared ceiling, and it had none.** §15 sized the copy-on-write side buffer by
  arithmetic and then left it a map that grows, which is rule 20's shape exactly — the real bound was the
  allocator. It is declared in buckets now and a dump that breaches it is **abandoned**, which costs the work
  and nothing else: the current snapshot is untouched and the cadence tries again. Measured at 4KB the peak
  is 63,863 buckets at the ceiling and 19,951 at a tenth of that rate — the slower dump holding three times
  as much, which is the whole trade in one line. Every run reports the peak beside the budget, because a
  throttle too slow for its cadence shows up as `abandoned` climbing with `written` at zero.
- **A failed write ends the dump rather than retrying the chunk**, and the shadow is why: producing a chunk
  consumes the shadow entries for the buckets it read, so a chunk that was produced and not written cannot be
  produced again. Retry is at the granularity of a dump, which is the granularity the cadence already has.
- **A broken store ends it too.** The apply path is about to seal, so nothing more will be applied, and
  finishing a dump of a state that has stopped moving would hold the shadow for the whole of it.

### Starting a node from one is built, and the order is what made it one call

`Snapshots::read_into` restores the index, the group totals and the coverage, and then the engine puts its
log back where the last life left it. A restored engine can be **written to**, which is the whole of what
was missing: before, it answered lookups against the blocks that were there and its one caller was a test.

**Two sources, and neither could do it alone.** The restored slots say which blocks still matter, so a day's
range is the span they cover — a block outside it holds only dead records, which is the same condition
`reclaim` already uses on a whole day. What the slots cannot say is how far the blocks *went*: a block whose
records all died leaves nothing to find it by, and writing the next one at its number would give two records
one address. The volume answers that, because offsets are absolute and a file therefore ends where its last
block does (§16) — the length *is* the high-water mark, and the filesystem has been keeping it all along.

**The second half is why it is one call and not two.** `reclaim` hands back the blocks of any segment the
index has no entry in, and it is right to. Run before an index exists, it finds nothing alive anywhere and
deletes **every file**. So the reconcile is inside `PendingEngine::restore` rather than beside it: a caller
cannot get the order wrong because there is no order for it to get (rule 16). It also closes the leak the
other way — a day with a file and no live slot is never reclaimed in the ordinary course, because `reclaim`
skips a day whose range is empty and a restored range is empty exactly when nothing points into it.

`open_with`'s `O_EXCL` refusal stays where it is (§16, and
`a_segment_file_left_behind_is_refused_rather_than_written_over`). It is no longer standing in for the
reconcile; it is what catches a file the reconcile did not account for, which is a different and still
useful thing.

What is still missing before this is a *node* start-up rather than an engine one: nothing calls it at boot,
because `ledgerd` has no restore path yet and the account component has no snapshot at all — see the two
decisions below on the account side.

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
- **`ledgerd` can be pointed at a directory** (`--store-dir`), and `ledgersim` deliberately cannot: a virtual
  clock with real IO under it measures neither of the two, so the simulator's backing stays memory and what
  varies per seed is the *model* — read and write timings, a refusal every nth call, a bit flipped in every nth
  block. Two seeds in three draw a store with timing; the third keeps the exact store, and that share is
  measured rather than chosen: with every seed slowed, 87,000 store reads across the sweep became 4,000,
  because a synchronous write holds the component's thread and the sweep's step budget is fixed.
- **Consensus is an echo**, the idem map has no expiry, and the log has no compaction. All three
  grow with a run, which the tools say out loud.
- **One `ledgerfio` configuration overloads intermittently, and it is the store-read one.**
  `--resolve-after 100000 --residency 1 --overlay-limit 10000` — the combination that makes every read a store
  read — refuses 28,000 to 77,000 requests as overload in roughly one run in five, at 100k/s over five
  seconds. The other four are clean at 100k/s with nothing rejected. It is not the store's queue depth (both
  128 and 512 do it) and it is not the block checksum (the same runs on the commit before it do it), so it is
  the same stall that owns every long-run tail here: the idem map rehashing on the thread every request passes
  through, with the overlay's eviction beside it. Recorded rather than chased because the cause is a stand-in
  that is already listed below as unbounded — but a report from this configuration has to be read knowing it.

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

- **The drain has left apply, and the rest of §20 has not been built.** `PendingEngine::drain` is now a
  stage of the worker's round on a declared budget, and applying an effect appends and points the index and
  stops there. Measured with the two binaries alternated run by run, the move costs nothing — 2.60M / 2.67M
  / 2.56M tx/s against 2.57M / 2.59M / 2.50M before, ahead in all three pairs. What came with it is the
  stall that had to: the buffer has a ceiling and applies pause at it, because with the producer draining
  the window could not be exceeded at all and nothing had declared that (rule 18). At the ceiling a
  five-second run reports 984 stalls against 214,138 blocks drained, and removing the ceiling changes
  throughput by nothing.

- **Closing a block and writing it are two calls now.** `seal_block` stamps the checksum and hands
  the block to `pending_writes`; `submit_writes` offers them in order and `collect_writes` takes the
  answers, and only on the answer does the block enter residency. The merge of the two was what made the write immovable — no seam to hold the first half and
  hand off the second — and the lane is what replaces the flush. Two readers had to learn about the gap: a
  lookup, and the expiry walk. Coverage did not, because `unsynced` is recorded at the close. Measured, the
  split is free: eleven interleaved pairs, mean −1% against a ±7% machine band.

- **Writes have a lane, and it is worth thirty-one times the tail.** `submit_write` / `submit_barrier` /
  `poll_written` replace `open_with` / `append` / `sync`, the way reads were already submitted and polled.
  `FileStore` serves them on **one** thread — writes do not commute, so an ordered lane rather than a pool —
  and zero threads stays the synchronous baseline. Measured against real files, `hold-settle` at 200k/s with
  `--resolve-after 900000`, three interleaved pairs: p99.9 **103–124ms synchronous against 3.4–3.7ms on the
  lane**, and throughput at the ceiling +7 to +9% in four pairs of four. On macOS with a page cache, where a
  `pwrite` is a memcpy — the tail was `fsync` holding the thread that answers lookups.

  Two things had to be exact and both are the same shape — an ordering that used to hold because a call was
  synchronous, and holds by nothing once it is not (rule 18).

  **Residency takes a block when it is closed, and a completion only opens eviction.** The invariant the read
  path rests on is *a block that is not in the memory tier has already been written* — that is what lets a
  miss go straight to the device without asking anything else — and it was structural until the write stopped
  being the call that closed the block. Filling residency on the completion instead left a block closed,
  unwritten and unresident at once, and four readers were patched for it before the structure was put back:
  a lookup path, the expiry walk, a reassembly of completions into block order, and a per-block check to
  catch what that might miss. All four came out again. What replaces them is one condition — a block whose
  write is outstanding does not leave memory — and residency may sit above its window by at most the store's
  queue depth, which is a declared number. CLAUDE.md rule 22 is this generalised; design notes §20 has the
  sequence.

  **A block closed while a barrier is outstanding is not covered by it**, so it joins a second run and the
  barrier's completion clears only the first. Folding them would let a snapshot carry slots naming a block a
  restart cannot read.

- **One read still runs on the thread that answers lookups, and it is the one that has a reason to.** The
  apply-path fallback is in order and cannot park a decision half way, and it is measured at zero: the
  record it wants was appended moments ago and the buffer is an hour wide. Everything else submits. The
  expiry sweep's block reads were the last that did not, and they went to the queue **for consistency
  rather than for a number** — after the read cache they were 464 device reads in three seconds. A run
  reports `+ 0 inline` on its volume line now, which is the claim in one figure. Reads have a pool and it is off by default; writes have a lane and it is off
  by default, and both defaults are the synchronous baseline every number is compared against rather than a
  recommendation.

  **The snapshot goes through the store now**, so its writes are counted, queued and bounded rather than
  being a `File::write_all` beside it, invisible to every IO figure the tools print. §20's answer was not a
  layer under the store but the store itself: it is already the device abstraction — `RecordLog` computes
  the offset and hands down a block — and it took three things, all of which are built. A file named by an
  **object id** rather than `segment: u8`, so the days and the snapshot's two files live in one namespace.
  **`rename` beside `remove`**, because publishing is a namespace change and the directory sync belongs
  inside it. And the **stream padded to whole blocks** (version 4), because a block is what the store takes.

  **Everything that changes the volume is on one queue.** A write, a barrier, a rename and a removal are
  submitted the same way and served in the order they were asked for, which is what decides three
  orderings that used to be the caller's to arrange or nobody's at all: a removal cannot overtake a write
  into the file it removes, a rename cannot overtake the writes it publishes, and a removal cannot
  overtake a *read* of the file — which held under unix semantics rather than by anything declared (§20).
  The caller still waits for a completion where it needs an **outcome** rather than an order: a barrier
  that failed must not be followed by a rename. And the directory `fsync` inside a rename is now the
  lane's rather than the worker's, which is where it belonged.

  **A dump may hold only half the volume's queue**, derived from the depth rather than configured beside
  it. Within a round the blocks already ask first, so the dump takes what they did not want — but a slot it
  takes it holds until the device answers, and on a device that has stalled while the ledger happens to
  have nothing to write, a chunk a round is enough for the dump to end up holding the whole queue. The
  blocks would then wait on a background job's completions, applies would stop at the buffer's ceiling and
  a client would be refused for a snapshot. Rule 18: it held by a coincidence of two line orderings, so it
  is decided once and in one place.

  **One instance per volume is what makes "the same disk" mean something, and half of it is built.** The
  same directory for both is one store, which is the case a path cannot be wrong about; two different
  directories on one disk needs a declaration, and that is the configuration question below. Nothing is
  guessed in the meantime — `st_dev` is refused in §20 for being wrong in both directions.

  **The read pool's default was never the problem, and saying so matters because the first version of this
  entry claimed it was.** Zero is recorded below as "a refusal to pick rather than a measurement", with the
  curve's non-transferability and the condition that makes zero a ceiling both written down. That question
  was asked and answered. What was missing is that **only reads were ever asked**: that a write, a barrier
  and the sweep's reads hold the engine's thread was stated in three places as a fact about where a cost
  lands, and nowhere as a question about whether it should.

  Measured, the write is rare for a workload whose holds die young (`hold-settle` at 100k/s: 58% die in the
  buffer, `engine record blocks peak 0`) and one `pwrite` every 56 records carried on for one whose holds
  live (`--resolve-after 900000`: 458,388 carried on, 8,987 blocks). Neither absent nor amortised, which is
  the worst shape for a tail.

  What it costs to leave: §19's throttle is a number chosen against this arrangement, so it moves when the
  arrangement does — §20 lists the four other numbers in the same position.
- ~~**`flush` means two things in this crate and `seal` does two jobs.**~~ Both are fixed, and they waited
  for §20's work on purpose: `flushed` would have counted a different event once the drain existed, and
  renaming a counter twice is worse than carrying the divergence for one piece of work.

  `flush` now means one thing — reaching the device — which is what `flush_window_hours` always meant. The
  counter that meant something else is `carried_on`: survivors leaving the writeback buffer for the block
  being packed, which is memory. The load driver had been printing it as `carried on` to work around the
  name, and the harness's `tick_until_written` was waiting on it under a third name again; both say what
  they mean now.

  `seal` means a block being closed, and `seal_block` does that and nothing else — the write left it when
  closing and writing became two calls.
- **The design has one memory tier and the code has two.** Design §4.3 is a *hot buffer*: one structure that
  answers reads and holds what write-back has not written, sized `peak × writeback` — 200k/s for a quarter of
  an hour, about 24GB, with backpressure at 12GB. The code splits it into the **writeback buffer** (the flush
  window, everything appended) and **residency** (the residency window, only survivors, already written).

  **What the split buys is a read window twenty-four times longer at the price of the survivor share.** At
  the design's scale, holding a day of *everything* is 12GB; a day of *survivors* at the measured 9% is
  1.1GB, and the hour of everything beside it is 0.5GB. Measured directly — `hold-settle --resolve-after
  100000`, residency at 24h against 1h — the second tier turns **200,016 store reads into zero**. What it is
  worth in time is not measurable here, because the store under it is memory: p99.9 moved 2.4ms to 3.3ms,
  which is a syscall path and not a device.

  **And the same measurement says when the design is right instead.** The buy is proportional to what dies in
  the buffer, and that is a workload's property: `partial-settle` kills 91% and the second tier wins large,
  `hold-settle --resolve-after 900000` kills **nothing** and it wins zero. `survives_flush_window` declares
  it and the report's `died in buffer` measures the same quantity beside it. A deployment whose holds mostly
  survive the flush window should collapse the two back into one.

  Kept as a divergence rather than settled, because settling it needs a device: what residency saves is IO,
  and this machine has no IO to save.

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

- ~~**Should reads for one block be coalesced, and where does the read path learn that they can be?**~~
  Answered by building it, and the answer to "where" is what took three attempts. **A volume keeps a cold
  read cache** (`Cached`, a `DurableStore` of its own), and the expiry path's device reads go from **92,000
  to 464** in the same run — the sweep reads a day's block once and every one of the fifty-one lookups that
  judge its voids is answered from it.

  Three things had to be right and two of them were got wrong first.

  **Where.** At the requester it buys nothing: the fifty-one lookups are all *submitted* before any
  completes, so at submit time the block has not been read. Measured at zero, twice, before that sank in.
  It has to be where the read happens.

  **How big.** One block is not enough even though the burst is one block, because the lookups' own
  completions evict it between the sweep's read and the judgements. Sixty-four blocks, 256KB, declared.

  **Which layer.** A store of its own rather than something inside a backing, so its position says what it
  caches: above `LatencyStore`, a hit costs no modelled device time, which is correct — the model prices a
  device and a hit never reached one.

  Coalescing is in the same type — a read for a block already on its way down waits on it instead of asking
  again — and it **measures zero on this workload**, because the sweep's own read populates the cache before
  any lookup asks. It is kept as the cache-miss path's only protection; a reader who wants it gone has the
  number.

- **The old question, for the record:** they were not coalesced, and fifty-one lookups for records on one
  block became fifty-one store reads of that block and fifty-one queue slots.
  *Where it showed:* expiry, by construction. A block holds fifty-one records, the sweep turns them into
  fifty-one voids in one slice, and each of those is judged with a lookup of a record on that same block.
  The day being emptied is `retention + grace` old and residency is a day wide, so every one of those
  reads misses memory and reaches the device. Measured: **92,000 store reads for 92,000 holds released**,
  and the queue at its full depth of 128 with 1,592 refusals against it.
  *What was tried and did not work:* keeping the last block read, in the buffer it was read into. It
  changed nothing, and the reason is the shape of the burst rather than the size of the cache — all
  fifty-one lookups are *submitted* before any completes, so at submit time the block has not been read
  yet. Coalescing is the version that fits: a read for a block already in flight registers as a waiter
  instead of submitting, and one completion answers all of them. That saves the queue slots as well as
  the reads, which a cache cannot.
  *Correctness rests on one thing:* a sealed block's bytes never change, which is what makes a whole-block
  checksum possible and what makes block numbers safe to key on — they count on across days and are never
  reused, so a number names one set of bytes for the life of the ledger.
  *Stops being safe:* it is not a safety question, it is an eight-fold one. On a device at 100µs a read,
  92,000 reads a second is nine thread-seconds a second; coalesced it is 1,800.

- **Where does `expiry_blocks_per_round` come from, and should it be a budget per round at all?**
  *Default:* a constant, two blocks a round. The headroom argument for it measured the **requirement** — a
  design day's survivors against a day — and never measured the **cost**, which is the other side of the
  same number and is now measured.
  *What the cost is:* the sweep's own reads turn out to be small — measured, it walks fewer than two
  thousand blocks in three seconds while releasing ninety thousand holds. **The reads that looked like the
  sweep's were the re-offers'**, and they are gone: an expiry void is now offered again only when the
  sequencer hands it back, which took a store read count from twenty-two per hold released to one. The
  attribution is worth keeping because it was wrong twice — first the sweep's walk was blamed, then a
  low-water mark was built for it that changed nothing, and only counting the lookups separated them.
  *Stops being safe:* if a day's blocks ever grow faster than the rounds available to read them — a much
  larger `daily_arrivals`, or a store whose reads are slow enough that a round no longer fits beside the
  lookups. The budget being per round rather than per second is still a number nobody declared in those
  units; it has simply stopped being the expensive one.

- **When does the idem engine get its rotating generations?**
  *Default:* a map that only grows, which owns the worst tail of any long run (see above).
  *Stops being safe:* it already is not, for measurement — no latency gate on a run longer than a few
  seconds is measuring this ledger rather than the stand-in. It becomes a correctness-adjacent question
  when a node has to run for a day.

- **What bounds a component's answer, and what happens when it misses that bound?**
  *Default:* nothing, for any of the three. Contract 2 says a component answers within a bounded time and
  there is **no detector for it anywhere** — the glossary's own entry points at `ledgerfio`'s latency knobs,
  which are a plan's inputs. A store that hangs holds the engine's thread for ever; a submitted read that never
  completes stalls one lane permanently. Neither is noticed, so neither is acted on.
  *Stops being safe:* at the first component that stops answering rather than answering slowly — which is a
  hung device, a wedged thread pool, or a network volume that has gone away. The reaction is the decision, not
  the detection: a bound, and then whether missing it quarantines a lane or fail-stops the node. A hang is not
  lane-local, which argues for the second. It is the same question for idem and for consensus, which is why it
  is not part of the store's work — and why the store has no `--store-hang-every`, since a knob whose reaction
  does not exist tests nothing.

  **The detector's home is decided even though the detector is not.** Design notes §20 works it out: a hung
  IO means the thread that issued it is inside a syscall, so the detector cannot be that thread — and it
  cannot be the worker's round either, or the detector's liveness depends on the liveness of what it watches.
  One sleeping thread that owns no IO, over every store's in-flight table. §20 builds the in-flight
  accounting, because a queue needs it anyway, and stops there: **the watchdog is deferred to this entry**,
  and lands when the bound and the reaction here are chosen. It is written down in both places on purpose,
  because a deferral recorded only where the work was is one nobody finds again.

- **What queue depth should the store hold, and who says so?**
  *Default:* 128 each, and they are two flags now rather than one: `--store-queue-depth` for the reads and
  `--store-write-depth` for the lane. **One number could only ever be right for one of them** — a read side
  wants Little's law on the read rate, reads a second times the latency of one, and a write side wants the
  block seal rate against a single ordered thread. Every depth figure recorded here is a read-side one,
  which is why that flag kept its name and its meaning.
  *Neither is variable at run time and neither should be.* A full queue is the signal that the device is
  sustainably slower than the ledger produces; growing it hides that until the memory runs out, which is
  rule 12's failure with the signal removed. What the volume line now prints — the peak each queue reached
  and the refusals against it — is how a declared number is found to be wrong in either direction.
  *(The rest of this entry is the original, and its numbers are read-side.)* 128 It is enough until a
  read is slow, and then it is the whole answer: at 40,000 store reads a second against a real filesystem,
  128 gives p99 93.7ms and 2048 gives p99 5.4ms. The same thing happened against the modelled store, where a
  `--store-read 5000` run reported p50 212ms at 128 and p99.9 9.7ms at 512.
  *Stops being safe:* it already is not, for any run that means to price a device — **twice a number read as
  "the device is too slow" has been a queue too short**, and the two are not separable from outside. What the
  depth should follow is arithmetic nobody has written down: reads a second times the latency they take, which
  is Little's law and the same rule the client's own queue depth obeys. Whether the engine should derive it
  from `PendingCapacity` rather than be told is the decision.

- **Who records the apply index on the *account* side, and how is it restored?**
  *Default:* nobody, and it is now one side rather than two. The pending engine's half is closed: a snapshot
  carries its coverage in the header, `Snapshots::read_into` puts it back, and a run reports where the last
  published one covered to. The account component keeps none and restores none. The seam that names the
  concept is still what holds it open — `ApplyIndex`, a commit carrying its batch's log position, the reactor
  recording the last one it applied, and two tests.
  *Stops being safe:* at the first snapshot of the account component. Its shape follows from that snapshot's
  shape the same way the engine's followed from §15's, which is why this stays a question rather than
  becoming a half-written method — and see the replay-idempotency entry below, which is the same seam from
  the other end.

- **Is applying a committed effect twice safe on the *account* side?**
  *Default:* unknown. The pending engine's replay-idempotency is established (§15), and it has to hold for
  the account component too the moment both checkpoint at different points — recovery then replays from
  the earlier one and the later component sees effects it already applied. The reactor already compares
  the two views' apply counts every tick and seals on a mismatch, so the invariant is live; what is not
  established is that it survives a restart.
  *Stops being safe:* the first time two components are snapshotted independently, which is the first
  checkpoint of either.

- **Is cold start local or from a peer?**
  *Default:* local, and it is half a decision rather than a whole one. *Where* a snapshot goes is answered —
  a directory of its own, one file replaced by rename (§19) — and a node can read that file back into an
  engine. What it cannot do is *start* from it, because the reconcile above is not built. The peer half is
  untouched and needs nothing new from this engine: the same bytes are what Raft's `InstallSnapshot` carries,
  over a wire that does not exist.
  *Stops being safe:* whenever a deployment has to survive losing every node at once — which is a question
  about the operation rather than about the code. A node that always fetches from a peer needs a healthy
  peer; a cluster that loses power together needs the local copy, or a log long enough to replay from
  nothing.

- **Does the client get told when a hold is voided for outliving its retention?**
  *Default:* no. The negative answer the design asked for is refused for a stated reason (above), and
  nothing replaces it.
  *Stops being safe:* whenever a client's correctness depends on distinguishing "expired" from "never
  existed". It needs a push channel the ledger does not have, so this is a protocol decision, not an
  engine one.

- **What does a barrier cover, now that one store instance can serve both the blocks and the snapshot?**
  *Default:* everything dirty on that store, and this is live rather than hypothetical — `--snapshot-dir`
  naming the same directory as `--store-dir` is one instance today. `submit_barrier` takes no argument on
  purpose: §16 made durability a fact about the store at a moment rather than a watermark per object,
  because a block can be durable inside a file whose *name* is not, and the file-then-directory order is
  what that call owns. **Per-file `fsync` is not the obstacle** and it is worth writing down, because the
  no-argument signature reads like a claim that it is: the backing already calls `sync_all` on each dirty
  file, and `fsync(fd)` is per file by definition — `sync()` is the system-wide one and `syncfs` the
  filesystem-wide one, and neither is what this uses.
  *Stops being safe:* not yet, and it is measured rather than assumed now. One directory against two,
  arms alternated, `hold-settle` with `--store-write-lane 1` and `--snapshot-every 200`: **the ledger pays
  nothing either way and the dump's share of the device is what moves.** Throughput at the ceiling is
  1.92–1.98M tx/s against 1.95–2.01M, mean +0.9% for the shared volume — inside the ±7% band, so no side
  (§10). p99.9 at 100k/s is 1.95ms against 1.98ms at the median, and the excursions above 8ms in sixteen
  pairs were the *two*-volume arm's, not the shared one's. What does move is what the dump gets through:
  **saturated it writes 37MB against 53MB, about thirty percent less**, because the chunk now queues behind
  the block writes on one device; rate-limited, with rounds to spare, it writes 117.5MB against 100.7MB —
  one dump more in the same two seconds. Both directions are what a shared queue should do. Which of two
  explanations owns the second — one barrier covering both writers instead of two, or one lane thread
  instead of two on a machine with four performance cores — is not separated by these runs, and neither is
  the scope question itself: this says what sharing costs, not whether a barrier should have been
  per-writer.

- **What declares this node's configuration, and in particular which directories are one volume?**
  *Default:* command-line flags, and there are already more than forty — plus one rule that is not a flag:
  the same directory for the blocks and the snapshot is one store, and two different directories are two.
  That rule answers the only case a path can answer, and it leaves the case that matters open. Two
  directories on one disk get two queues today, which means two queue depths against one device — the same
  shape of mistake as reading a number of ours as a number of the device's, and the reason `st_dev`
  detection was refused (§20). Every knob here has landed as a flag because that is where the tools' knobs
  go, and a node's configuration is not a tool's argument list — a declared file (TOML, YAML, JSON) is what
  a deployment can review, diff and keep.
  *Stops being safe:* it is already awkward, and the volume rule above is where it becomes a number rather
  than a nuisance. Recorded here so that adding volumes as flags is not mistaken for an answer to this.

- **How many threads should issue the store's reads in a deployment?**
  *Default:* none, and that is a refusal to pick rather than a measurement. The curve on this machine peaks at
  two threads and falls away after, because what bounds the count is the cores left after the reactor, the
  pending worker and the client — a deployment's property, not a constant. The design says sixteen, which is
  Little's law at half a millisecond a read.
  *Retaken with the write lane, which was the ⚠ on it:* the shape survives and the fall is steeper, which is
  the competition for cores showing up as predicted. Two pairs, saturated, every read a store read —
  **lane off: 1.03M tx/s at zero threads, 1.05M at two (+2%), 0.90M at four, 0.82M at eight, 0.72M at
  sixteen (−30%). Lane on: 1.67M, 1.72M (+3%), 1.37M, 1.04M, 0.90M (−46%).** The peak is in the same place
  and worth less, and past it a thread costs more than it did, because the write lane is one of the things
  it is now competing with. Nothing about the default changes; what changes is that the ⚠ is discharged.
  *Stops being safe:* the moment `O_DIRECT` is in play, and then zero is not merely suboptimal but the ceiling:
  the synchronous read happens *inside* `poll` on the worker's own thread, so a 100µs device read caps store
  reads at 10k a second and stalls the whole component for each of them. **The measured curve does not transfer
  either** — here a read is CPU, so a thread competes for a core; on a device it blocks in the kernel and costs
  nothing, which is the contention that shapes the curve disappearing. What a deployment needs is the device's
  latency, and 100µs at 30k reads a second wants three threads while 0.5ms at 200k wants a hundred — the last
  of those is the only corner a pool cannot reach, and it is io_uring's.

- **Which of the design's storage questions are still untouched, and is that acceptable?**
  *Default:* three of the five have moved and two have not. `SE-OQ-4` (io_uring against a thread pool) is now
  a choice between implementations of two methods on `DurableStore` rather than a question with nowhere to
  live; `SE-OQ-5` (compression) is narrowed to inside a block, because block-level compression would break the
  offset rule; `SE-OQ-6` (the ≤5ms worst case) is measurable against the model — 5ms reads sustain 100k tx/s
  at p99.9 9.7ms with a queue depth of 512 — but not against a device, because macOS has no `O_DIRECT`.
  Untouched: `SE-OQ-3` (a group spilling across blocks and what it costs in IO) and `SE-OQ-8` (provisioning
  down on the cache hit rate).
  *Stops being safe:* at the point a real device goes in. Fewer of them go live at once than before, and the
  two that remain are both about IO volume rather than about the interface.

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

  **The unit changed when it became code, and the change is the finding.** An interval in time needs a clock
  the engine does not have and should not get: a wall clock steps backwards and a monotonic one restarts at
  zero, so neither can express "since the last snapshot" across a restart. In log positions both problems go
  away, and the quantity being measured is the one that matters anyway — what recovery replays and what the
  log has to retain are both counted there. `--snapshot-every` is a distance (§19).
- **What throttle paces the snapshot's write, and what number?** Bytes a round, and **4096** — a block's
  worth, 128 buckets. Measured, per byte a larger chunk is straightforwardly cheaper: 64KB costs 0.11% of
  throughput for each MB/s it writes against 4KB's 0.28%, because the syscall amortises. The tail says the
  opposite and the tail wins. A chunk is written inside one worker round, so it stalls the thread every
  lookup passes through, and while a dump runs the median is 1.5ms at 4KB and 6.5ms at 64KB against a 1.3ms
  baseline and a 5ms contract — a small chunk running more of the time costs the median a little, a large one
  running less of the time costs a percentile a lot, and a percentile is what the contract is written in.
  Design notes §19 has both curves and the command.

  **A second bound came out of writing it, and it was the one nobody had declared.** The copy-on-write
  shadow §15 sized by arithmetic was a map with no ceiling, so the real bound was the allocator — rule 20
  exactly. It is declared in buckets now and a breach abandons the dump, which costs the work and nothing
  else.

  **Retaken after §20, and the number survives with a different reason behind it.** Moving the chunk onto
  the store changed what the flag means: the store's unit is a block, so it now sets how many 4096-byte
  writes a round does rather than the size of one write — and both halves of the trade above were about one
  big syscall. Seven alternated pairs, `partial-settle` at 1M/s, continuous dumping: off the write lane the
  median is 1.51ms at 4KB, 1.55ms at 64KB and 1.73ms at 256KB against 1.34ms with no dump, so the shape
  holds and the slope does not — sixty-four times the bytes now costs 15% where sixteen times used to cost
  313%. **On the lane it is flat**: 1.40ms at every size against 1.35ms with no dump, and the dump runs
  2.1GB in three seconds against 1.65GB. So one block a round stays, because on the lane a larger chunk
  costs nothing and buys nothing — the dump is bounded by the rounds it gets — and off the lane smaller is
  simply better. Design notes §19. It stays here as closed because it was genuinely measured and it is the right number
  for the code that exists — but it has to be taken again with the lane, and the same is true of the block
  durability entry below. Left as a note rather than reopened, because reopening a question that *was*
  answered would lose the measurement with it.
- **Does the snapshot share a disk with the Raft log?** Not this code's to say, and now it cannot be:
  `--store-dir` and `--snapshot-dir` are separate flags and neither implies the other. The design puts the
  log and the snapshot on Disk 1 (§2.2); what sharing changes is only the arithmetic — a shared volume
  measures the snapshot's share against the log's own write rate, a separate one against the engine's reads —
  and the throttle is required either way, which is what makes this a provisioning decision rather than a
  hole. At a long interval the share is small in absolute terms: 42.7GB an hour is 11.9MB/s, 2.4% of a
  500MB/s volume, and ten minutes is six times that.
- **What sizes the expiry throttle's slice?** The day does. The requirement is met three orders over and the
  binding constraint is a single round, which is bounded by declaration rather than by density.
- **How often should the engine make its blocks durable?** Every worker round, and there is nothing to trade.
  The answer at the design's own rate is arithmetic rather than a curve: 150M arrivals a day is 1,736/s, which
  seals about thirty-four blocks a second, so a 500µs `fsync` is 1.7% of one thread against a real NVMe's
  50–500µs. Two orders of headroom, and a deferred sync would buy that back at the price of coverage.
  What a curve adds is where it *stops* being free, and the transferable form of it is a budget rather than a
  rate: **one thread divided by the block seal rate.** Measured at 1M tx/s, where the seal rate is 19,444
  blocks a second and so the budget is 51µs, `--store-write 50` costs 29% of throughput and `--store-write
  100` costs 56% — the budget is the knee. A sync is four times cheaper per microsecond at the same point,
  because group commit means one sync covers every block a round sealed and a slower device gets fewer of
  them. Design notes §16 has both curves, the read curve beside them, and why 1M is the rate they were taken
  at.

  **Retaken on the lane, and the answer holds with the same arithmetic behind it.** The retake needed the
  model fixed first: `LatencyStore` charged every write to the caller's thread, so it priced a lane as
  though it were not there and gave the same figure either way. It asks the backing now
  (`writes_are_queued`) and gives a queued write a deadline the way it always did a read.

  With that, `hold-settle --resolve-after 900000` saturated, three alternated pairs. **A sync is cheap on
  either arrangement**: 400µs costs 9.6% of throughput off the lane and 3.9% on it, and 100µs costs 5.6%
  and 5.8%. Every round stays the answer, and group commit is still why — one barrier at a time, covering
  everything sealed since the last, so the barrier rate limits itself.

  **The budget's shape survives and the thread it divides has changed owner.** The seal rate here is
  19,667 blocks a second, so the budget is 51µs — the same figure as the original run's 19,444 and 51µs,
  which makes the two directly comparable. Below the budget the lane absorbs most of the cost:
  `--store-write 50` takes 41% of throughput off the lane and **16%** on it. At twice the budget both are
  swamped, 61% against 56%, because one lane thread cannot serve 19,667 writes a second at 100µs — that
  needs two. So: **one thread divided by the block seal rate, and the thread is the lane's.** What the lane
  changes is not where the knee is but who pays below it — off the lane every lookup does, on it nobody
  does until the lane itself saturates.

  **And the lane's own worth at saturation is +37%**: 1.26M tx/s against 1.72M with real files on that
  workload. §20 records +7 to +9% for the same lane on a rate-limited run — this is the ceiling, where the
  thread the writes left is the thing in short supply. It is the default now.
- **Where does the writeback buffer's drain run?** In the worker's round, on a declared budget — not on a
  thread of its own. A drain asks the index what is still alive and repoints the survivors, and four ways of
  moving that off the worker all fail or collapse: a lock is forbidden on that path (rule 10), a drain that
  owns the index inverts every lookup through a queue, a cuckoo kick crosses arbitrary buckets so there is no
  partition, and having the worker compute the survivors leaves the index work — most of the cost — exactly
  where it was. So a drain thread is really *who owns the index*, which is a different and larger piece of
  work, and **it cannot be judged today because the drain has never been measured apart from apply.** Moving
  it to the round is what produces that number. Design notes §20.

  **What it costs is worth keeping, because it is a bound nobody had declared.** Today the flush window
  cannot be exceeded — the producer *is* the drain, so one in, one out. That holds by the arrangement rather
  than by anyone declaring it, which is rule 18's shape. Moving the drain means a round can dequeue thousands
  of commands while draining a budget's worth, so apply has to pause when the buffer is over its window. New
  machinery, and the honest price of the move.
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
command that reproduces it.

**And a comparison names both arms, run together.** This machine drifts about ten percent over an hour of
sustained benchmarking, which is larger than most of the effects here — a change measured against a
baseline from earlier in the same session once produced an eight percent regression that did not exist.
`--repeat` sees noise within a set and cannot see drift between sets.

**Then read the band before reading the result.** Eleven interleaved pairs of one comparison spread from
−8.4% to +6.5%, mean −1%, and ten-second runs were no tighter than five-second ones. **Nothing smaller than
about seven percent is resolvable here without many pairs**, and a result inside that band has to say so
rather than pick a sign. Design notes §10.

The ones that decide the most:

- `ledgerfio run --workload partial-settle --duration 10s` — where reads land across the two
  windows, and what compaction saves.
- `ledgerfio run --workload hold-settle --resolve-after 100000 --residency 1 --overlay-limit 10000` — the
  one combination that makes every read a store read, which is what puts a device's latency on the path it
  is meant to price. Both flags are needed and neither alone does it; `--resolve-after 900000` lands no
  resolution at all in a five-second run, which is how a note claiming this tool could not reach the read
  path came to be written.
- `ledgerfio run --workload partial-settle --rate 1m --sweep store-write=...` — what a device costs on the
  paths that hold the thread. 1M/s is a ceiling-finding rate rather than a target, and the number that
  transfers off it is a budget: one thread divided by the block seal rate.
- `ledgerfio run --workload hold-settle --resolve-after 100000 --residency 1 --overlay-limit 10000 --store-dir
  <path> --store-queue-depth 2048` — the same read path against real files instead of a model. The depth is in
  the command on purpose: at 128 the same run reports p99 93.7ms and reads as a slow device, at 2048 it is p99
  5.4ms. Not a device measurement on macOS, which has no `O_DIRECT`.
- The same with `--rate 0 --store-read-threads 0,2,4,8,16` — what the read pool is worth, and the reason a
  number off it does not travel: the curve peaks at two threads here because two is what four performance cores
  have spare, and on a device the threads block in the kernel instead of competing, so the shape inverts.
  Design notes §18.
- `ledgerfio run --workload hold-settle --resolve-after 900000 --external-ratio 30` — what order
  exemption is worth, as lane depth.
- `ledgerfio run --workload partial-settle --rate 0 --snapshot-dir <path> --snapshot-every 1
  --snapshot-bytes <n>`, **each arm preceded by the same run with no `--snapshot-dir`** — what a snapshot
  costs a node that is serving. Two columns, and they disagree: cost per MB written falls as the chunk
  grows, and the median under a dump rises with it. `--rate 1m` for the second, since a saturated run has no
  median left to move. Continuous dumping is what `--snapshot-every 1` is for, and no deployment does that —
  it is how the duty cycle is taken out of the measurement. **Not a sweep**: a sweep runs the arms minutes
  apart with no baseline between, and this machine moves ten percent in an hour.
- `ledgerfio run --workload hold-settle --duration 2s --resolve-after 900000 --store-write-lane 1
  --snapshot-every 200`, once with `--store-dir` and `--snapshot-dir` naming **one** directory and once
  naming two, alternated pair by pair — what declaring the two writers onto one disk costs. Read the
  dump's own `wrote NN MB` rather than the ledger's throughput: the throughput difference is inside the
  band and the dump's is not. `--rate 0` and `--rate 100k` answer opposite halves of it, which is the
  point — a shared queue slows the dump when the device is busy and speeds it when it is not.
- `ledgersim check --seeds 64` — every invariant under fault injection, including the store path.
- `cargo bench -p ledger-pending --bench sweep -- --repeat 5 --pin 2` — what emptying one day costs,
  driven through the real engine. Three rows, and the middle one is the one a policy needs: records read
  per void against how much of the day is still alive, because a day's blocks hold its dead as well as
  its living. The third says a round stays bounded as the blocks it reads grow, which is the property the
  index scan did not have at any setting.
- `ledgerfio run --workload void-heavy --rate 100k --daily-arrivals 150m --index-budget 4100m
  --expiry-days 60` — expiry as a client sees it. Sixty days forces a sweep per day; it is the run that
  showed the old index scan as a 108ms tail and shows nothing now.
