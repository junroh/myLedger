# Design notes

Decisions the code cannot explain on its own: what the source design left open, where the code
departs from it, and the measurements behind each choice.

## 1. Lane ordering when requests traverse different stages

The design gives each external component contract 1: return a lane's results in seq order.
It also lets single-phase transfers and holds skip the pending round trip entirely, and it
forbids the sequencer from reordering. Those three cannot hold together.

A request becomes judgeable when *all* of its external results have arrived. Settle and
void wait on idempotency and pending; single-phase and hold wait on idempotency only. Two
ordered streams joined by AND are only ordered if every item traverses the same streams, so
a later hold overtakes an earlier settle on the same lane as soon as the pending path is
slower than the idempotency path. Every component honoured contract 1; the join broke it.
Measured with `ledgersim check`: equal latency on both paths gives no gaps, and making the pending
path slower produces them immediately.

**Resolution — lane fence.** A request needs a reply from the pending path when either

- it needs hold data that is not seeded yet (a lookup), or
- its lane already awaits a pending reply (a fence: a token that reads nothing).

Fences travel the same channel and the same lane, so the pending engine's ordering also
orders them. Ordering therefore stays outside the sequencer, the sequencer keeps only its
seq check, and a gap keeps its original meaning: an external component misbehaved.

Costs accepted with this choice: a pending hiccup now also delays the fast kinds on lanes
that have settles in flight, and intake is coupled to the pending queue's capacity. Both
are observable (`fences`, `dispatch_deferred`).

Rejected alternatives: routing every request through pending (loses the fast path for all
traffic), and parking out-of-order arrivals inside the sequencer (cheapest, but it turns
the always-on gap detector into a metric and moves ordering back into the core).

The lane's outstanding-reply counter lives in `AccountRecord`, so intake is refused with
`Overloaded` once a lane reaches the counter's ceiling. A wrapped counter would
under-report outstanding replies and silently break ordering.

### Order exemption: an unconstrained debit keeps no place in the lane

The fence exists because judging a request can depend on the lane's earlier requests. That
dependency comes from one place: the balance check. An account the ledger does not constrain
(`AccountFlags::CONSTRAINED` absent — a suspense or clearing account) has no balance to protect,
so a single-phase transfer or a hold debiting it depends on nothing that came before it. Those
requests take no seq, are not continuity-checked, and never fence.

A settle or void is never exempt, whatever it debits: its order against other resolutions of the
same hold decides which one wins, and it consumes external data, which is exactly what
contract-1 detection is for. So the lane still orders every resolution, and the exemption covers
only the kinds that read nothing.

Measured with `ledgerfio run --workload hold-settle --external-ratio 30`, where many debits land on one
clearing account: the exemption raises throughput and cuts fences by most of their count. Without it a
busy suspense account serialises everything behind it, which is the cost of promising an order nobody
asked for.

### Why the fence is conditional, with the price of the alternative

The obvious simplification is to send *every* request down the pending path: the engine then orders
everything by construction, and the sequencer loses a condition, a counter and a branch — about
thirty lines. It was measured rather than argued.

A fence reads nothing, so the cost is not a lookup. At a fixed rate below saturation it is almost
invisible in CPU terms. What it buys is latency and slot occupancy, and those show at the ceiling: peak
throughput falls and the tail widens even at a fixed rate. Part of the peak difference is a closed-loop
effect — throughput is queue depth divided by latency — but the tail is not. Re-run with
`ledgerfio run --workload hold-settle` against a build with the fence made unconditional.

So the fence stays conditional, and the reason is worth stating precisely: a fence is only issued
when the lane already has a reply outstanding, which means the request was going to wait for lane
order anyway. It adds no latency that was not already there. Making it unconditional turns that
into a constant, and the speed contract is about the tail.

## 2. Hold overlay, commit, and pending apply

The plan marked the pending contract provisional because the overlay and the store's view
of a hold could disagree between propose and commit.

A hold's remainder is owned by the pending engine, including the part that is only promised. The
engine has two ways in and the port shows both: an **overlay** the sequencer reads inline (the
judge cannot continue without knowing what is left) and a **fetch tier** behind a queue for holds
that the overlay does not have. The overlay lives on the reactor's thread — like the account component,
crate ownership is not thread ownership — and it carries the uncommitted reservations, the pending
overlay. Leadership change discards them with one call.

The sequencer therefore keeps no hold state of its own. It tells the engine what it decided and
reads the remainder back with the reservations already deducted:

- The first settle or void of a hold fetches it; the engine marks it as being fetched, so later
  requests only need to be ordered behind that fetch.
- Once the overlay has it, requests judge against the overlay — they never fetch it again, so no stale
  answer can revive an amount already spent.
- Judge reserves the reduction at propose time, so a following settle sees the smaller remainder
  immediately.
- Apply turns the reservation into the committed remainder and sends the store the decision
  (`Reduce` or `Remove`). A failed commit compensates it, which can only cause a false reject.
- An entry disappears when the hold is fully consumed, or when the engine's own eviction drops an
  idle one. A later fetch reads the store, which is authoritative because nothing is in flight.

Two ordering rules make this safe:

- Lookups, fences and applies share one channel, so a lookup issued after a removal cannot
  observe the store as it was before that removal.
- A queued apply blocks new lookups, because the queue is what preserves that order.

A missing hold is deliberately not cached: the hold may simply not have been applied to the
store yet, and caching its absence would reject every later settle of it forever.

### The overlay is a copy plus the reservations, and it starts at create

Two different things live in one entry: a copy of what the store last confirmed about a hold, and
the reservations no batch has committed yet. The copy is why judging needs no round trip; the
reservations exist nowhere else, because the store only learns a decision when its batch commits.
Splitting them would gain nothing — judging reads `committed_remaining - reserved`, one subtraction.

Because the copy is just a value the ledger already decided, a hold the engine is told to create
goes straight into the overlay: paying a lookup afterwards would be asking to be told what was already
committed. Measured on `hold-settle`: every resolution took a lookup before, none after, and the report's
hit ratio shows it. Holds in a budget group are the exception — membership
and the group's remainder are the store's to report, and judging a group needs both, so those
still take the lookup path.

The pending engine's design document records the insert at propose too, so a hold is visible to a
lookup the moment it is proposed. That is not safe as written: a resolution in a *later* batch could
then commit after the batch that created the hold was refused, applying a settle against a hold that
never existed and driving the pending column negative. The document's failure analysis only covers
the resolved side, where the failure mode is a false reject.

So the visibility is kept where it is provably safe: a hold a chain creates lives in that chain's
scratch, next to the availability an earlier leg brings in, for exactly the same reason — a chain is
judged as one unit and cannot be split across batches (`Batcher::chain_boundary`, and a config that
would allow it is refused), so a resolution and the creation it depends on share one commit outcome.
Outside the chain the hold does not exist until its batch commits.

That split also decides who owns which number at commit: the reservation is released by the
sequencer, and what the hold has left follows from the `Apply` the engine is sent. One value, one
owner — otherwise a resolution judged inside the chain that created the hold would give back a
reservation it never took, and the remainder would grow.

Eviction has to leave alone every entry a dispatched request is still going to read. Without that, a
lookup is answered, the entry is evicted before the judge gets to it, and a resolution is refused
for a hold that exists. So the entry is pinned when the sequencer decides a request will read it —
whether it sent a lookup or found the hold already there — and unpinned when that request is
answered. Answering is the right place to unpin, because every request reaches it, including the
ones that are rejected; unpinning where the judge reads would leak a pin on every rejection.

An answer of "not there" is kept rather than thrown away. A write always reaches the store before a
later lookup (that is what the write queue is for), so the answer cannot be stale, and keeping it
means a second resolution of the same missing hold costs no second round trip. The engine's own
design document splits that answer in two — a hold that was resolved or expired versus one that
never existed. The stub cannot tell them apart, because it keeps no history of what it removed, so
it answers with one negative state; the split belongs with the segment expiry that is not built.

## 3. Linked groups need two mechanisms the design did not spell out

A chain is judged as one unit, and that unit needs both of these to work at all:

**The chain scratch.** The overlay deliberately holds back availability a transfer would add, so a
third party cannot spend money that is not committed yet. Inside a chain that rule is wrong: the
whole point of `deposit then spend` is that the second leg spends what the first brings in, and the
chain commits or rolls back as one. So the chain judge keeps the gains of its own legs, consults them
for each following leg, and throws them away when the chain is judged. They never reach a third
party.

This is related to the design's third layer but not the same thing. The design's scratch exists
because it records the overlay when a batch is *proposed*, leaving requests in the same batch unable
to see each other; here a reservation is taken when a request is *judged*, so that window does not
exist and no batch-wide layer is needed. What the chain scratch adds is the sign: the overlay carries
only availability-reducing deltas, and only an atomic unit may rely on the positive ones.

**Lane gates.** The group barrier is a second source of reordering: groups complete in the
order their external results arrive, not in lane seq order, so a later chain's leg can be
ready before an earlier one on the same lane. Every chain whose first leg debits the same
funding account shares one lane, so this is the common case, not an edge case.

Unlike the pending path, there is no external component to push the ordering onto — the
sequencer created this wait, so the sequencer keeps the order: a lane with an unjudged group
leg gates the requests behind it, and they are released, in arrival order, when that group
is judged. A gap therefore still means exactly what it meant before: an external component
returned out of order.

The cost is visible and expected: chains that share a lane serialise on it. The linked
workload runs at roughly an eighth of the single-phase rate for that reason, since every
chain's first leg debits the same external account.

## 4. The account component is external, so the state had to be split

The account view is called inline — the judge cannot continue without a balance — which
made it tempting to treat it as a field of the sequencer. It is not: it is a component that
holds every account in DRAM for speed and persists and recovers on its own, exactly like the
pending engine holds holds. Only the sequencer's own volatile state is the sequencer's.

| state | owner |
|---|---|
| four columns, ledger, constrained flag | account component (durable, checkpointed) |
| lane seq counter and last accepted seq | sequencer (volatile, new leader restarts) |
| propose-time overlay | sequencer (leader-local, discarded on failover) |
| outstanding pending replies, quarantine | sequencer (lane policy) |

The judgment itself stays entirely in the sequencer: the account supplies committed
availability, the lane supplies the overlay, the group supplies its scratch, and the
sequencer adds them up and decides.

The cost is real and measured. A judge plus an apply touches two cache lines (lane and record)
instead of one:

```
lane+apply split (current), 1M accounts    24.0 ns/op    41.6 M ops/s
lane+apply fused,           1M accounts    15.0 ns/op    66.5 M ops/s
```

41 M ops/s is still more than a hundred times the 300k target, so correct ownership is worth
the line. If the inline ceiling ever matters, the fix is to give the account component an
opaque per-account sidecar the sequencer owns, keeping both halves in one line without either
side reading the other's fields.

## 5. Line-aligned or packed: measure, per structure

Padding follows the machine's own line size — 128 here, 64 on x86 — because aligning to anything else
pays memory for nothing. Going to 128 everywhere would buy exactly one thing: x86 pulls adjacent lines
in pairs, so a pair of padded atomics can still share a fetch. That is a real effect and it is also one
nobody here can measure, so it is not bought. It would reach very little anyway: only the two padded
atomics in each SPSC ring and `WorkItem`'s alignment, since `WorkItem` is 128 bytes because of its
fields. Nothing that dominates memory is padded at all — an `AccountRecord` is 40 bytes and a
`LaneState` is 32, both by deliberate claim.

What makes this portable is not the padding but the checking: every claim is verified against **all** of
`SUPPORTED_LINES` at build time, so one that holds here holds on a 64-byte machine. `LaneState` at 32
bytes is `Inside` on either; `WorkItem` at 128 occupies whole lines on either.

A smaller line is not simply worse, and the tempting number is the wrong one. `ledgerfio layout` prints
how many values share a line, but that is density, not the cost of a random access. A random access
costs the lines it touches, `1 + (size - 1) / line`, so a smaller line touches more of them — while
fetching fewer bytes each time. The two pull opposite ways, a sequential scan fetches the same bytes
either way, and which wins depends on whether the machine is latency-bound or bandwidth-bound. So line
size is not a term in a prediction; it is one more reason a prediction for a machine nobody here can run
is a bracket (`ledgersim capacity --cost-scale`) rather than a formula — see §10.

The rule that survived measurement is not "align hot state to a cache line". It is: **make hot
state a size that divides the cache line.** Such a value can never straddle — the line holds a
whole number of them — and it costs a fraction of the memory that a whole line per value does.
Occupying a whole line is a stronger claim, and it is only worth it for cross-thread isolation,
which is what `CachePadded` is for.

**The lane is 32 bytes, not a line.** It is the most touched state in the sequencer (issue, check,
overlay, all per request), and 32 divides both 64 and 128, so it never straddles. One line per lane
would also never straddle, at four times the memory — and it is slower wherever the array still fits a
cache, equal once nothing fits anyway. `LineFit` states the contract and the build enforces it.
Re-measure with `cargo bench -p ledger-sequencer --bench pipeline`.

**The account record is 40 bytes and does straddle.** Forty divides neither line size, so some records
span two lines. Padding to 64 would end that and grow the array by half again — and footprint beats
straddling: while the packed array still partly fits a cache the padded one does not, and once neither
fits the two are the same. The record stays packed. Re-measure with
`cargo bench -p ledger-account --bench columns`.

**The work item is padded to whole lines.** It is reached by slot id — at random — several times per
request, and its fields straddle if left packed. Whole lines cost a few bytes more per slot and are
faster at every pool size measured, so it is padded. Re-measure with
`cargo bench -p ledger-sequencer --bench pipeline`.

**Streamed messages are judged by size, not straddling.** `Request`, `Ack`, `Effect`,
`PendingCommand`, `PendingReply`, `IdemRequest` all travel through rings and batch buffers in
order. Consecutive access touches every line anyway, so where a value starts does not matter;
only how many bytes move does. They are listed in the layout report so growth is visible, and
they carry no alignment.

Summary of the decisions. The sizes are build-checked claims, not measurements:

| structure | size | reached | decision |
|---|---|---|---|
| `LaneState` | 32 | at random, several times per request | sized and aligned to divide the line |
| `WorkItem` | 128 | at random, several times per request | padded to whole lines |
| `AccountRecord` | 40 | at random, twice per effect | left packed; footprint beats straddling |
| `Transfer`, `Request`, `Ack`, `Effect` | 64-112 | streamed in order | size watched, no alignment |
| SPSC head and tail | one line each | across threads | `CachePadded`, to stop false sharing |

So: pack by default; size hot random-access state to divide the line, or pad it to whole lines
when it cannot fit in one; and spend a line to keep threads off each other.

None of this is left to a comment. Every watched type declares its `LineFit` next to the struct
itself, and `layout_claim!` asserts it there, so weakening `LaneState`'s alignment or calling the
account record straddle-free fails the build on that line. The exceptions are declarations too:
they carry the reason with them, and `ledgerfio layout` prints which claim each type makes.

The size in a claim is the one number written by hand. `size_of` supplies the real one and the
build requires them to be equal — deriving the expectation from `size_of` would make the check
vacuous. So adding a field to a watched struct fails the build until someone updates the number on
purpose.

The claim states a property; it does not create one. Size and alignment come from the struct —
`repr(align(..))` or `cache_aligned!`. `Inside` tightens only alignment and pads nothing: a
32-byte lane stays 32 bytes and four share a 128-byte line. `WholeLines` is the one that pads: a
113-byte work item becomes 128, the tail being padding, which is what buys it never straddling.

Benchmarks that walk effects randomly measure the effect array instead of the ledger. A
committed batch is streamed in order, so the apply benches walk effects sequentially with
scattered accounts; the earlier random walk reported apply at 45 ns/op where it is 13.

## 6. Batch boundaries, backpressure, and logging

**A batch boundary is what terminates a linked chain.** A chain is a run of consecutive
requests, so without a boundary an abandoned chain stays open forever and gates its lanes:
those accounts stop. `Request::end_of_batch` marks the last request of a submission, and a
chain still open at the boundary is rejected as `LinkedChainUnterminated`. That removes the
hang structurally rather than with a timeout.

Batching was added for that boundary, not for throughput: writing a batch into the ring with
one release store measures the same as pushing item by item (4.4 M/s versus 4.0 M/s at 4096
per batch, the larger batch slightly worse because it makes the load bursty). The SPSC push
was never the bottleneck.

**Every backlog is bounded.** Acks the client has not taken and committed hold decisions the
pending engine has not taken both have limits; reaching either pauses intake so the pressure
reaches the client instead of growing memory inside the sequencer. The pause is edge-logged
and exposed through `Backpressure` for a rate limiter to read.

**Logging stays off the hot path.** The reactor records fixed-size events — kind plus two
numbers plus a timestamp — into a ring, and a separate thread formats and prints them. There
is no allocation, no formatting and no syscall on the reactor thread, and if nobody drains
the ring the events are counted and dropped rather than slowing anything down. Only state
transitions are recorded: gaps, quarantine, fail-stop, refused commits, intake pause and
resume, aborted chains, hold eviction. Per-request facts stay counters.

**Time is injected.** Batching asks a `Clock`, so a simulation can replay the same batching
decisions with a `ManualClock`. The default is a monotonic system clock.

## 7. A linked chain and a shared budget group are not the same thing

The design describes two mechanisms that are easy to conflate, and this implementation did
conflate them at first.

- A **linked chain** is *atomicity at submission time*: these legs commit or roll back
  together. It lives for exactly one judge and one propose, and it is a property of a
  submission. `ChainId`.
- A **shared budget group** is a *lifetime property of holds*: several holds draw on one
  budget and must be resolved together, tracked while they are undecided and forgotten once
  they are resolved. It outlives the request that created it. `BudgetGroup`.

They now have separate types, and the current policy is stated rather than implied: **the
holds a chain creates share one budget group, named after the chain.** That covers the common
case, but not the general one. A budget group that spans holds created by different
submissions needs a client-supplied durable id on the transfer, and full-coverage checking
("this resolution covers every member") needs the pending engine to index holds by budget
group and report the membership. Today the sequencer only enforces the weaker rule: a hold
that belongs to a budget group may not be resolved outside a chain.

A chain also has to arrive as one submission. The client API makes that the only way to
express it: `submit(tx)` refuses a linked leg, and `submit_chain(legs)` sets the link flags
itself and publishes the whole run or nothing — a half-published chain would meet the batch
boundary and be aborted. Allowing a chain to span submissions would mean a timer plus lanes
gated for the length of that timer, which is the hang the boundary removes.

## 8. Who runs the loop

The sequencer is a state machine with a `tick()`: no threads, no transport, no clock of its
own. That is what lets a test drive it step by step and a simulation replay it. But something
has to run it, and that something is not a client.

`ledger-service` assembles a node: it creates the client queues, constructs the reactor,
spawns the reactor thread (pinning it where the platform allows), spawns the log stream drain,
and hands the caller a `ClientEndpoint`. `shutdown` stops the loop and gives the reactor back
so final state can be read. `ledgerfio` uses that endpoint like any other client.

The endpoint is a pair of in-process queues today. A network listener belongs on exactly this seam,
owning one endpoint per connection; the reactor's single intake queue is what will need fan-in when
there is more than one client.

**Stopping is two phases, and the library catches no signals.** A stop can be asked for from
anywhere through a `StopToken` — a signal handler, an admin endpoint, another thread — because which
signal means "stop" is the process owner's decision, not a library's. In `ledgerfio`, which *is* the
process owner, `signals::Signals` catches interrupt and terminate: the load driver stops submitting,
the service drains, and the report still prints for the part of the run that happened. On a stop the reactor closes
intake and keeps ticking until it owes nobody anything: no request in the pipeline, no proposal
outstanding, no ack or hold decision waiting to be handed over. Only then does the loop exit.

The drain is not politeness. Uncommitted work is safe to abandon, since the client retries and
nothing was promised. A batch consensus has already committed is different: it still owes an apply,
and dropping it would lose a decision the log already holds. `Stopped::drained` says whether the
drain finished inside `drain_timeout`, so an operator can tell a clean stop from an abandoned one.
Dropping the service without calling `shutdown` still stops and still drains; it only throws the
final state away.

## 9. Two ways the ledger can be wrong about itself

Contract-1 detection says an external component misbehaved. Two failures say something worse — that
the sequencer's own bookkeeping is off — and they are handled differently from each other.

**Half an effect.** The account component writes the debit side and then the credit side. If the
second one overflowed after the first was written, the accounting identity would be broken for good:
the effect is already committed, so there is no way back and no way to refuse it. The far side is
therefore asked whether it fits before the near side is touched. The two sides write different
columns, so an effect whose debit and credit are the same account needs no special case.

**A commit that answers the wrong batch.** The effects come from consensus and the slots from the
batch that was waiting; if the ids do not match, the two cannot be paired. Applying them anyway
would ack the wrong requests and release slots that other requests still hold — the classic mistake
of carrying on after detecting corruption. So the apply path is sealed: nothing more is applied,
nothing more is answered, and there is deliberately no operator action for it. A fail-stop from
quarantined lanes is recoverable because our own state is intact; this one is not, and the drain that
never completes is the signal to replace the leader.

A committed effect that cannot be applied at all is the same case as the second: the state would stop
following the log, so the node stops instead — on the leader and, since `replay` returns the same
error, on a follower reading the same log. Skipping it would make the two diverge silently.

All three are exercised by fault injection rather than argued: the consensus stub can answer batches
in the wrong order, and the store can be filled until the next effect cannot land.

## 10. Measurement conditions

Measurements are not recorded in these documents. The code changes, so a number written here would be
stale without saying so; what is kept is the direction a measurement showed and the command that
reproduces it. Every report prints its own conditions — thread placement, workload, flags — and that is
where the numbers belong.

The crates' benches and `ledgerfio run` report the thread placement they actually got, and
take `--pin <cpu>` / `--repeat <n>`:

- On Linux, `--pin` binds the measured thread with `sched_setaffinity`. Pair it with
  `isolcpus`/`nohz_full` on the target instance before trusting absolute numbers.
- On macOS there is no affinity control for Apple Silicon, so the fallback raises the
  thread's QoS class to keep it off the efficiency cores. Reports show
  `performance-qos` in that case.
- Benchmarks report median, min and max over the repeats; single-shot numbers hide the
  couple of percent of placement noise that remains.

What is still missing for absolute numbers: pinning the stub threads too, a target-instance
run under `isolcpus`, and cache-miss counters (`perf` is unavailable on macOS). Until then,
only ratios measured under one placement — colocated versus split, or the working-set
curve — should be treated as conclusions.

Account selection in the benches uses an odd multiplicative stride, not a sequential walk: a
sequential walk lets the prefetcher hide most of the miss cost. Effect lists are walked in
order, because that is what a committed batch is.

### Why there is no hardware profile, and what to do instead

The obvious way to answer "what would this do on another machine" is a profile: two physical constants
and a per-stage shape, `ns = cycles / freq + misses × dram_latency`. It was the next thing to build.
Measuring the inputs first killed it.

Two benches settle it. `cargo bench -p ledger-base --bench memory` walks a dependent pointer chase at
several working-set sizes, which is memory *latency*. `cargo bench -p ledger-account --bench columns`
does the component's own work over the same range, where the accesses are independent.

Over a working set far larger than any cache, a dependent access costs many times what a record read
costs — same machine, same span. The stage's misses are independent, so several are outstanding at once,
while the formula prices each at full latency. That is the form being wrong rather than the constant:
`misses × latency` describes a chain, and none of these stages is one.

Fitting the pairs from whole-system runs does not work either. Run `ledgerfio run --workload hold-settle
--cpu` at three account counts: intake rises with the working set, but no single (compute, misses) pair
fits all three points — fitting two of them mispredicts the third by a wide margin. Judge and apply do
not even move monotonically, because changing the account count also changes the fence rate and the
effects per tick, and `--cpu` attributes per tick.

So the portable quantity is the **curve of cost against working set**, measured per structure. Moving to
another machine means running the benches there and passing the numbers to `ledgersim capacity
--cost-intake/-judge/-propose/-apply`.

Cache-line size belongs in the same bracket rather than in a term of its own: a 40-byte record is 1.30
lines per random access on a 128-byte line and 1.61 on a 64-byte one, while the bytes fetched move the
other way (166 against 103), so the sign of the effect is not fixed.

For a machine nobody here can run, the answer is a bracket rather than a derivation: `--cost-scale
<percent>` moves every stage together, so what the stages cost relative to each other stays measured and
how much slower the core is stays an assumption with a number on it. Sweeping that percentage shows
something a formula would not: past some point the core is the limit and throughput tracks the scale,
and below it the client's queue depth is the limit and throughput stops moving.

Two conditions belong next to any stage cost: the working set, since the same stage costs several times
more over a large account set than a small one; and the effects per tick, since `--cpu` times each stage
per tick. Two `--cpu` runs are comparable only when both match.

### Utilisation is not a tick count

One tick can carry hundreds of requests, so the share of ticks that found work says nothing
about how full the core is: a saturated run spends a tiny share of its ticks doing
work. Saturation therefore needs `--cpu`, which times each stage and divides by the reactor's
lifetime; the free tick counter only says whether the loop is starved. A profiled run costs a
few percent of throughput, so the two numbers come from different runs.

Latency is split where it can be measured without touching the request path: the client owns
the end-to-end stamp, and the reactor times propose-to-commit once per batch. There is no
per-request phase breakdown, because that would need clock reads per request on the hot path.
