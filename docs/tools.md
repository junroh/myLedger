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
§5 and §10.

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
- **A hit never reaches it.** The sequencer reads the resident view inline, so what the engine is asked
  for is misses, fences and writes. The command rate it must sustain therefore follows the hit ratio
  (`--pending-hit`), and writes put a floor under that rate which a perfect hit ratio does not remove.
  There is one rate, not one per kind: a slow write path shows up as reads queueing behind it.
- **The tail is what makes answers finish out of order**, and that is a cost of its own: an answer that
  is ready waits for an earlier one on its lane, so the wait is the queue depth times a latency, which no
  per-command bound covers. The run reports it as `order wait`, separately from the engine, because the
  two are fixed by different things — a faster engine for one, fewer constrained requests on a hot lane
  for the other. Concentrating accounts (`--skew`) multiplies the fences and deepens that wait.
- **Consensus** has a tail too (`--raft-tail-us`), and answers in commit order regardless: a batch that
  would finish early waits for the one in front of it.
- **What the engine is asked for** follows the hit ratio, not the transaction rate. The report prints the
  hit ratio it measured next to the command rate, so the knob is checked rather than assumed. How many
  entries a cache needs to reach that ratio is the engine's own question, and the resident view the
  sequencer reads inline is not the same thing as a cache inside the engine — the model treats them as
  one layer on purpose.

## What a harness must not assume

Two rules, each written after breaking it. They are about the simulator, not the ledger, and both cost
a run that looked plausible and was wrong.

- **Owed work is a monotone accumulator, never a function of `now`.** A virtual clock jumps — to the
  next component event, or by whatever the tick's work cost — so a deadline set as `now + gap` throws
  away every gap the jump covered. Counted from the previous deadline instead, nothing is lost. The run
  that broke this asked for 500k arrivals a second and offered 17k, and looked like a quiet ledger.
- **A loop waiting for the ledger exits on the ledger's terminal state, not on a timeout.** A sealed
  apply path means no commit is ever coming, which is the design working; a harness that waits for one
  anyway turns a reportable outcome into a hang. A timeout is not a substitute: it spins and then reports
  the wrong thing.

## What a sweep proves

Only what it reached. `check` prints what it visited — commits, rejections, duplicates, chains,
fences, lookups, evictions, seq gaps, quarantines, refused commits — because "no invariant broke" and
"nothing happened" look identical otherwise. That report is how a missing fault gets noticed: the
first version of the simulator injected out-of-order replies that could never fire, and the coverage
line is what showed it.
