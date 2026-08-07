# How this code is measured

Five tools, each answering a question the others cannot. Confusing them produces numbers nobody should
trust. No results are quoted here — each tool prints its own, with the conditions it ran under.

| | Question | Runs the real ledger? | Time |
|---|---|---|---|
| benchmarks | what does one structure or stage cost? | yes | real |
| `ledgerfio` | how fast is the node as built, and does it meet its SLO? | yes | real |
| `ledgersim check` | does any interleaving or fault break an invariant? | yes | virtual, costs ignored |
| `ledgersim capacity` | what would it do on hardware or components we do not have? | yes | virtual, costs modelled |
| `ledgersim require` | how slow may a component be and still hold this rate at this tail? | yes | virtual, solved by bisection |

## Why each exists

**Benchmarks** answer *why* a number is what it is. A load driver reports what the ledger does per
second; it cannot say whether a 32-byte lane beats a padded one, whether padding a work item pays, or
whether the hand-written queue still beats `rtrb`. Those decide how the code is written, and only a
microbenchmark answers them. The numbers and the conditions they were taken under are in design notes
§5, §10, §11 and §12.

**The load driver** is the only tool that measures the thing itself. Its numbers include everything
the others model away: allocator behaviour, cache misses, the scheduler, real threads. It is also the
only one that can fail an SLO, and `--cpu` turns it into the source of the cost model the simulator
uses.

**check** explores what no fixed test does: the order in which things happen. Faults are injected —
replies out of a lane's order, refused commits, commits answered for the wrong batch, an overlay that
keeps nothing — and after every step the ledger is asked to audit itself. A seed decides the faults
and the traffic, and nothing reads the wall clock or starts a thread, so a failure reports a seed that
reproduces it. Its oracle is the sequencer's own `audit`, which means an invariant added there is
explored by every seed from then on without the simulator changing.

**capacity** answers the sizing questions. The pending engine's disk tier does not exist, so nobody can
measure what a slow lookup does to the tail, and this machine is not the target machine. So the same
virtual clock is advanced by what the work *would* have cost, with the per-stage costs measured by
`ledgerfio run --cpu` rather than guessed. The control flow is the real reactor's — batching, fences,
chains, backpressure — which is the whole difference from a model that reimplements the sequencer and
then drifts from it.

**require** is `capacity` run backwards, and it is the question a design actually asks. Not "what
happens at this latency" but "how much latency does the pending engine get". It bisects on the forward
model — so the answer still comes from the real reactor — and prints the run it settled on, because a
budget without the evidence behind it is a number to argue with rather than to build against.

One unknown at a time (`--solve pending|raft|idem`): a budget for two at once is a curve, not an answer.
The report leads with every input and marks the unknown. A target the consensus round trip and its tail
already spend leaves the component nothing, and the tool says that instead of solving — what has to
change then is not the component being asked about.

## Reading a capacity prediction

It is an estimate and says so. Three things decide whether it means anything:

- **The costs it was calibrated with.** They come from one workload on one machine. Another machine means
  measuring again, or `--cost-scale` to bracket it.
- **How much work each committed effect took.** The simulator's traffic is deliberately messier than a
  benchmark's: duplicates, resolutions aimed at holds that were refused. It reports admitted work per
  committed effect for that reason: a prediction and a measurement with different work per commit can be
  the same cost model rather than a disagreement.
- **Which limit the run actually hit.** A closed loop measures the smaller of two things: the ledger, or
  the **client's queue depth** divided by the latency — the quantity `fio` calls `iodepth`, and the one
  Little's law is about. A depth too small for the latency caps the run however fast the ledger is, so
  the report names the client when the client was the limit; the fix is then more concurrency, not a
  faster ledger. Depth has to cover rate times latency, and the sequencer's slots have to cover depth, or
  the excess is refused as overload.

Arrivals are drawn, not evenly spaced (`--arrivals smooth` for the comparison): nobody coordinates
clients, and a tail is made of the moments when several arrive at once.

Two things the modes deliberately do not share. Queues are shallow in `check`, because a bounded queue
that is never reached explores no backpressure, and deep in `capacity`, where a queue this tool chose
must not be the answer. And the client is messy in `check` — it resolves holds that were never
committed, which is a shape the ledger has to refuse — but well-behaved in `capacity`, where a run
that spends its core on refusals predicts nothing about one that does not.

The `audit` oracle runs between ticks, never inside one: it walks every account, so `check` asks after
every step, while `capacity` asks periodically — asking every step there would cost more than the run
it is measuring.

## What the components cost in a prediction

The reactor's CPU is charged per stage. The components are charged as time, and how they are modelled
decides what the prediction is worth:

- **Consensus** is bounded the way the real thing is: `in_flight` batches of at most `max` effects, so
  the batch policy caps how much can be outstanding. Its round trip does not grow with load, though, so a
  prediction assumes a network that carries the log those effects make, once per follower.
- **The pending engine is one black box** with a latency, a tail and a rate (`--pending-us`,
  `--pending-tail-us`, `--pending-rate`). What it does inside — an index, a cache, a disk tier — is its
  own design question, and modelling that here would answer a question this tool is not asking. Rate
  times latency is how many commands it can have outstanding before one starts waiting, and the rate
  matters as much as the latency: a component modelled as latency alone has unlimited parallelism, and
  against one of those even a very slow tier costs almost nothing, which is wrong rather than
  encouraging.
- **The windows are declared, not set.** `--daily-arrivals`, `--flush-survivors`, `--flush-window` and
  `--residency` are business inputs; the engine derives its block counts from them and refuses a
  combination that does not describe a workload — residency shorter than the flush window, or a
  retention-end survivor share larger than the flush-window one. The refusal is at startup, with exit 2,
  because every size in the engine follows from these and a nonsense declaration would otherwise become a
  window nobody meant. `--index-budget` is the same idea for the index.
- **Expiry needs a calendar the run can move.** `--expiry-days <n>` advances the engine's day *n* times
  across the measured phase, evenly, instead of leaving it on the wall clock — where a run of seconds
  never crosses a day and nothing about expiry can be measured at all. Past `retention-days + grace-days`
  to reach the expiry of holds the run created itself, and well past it to make the sweep frequent enough
  to find in a tail: three sweeps in ten seconds hide, sixty do not. `--expiry-blocks <n>` is the blocks of
  the expiring day a round reads, which bounds the voids too at fifty-one records a block. The report's
  `sweep` line prints the blocks read and the records read per void released — the second is the ratio a
  day's density decides, and the one worth watching.
- **The simulator declares the same two things**, as `--resolve-after`, `--flush-blocks` and
  `--resident-blocks`, and its capacity report answers the question they exist for: at this age and these
  windows, what share of resolutions costs an IO. With the default windows that share is zero at the
  design's target rate; with four blocks each it is 95% at age zero and 100% at age five thousand.
- **`check` draws narrow windows on purpose.** Three seeds in four get windows of a few blocks, so records
  leave memory inside a two-thousand-step run and the fetch path — the candidate walk, the fingerprint
  confirmation against a record, replies completing in the device's order rather than the lane's — runs
  while the faults are on. The sweep test asserts the store was reached, because before this the sweep
  reported that every invariant held about a path it never entered. It asserts the same about exempt
  lookups: some holds debit the unconstrained account, so their resolutions keep no place in a lane and
  the order exemption itself runs under faults, not only the data check that covers it.
- **A hold needs an age before any of this is exercised.** `--resolve-after <n>` resolves a hold once *n*
  more have been created behind it, so its record is read back at a declared age rather than moments after
  it was written. Without it a workload has only two settings — resolve at once, or never — and the engine
  answers everything from its newest blocks, which is not a measurement of the read path but of its
  absence. The age decides which window answers: `hold-settle` at age 0 answers every read from unwritten
  blocks and 99% of records never reach the store; at 100,000 nothing dies in the buffer and every read
  comes from residency; at 900,000 every read is a store read. Those three regimes are the three zones,
  and they are what `engine reads` prints.
- **Every resolution reaches it.** The record a resolution is judged by is the engine's, so what it is
  asked for is one command per resolution, plus fences and writes — the command rate follows the traffic
  and nothing on the sequencer's side reduces it. What the engine's own memory saves is the IO *below*
  that command, which is `--store-read` and `--store-iops`, reported as `reads: memory=N store=M`.
- **The device's two kinds of cost are separate flags, and each is charged where the backing puts it.** A
  lookup's read occupies the *device*: `--store-read` and `--store-iops` set a deadline per read and the
  engine keeps working while the queue serves it. A write (`--store-write`) and a sync (`--store-sync`)
  occupy whichever the arrangement says — with `--store-write-lane 1`, the default, they are a deadline on
  the lane's one ordered thread; with `0` they hold the engine's own thread, so every lookup's latency
  includes them. The model asks the backing rather than assuming, which it did not always do: it charged
  the caller either way and so priced a lane as though it were not there. Zero for all of them is the exact store, which every other answer is measured against. Design notes §16 has the measured curves; the
  short of it is that a per-block write is roughly four times as expensive per microsecond as a sync, because
  one sync covers every block a round sealed and a write does not.
- **`--store-dir <path>` puts the engine's blocks in real files**, one per segment, and unset is memory —
  which is what every other number in these documents was taken against. It is the only way to measure the
  syscall path rather than a model of it, and on this machine it is *not* a measurement of a device: macOS has
  no `O_DIRECT`, so reads come through the page cache. Design notes §16 has both columns.
- **`--store-read-threads <n>` issues the store's reads on a pool** instead of synchronously inside `poll`.
  Zero is the default, and that is a refusal to pick rather than a measurement: the curve peaks at two threads
  here — 9% better than synchronous — and is 43% worse at sixteen, because what bounds the count is the cores
  left over after the reactor and the worker. Design notes §18 has the curve, and the reason a number read off
  one machine's spare cores does not travel.
- **`--store-write-lane <0|1>` puts `pwrite` and `fsync` on a thread of their own.** Off by default, which
  is the synchronous baseline the way zero read threads is — not a recommendation. It is worth far more than
  the read pool and for a reason a page cache does not hide: measured against real files at 200k/s, p99.9
  goes from 103–124ms to 3.4–3.7ms, because an `fsync` is a durability barrier whatever the cache does and
  it was running on the thread that answers every lookup. **One thread, not a pool**: a segment's first block
  has to land before the ones after it and a barrier has to follow what it covers, and one queue keeps both.
  Design notes §20.
- **`--snapshot-dir <path>` is where snapshots go, and whether it is `--store-dir` is the declaration.** Two
  flags because they may be two volumes, and which they are is a provisioning decision the design makes
  elsewhere (§2.2). **Naming the same directory for both is how a run says they are one disk**: then one
  store serves the blocks and the dump, one queue between them, and the dump's writes are ordered with the
  blocks' on the same lane. Two different directories are two volumes and two queues, and two different
  directories that are really one disk is a case nothing here can detect — see `status.md` on what declares
  a node's configuration. Naming the directory is also what turns the policy on: a cadence with nowhere to
  write does nothing, so neither flag alone makes a node write files. The run reports `engine snaps` only
  when one was named.
- **`--snapshot-every <n>` is a distance, not a duration**, counted in committed batches. That is why there
  is no snapshot clock to inject: what recovery replays and what the log has to retain are both counted in
  log positions, and a duration would need a monotonic clock that restarts at zero. A run at 100k/s reports
  about eighty effects a batch, so the conversion is one multiplication.
- **`--snapshot-bytes <n>` is the throttle and `--snapshot-shadow <n>` is its ceiling.** A whole number of
  4096-byte blocks, because a block is what the store takes — anything else is refused at start-up rather
  than rounded into a number nobody declared. The chunk is written inside one worker round, so with the
  write lane off it is a stall on the thread every lookup passes through, which is why the default is one
  block. **It no longer sets the size of a write** — the store takes blocks, so it sets how many 4096-byte
  writes a round does, and the per-byte amortisation a larger chunk used to buy went with that. Retaken:
  off the lane the median is 1.51ms at 4KB and 1.73ms at 256KB; on the lane it is 1.40ms at both. Design
  notes §19 has the table. The shadow is the buckets the
  stable read holds aside, and a dump that breaches the budget is abandoned rather than allowed to grow; the
  report prints the peak beside the ceiling, so a throttle too slow for its cadence reads as `abandoned`
  climbing while `written` stays at zero. Design notes §19 has both curves.
- **Measuring what a snapshot costs needs `--snapshot-every 1`**, which dumps continuously and is what no
  deployment does. That is the point: it takes the duty cycle out, so the number is what a dump costs *while
  it runs*, and a deployment multiplies by its own cadence. Read the median at `--rate 1m` and the throughput
  at `--rate 0` — a saturated run has no median left to move, and a rate-limited one has no throughput left
  to lose.
- **The read queue's depth is a flag, and it bounds what a slow read can hide behind.**
  `--store-queue-depth` (128) is how many reads the store holds at once; past it reads are refused and the
  engine keeps the command. At 40,000 store reads a second, 5ms reads need about two hundred outstanding — at
  128 the run reports p50 212ms and that number is the depth rather than the device, at 512 it is p99.9 9.7ms
  with throughput intact. A `--store-read` number is only about the device once the depth is not the limit.
- **`--store-fault-every` makes the store refuse** and **`--store-corrupt-every` makes it answer wrongly**,
  which are the only ways to reach the seals those produce: `MemoryStore` neither fails nor lies. The run says
  `SEALED` with the two counts apart, and `fail-stop=true`. The second needs a run that actually reads the
  store, so it needs the two flags in the next point as well. A device that *hangs* is the third way and there
  is no knob for it, because nothing in the ledger detects a component that stops answering —
  `status.md` has that as a decision rather than a gap to fill in passing.
- **Reaching a store read takes two flags at once, and neither alone does it.** A read happens only when a
  resolution needs a record that is no longer in memory, so the residency window has to be short enough to
  fall out of (`--residency 1`) *and* the hold has to be resolved after it does but still inside the run
  (`--resolve-after 100000`). With both, `engine reads` shows every read going to the store; with
  `--resolve-after 900000` a five-second run at 100k/s lands no resolution at all and the whole run is holds.
  A number from `--store-read` on a run whose `engine reads store=` is zero is a number about nothing.
- **The tail is what makes answers finish out of order**, and that is a cost of its own: an answer that
  is ready waits for an earlier one on its lane, so the wait is the queue depth times a latency, which no
  per-command bound covers. The run reports it as `order wait`, separately from the engine, because the
  two are fixed by different things — a faster engine for one, fewer constrained requests on a hot lane
  for the other. Concentrating accounts (`--skew`) multiplies the fences and deepens that wait.
- **Consensus** has a tail too (`--raft-tail-us`), and answers in commit order regardless: a batch that
  would finish early waits for the one in front of it.
- **What the engine is asked for** follows the traffic: one command per resolution, plus fences and
  writes. Nothing on the sequencer's side takes a command away, because the record a resolution is judged
  by is the engine's. Whether the engine answers from its own memory or reads the store is the layer below
  — reported as `reads: memory=N store=M`, and priced by `--store-read`.

## What a harness must not assume

Three rules, each written after breaking it. Two are about the simulator and one about any measurement here,
and each cost a run that looked plausible and was wrong.

- **Owed work is a monotone accumulator, never a function of `now`.** A virtual clock jumps — to the
  next component event, or by whatever the tick's work cost — so a deadline set as `now + gap` throws
  away every gap the jump covered. Counted from the previous deadline instead, nothing is lost. The run
  that broke this asked for 500k arrivals a second and offered 17k, and looked like a quiet ledger.
- **A loop waiting for the ledger exits on the ledger's terminal state, not on a timeout.** A sealed
  apply path means no commit is ever coming, which is the design working; a harness that waits for one
  anyway turns a reportable outcome into a hang. A timeout is not a substitute: it spins and then reports
  the wrong thing.
- **Before a number is about the thing being measured, name every bound in the path that *this repository*
  chose — and vary it.** Three times in one session a figure was read as a device's and was ours: a
  `--store-read 5000` run reporting p50 212ms was a queue depth of 128 that a runner had written as a
  constant; "a read pool costs a third of the read ceiling" was sixteen threads on a machine with four
  performance cores; and a thread-count cliff at sixty-four was a fifty-microsecond park timeout waking every
  idle thread twenty thousand times a second. Each time the fix was a knob of ours, not a slower disk, and
  each time the giveaway was the same — the number moved when something we owned did. Vary our bounds first;
  the component's turn comes after they are excluded.

## What a sweep proves

Only what it reached. `check` prints what it visited — commits, rejections, duplicates, chains,
fences, lookups, evictions, seq gaps, quarantines, refused commits — because "no invariant broke" and
"nothing happened" look identical otherwise. That report is how a missing fault gets noticed: the
first version of the simulator injected out-of-order replies that could never fire, and the coverage
line is what showed it.
