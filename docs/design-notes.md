# Design notes

Decisions the code cannot explain on its own: what the source design left open, where the code
departs from it, and the measurements behind each choice.

Every section opens with the same four lines, so a decision can be re-read without re-reading its
reasoning: what was **tried** first, what **broke**, what was **weighed** against it, and what was
**chosen** and why. The prose below each is the evidence; the four lines are the claim.

The format is not decoration — it was added after three findings in one week ended in a question rather
than a fix, and it immediately showed which decisions never recorded what they rejected. Where it says
**Weighed — not recorded**, that is a fact about this repository rather than a section that needs
rewriting: nobody wrote down what the alternatives were, and only whoever made the call still knows.
`status.md` carries that as an open item.

Two other places hold decisions of their own, and neither is folded in here. `CLAUDE.md`'s rule 15 is the
library ledger — hand-rolled on purpose, replaced by a crate, weighed and refused — because those choices
are about dependencies rather than about this design. `status.md` holds what is *not* decided, under
*Decisions waiting on someone*.

## 1. Lane ordering when requests traverse different stages

> **Tried** — the design's three rules at once: contract 1 on every component, the fast kinds skipping the
> pending round trip, and no reordering in the sequencer.
> **Broke** — a request is judgeable when *all* its results are in, and two ordered streams joined by AND
> are only ordered if everything traverses both. A later hold overtakes an earlier settle on the same lane
> as soon as the pending path is slower. Every component kept its contract; the join broke it.
> **Weighed** — route every request through pending (loses the fast path for all traffic); park
> out-of-order arrivals inside the sequencer (cheapest, but it turns an always-on gap detector into a
> metric and moves ordering back into the core).
> **Chose** — a *conditional* lane fence: a token that reads nothing, on the same channel and lane, so the
> engine's own ordering orders it too. Ordering stays outside the sequencer and a gap keeps its meaning.
> `ledgersim check`; `ledgerfio run --workload hold-settle --external-ratio 30` for the exemption.

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

A settle or void **is** exempt when its debit account is unconstrained, and the reason it took a
measurement to get there is that the argument for it was always sound and never priced. What the lane
buys for a resolution is **not** safety. Double resolution is prevented per
*hold*, by the pending overlay: `view` reports the committed remainder minus what
proposed-but-uncommitted resolutions have already taken, `resolved` refuses a second one outright, and
a resolution judged with no hold data at all is rejected rather than accepted. What the lane buys is
two other things — which of two concurrent resolutions of one hold wins, and a place in the seq order
for a reply that carries external data, which is what contract-1 detection watches. So the exemption
covers only the kinds that read nothing.

Measured with `ledgerfio run --workload hold-settle --external-ratio 30`, where many debits land on one
clearing account: the exemption raises throughput and cuts fences by most of their count. Without it a
busy suspense account serialises everything behind it, which is the cost of promising an order nobody
asked for. That measures the exemption as it is implemented — over the kinds that read nothing. The
sentence above about resolutions has no measurement behind it; it is an argument, which is why the next
section revisits it.

### Order exemption: the hold is the serialisation unit

**Built 2026-07-31.** A resolution debiting an unconstrained account is exempt too, so the rule is one
clause — the lane exists to protect a balance, so no balance constraint means no lane.

What decides it is a product. Order-wait is a lane's queue depth times a read's latency, and the speed
the engine owes is per read, so the product is the term that contract cannot cover: a lane deep enough
turns a bounded read into an unbounded wait. Exemption reduces it to one read, because an exempt
request still waits for its own answer — it just waits for nothing else. Hot *constrained* accounts
keep the product, so this narrows the risk rather than removing it.

**The depth is now measured, and it was an argument before.** Two knobs were missing to see it:
`--resolve-after`, which gives a hold an age so its resolution actually reads a record rather than one
written moments ago, and reporting the engine's own orderer at all — it computed the wait and nothing
published it. With both, at holds resolved nine hundred thousand old:

| workload | replies arriving behind their lane | deepest lane |
|---|---|---|
| uniform accounts | 5% | 5 |
| `--skew 8` | 55% | 7,443 |
| `--skew 64` | 90% | 20,044 |
| `--external-ratio 30` | 99.9% | 13,281 |
| `--external-ratio 70` | 99.9% | 19,584 |

The first row is why this looked like a non-problem: spread over two hundred thousand accounts a lane is
five deep, and five times a read is nothing. The last two are the case exemption covers — one
unconstrained clearing account, every resolution debiting it in one lane, that lane thirteen to twenty
thousand deep. At a real read latency that product is the unbounded wait, and it is the *depth* that
carries it, not the latency. The skew rows are the residue exemption does not touch: those debits are
constrained user accounts, so they keep their lane by the rule that is left.

Two cautions about reading that table. The mean wait *falls* as concentration rises, because the runs
differ in what else they are doing — at `--skew 64` a third of submissions are rejected — so depth is the
comparable column and the wait is not. And the first version of this measurement was wrong: the number
published as lane depth was the orderer's total held across all lanes, which read as twenty thousand deep
when the deepest lane was five, and most of the wait it attributed to ordering was the worker's own loop
and the reactor's backpressure. Order-wait and delivery-wait are now split at fill — a reply arriving
behind an unfilled earlier place is the lane's, one arriving as its lane's head is not — because only the
first is what exemption removes.

Two things are given up. Arrival order stops deciding which of two concurrent resolutions of one hold
wins; that only bites when both together exceed the remainder, and exactly one still wins either way.
The real cost is detection: an exempt reply takes no seq, so an engine that answers with something stale
is no longer caught by the order check. **The replacement is built with it**, and it is a check on the
data rather than on the order: the sequencer counts the committed decisions it has handed over, records
that count on the request when it dispatches the lookup, and the engine's reply carries how many it had
applied when it answered. Fewer than the request was dispatched behind means the engine answered from
state older than its own queue. The lane is quarantined for it, as for a seq gap — the component is
broken and our state is intact — and it is counted as `stale_answers` so a run can say which of the two
checks fired.

Three things make that cheap. The count fits in the padding `PendingReply` already had, so a reply is
still 128 bytes. The expectation is the *sequencer's* own number, never one the engine supplied — a value
the component produced could not check the component. And it lives beside the slot with the record, so it
dies with the request rather than being kept anywhere.

It is also strictly stronger than what it replaces, which is worth stating plainly: the order check
notices a reply arriving out of turn, and this notices an answer that is *wrong* however punctually it
arrived. Both are exercised by faults the component owns — `violate_order_every` and
`stale_answer_every` — and the second is drawn in `check` too, where it interacts with the rest.

**What it bought, measured.** `hold-settle --resolve-after 900000`, five seconds, two hundred thousand
accounts:

| | replies arriving behind their lane | deepest lane |
|---|---|---|
| all debits constrained | 4.9% | 5 |
| `--external-ratio 30` | **0 of 4,936,038** | 0 |
| `--external-ratio 70` | **0 of 3,927,407** | 0 |

The first row is unchanged and must be: those debits are constrained, and the exemption does not reach
them. The other two were 99.9% at depths of thirteen and twenty thousand.

**And the arithmetic this was aimed at was wrong.** The plan said order-wait is `lane depth x read
latency`, which assumes a lane's reads are serialised. They are not — the store is asked for many at once
and the lane serialises only the *release* of the answers — so it is `lane depth / store queue depth x
read latency`. Measured before the change with a 200-400us store: deepest lane 286, queue depth 128, so
about 670us predicted against 464us measured, where the old form would have said 86ms. The concurrency
divides it. Note also that depth is not independent of the device: the same store that makes reads slow
caps throughput, so the deepest lane fell from 13,281 with a free store to 286 with a slow one. What the
exemption removes is the ordering coupling; what no ordering change can remove is the device's own
ceiling, and with every resolution cold that ceiling is what a run measures.

**Weighed and refused.** Exempting a resolution only when its hold is already answerable inline. It
targets the same collapse for less, but it makes ordering depend on cache state, so the guarantee
becomes "two resolutions are ordered only if the second one missed the cache" — a contract that cannot
be stated to a client. A rule that holds always or never is worth more than a rule that holds usually.

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

### The engine's side of contract 1: places reserved in command order

Reordering is the engine's work and checking is the sequencer's, so the structure that keeps a lane in
order lives in the engine. It reserves a **place** when a command is taken off the queue and fills it when
the work behind it finishes. Releasing in the order things finished would be the device's order, not the
lane's — which is the whole reason a read that completes out of order needs this at all.

**The engine cannot know a lane's numbering.** It sees only a subsequence of a lane's seqs: a hold or a
single-phase transfer never travels this path. So a run of commands begins wherever the fence rule makes
it begin, and the first place reserved after a lane falls quiet is what defines it. A structure that
assumed a lane starts at one waits forever for seqs that never arrive — found the direct way, by two
existing tests hanging.

**An order-exempt reply keeps no place**, so it leaves as soon as its own work is done, and it needs a
queue of its own: several can be outstanding on one lane and they all carry the same absent seq, so a
map keyed by seq would let each overwrite the last. That was a lost reply, not a slow one.

**The fault makes two places trade contents.** Breaking the contract on purpose is how the sequencer's
gap detection is tested, and the temptation is to write a reply into someone else's place — which loses
the place and hangs the lane instead of testing it. Trading means both replies still leave, each in the
other's turn, and what the sequencer sees is an arrival out of order.

It lives beside the engine rather than in `stubkit` because contract 1 is the engine's to keep.
`stubkit`'s simpler queue stays for the idem stand-in, which answers every command where it dequeues it
and so never needs a place at all. And what remains of `MemoryPending`'s invented latency is now a test
fault rather than a model: the index, the buffer and the block store do real work, so a delay on top of
them would count the same time twice.

## 2. Hold overlay, commit, and pending apply

> **Tried** — an overlay on the reactor's thread holding the whole record, so a resolution could be judged
> with no round trip at all. It was built that way because the engine was a stub with no memory of its own.
> **Broke** — once the engine had a memory tier, the copy was a second owner of the record with nothing to
> say which was true (rule 18), and the rule's one escape — a paired write plus an invariant check — is
> unavailable here, because checking agreement means reading the record the copy exists to avoid reading.
> **Weighed** — keep the copy and rename it, on the grounds that a neutral measurement does not buy a
> refactor. Refused: the measurement's job was to say the round trip was affordable, not whether a second
> owner was allowed.
> **Chose** — the overlay holds decisions only; the record rides the reply to the slot of the request that
> asked and dies with it. Measured free at the target rate — the correction below records three ways the
> experiment was misread first.

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

### Correction: the overlay is not where a record belongs

The subsection above is how this was built, and it is wrong about one thing. The source design's overlay
is `tx_id -> live | resolved` — a **state**, a few megabytes, bounded by requests in flight, excluded from
the checkpoint. It holds no payload. What holds payloads is the engine's own memory tier, on the engine's
thread, and "immediate" in the design means **no IO** rather than no round trip.

The copy got into the overlay because the overlay was built while the engine was a stub: a hash map
behind a queue, with no memory of its own. There was no tier to read from, so a reactor-side copy was the
only way to answer inline. Once the engine had an index, a writeback buffer and a block store, that copy
became an unnamed cache in the wrong place — and it is what made the layering hard to talk about, because
"overlay" then meant two things.

**The layering, stated once.**

| | thread | for | bounded by |
|---|---|---|---|
| overlay | reactor | consistency: reservations taken at propose, released at apply | requests in flight |
| memory tier | engine | answering a lookup with no IO | a day of residency |
| write staging | engine | not writing records that die young | an hour before flush |
| block store | engine | the rest of the retention window | thirty-two days |

**Two windows, not one.** The source design's hot buffer conflates them and derives its size as
`peak x writeback`. They are independent: a record is *written* after an hour — which is what bounds
recovery, because anything unflushed is memory-only and has to be in the checkpoint — and stays
*readable in memory* for a day, which is what keeps IO off the resolutions that happen within a day. And
because a flush carries only survivors, a day of residency is not a day of arrivals: it is the current
hour in full plus the survivors of the previous twenty-three, which is a fraction of it.

**What was measured, and two ways of reading it wrong.** Disabling the reactor-side payload made no
difference at the design's target rate: p50, p90, p99 and p99.9 all inside run-to-run variation, with
tens of thousands of lookups and hundreds of thousands of fences occurring. It showed a difference only
in a saturated run, where the client's queue depth is the limit and so the run is not about the ledger.

The first misreading was mine twice over. A saturated run said the payload was worth about a tenth of
p50, and I took that as the number. Then, arguing it away, I predicted fence amplification would fall
with the arrival rate — it does not: it is flat at about nine fences per lookup from the target rate to
saturation, because the client sends in batches and what latches a lane is the burst, not the rate. And
then a single rate-limited run showed a fourteen-fold tail improvement from removing the payload, which
did not reproduce. Three readings, three corrections; the conclusion that survived is the one where
repeats agreed.

**And then a fourth correction, about what the measurement was for.** Having established that the copy
cost nothing, I recommended keeping it and renaming it, on the grounds that a neutral measurement does not
buy a refactor. That was wrong, and the argument against it is not a number: the same fact under two
owners with nothing to say which is true is what rule 18 forbids, and the rule's one escape — a paired
write and an invariant check — is unavailable here, because checking agreement means reading the record the
copy exists to avoid reading. So the copy went. The measurement's job was to say the round trip was
affordable, not to say whether the copy was allowed.

### The overlay holds decisions, and the record it is judged against comes from the engine

An entry is what the sequencer has decided about a hold and not handed over: the remainder it last told
the engine, what proposed-but-uncommitted resolutions have taken of that, and whether one of them took
all of it. Judging reads `committed_remaining - reserved`, one subtraction. None of it is anywhere else,
because the store only learns a decision when its batch commits.

It used to hold a copy of the record as well — the accounts, the ledger, the group and its totals — so
that a resolution could be judged without asking anything. That is gone, and **rule 18 is what decided
it, not a measurement.** The rule allows a local copy of a fact that lives elsewhere on one condition:
one call sets both and an invariant check proves they agree. This copy had neither, and it could not have
had them, because proving agreement means reading the record the copy exists to avoid reading. Two owners
for one fact with nothing to say which is true is the shape every integrity bug in this code has had.

The measurement is what made the change free rather than what asked for it: at the design's target rate,
removing the copy moved no quantile outside run-to-run variation (`docs/design-notes.md` §2's correction
has the three ways I misread that experiment first). So the round trip was affordable, and the rule
decided.

What replaced it: a lookup's reply carries the record to the slot of the request that asked, where it
lives until that request is answered — which is what bounds it by work in flight, and is why it sits
beside `WorkItem` rather than inside it (a work item is padded to whole cache lines and has no room).

**And "bounded by work in flight" was not true until it was measured.** A created hold was given an entry
at once, to carry its remainder and to invalidate any earlier answer of "not there". For a hold resolved
soon that entry costs nothing; for one resolved late it lives as long as the hold. With
`--resolve-after 900000` the overlay reached a hundred megabytes and nine hundred thousand entries, none
of which answered anything — the remainder they carried was what the record already said, and no decision
had been taken about a hold nobody had resolved. So an entry is created only when something is already
there to correct, and the same run then holds twenty thousand entries: bounded by requests in flight, as
claimed, rather than by holds outstanding. The claim came first and the measurement corrected it, which is
the order this document exists to record.
`HoldView::compose` puts the two halves together at judge time. Where they disagree about the remainder
the overlay wins, because a remainder only ever decreases: the sequencer's reading is taken the moment it
decides, and the engine's can have been in flight across that. The same rule the other way round is why
an answer of "not there" is dropped when the hold has since been created — otherwise every later
resolution of a hold that exists would be refused.

Two consequences worth stating. The apply path reads the hold's original size off that same record before
the slot is released, so a partial settle still costs the engine no read to append the new version — bar
a resolution judged inside the chain that created the hold, which has no record and sends zero, and then
the engine reads the version it appended moments ago from its own buffer. And the reactor no longer has a
hit ratio to report: every resolution asks, and whether the engine answered from memory or from the store
is the engine's number (`reads: memory=N store=M`), which is where it belonged.

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

Eviction has to leave alone every entry a dispatched request is still going to read. The failure it
prevents changed with the copy: it used to be a lookup answered, the entry evicted, and a resolution
refused for a hold that exists. Now the record is in the slot, so what eviction would lose is the
*remainder* — and losing that lets an answer already in flight be believed, which is an overdraft rather
than a false reject. So the entry is created when the lookup is sent, pinned when the sequencer decides a
request will read it, and unpinned when that request is answered. Answering is the right place to unpin,
because every request reaches it, including the ones that are rejected; unpinning where the judge reads
would leak a pin on every rejection.

An answer of "not there" is kept rather than thrown away. A write always reaches the store before a
later lookup (that is what the write queue is for), so the answer cannot be stale, and keeping it
means a second resolution of the same missing hold costs no second round trip — the one resolution that
needs no record at all. The engine's own design document splits that answer in two — a hold that was
resolved or expired versus one that never existed. The engine cannot tell them apart, because it keeps no
history of what it removed, so it answers with one negative state — and that is now a refusal rather than a
gap: keeping "expired" needs per-hold state past retention, which is the data the promise says is deleted.
§14's *Weighed and refused* has the argument.

## 3. Linked groups need two mechanisms the design did not spell out

> **Not a decision either, and for the same reason as §7.** What is here is what was intended from the start.
> The header used to claim alternatives were weighed and then lost; there were none. This implementation kept
> arriving at something else and being corrected, and what nobody remembers is the sequence of wrong versions
> — not a set of options.
>
> **What the design left implicit**, which is why it could be got wrong repeatedly: a chain needs two
> mechanisms it does not name. Its own legs cannot see the availability an earlier leg brings in, and the group
> barrier completes groups in the order their results arrive rather than in lane seq order.
>
> **What it is:** a chain scratch holding its own legs' gains, thrown away when the chain is judged, plus lane
> gates for the barrier the sequencer itself created. Cost accepted and visible: chains sharing a lane
> serialise, so `linked` runs at about an eighth of the single-phase rate.

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

> **Tried** — treating the account view as a field of the sequencer, which is tempting because the judge
> calls it inline and cannot continue without it.
> **Broke** — it is a component: it holds every account in DRAM and persists and recovers itself. Only the
> sequencer's own volatile per-request state is the sequencer's.
> **Weighed** — fusing the lane and the record into one cache line, measured at 15.0 ns/op against 24.0
> for the split.
> **Chose** — split ownership and pay the second line: 41M ops/s is more than a hundred times the target.
> If the inline ceiling ever matters, the fix is an opaque per-account sidecar the sequencer owns, not a
> merge. `cargo bench -p ledger-account --bench columns`.

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

> **Tried** — "align hot state to a cache line" as a blanket rule.
> **Broke** — padding everything pays memory for nothing, and footprint beats straddling: while a packed
> array still partly fits a cache the padded one does not, and once neither fits the two are equal.
> **Weighed** — 128 bytes everywhere (x86 fetches adjacent lines in pairs; a real effect nobody here can
> measure, so not bought); padding `AccountRecord` to 64 (grows the array by half again); a whole line per
> lane (four times the memory, slower wherever the array still fits a cache).
> **Chose** — per structure, and the rule that survived is not the one we started with: make hot
> random-access state a size that *divides* the line, pad to whole lines only when it cannot fit in one,
> and spend a whole line only to keep threads off each other. Every claim is build-checked, so weakening
> one fails the build rather than a benchmark.

Padding follows the machine's own line size — 128 on Apple Silicon, 64 on x86 and generic ARM64 — because aligning to anything else
pays memory for nothing. Going to 128 everywhere would buy exactly one thing: x86 pulls adjacent lines
in pairs, so a pair of padded atomics can still share a fetch. That is a real effect and it is also one
nobody here can measure, so it is not bought. It would reach very little anyway: only the two padded
atomics in each SPSC ring and `WorkItem`'s alignment, since `WorkItem` is 128 bytes because of its
fields. Nothing that dominates memory is padded at all — an `AccountRecord` is 40 bytes and a
`LaneState` is 32, both by deliberate claim.

Generic ARM64 does not identify a cache-line size, so it defaults to 64 bytes; a measured 128-byte
deployment changes the central Cargo target configuration at build time. What makes the packed state portable is the checking:
an `Inside` claim is verified against **all** of `SUPPORTED_LINES`, while a deliberately padded
`WholeLines` claim is verified against the line selected for that build. `LaneState` at 32 bytes is
`Inside` on either; `WorkItem` is aligned and padded to the selected line.

The setting is workspace-wide rather than an executable feature: `.cargo/config.toml` selects the
Apple Silicon target and therefore applies to both the base layout types and the sequencer's `WorkItem`.

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
| `Bucket` (hold index) | 32 | at random, twice per lookup | sized and aligned to divide the line — see §11 |
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

> **Tried** — batching for throughput.
> **Broke** — it was never the bottleneck: 4.4 M/s batched against 4.0 M/s pushed item by item, and the
> larger batch is slightly *worse* because it makes the load bursty.
> **Weighed** — a timeout to close an abandoned linked chain, which would leave lanes gated for the length
> of the timeout.
> **Chose** — keep batching for the *boundary* rather than the throughput. `Request::end_of_batch` marks a
> submission's last request and a chain still open there is rejected, which removes the hang structurally
> instead of waiting it out.

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

> **Not a decision — a misreading corrected.** The design has these two apart and always did. This
> implementation merged them because a chain's holds are in practice the group, and the merge was found
> afterwards by the person who had written them separately in the first place. So there is no set of
> alternatives here, and the header says that rather than pretending to one: what was weighed was nothing,
> because the answer was already written down somewhere the implementation had not read closely enough.
>
> **What the merge cost**, which is the part worth keeping: their lifetimes differ, and a single type takes the
> shorter of the two. A chain lives for exactly one judge and one propose; a budget group outlives the request
> that created it and has to be tracked until every member is resolved. Merged, the second silently became the
> first, and a group could be resolved a member at a time.
>
> **What it is now:** separate types (`LinkedChainId`, `BudgetGroup`), separate modules, and a policy stated
> rather than implied — the holds a chain creates share one group named after the chain. The general case, a
> group spanning submissions, needs a client-supplied durable id, so only the weaker rule is enforced today.

The design describes two mechanisms that are easy to conflate, and this implementation did conflate them at
first — not as a choice between them, but by reading one where there were two.

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

> **Tried** — nothing failed here; the section records where a decision had to be made rather than a
> reversal. A `tick()` state machine with no threads, transport or clock needs something to run it.
> **Broke** — n/a. What it must *not* be is a client, and what a library must not do is decide which
> signal means stop.
> **Weighed** — signal handling inside the library, refused: which signal means "stop" belongs to whoever
> owns the process.
> **Chose** — `ledger-service` assembles the node and hands out an endpoint; anyone asks for a stop through
> a `StopToken`; the drain runs until the node owes nobody anything, because a committed batch still owes
> an apply and dropping it would lose a decision the log already holds.

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

> **Tried** — detect and carry on, which is what a report-and-continue path amounts to.
> **Broke** — applying a commit that answers the wrong batch would ack the wrong requests and release
> slots other requests still hold. That is the classic mistake of continuing after detecting corruption.
> **Weighed** — skipping a committed effect that cannot be applied. Refused: `replay` returns the same
> error, so a follower would skip it too and the two would diverge in silence.
> **Chose** — seal the apply path, with deliberately no operator action: the drain that never completes is
> the signal to replace the leader. A half-written effect is prevented differently, by asking the far side
> whether it fits before the near side is touched. All of it exercised by fault injection.

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

> **Tried** — a hardware profile for the simulator, `ns = cycles/freq + misses x dram_latency`, the shape
> the source Python model uses.
> **Broke** — measuring its own inputs refuted the form: the stages' misses overlap, and the formula
> prices every one of them at full latency.
> **Weighed** — writing measured numbers into these documents. Refused, because the code changes and a
> number here goes stale without saying so.
> **Chose** — keep the *direction* a measurement showed and the command that reproduces it; the numbers
> live in each report beside the conditions they were taken under. For a machine nobody here can run, a
> measured curve of cost against working set plus `--cost-*` and `--cost-scale` as a bracket, not a
> formula.

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

## 11. The hold index: a bounded probe, and what identifies a slot

> **Tried** — a fast average, which is what a generic hash table gives.
> **Broke** — its probe has no bound, and the tail is exactly what the engine owes the sequencer. Cuckoo
> hashing does not remove the unbounded part either; it moves it from the lookup to the insert.
> **Weighed** — a stash for entries that cannot be placed (at a 32-hop cap the design's scale needs
> hundreds of thousands, so the miss bound collapses; at 128 hops there is nothing for it to hold); a
> 64-bit fingerprint in a 16-byte slot (buys a rehash that needs no keys, which only matters if growth is
> a path the engine takes — it is not, and it costs twice the index); linear probing (unbounded under load
> or tombstones).
> **Chose** — (2,4) cuckoo, an 8-byte slot, a cascade cap of 128 as a latency budget, a 0.90 target, and
> ambiguity **detected** at insert rather than assumed away. The slot width moved twice and both
> reversals are recorded, along with one arithmetic error: the wider fingerprint was defended with a
> per-lookup collision probability where the question is a birthday problem over the live set.
> `cargo bench -p ledger-pending --bench index`.

The store finds a hold by transaction id, and the structure that does it was chosen for a bounded probe
rather than a fast average: two candidate buckets of four ways each, so a lookup compares at most eight
fingerprints and reads at most one record. A generic hash table reaches a similar average with a probe
that has no bound, and the bound is the whole point — the tail is what the engine owes the sequencer.
Cuckoo hashing does not remove the unbounded part; it moves it from the lookup to the insert, where a
displaced entry cascades until it finds room. Everything below is about paying for that move.

**The slot is eight bytes.**

```text
 63          48 47        47 46                                    0
| fingerprint  | ambiguous | address (segment | block | index)      |
```

Sixteen bits of fingerprint, one bit saying whether that fingerprint is shared, and forty-seven bits of
address. Four ways make a thirty-two-byte bucket, which divides both supported line sizes, so a probe is
one line per bucket — two random reads, whatever the load factor. That is the property the whole choice
exists for.

**A fingerprint does not identify a key, and correctness does not rest on it.** Sixteen bits over the
design's live set puts roughly a hundred thousand pairs of keys on the same fingerprint *and* the same
bucket. So the ambiguity is **detected** instead of assumed: a hold the store has never held is the one
moment uniqueness can be checked for free, so `insert_new` looks, and if anything already in those
buckets shares the fingerprint it marks both slots. An unmarked slot is known to be the only one with
its fingerprint there, so finding it reads nothing; a marked one is told apart by reading a record. At
scale that is a few thousandths of one percent of holds.

This is what lets the path that applies committed decisions read nothing at all. Apply is in order, so a
read there is an IO nothing can hide — and identifying a slot was the last read on it.

**The cap on a cascade is a latency budget, not a tuning dial.** A hop is a random bucket read, so the
cap is the worst an insert may cost in cache misses, and an insert sits on the apply path. It is a
hundred and twenty-eight, measured: at the target load factor a cascade that long always finds a home,
and the longest observed stops well short of the cap, which is what says the cap is not the binding
constraint. A cascade also refuses to kick back into the bucket it just came from — one that oscillates
spends its budget without exploring anything.

**Ninety percent is the target, and the ten percent left empty is not waste.** It is the headroom that
absorbs a lifetime distribution drifting and a mass expiry falling behind. Going to ninety-five saves a
twentieth of the index and costs twice the cascade, and still leaves entries that cannot be placed at
all. The source design's own ninety-percent target is the measured one.

**The table never grows.** Its size is derived from what the configuration declares — arrivals, worst-case
survivor fraction, retention — so growth would mean the declared worst case has been passed, which is a
business change and not an event a data structure should paper over. An insert that cannot be placed is
therefore reported, not absorbed: the remedy is a configuration change and a rolling restart, and the
index is a derived structure that is rebuilt on the way back up anyway. What makes that safe is that the
limit is visible long before it is reached — the load factor against what the table was sized for, and
the longest cascade against the cap, both move well before an insert can fail. `ledgerfio` prints both.

Reproduce with `cargo bench -p ledger-pending --bench index`, which reports lookup hits and misses
against load factor and size, the relocations per insert with the worst chain, how many entries a table
filled to a given load factor with a given cap cannot place, and the same lookup against the hash table
this replaced.

### Weighed and refused, and one conclusion that moved twice

**A stash.** The standard way to absorb an insert that cannot be placed: a small array scanned when the
buckets miss. It was measured rather than argued, and it does not work here. At a thirty-two-hop cap the
design's scale would need hundreds of thousands of stashed entries, which is no longer a few cache lines
in L1 but megabytes scanned linearly — the miss bound collapses. At a hundred and twenty-eight hops there
is nothing for it to hold. Either way it is the wrong instrument.

**A sixty-four-bit fingerprint (a sixteen-byte slot).** It buys one thing that a shorter fingerprint
cannot: the home bucket sits inside the fingerprint, so a rehash needs no keys and growing the table is a
pass over slots rather than over every record. That mattered while growth was thought to be a path the
engine takes. It is not: growth only follows a business change, which is slow and observable, and the
remedy is a restart. With growth gone, the wider slot buys nothing and costs twice the index.

The slot width moved twice, and the reasons are worth keeping because each reversal came from a
correction rather than a preference. Eight bytes first, chosen by pricing the lookup path only. Sixteen
after the apply path was priced and its identifying read showed up. Back to eight once that read was
removed by detection rather than by probability — and once it was clear that the growth cost which had
justified the wider slot belongs to a planned migration, not to an emergency.

One arithmetic error is worth recording with it. The wider fingerprint was first defended with a
per-lookup collision probability, which is the wrong quantity: what matters is whether *any* pair of live
keys collides, which is a birthday problem over the live set, and at the design's scale that is a coin
flip rather than a vanishing chance. The mechanism that replaced the argument — detect at insert, verify
only where marked — is exact, and needs no probability at all.

## 12. Records live on blocks, and blocks are written once

> **Tried** — changing a record where it lies, which is what a resolution wants to do.
> **Broke** — it costs a read before every write on a path that has none, and it gives up the property
> everything else rests on: a block once written is never rewritten, so an address names one version for as
> long as that version exists. Four things rest on it — an index slot holds an address and nothing else,
> compaction decides what is alive by comparing addresses, the expiry walk does the same, and the sweep
> retires an offered void the same way.
> **Weighed** — writing each buffered block out whole instead of compacting it (the store then grows with
> holds *created* rather than holds alive, which is the figure the capacity estimate rests on); one store
> block per flush (multiplies the space a surviving tenth occupies by ten); a window measured as a
> duration (the engine has no clock and should not grow one for this).
> **Chose** — append-only with a new version at a new address, a writeback buffer that compacts on the way
> out, and **two** windows rather than one — an hour before flush bounding recovery, a day of residency
> bounding latency. Every length and share is configuration, and the declared share is checkable: `died in
> buffer` measures the same quantity the declaration claims.

An address is `(segment, block, index)` packed into the forty-seven bits a slot has spare: six bits of
segment, thirty-five of block, six of index. Forty-seven and not forty-eight because the slot spends its
last bit on saying whether a fingerprint is shared (§11), which is where the block field's thirty-fifth
comes from — this said thirty-six, and the code has never been. The source design packs the same three into
forty because its slot is a byte smaller and spends the difference on a narrower block field. A block is four
kilobytes — the unit one read fetches, and so the unit the speed contract is written against — and a
record is eighty bytes packed, which puts fifty-one on a block. The design's sixty-four per block needs
its 128-byte record halved by compression; uncompressed that figure is thirty-two. The intra-block
index is six bits wide either way, because sixty-four is what the format allows.

The bytes are little-endian by declaration, not by inheritance. The moment blocks leave this process
they are a format, and a format that borrows the machine's byte order is not one.

**A partial resolution appends a new version and the index is repointed.** Blocks are never rewritten.
The alternative — changing a record where it lies — costs a read before every write on a path that has
none today, and it gives up the one property everything else rests on: a block once written is never
rewritten, so an address names one version for as long as that version exists. That is what lets an index
slot hold an address and nothing else, and what makes "alive exactly when the index points at it" an exact
test rather than a heuristic. What it costs instead is
space: the old version sits there until its segment expires. Partial resolutions are a minority of
traffic, and this is what append-only means.

**The space comes back only with segment expiry**, which is built: a day's blocks go back once the index has
no entry in them (§14). Until then a resolved hold's record sits where it was written, so the blocks of a live
day hold its dead as well as its living — that is what append-only costs, and it is bounded by retention
rather than growing with holds created. The load driver says which totals are a steady state and which are
not, because a figure that looks like one and is not is worse than no figure.

**Growing the index reads the keys back.** A home bucket comes from the whole hash, so doubling the
table changes it, and a slot carries only a fingerprint — the records are the only thing that knows a
key. That makes growth the safety net rather than the plan: a deployment sizes the table once, and the
cost of being wrong about it is a pass over the records rather than a lost hold.

Reproduce with `cargo bench -p ledger-pending --bench index`, which reports lookup hits and misses
against load factor and size, the relocations per insert with the worst chain, and the same lookup
against the hash table this replaced.

### The writeback buffer, and why it has to compact

Records are written into a buffer of recent blocks first, and a block leaves it by being compacted:
only what the index still points at is carried on, packed with the survivors of earlier blocks. The
alternative — writing each block out whole — makes the store grow with holds *created* rather than holds
*alive*, which is the figure the whole capacity estimate rests on. The load driver showed the difference
plainly before the buffer existed: a run that created and resolved the same holds left every record
behind.

**A record is alive exactly when the index points at it.** Nothing else is tracked: a resolved hold has
no entry, and a superseded version's entry has moved to the newer one. Both tests are address
comparisons against the two candidate buckets, so compaction reads nothing it does not already have in
hand. This is what makes append-only affordable — the garbage identifies itself.

**The window is a count of blocks, not a duration.** The engine has no clock and should not grow one for
this; the source design's own ceiling for the buffer is bytes, and its one-hour figure is that divided
by a rate. A count is also what a test can fill.

**And there are two windows, not one.** This was the last place the two were still the same event: a
record left memory *by being written*, so "on the store and still readable" did not exist as a state. They
answer different questions and so they have different lengths:

| | length | what it bounds | what it costs |
|---|---|---|---|
| flush window | an hour | recovery — what is unwritten is memory-only and has to be in the checkpoint | an hour of arrivals, in full |
| residency | a day | latency — a resolution inside it costs no IO | a day of *survivors*, because what is resident has been compacted |

A day of arrivals would be tens of gigabytes; a day of survivors is a fortieth of that at the shares these
runs measure, which is what makes the second window affordable at all. Making the flush window a day
instead would save writes and is the wrong trade: it is recovery time, and the checkpoint has to carry
everything that has not been written.

**Both lengths and both shares are configuration, not constants** (`PendingCapacity`), and every size in
the engine is derived from them rather than set beside them — a block count configured next to the inputs
could disagree with them, and then the sizing rule would live in two places. The arithmetic is one line
each: the buffer is an hour of arrivals, residency is a day of arrivals times the share that survives the
flush window.

**The declared share is checkable rather than trusted.** `survives_flush_window` is a declaration about a
workload, and `died in buffer` measures the same quantity while the run happens, so the report puts the
consequence of the declaration next to what the traffic did. On `partial-settle` the default declares half
survive and the run measures six percent, which says the window is oversized for that workload by more
than a factor of eight — the kind of statement a constant could not make about itself.

**Survivors are packed together, not one block per flush.** If a tenth of a block's records survive,
writing them out as their own block would multiply the space that tenth occupies by ten. So survivors
accumulate into a store block that is written when it is full, which is also what makes the writes
sequential.

**Compaction moves addresses, and that is why the location token has a fallback.** A survivor's index
entry follows it — free, because the address is the key to the slot — but a token the sequencer is
holding now points at a block that no longer exists. It matches no slot, and the resolution falls back
to the probe. The fallback was built for staleness across a flush before there were flushes; this is the
case it was for.

### Two ways to read, and which one the traffic actually uses

A lookup submits a read and harvests it later, so a store with a latency does not stop the engine's loop;
the place its reply will take was already reserved when the command was dequeued, which is what keeps the
lane in order while completions arrive in the device's. What comes back has to be checked against the key
before it is believed — the index matches fingerprints, so a fetch can return somebody else's hold, and
the walk continues to the next candidate rather than answering "not there". Answering absent there would
reject a hold that exists, a few times in every ten thousand cold resolutions.

Applying a committed decision reads synchronously instead. It has to: apply is in order, and on a virtual
clock a wait that only time can end never ends. A store that models a device therefore charges that read to
the **thread** rather than to its read queue, which is what a synchronous `pread` costs and what the queue
cannot express — §16's `take_charge`. That is priced now; it used to be the one number this could not answer.
Apply-path reads are still counted on their own, because only they are what a read cache would remove.

**A committed write should carry what the engine would otherwise read back.** A partial resolution has
to write the record again — append-only — so it needs the whole record, and the engine used to read the
old version for it. But the request already has it: the record it was answered with lives in its slot
until it is answered, so the decision carries the hold's original size along with the new remainder and
what it consumed, and the engine appends without reading. A resolution judged inside the chain that
created the hold was never answered with a record, so that one still reads — an optimisation with a
fallback, like the location token. The `linked` workload exercises it: half its chains create a hold
and resolve it inside the chain (partial settle, then void of the rest), so every such settle is a
`Reduce` carrying no original size and the engine reads the version it appended moments ago. The run
reports those as unwritten-block reads, one per in-chain settle — reproduce with
`ledgerfio run --workload linked`.

What it did not remove is the floor. A resolution that follows a partial one arrives with a stale
location: the record moved when the new version was appended, and the sequencer cannot know where, because
the address is assigned on the engine's thread. So the slot is found by probing, and a probe's
fingerprint has to be confirmed against a record — which is a store read when the hold is cold. That
residue is the verification the wider fingerprint would remove, weighed in §11 and deferred; it now has a
measurement behind it. Reproduce with `ledgerfio run --workload partial-settle`.

**The load driver reaches neither store path, and the reason changed.** It used to be that a hold cold
enough to have left the buffer was still in the sequencer's overlay if anything was resolving it, so the
engine was asked only to write to it — the lookup path was quiet because the sequencer was answering
instead. That is gone: every resolution now asks the engine. And store reads are still zero on every
workload here, because residency answers them: on `partial-settle`, of twenty-one million reads about
eight million were of unwritten blocks and thirteen million of blocks already on the store and still in
memory, with none reaching the store itself — this while residency ran at its full window and pushed
records out of memory, so the window was not merely oversized. What that says is that reads concentrate in
the recent part of the window, which is the assumption the day-long residency was chosen on, now measured
rather than assumed.

So the fetch path is still exercised only by a store with a latency and a residency window small enough to
miss — the engine's own tests, and `--store-read` with the windows a deployment would declare. A speed
contract written about lookups is measuring a path that a correctly sized residency keeps empty; what it
should be written about is what happens when residency is wrong. Re-run with
`ledgerfio run --workload partial-settle --duration 10s`.

What the buffer cannot show yet is latency, because the store underneath it is memory: a flush moves
bytes from one allocation to another. What it can show is counts, and those are the ones that matter
first — how much of what was written never had to be written out, and how many reads went past the
buffer. `ledgerfio` reports both, and the first is the number the source design's own inputs disagree
about: its settle-age distribution implies almost everything is resolved within a day, while its
survivor fraction implies half of it lives for the full retention. A run now says which.

## 13. The engine speaks first, and what it is allowed to say

> **Tried** — answering a hold the index could not take on the reply channel, like everything else.
> **Broke** — a reply names a work slot and there is no slot behind it. A sentinel correlation would make
> one field mean two things, which is the shape rule 18 forbids; and a notice is not on a request's
> latency path, so sharing the queue lets a slow reader of one delay the other (rule 10).
> **Weighed** — a flag instead of a queue. It would do for the one notice built here — a seal is the same
> news however often it is said — and was refused because expiry is the other user of this direction and
> carries a hold id a flag cannot hold. Building the flag now means building the queue later, and then the
> mechanism exists twice (rule 3).
> **Chose** — a third direction on the port with a channel of its own, drained before every other stage so
> a seal decided this tick is in effect before this tick applies anything, and no stage in `--cpu` for
> something that happens once in a node's life.

Every method on the pending port was call-and-reply: the sequencer sends, the engine answers, and the
answer carries the `Correlation` of the request that asked. Two things need the engine to start the
conversation instead — sealing when a committed hold cannot be stored, and expiry — and neither could
be built without a direction that does not exist.

**Why not a reply.** The obvious economy is to carry the news on the existing reply channel. It does
not work, and the reason is not the channel but the payload: a reply names a work slot, and there is
no slot behind a hold the index could not take. A sentinel correlation would make one field mean two
things, which is the shape rule 18 exists to forbid. The second reason is separation: replies are on
a request's latency path and a notice is not, so putting them in one queue makes a slow reader of the
one delay the other (rule 10).

**Why a queue rather than a flag.** For the one notice built here a flag would do — `HoldNotStored`
means the node stops, so a single atomic bool could never overflow and could never be lost. It was
refused because expiry is the other user of this direction, and an expiry proposal carries a hold id
that a flag cannot hold. Building the flag now would mean building the queue later, and then "the
engine speaks first" would exist as two mechanisms (rule 3). So it is a queue, and expiry lands on it
without a new one.

**Why the queue is bounded and the news is still not lost.** Rule 12 says every backlog has a limit,
and a full notice queue would be exactly the hole this change closes. So the worker latches a notice
it cannot hand over and retries it every round — the same shape as the command it defers when the
store is busy. The latch is one slot, not a queue, and that is a deliberate limit rather than an
oversight: the only notice there is means the node stops, so a second one is the same news, and the
count the report prints comes from the engine's own counter. Something that has to deliver *every*
notice needs a queue in that slot, and expiry is the change that will put one there. A run makes the
asymmetry visible rather than hiding it: at a declared maximum of twenty-four the engine reports
twenty thousand overflows and the sequencer reports one seal, printed side by side.

**Why it is drained first in the tick and has no stage of its own.** A sixth stage would add a column
to every `--cpu` report — and §10's "two `--cpu` runs are comparable only when both match" would gain
one — for something that happens once in the life of a node. `drain_backlogs` already runs before
every other stage, which is what a seal needs: one decided this tick has to be in effect before this
tick applies anything.

### Why a hold the store could not take is the apply path's problem

§9 has two ways the ledger can be wrong about itself. This is a third of the same kind, and it is
worth stating why rather than asserting it. The effect was applied: the columns moved, and the client
was told the hold committed. Neither can be taken back. What can never happen now is the resolution
that brings that pending column back down, because no resolution of a hold the store does not have can
be answered. So the money is reserved for good and this node's state has stopped following the log —
the same conclusion as a committed effect that cannot be applied, reached from the other end. A
follower replaying the same log with the same sizing stops in the same place, which is what makes
stopping right rather than merely safe.

There is deliberately no operator action, for the reason §11 gives about the table never growing: the
index is sized from a declared maximum, so passing it is a business change. The remedy is a
configuration change and a rolling restart, and the index is a derived structure rebuilt on the way
back up. What makes that tolerable is that the limit is visible long before it is reached — load
against the target, worst cascade against the cap, both printed by `ledgerfio` — and the load-factor
alarm that would warn on the same channel is still not built.

Exercised rather than argued, like the other three: `ledgersim check` gives one seed in five an index
of a few dozen slots, and the sweep asserts per seed that every overflow was answered by a seal. That
replaced an assertion that overflows never happen, which was a statement about a path the sweep never
entered. Reproduce the whole thing in one run with
`ledgerfio run --workload hold-settle --daily-arrivals 24`, which seals in about thirty milliseconds
and exits 1.

## 14. Retention is a promise, so expiry rounds late

> **Tried** — a stored per-record expiry timestamp, and a wall-clock sweep to act on it.
> **Broke** — a retention window is *configuration*, and a configuration changed and restarted has to
> apply to records already written; a stored deadline was computed under the old policy and could only be
> reached by rewriting every record. A local wall clock deciding expiry also diverges between nodes.
> **Weighed** — a per-segment lifetime (makes a deadline depend on which block a hold landed in); a
> separate durable structure keyed by deadline (a second owner of what a hold is, with its own liveness,
> compaction and recovery); searching the index for an expiring segment's addresses (exact, and it bounds
> the voids collected rather than the slots walked: 2.2s a pass at the design's size, on the thread that
> answers lookups); materialising the imminent day's detail, which is what the design's 2GB wheel budget
> actually buys (1.2GB at the design's scale, and it buys only "no block reads on a retry" — worth nothing
> against a resumable sequential walk); a per-void deadline needing the wheel's other three levels (nothing
> can express one).
> **Chose** — a deadline computed from the segment's own day, one wall-clock reading per day through an
> injectable `DaySource`, the wheel reduced to one live count per day plus the offered-and-unlanded slice,
> and the survivors read out of that day's own blocks a declared number at a time. One number —
> `grace_days` — buys away every source of *early* deletion at once, and its price is linear and only ever
> space. The resolution it proposes is a kind of its own, `VoidExpiry`, because it is nobody's request and
> three stages have to know that. Reclaiming a dead day's blocks is split from proposing its voids, because
> only the second is the leader's.

The thirty-two days is not an internal window. It is told to the customer — *your pending data is kept
for at most thirty-two days, then deleted* — and a promise like that has two edges, only one of which is
safe to miss.

- **Keeping a record longer** breaks the deletion half of the promise. It costs space.
- **Deleting it sooner** refuses a resolution that was still entitled to arrive. That is a wrong answer.

So every rounding in this mechanism goes late, and one configured number — `grace_days`, a day by
default — pays for all of it at once. What it buys is worth listing, because the temptation is to price
it against segment coarseness alone:

| where an early deletion could come from | what the grace does |
|---|---|
| a segment is a whole day, so holds in it differ in age by up to one | covers it |
| a wall clock jumping forward (NTP) | covers it, up to the grace |
| a record written after midnight for a hold created before it | already late, and inside the flush window |
| the sweep not having run, or not keeping up | already late |

The bottom two are safe by construction; the top two are what the number is for. **Raising it is the
answer to any of them**, and the price is linear and only ever space: the index and the store are sized
for `retention + grace`, which at thirty-two plus one is three percent.

### Nothing about a hold's lifetime is stored

A record carries no timestamp and stays eighty bytes. Expiry is computed:

```
a segment's records are deleted at (that segment's day) + retention_days + grace_days
```

This was not the first answer. A per-record expiry timestamp was, and the argument that killed it is not
the four bytes — it is that a retention window is **configuration**, and a configuration changed and
restarted has to apply to the records already written. A stored deadline is one computed under the old
policy; the new one could only reach it by rewriting every record. Derived, it just applies.

The same rule refuses two other placements, and neither of them on cost. A per-segment *lifetime* makes a
hold's deadline depend on which block it happened to land in — the log-position idea in disguise, and
early under load for the same reason. A separate durable structure keyed by deadline puts part of what a
hold *is* under a second owner with nothing to say which is true (rule 18), and it would need its own
liveness rule, compaction and recovery, when the one the records have is a single sentence.

**And the day needs no storage either.** A segment number *is* its day, modulo the sixty-three the address
field has room for, and only a lifetime's worth of days is ever live — so the day is recoverable from the
number. `MemoryPendingConfig::validate` refuses a lifetime that would make two live days share a number,
because that is not a slow degradation but expiry deleting the wrong day's records. The bound is a
retention of sixty-one days against the thirty-two the design asks for; past it, a small per-segment table
of days is what removes the bound.

### The clock, and how little of one is needed

Design notes §12 says the engine has no clock and should not grow one *for the window*, and that stands:
the window is still a count of blocks. Retention is different in kind — it was always a duration, because
it is a calendar promise — and what it needs is one reading per day.

The distinction that matters is not whether the engine knows the time but whether it **owns a clock**. It
does not, and it did not before: `begin_lookup` and `harvest` are handed `now`, and expiry is handed
`today`. That is what makes it testable, and the reason is concrete rather than tidy: `ledgersim` drives
`PendingEngine` directly and never runs the worker, so a wheel in the worker would be invisible to every
fault-injection seed. A retention window measured in days is also one no test could reach by waiting, so
the day is injectable (`DaySource`) exactly as the reactor's `Clock` is, and `ledgersim` compresses a day
to a few hundred virtual microseconds.

It has to be **wall** time, not the monotonic clock the rest of the engine runs on: an origin-relative
clock restarts at zero, and a promise in calendar terms has to outlive a restart. Read to the day, so
drift of minutes changes nothing.

**There is no timing wheel, and what stands in its place is eight kilobytes.** The engine's design has a
hierarchical wheel — day, hour, minute, second, with only the imminent day loaded in detail — and ~2GB
budgeted for it. Getting that number's meaning right took two tries.

It is not counts for every live hold: anything per-hold over 4.8 billion is tens of gigabytes and the design
never meant that. It is the **imminent day's detail**, exactly as its lazy-load note says — one day's
survivors, 150 million addresses, about 1.2GB. That is the 2GB.

And even that is not needed here, because the walk is **resumable**. Materialising the day would buy one
thing: no block read when a void has to be offered again. Retries are rare, and the walk that would replace
them is sequential and costs 2.9 million blocks a day at the design's scale — thirty-four a second against a
200k/s read budget. So only what has been offered and not landed has to be remembered, which is one slice:

| | what it answers | size |
|---|---|---|
| `live_per_segment` | is this day done? may these blocks go back? where does a new leader start? | 256 B |
| `days` block ranges | where did this day write? | ~1 KB |
| `outstanding` | which of the voids I offered have not landed? | ~6.5 KB |

All of it works because deadlines are day-granular and derived from the segment, so detection is free: a day
runs out, and its blocks are where its survivors are. Per-hold deadlines firing at arbitrary times would
need the other three levels — and nothing can express one, since `Transfer` is sixty-four bytes and full and
a hold's `pending_ref` already means its budget group (rule 4). If that changes, this does not extend.

Here that is `HoldTable::live_per_segment`: sixty-four numbers, one per value of the address's segment
field, maintained by the one method that writes a slot. Expiry asks the index exactly one question — *is
this day empty yet* — and a count answers it in constant time. There are no hour, minute or second levels
because nothing needs them: every hold in a day shares that day's deadline, since the deadline is computed
from the segment (above). Per-hold deadlines firing at arbitrary times would need the rest of the wheel,
and nothing can express a per-hold lifetime today — `Transfer` is sixty-four bytes and full, and a hold's
`pending_ref` already means its budget group — so the other three levels are structure for a feature that
cannot be asked for (rule 4).

### The survivors are found in the day's own blocks, and the void is judged

Two ways to find a day's survivors, both exact, because a record is alive exactly when the index points at
it — the same sentence compaction rests on. The choice between them was never about correctness.

**Searching the index** for addresses in the expiring segment reads no dead record at all, which is why it
was the first answer. What it cannot do is bound a round. The bound was on the voids a round *collected*,
and a day thinning towards empty runs out of voids long before it runs out of table — so the last rounds of
every day scanned most of it, and the round that found the day empty scanned all of it. Measured: 0.42ns a
slot, flat from 33MB to 2.1GB, which is **2.2 seconds** at the design's 5.33 billion slots. On the engine's
own thread, ahead of the lookups queued behind it, against a 5ms speed contract. This is rule 20 — the real
ceiling was the segment's density, which nobody declared and no build could check.

**Reading the day's own blocks** is what replaced it. A day's survivors are recorded in that day's blocks,
so the walk costs what that day wrote rather than what every day did, the reads are sequential, and a round
is bounded by a declared number of blocks — which bounds the voids too, at `RECORDS_PER_BLOCK` each. It
reads dead records, and that is the price: at a tenth of a day still alive it reads ten records per void.
Measured, the worst round is tens of microseconds at any density and a design day's work is a few seconds
spread across the day.

The read goes to the store without trying memory first, and that is not an oversight: a day being expired
is `retention + grace` days old and a configuration is refused unless residency is shorter than that, so
its blocks left memory long ago. Asking memory would be a test whose answer is always no.

The block range each day wrote is two numbers per segment, because block numbers count on across day
boundaries and a day's blocks are consecutive by construction (§12). Nothing else had to be remembered.

### Reclaiming and proposing are two jobs, and the wheel is what tells them apart

Handing a day's blocks back and asking for its holds to be released look like one job and are not, and the
question that separates them is *who may do it*.

**Reclaiming needs nothing.** A segment the index has no entry in holds only dead records — that is what the
count means — and it is equally true of a day whose retention ran out and of one whose holds all resolved the
ordinary way weeks early. No clock, no cursor, no consensus, and no notion of retention. So every node does
it for itself and they need not agree on when. That is not a convenience: on a follower nothing proposes
voids, so a reclaim tied to the expiry cursor would leave its store growing while the leader's shrank.

It also reclaims sooner than expiry ever did. Waiting for the retention window was never the rule; it was an
artefact of the search. Finding a day empty used to cost a pass over the index, so only the one day that had
to be checked was checked. Sixty-three counts cost nothing to read.

**Proposing needs the leader's clock**, because which day has run out is a judgment and a proposal needs
somewhere to go. That is the half a leadership gate belongs on, and the half whose cursor is volatile.

### Two things weighed here that the code does not show

**Pacing the retry.** Once a declined void is offered again, something has to say *when*, or a persistently
full backlog turns the retry into a re-offer every round — measured at 780,000 declines in five seconds, with
p99.9 three times what it was.

A doubling backoff counted in rounds was the first idea and is the wrong unit: the worker spins under load
and sleeps when idle, so the same count of rounds is microseconds in one case and seconds in the other, and a
limit expressed that way is one nobody declared (rule 20 again, in miniature).

Splitting the notice channel is the better long-term answer and is deliberately not taken yet. `ExpiryQueue`
declines rather than blocking for exactly one reason, stated in its own doc: the apply-path seal travels the
same wire, so the reactor has to keep reading whatever the backlog's state is. Give expiry a channel of its
own and a full one *becomes* the backpressure, the queue never declines, and `expiry_dropped` stops being a
concept. What is here instead is an advisory flag, because the sequencer already knows the answer and
`Backpressure` is the shape this codebase already uses for telling someone. Revisit when declines matter
again; today they are a few hundred in five seconds and every one of them is retried.

**Where the retirement cost goes.** Knowing which offered voids have landed costs one index probe each,
because it asks whether the index still points at the address — about thirty percent on top of the walk
(19ns a void to 27ns at the largest size the bench covers). The alternative was to match each removal against
the outstanding list as it is applied, which is cheaper in total and puts the cost on the path that applies
committed decisions in order. Background work bounded per round is the right place to pay, so it pays there.

### The segment wrap, and why the bound is declared rather than relied on

A sweep more than `SEGMENTS - lifetime` days behind meets its own target as the day being written. Driving
the engine there by hand showed the outcome is late rather than early — but only by accident, and the accident
is worth recording because it is what made the bound worth declaring.

Three things had to hold for it not to be a *wrong* answer. A day's blocks are remembered as a range —
`(first, count)` — so a second day's blocks extend the count without moving `first`, and the walk then
enumerates numbers between the two days that no block has; those reads miss and offer nothing. `free_segment`
resets the range and advances the cursor in one step, so the range can never be reset while the cursor still
points at that segment. And days cannot be skipped. Change any one — make the range a list, let `note` move
`first`, allow a skip — and the walk reaches a day's *live* records and voids holds created today. That is
early deletion, which is the one direction this whole mechanism exists to avoid.

So the ceiling is declared from the one number it follows from, in `day_has_a_segment`, rather than left to
whichever structure happens to misbehave first.

### What survives a leadership change, and what does not

The cursor is leader-local on purpose — it is a judgment from a clock, never a log entry — so a new leader
starts without one. Taking it from the clock alone, as `today - lifetime`, **abandons every day the old
leader had not finished**: those holds are never released, their pending columns never come down, and their
blocks never go back. It is the defect `Sweep` records having been found once already, reached from the other
end.

The counts are the recovery, and they can be because they are a function of the log: a node that has applied
the same prefix has the same counts, whatever its table size or its clock — a count is placement-independent.
A segment with entries in it whose day has already expired is a day somebody left unfinished, and the oldest
of those is where to resume. So the same sixty-four numbers answer three questions: is this day done, may
these blocks go back, and where does a new leader start.

The rest of what a leadership change loses costs a walk and nothing else. The offered-and-unlanded slice goes,
and the new leader rediscovers it by walking the day. A void the old leader proposed and did not commit may
still commit afterwards; the new leader's re-offer carries the same id, derived from the hold, so the second
is a duplicate. That is what the derivation is for.

**And one thing it is easy to get backwards.** A leader whose clock is fast does not make nodes diverge. The
void goes through consensus, so every node applies the same release at the same log position — the execution
is deterministic and that is design §5.2 working. What it does is make the *whole ledger* delete early,
uniformly and durably, with no downstream check possible because a record carries no timestamp. So the clock
defence in §5.3 is not a consistency mechanism; it protects the promise, and it is the one place expiry can
be wrong rather than late.

The expiry void is then **judged like any other resolution, not applied**. It has to be: a client void or
a settle may be in flight for the same hold, and only the judge sees both. So it takes a slot, a place in
its lane and a lookup, and it is refused by the same rules — a hold already resolved, a quarantined lane, a
sealed apply path — with the sweep offering it again next time round.

**And it is a kind of its own, `TransferKind::VoidExpiry`, not a void with a note attached.** For a while
one word covered both and three stages told them apart by reading the reserved top bit of the transaction
id, each for a slightly different question: is a client using a reserved id, does this want idempotency's
queue rather than its map, is anyone waiting for an ack. They agreed only because the one ledger-origin
transfer that exists is an expiry void, so a second one would have made all three quietly wrong — rule 18,
a judgment everything depended on that nothing owned. Naming it a kind puts the decision in `kind()` and
lets the compiler ask every reader, which is what a comment was doing before.

It is not an *origin* beside the kind, which was the first attempt at this. Origin does not vary
independently of kind — a hold, a settle and a single-phase transfer are always a client's — so an
orthogonal axis names eight combinations of which five cannot exist and nothing forbids building them. And
the discriminator stays the id's reserved bit rather than a flag of its own: that bit has to exist anyway,
so reading it is one owner with two readers, where a flag beside it would be two owners of one truth.

Two things follow that are worth stating because they are wire-visible:

**The top bit of a transaction id is the ledger's.** An expiry void's id is *derived* from the hold it
resolves, so that two leaders propose the same one and the second is a duplicate rather than a second void.
Derived means not unique by construction, so it needs a space of its own — a client colliding with one
would have a real transfer answered as a duplicate. `Transfer::validate` stays shape-only and the
refusal lives at the client boundary in `admit`, because a ledger-origin id is perfectly well shaped and
the ledger submits one.

**The ledger does not ack itself.** Nobody sent the void, so an ack for it would put a transaction id no
client sent into the client's stream. The reserved bit is what makes that readable off the id, so no stage
has to carry a flag saying whose work a request was.

### Weighed and refused: the two negative answers

The engine's design document asks for a negative answer split in two — a hold that was resolved or
expired, against one that never existed. It is refused, and not for cost.

Answering "expired" means keeping something about that hold past its retention, and **a tombstone is
exactly the data the promise says is deleted**. There is no way to infer it either: a `TxId` is
client-chosen and carries no time, so an unknown id cannot be dated; the idem map's window is an hour
against thirty-two days; and the consensus log is compacted and not queryable by id. So the split cannot
be built without breaking the promise it exists to serve.

What it was really for is the client's reconciliation — *I created hold H for a hundred; where is it?* —
and that is served better by telling the client **when the void happens** than by answering questions
about it for ever. The ledger has no outbound channel for that yet, which is the honest gap; the current
single negative answer at least makes no false claim, since `PendingRefNotFound` says "not found" rather
than "never existed".

Reproduce the whole mechanism with `ledgersim check --seeds 64`, whose coverage line reports expiry
offered, admitted and refused, and whose sweep test fails if no seed outlived its retention.

## 15. The snapshot is the index, and the log being truncated is the only reason it exists

> **Partly built.** The format, the round trip, coverage, replay and the copy-on-write stable read are code
> with tests; what is not is **where a snapshot goes** — nothing writes one anywhere, the throttle that would
> pace it is a number nobody has chosen, and whether a cold start reads a local copy or fetches from a peer is
> a decision. `status.md` tracks those three. Everything below is the reasoning either way, including the
> turns that were wrong, which is what this file is for.
>
> **Tried** — "a checkpoint of the engine's state", taken to mean the index, the buffer, the block
> metadata and the per-segment counts, written every N minutes.
> **Broke** — three of those four have no business being in it, and the fourth is the only one that does.
> Blocks are already on disk and immutable. The buffer is records not yet on disk, which the log has, so
> replay rebuilds it. Counts are functions of the slots, and so is every range the *expiry walk* needs.
> What is left is the index, and it is the only thing that says which of the records on disk are alive.
> **Weighed** — carrying the buffer as well, so coverage is *now* and no replay is needed (more bytes,
> and a moving set to snapshot); a block-number cutoff to keep the dump coherent while it is written,
> which was a self-inflicted problem and lost a hold repointed across it; two full copies of the array
> for a stable read (42.7GB against the tens of megabytes copy-on-write costs); deriving a group's
> totals instead of accumulating them, which would need the membership index §4.8 asks for and is
> unnecessary once a partial resolution of a group member is known to be impossible.
> **Chose** — the raw slot array minus what points at records not yet on disk, plus the group totals and
> one coverage scalar; a stable read bought by copying only the buckets touched while it is written; and
> the log's tail replayed to make up the difference. The same bytes serve a follower's catch-up and a
> local restart, so the serialisation is the work and the cadence is a policy on top of it.

**Nothing here is needed if the Raft log is never truncated.** That is worth stating first, because every
other justification is downstream of it. A follower that is behind can be caught up by log replay; a node
that restarts can rebuild by log replay; a cold start can rebuild by log replay. All three stop working at
the same moment and for the same reason — the log has to be truncated, because it grows with every
committed effect and nothing else bounds it. Once entries are gone, the state they described has to come
from somewhere, and that somewhere is this.

### What the disk already has, and the one thing it does not

The blocks are on disk and immutable, and a block is written once. So "the records in blocks written
before B" is a fact that never changes, whenever it is read.

What is *not* on disk is which of them are alive. A resolution appends nothing — a `Remove` writes no
record and a `Reduce` writes a new version — so a resolved hold leaves its old record sitting on its
block with no marker. Aliveness lives in the index and nowhere else, which is the same sentence
compaction and the expiry walk both rest on.

That is why scanning the blocks cannot rebuild the index, and the counterexample is one line: a hold
created thirty days ago and resolved twenty days ago has its record on a block and its `Remove` outside a
24-hour log. A rebuild from blocks resurrects it, and its pending column is reserved again for a hold that
was released. Losing the index is not losing data — it is losing the ability to answer any resolution,
which leaves every hold's money reserved for good.

### The boundary, and why the buffer is not in it

Coverage is the apply index of the **last durable block**. Not "the last record to leave the buffer": a
record in the block still being filled has a real address and is not on disk yet. And not the last *sealed*
block either — that was the test here until the store's interface grew an `fsync`, at which point written
and durable stopped being the same event and only one of them is what a crash agrees with (§16).

Everything after that point is rebuilt by replaying the log, which is where the writeback window's hour
finally gets a justification that can be checked without a device: **an hour is what recovery replays**,
not what a checkpoint has to carry. The claim is arithmetic on the log's own size, and the log keeps
whatever the snapshot interval needs (below).

So the dump excludes every slot whose address is in the buffer segment, in the block being filled, or on a
block no sync has covered. Their holds were created after coverage, so replay creates them again.

### Replay over already-applied state, which is what a smear needs

The dump is paced — 42.7GB cannot be written in one worker round — so slots written early and late reflect
different moments. Coverage is the frontier at the *start*, which means replay re-applies effects the dump
already reflects. That is only safe if applying twice is a no-op, and it is, one case at a time:

| replayed | what happens |
|---|---|
| `Remove` of a hold already gone | `index.remove` finds nothing and returns before touching anything |
| `Reduce` already applied | repoints to the same or a newer address; appends one wasted record version |
| `Reduce`'s group arithmetic | does not exist — a group member cannot be resolved in part, so a `Reduce` never carries a group (§14's assert) |
| `Create` already applied | **inserts a second slot for the same key** |

So exactly one change is needed: a `Create` for a key the index already holds becomes a repoint. It is
unreachable in normal operation — idem refuses a resend, so a hold is created once — and correct in
replay, where the record is appended again and the index follows the newest.

An earlier attempt avoided this with a block-number cutoff: exclude any slot whose address is beyond the
coverage frontier, so a `Create` after coverage never appears. That works for `Create` and loses a hold
that was *repointed* across the cutoff — its slot is excluded, and `repoint` does not insert, so replay
drops it. The cutoff was solving a problem the one-line change does not have.

### The kick cascade is why the read has to be stable

Effects are replayable. A cuckoo relocation is not: `insert_new` displaces an existing entry into another
bucket, and that movement is nowhere in the log. So an entry that moves while the dump is in progress
produces one of two silent wrong answers:

- from an already-dumped bucket to one not yet dumped: it appears **twice**. On restore, one `remove`
  clears one slot and the other survives, so a resolved hold is alive again and its money is reserved for
  good.
- the other way: it appears **nowhere**. Its `Create` is before coverage, so replay does not restore it.

Hence the dump needs a stable view of the array. Not two copies of it: copy the buckets that are about to
change, and let the dumper read the pre-image. At the design's rates the index takes a few thousand writes
a second, so a dump lasting a minute or two leaves the side buffer holding tens of megabytes — against
42.7GB for a second array.

**Its size follows the dump's duration, and the duration is a declaration rather than a device.** The write
is throttled — a bounded number of buckets per round, like every other background path here — so how long
a dump takes is chosen rather than discovered, and the side buffer it needs follows from that choice by
arithmetic. That makes the buffer one more size derived from `PendingCapacity` and refusable at startup,
which is what rule 20 asks for: the ceiling lives on the declaration and not on whichever disk the
deployment happens to have.

Throttling is needed whether or not the snapshot shares a disk with the Raft log. Even alone it competes
with the engine's own reads, and those are on the critical path against a 5ms contract.

### What is carried, and what is derived

| | in the snapshot | why |
|---|---|---|
| index slots, raw | ✅ | the only thing that says what is alive |
| group totals (`budgets`) | ✅ | RAM-only, and not a function of the slots: a slot does not name a group |
| coverage (one apply index) | ✅ | where replay starts |
| blocks | ❌ | already on disk; a follower gets them by bulk transfer (§6.3) |
| writeback buffer | ❌ | not on disk, so the log has it — replay rebuilds it |
| residency | ❌ | memory copies of blocks that are on disk |
| per-segment block ranges | ❌ | what the expiry walk needs of them is the span the slots reference; where a block *sits* needs no range at all (§16) |
| per-segment live counts | ❌ | count the slots |
| overlay, sweep cursor, offered slice | ❌ | leader-local and volatile by design |

**Raw, in slot order, because there are no keys.** A slot holds a fingerprint, not a key, so a placement
cannot be recomputed — which means a sparse dump would have to write each entry's bucket and way
explicitly, and that is larger than letting position be implicit. Raw also makes restore a copy rather
than a rebuild. The ten percent of slots left empty at the load target are written as zeroes, which is
also the part that compresses.

**42.7GB, not the design's 37GB.** The design's slot is seven bytes — a two-byte fingerprint and a
five-byte offset — and ours is eight, because the address is forty-seven bits rather than forty (§12: a
wider block field, 137TB addressable against 1TB) and there is an ambiguity bit beside it (§11).

### The interval, and the one number nobody has

Recovery replays from coverage, and coverage is the flush frontier at the time of the dump. So:

```
log retention  >=  flush window + snapshot age
```

With an hour of window, a 24-hour log allows a snapshot up to 23 hours old. But that reads the dependency
backwards: the retention is a *consequence* of the interval, not a constraint on it. Both are dials, and
24h/1h is one consistent pair.

| interval | snapshot writes | replayed on recovery |
|---|---|---|
| 10 minutes | 42.7GB × 144/day ≈ 71 MB/s | 1h10m |
| 1 hour | ≈ 12 MB/s | 2h |
| 23 hours | ≈ 0.5 MB/s | 24h |

A long interval is nearly free and a short one is not, so the interval is decided by how long recovery may
take. **That is the number nobody has**: replaying twenty-four hours is a few hundred million effects, and
how fast the engine applies them is not measured, because there is no replay path to measure. An estimate
of a million a second puts it at minutes rather than hours, which would say the design's "base every N
minutes plus deltas every few seconds" is built for an MTTR this workload does not need. An estimate is
not a number, so the order is: serialise, replay, **measure the replay**, then choose — because choosing
first would add one more figure with nothing behind it.

### Catch-up first, disk second

A follower too far behind gets the state rather than the entries, which is Raft's `InstallSnapshot`, and
the leader can serialise its index on demand for that — no stored copy involved. A local restart is the
same bytes read from disk instead. So the serialisation and its stable read are the whole of the
mechanism, and whether it is also written down periodically is a policy question about cold start: a node
that always fetches from a peer needs a healthy peer, and a cluster that loses power together needs a
local copy or a log long enough to replay from nothing.

### Against the design's §6, item by item

The design's checkpoint section is a list of targets and three rules. Most of it survives; the
disagreements all follow from the buffer being left out, and two of its rules were missing here.

| design §6.2 target | here | why |
|---|---|---|
| Cuckoo index | ✅ carried, raw | the only thing that says what is alive |
| bitmap + `min_live_seg_id` | — neither exists | no occupancy bitmap: an empty slot is the zero word. No epoch: a day's blocks go back only once the index has no entry in it, so there are never dead slots to reclaim |
| hot buffer | ❌ not carried | it is not on disk, so the log has it. This is the disagreement everything else follows from |
| timing wheel | ❌ derived | the wheel is one live count per day, and counting the slots gives it |
| `group_index` | ✅ carried, as totals | the design's is a membership set; ours is a count and a sum, because coverage is checked by count and nothing enumerates members |
| overlay excluded | ✅ agreed | leader-local and volatile |
| `apply_index` recorded | ✅ carried | and see below — the design means more by it than replay's starting point |

Three rules, and they hold:

- **§6.1, the store is a derived view and replay is what prevents resurrection.** Agreed, and §15 is that
  sentence taken to its conclusion. Worth noting the sharpening: replay prevents resurrection *only within
  the retained log*, which is exactly why a snapshot has to exist once the log is cut.
- **§6.4, truncation is bounded by coverage and never by time.** Agreed.
- **§6.4, entries committed and not yet applied are outside coverage.** Satisfied by construction rather
  than by a check: coverage here is the flush frontier, which lags the apply index by the writeback window,
  so nothing unapplied can be inside it.

Two of the design's rules were absent from the reasoning above and belong in it.

**The write has to be throttled, and the interval table quietly assumed its bandwidth was free.** It is
not free, and it is not the disk layout that makes it so. Whether Disk 1 carries both the log and the
snapshot, as §2.2 has it, decides only *what* the snapshot competes with — the log's commits, which are on
the critical path, or the engine's own reads, which are on it too against a 5ms contract. Either way the
answer is the same and the design says it: a rate limit and low-priority IO, which here takes the shape
every other background path takes — a declared number of buckets per round.

The layout does change the arithmetic, so it is worth keeping the numbers apart from the rule. Sharing a
disk, a long interval puts the snapshot at a fraction of the log's own write rate and ten minutes puts it
at twice — another argument for the long interval. On its own disk that comparison disappears and the
competition is with reads instead. The throttle is required in both.

**`apply_index` is a cross-component invariant, not just a resume point, and it did not exist.**

Three numbers looked like one and none of them was: the sequencer's committed count, `AccountPort::applied`,
and the engine's own. All three are per-process counters that restart at zero, and `RaftCommit` carried a
batch id rather than a position. So no component could have recorded where its state stood even if it had
wanted to.

The seam is now open and deliberately shallow. `ApplyIndex` names the concept, a commit carries the log
position of its batch — advanced by committed batches only, so it is gapless, which is what makes
"everything up to here" a well-formed sentence — and the reactor records the last one it applied and
exposes it. Two tests hold it open (`sequencer/tests/apply_index.rs`): that a refused batch takes no
position, and that the index and the account view do not drift.

What is *not* built is the per-component half: recording it on each side and restoring it. That is left
undone on purpose rather than forgotten — the pending engine sits behind a queue and cannot be asked
synchronously, and the shape of the recording follows from what the snapshot turns out to be. Opening the
seam before knowing that shape is a deliberate exception to rule 4, taken because a seam that is not there
is one nobody remembers is missing.

**And the account component's size is worth having written down**, since it decides whether its own
snapshot can stop the world. At 400 million accounts it is 16GB of records at forty bytes plus about 8.8GB
of index at the twenty-two bytes an entry a run reports — roughly 25GB, which is the same order as the
pending index rather than a tenth of it. So the tempting asymmetry — pending is too big to copy, the account
view is small enough — is not there, and both need the same copy-on-write treatment.

**The rest of the cross-component argument.** The account component
checkpoints too, and the design records the index in both so the two views can be compared. That check
already exists in flight — the reactor compares `accounts.applied()` against its own committed count every
tick, and a mismatch is `Broken::AccountViewDisagrees`, which seals. What does not exist is the check
surviving a restart: if the two components' snapshots are taken at different points, recovery has to
replay from the earlier of them and the later component has to tolerate seeing effects it has already
applied. That is the same idempotency argument as above, now needed on the account side as well, and
nothing here has established it there.

## 16. The store's interface is a filesystem's, and the offset is a function of the address

> **Tried** — adding a file and an offset to the block store as it was: a trait addressed by
> `RecordAddr`, with each backend working out for itself where a block sits.
> **Broke** — two ways. The one rule that has to be identical in every backend would have lived once per
> backend. And the offset that shape implies is *relative* to a segment's own first block, which is not a
> function of anything that survives: a leading block whose records all died leaves no live slot to find
> it by. The snapshot test that shares one store between two engines could not read a single record —
> `the index names a block that no day says it wrote` — which is the same defect shape as the group
> totals in §15, a value documented as derived that is not derivable.
> **Weighed** — carrying the sixty-four ranges and the next block number in the snapshot (about a
> kilobyte, and it makes an exception of §15's argument that the snapshot is the index and the rest is
> derived); a struct of its own above the seam to own the layout (the record log already owns the ranges,
> so it would have been a second owner of them, rule 18); a raw block device instead of a filesystem (the
> directory is the metadata you would otherwise have to write and `fsync` yourself); naming the trait for
> its unit rather than its purpose (`BlockStore` — but memory is the stand-in and durable space is the
> point, and this repo already names a port for the role and the stand-in for its backing:
> `PendingPort`/`MemoryPending`).
> **Chose** — a filesystem's vocabulary at the seam. A segment is a file: brought into being by its first
> block, appended to at an offset, read at an offset, removed whole, and able to fail. The offset is
> **absolute** — the block number times the block size — so it is a function of the address and nothing
> has to be restored. `MemoryStore` backs it today, `LatencyStore` prices a device in front of any
> backend, and `FileStore` is seven methods whenever a disk arrives.

**The seam is where it is because of what has to be identical.** Above it the engine speaks in
`RecordAddr` — segment, block, record — and knows about holds, days and retention. Below it there is a
segment, an offset and bytes, and nothing that understands any of the above. `RecordLog` is the
translation, and it is where it already was: it keeps the per-day block ranges for the expiry walk, so
asking it for an offset adds no state and creates no second owner of one.

### Absolute offsets, and what a filesystem is being asked to remember

Block numbers count on across day boundaries and a day's blocks are consecutive, so a segment's file
holds one contiguous extent that begins at `first_block × 4096`. Everything before that offset is a hole.

That is the whole trick, and what it buys is that **the filesystem's own extent map is the layout**. A
restart derives every offset from the address it already has in the index and asks nobody: no range in
the snapshot, no directory scan, no superblock. A raw block device would have to write and `fsync` that
mapping itself, and that — rather than throughput — is the argument for a filesystem here.

What it costs is an apparent file size and not space: allocation is the day's blocks and nothing else, so
`du` tells the truth and `ls -l` does not. Two consequences worth knowing before a deployment rather than
after one:

- **`ext4` caps a file at 16TB**, so a block number above 2³² could not be written to one. At the design's
  2.9M blocks a day that is 1481 years. XFS and APFS cap at 8EB and the question does not arise.
- **A copy that is not sparse-aware would expand the hole.** Nothing in the recovery path copies these
  files — the log and the snapshot are what recovery reads — but a backup script written in ignorance of
  this would try to write 137TB.

### The buffer's alignment arrives before the buffer's reader

`Block` is 4096 bytes aligned to 4096, and that is not a cache-line matter: the residency window is
already this engine's cache, so a real store reads and writes past the page cache rather than through a
second copy of it, and direct IO wants the *buffer address* aligned as well as the offset and the length.
The last two are whole blocks by construction. A `Vec<u8>` is aligned to one byte, so an unaligned buffer
would have bought a bounce copy per IO on a path that has none.

It is written as a literal `repr(align(4096))` rather than through `cache_aligned!` because that macro
exists to funnel the *target's* line size, which varies per build. This alignment is `BLOCK_BYTES` and
cannot vary with the target, so what keeps the literal honest is a const assertion that it equals the
constant. `ledgerfio layout` prints the claim beside everyone else's.

### What a real backend is, method by method

| seam | a file per segment |
|---|---|
| `open_with(segment, offset, block)` | `openat(dir, "seg-NN.blk", O_CREAT\|O_EXCL\|O_RDWR\|O_DIRECT)` then `pwrite` at `offset` |
| `append(segment, offset, block)` | `pwrite` |
| `read_at(segment, offset, into)` | `pread`. A short read is `Missing`; `EIO` is the fault variant that arrives with the device |
| `submit` / `poll` | the one pair POSIX has no answer for — `pread` on a thread pool, or io_uring. `SE-OQ-4` is a choice between two implementations of these two methods, and this is where it lives |
| `remove(segment)` | `unlinkat`, then close |

**`open_with` is one call and not a create followed by a write**, because a segment's first block *is* its
creation and two statements that always happen together are two that can come apart (rule 16). Which of
the two a write is, is the caller's to say rather than the backend's to discover: `RecordLog` knows from
the day's own block count, so a real backend never pays a syscall to learn what its caller already knew.

**Durability on a filesystem has two layers, and that is why a sync is one call with no argument.**
`fsync(fd)` makes a file's bytes durable; a file that has just been created also needs `fsync(dir)`, or a
crash can leave durable bytes in a file that does not exist. So durability is a property of the store as a
whole at a moment in time, not a per-segment watermark — an optimisation someone would otherwise reach
for. Removing a segment needs no such care in the other direction: `reclaim` uses no clock and no cursor,
so a leftover file is found and freed again on the next pass.

**`ENOSPC` is real on a filesystem and impossible on a preallocated device**, and its reaction is `EIO`'s:
the hold cannot be stored, which is rule 19's condition. Same reaction, so it is not a variant of its own.

**`O_DIRECT` cannot be verified on this machine.** macOS has no equivalent — `F_NOCACHE` is advisory — so
numbers from a local file backend would not be a device's. `SE-OQ-6` needs a Linux host, which is worth
knowing in advance of it rather than as a surprise.

### What this makes askable, and one answer it already narrows

`SE-OQ-5` is compression, and the layout above answers half of it before anyone measures: a block's offset
is derived from its number, so **compressing whole blocks would break the one rule this seam rests on** —
a shorter block leaves the next one's offset no longer derivable, and an offset map is exactly the state
absolute offsets exist to avoid. Compression therefore belongs *inside* a block, at the record, with the
block staying 4096 bytes. That also matches what compression is for here: the design's sixty-four records
a block needs its 128-byte record halved, and the gain is records per read rather than bytes per write.

### Written became durable, and coverage is what it changed

`sync` is on the seam and takes no argument, for the reason above: on a filesystem durability is a fact
about the store at a moment rather than a watermark per file. So **what a sync covered is remembered on this
side of it**, by the one thing that knows — `RecordLog` keeps the oldest block sealed since the last sync
and the log position it began at, one pair, because seals are in block order and a sync covers all of them.

That pair is what coverage now stops at, ahead of the two that were already there: a sealed-and-unsynced
block, then the block being filled towards the store, then the oldest block in the writeback buffer. The
order needs no comparison — each is drained out of the next, so each one's stamp is at or before the one
after it.

**Erring here is one-sided, which is why a lagging sync is safe.** Coverage that stops too early costs
replay a little; coverage that stops too late names a block a restart cannot read, and the holds on it are
gone. What is not durable is still in the log, so the only price of syncing rarely is replay.

The worker syncs at the end of its round, so one sync covers every block that round sealed — group commit,
without needing a name for it. Whether that is the right cadence is a question this cannot answer: what a
deferred sync buys is a device's `fsync` off the thread that answers lookups, and there is no device here to
price it against. It is in `status.md`'s decisions list with that stated, and §16's device model is what
will answer it.

`MemoryStore::sync` does nothing and there is nothing dishonest in that: memory has no second layer to push
bytes into. What it implements is the *barrier* — the caller learns what is covered from when it asked — and
that is the half a test can exercise without a device.
`a_sealed_block_is_carried_only_once_a_sync_has_covered_it` is that test: three engines over one store, and
a restore taken before the sync answers none of the holds while one taken after answers them.

### What the device model says, now that there is one

`LatencyStore` prices a device where there is none, and until now it priced only reads. It prices writes and
syncs too, and those are charged differently on purpose: a read occupies the *device* — submitted, served by
the queue, harvested later, with the engine working meanwhile — while a write, a sync and an apply-path read
occupy the *thread*, which on this component is the thread every lookup passes through.

**Where 1M tx/s comes from, because it is not a target.** The design's rate is 150M arrivals a day, which is
1,736/s — six hundred times lower. At `ledgerfio`'s usual 100k/s neither flag is visible, and not because it
costs nothing: the spread between repeats at 100k/s is 20–80ms at p99.9, which is the idem stand-in's own tail
(`status.md` records it), and it is larger than what is being looked for. 1M/s is where the engine rather than
the client or the stand-in is the limit, so the curve is readable. It is a ceiling-finding rate, and a number
read off it means nothing until it is expressed in something that transfers.

**What transfers is a budget: one thread divided by the block seal rate.** At 1M tx/s this workload seals
19,444 blocks a second (`engine record blocks peak 97222` over five seconds), so the budget per write is 51µs.

| `--store-write` | tx/s | | `--store-sync` | tx/s |
|---|---|---|---|---|
| 0 | 1.00M | | 0 | 1.00M |
| 20µs | 1.00M | | 500µs | 0.91–0.98M |
| 50µs | 0.71M | | 2ms | 0.74M |
| 100µs | 0.44M | | 4ms | 0.51M |

The budget is the knee: 20µs is free, 50µs — the budget itself — costs 29%, 100µs costs 56%. **A sync is about
four times cheaper per microsecond at the same point, and group commit is why.** One sync covers every block
the round sealed, so a slower device is covered by fewer syncs and the curve bends instead of falling. A write
is per block and nothing amortises it.

At the design's own rate the budget is thirty milliseconds a write and neither flag matters. That is what
settles the sync cadence, and it settles it by arithmetic rather than by the curve above: thirty-four blocks a
second against an `fsync` of 50–500µs is 1.7% of a thread.

**The read is priced too, and the combination that reaches it is the whole trick.** A store read only happens
when a resolution needs a record that is no longer in memory, so it takes two things at once: a residency
window short enough to fall out of (`--residency 1`) and a hold resolved after it does but still inside the
run (`--resolve-after 100000`). With both, every read is a store read — 199,933 of them, 100% — where
`--resolve-after 900000` produces none at all, because at 100k/s over five seconds no resolution lands and the
run is holds only (`engine told create=500032 reduce=0 remove=0`). The first version of this note said
`ledgerfio` could not reach the read path, on the evidence of runs that used the second figure. It can.

| `--store-read` | tx/s | p50 | p99.9 |
|---|---|---|---|
| 0 | 100k | 1.4ms | 11ms |
| 200µs | 100k | 1.5ms | 7.5ms |
| 1ms | 84.6k | 1.5ms | 10ms |
| 5ms | 60.7k | 212ms | 755ms |

That last row is not the `≤5ms` contract failing. It is 40,000 store reads a second needing about two hundred
outstanding, against a queue depth of 128 that `ledgerfio`'s runner writes as a constant — so the run refuses
reads and what the number shows is the depth as much as the device. The two are not separable from outside,
which is why the depth is in `status.md`'s decisions list rather than described here as a result. It is also a
stress point rather than a design one: forcing every resolution to miss memory is what the two flags are for,
and a 24-hour residency exists so that a deployment does not.

### The fault a device produces, and the one seal it shares

`StoreFault` has two variants and one reaction, which is the honest shape: `Missing` is this node's own record
of where blocks are having stopped agreeing with the store, `Device` is an `EIO` or the `ENOSPC` that retention
was supposed to make impossible, and either way a record the log says exists is one this node cannot read. So
both seal the apply path, through a third notice — `PendingNotice::StoreFailed`, which carries no id because a
block holds up to fifty-one records and a failed read is about a block.

Counted apart from `holds_not_stored` all the same. The two conditions are identical and their *causes* are
not: one is a table sized for a maximum the run passed, the other is a device, and a report that could not
tell them apart would send an operator to the wrong place.

**The fault is latched rather than returned up the stack, and the one round of delay is deliberate.** Three
paths meet a fault — a write inside compaction, an apply-path read, a harvested completion — and threading a
`Result` out of all three would put one decision in three places. Nothing is answered from a faulted read in
the meantime, so rule 19 still holds: what the latch delays is the seal, not the stopping.

`--store-fault-every` is what produces one, and it is a fault knob for the same reason
`--violate-order-every` is: `MemoryStore` cannot fail, so without it this seal would be code nothing had ever
run. `a_store_that_refuses_seals_the_apply_path_and_says_so_separately` is the test, and setting it up found
its own trap — the short windows that make a test reach the store at all also size the index for two dozen
holds, so `HoldNotStored` fired first and the test passed for the wrong reason.

### The checksum, because corruption was not a fault at all

A device has three ways to misbehave and this store could see one of them. It could refuse — that is a
`StoreFault` and it seals. It could hang — nothing here detects that, see below. And it could **answer with
bytes that changed**, which was not a failure path at all: `decode` turns any four kilobytes into records, so a
flipped bit became a `HoldData` and the answer was wrong rather than refused.

**Double-entry does not catch it, and that is the part worth being precise about**, because "the identities
would notice" is the answer that feels right and is not. A corrupted remainder moves both sides of the ledger
by the same wrong amount, so both sums still balance. Field by field:

| bit flipped in | what notices |
|---|---|
| the key | the walk does: it compares the record's key against the one asked for, so the record is passed over and the answer is "not there" — wrong, but on the safe side |
| `remaining`, too large | *sometimes*. An over-settle that drives that account's pending column below zero is `ColumnWentNegative` and seals; with another hold on the same account it takes from that one instead, silently |
| `remaining`, too small | nothing. A settle that was entitled to happen is refused |
| either account id | nothing, if the id exists. Money moves between the wrong two accounts and both identities still balance |
| `budget_members`, `budget_remaining` | nothing. The coverage check decides on a total nobody wrote |

So the seal that rule 19 asks for could not happen, because nothing detected the condition.

**It costs no space.** Fifty-one eighty-byte records leave sixteen bytes of a four-kilobyte block unused, and
a CRC32C is four of them. Per-record was weighed and does not fit: fifty-one stamps need two hundred and four
bytes, and widening the record to eighty-four to carry its own would drop the block to forty-eight records and
cost six percent of the store. So it is one checksum over the block's records, stamped at the one moment a
block's bytes stop changing — the seal — and verified by the three paths that read one back.

**A crate rather than the hasher already to hand.** `rustc-hash` is in `base` and would have been free, but it
is built for placing keys in a table: picking it here because it is available is choosing a tool by
availability. A CRC is the one that *guarantees* what this needs — every one-bit and two-bit error, and every
burst up to thirty-two bits, detected rather than probable — and CRC32C is a hardware instruction on both
supported targets. It is `pending`'s first external dependency, and it belongs there for the reason
serialisation belongs to whoever has a wire: the block format is this crate's.

`--store-corrupt-every` produces one, and it flips a bit in a record rather than in the stamp, because a stamp
that fails to match its own bytes is the easy case. Two tests: the record log refuses to decode a block that
came back changed and counts it apart from a refusal, and the reactor seals on it.

### Hang is the third way, and nothing here detects it

Not built, and the reason is larger than this store: **there is no detector for contract 2 anywhere in the
ledger.** The glossary's own entry says as much — its code column is "the latency knobs in `ledgerfio`", which
are a plan's inputs and not a check. Nothing in the sequencer or the components times anything out.

Modelling one is two lines either way. On the synchronous side an unbounded charge holds the thread for ever;
on the lookup side a `submit` that succeeds and a `poll` that never returns that handle stalls one lane
permanently. Neither is built, because a knob whose reaction does not exist tests nothing (rule 4) — what is
missing first is the reaction, and that is a decision: what bound, and whether missing it quarantines a lane or
fail-stops the node. A hang is not lane-local, which argues for the second. And it is the same decision for
idem and for consensus, so it does not belong inside the store's work. `status.md` has it.

### What the simulator found by being given a store

`ledgersim` used to build the engine on `MemoryStore::default()` and nothing else, so the seeds explored the
store's *path* — 87,000 reads across sixty-four of them — and none of its behaviour: no latency, no refusal, no
corruption. It draws a `StoreModel` per seed now, with the backing still memory, because a virtual clock with
real IO under it measures neither of the two.

**Two seeds in three get timing and the third keeps the exact store, and that share is a measurement rather
than a taste.** With every seed slowed, the sweep's store reads fell from 87,000 to 4,000: a synchronous write
or sync holds the component's thread, the step budget per seed is fixed, and the *volume* of the read path
collapses with it. Keeping a third exact holds the volume while the rest explore completions arriving out of
the order they were asked in, which is what the orderer exists for.

The fault periods are small — a refusal every four to twenty-four store *calls* — and that is not a taste
either. The first attempt used two hundred to a thousand, by analogy with the index fault's roominess, and it
never fired once: a short seed makes a few dozen store calls in total, so a period measured for a load run is
a fault that never happens. The coverage assertion is what said so, and it is the same one the index overflow
has: a sweep that met no store fault would be reporting that the seal holds about a path it never entered.

**And it found a defect in the second commit that used it.** When `MemoryStore` refused a write, the record log
advanced its block number and noted the block in the day's range anyway, so the next block was written one
place past what the store held — and the memory store's own assertion, that a block goes at the end of its
segment, caught the disagreement.

Advancing was right and the assertion was wrong. The records on the block that could not be written already
hold addresses, so reusing the number would give two records one address; what the failed write leaves is a
**hole**, and a hole is exactly what a store addressed by absolute offsets can express. So the assertion now
refuses only an offset *before* the end — a block written over one already there, which is a genuine
bookkeeping disagreement — and a gap is `None` in the memory store's sequence, answered as `Missing`. A file
differs and it is worth knowing which way: reading a hole in a file gives zeroes, so there the block fails its
checksum and is counted as corruption instead. Same seal, different cause, and both only after a write has
already failed.

### The modelled store had to hand the read down, and did not

`LatencyStore` claimed in its own doc comment that its composition is a floor rather than a sum — whichever of
the model and the store below is slower wins. For a synchronous read that was true. For the **submitted** read
it was not: `submit` recorded a deadline and handed nothing down, and `poll` did a synchronous `read_at` when
the deadline passed. Two stores that both read synchronously make that indistinguishable, which is why it sat
there.

It stops being indistinguishable the moment a backing has concurrency of its own. A thread pool or io_uring
lives *inside* the backing — io_uring owns the descriptors and issues the reads itself, so it cannot be a
decorator over a synchronous read — and the model would have bypassed the whole of it. The measurement a
modelled latency exists for is "how many threads does this rate need", answered without a device, and it would
have been the model measuring itself.

So `submit` hands the read down and takes the store below's refusal as its own, and `poll` releases a
completion when the store below has answered it **and** the model's time for it has passed. The later of the
two, which is what the doc comment always said.

**The bytes of an early completion have to be held**, and there is no way around it: the store below answers in
its order and the model releases in its own. Bounded by the queue depth — half a megabyte at the default
128 and eight at 2048 — which is a measurement tool's cost, stated rather than hidden, and the buffers are
recycled so the steady state allocates nothing.
`a_modelled_read_waits_for_the_later_of_the_two_times` checks both directions, the second by nesting two
models: a free one over a slow one releases when the slow one does, which is the forwarding being exercised.

### The read pool, and why its default is zero rather than the design's sixteen

`SE-OQ-4` names three read backends — io_uring as the mainline, libaio as the alternative, a thread pool as the
portable fallback — and the fallback is the one this machine can run. `FileStore` has it: N threads, each with
its own pair of `base`'s SPSC rings, taking `(handle, file, offset, buffer)` and handing back the buffer with
the result.

**A pair of rings per thread rather than one shared queue**, because `base`'s ring is single-producer
single-consumer and a queue several threads read is neither. That keeps every queue the one lock-free structure
here that is measured and tested, and costs a round-robin instead of a lock — nothing knows which thread will
be free first, and asking would cost more than the imbalance.

**The buffer travels as a `Box`**, so a pointer moves through the ring and not four kilobytes, and it comes back
down with the next ask, which is what makes the steady state allocation-free. Sharing one slot array between the
worker and the threads would have been faster still and needs `unsafe`, which rule 7 does not allow here.

**It is inside the backing rather than above it**, and that is io_uring's doing rather than a preference:
io_uring owns the descriptors and issues its own reads, so a decorator over a synchronous `read_at` could never
become one. The field is not a trait yet — the design abstracts the backend and this is one implementation, so
the trait arrives with the second (rule 4).

**Measured, and the answer is that it costs a third of the read ceiling and buys nothing here.**

| `--store-read-threads` | p99 at 100k tx/s | ceiling |
|---|---|---|
| 0 (synchronous in `poll`) | 5.43ms | 993k tx/s, 490k reads/s |
| 16 | 5.71ms | 663k tx/s, 322k reads/s |
| 64 | 5.74ms | — |

The reason is the page cache. A pool pays for itself where a read *blocks*; without `O_DIRECT` every read is a
cache hit of a few microseconds, the queue hop and the wakeup cost more than the read, and the threads take CPU
from the reactor on a machine with four performance cores. So the default is **zero**, which is what is
verified, and not the design's sixteen — that figure assumes half-millisecond reads, and a deployment which
bypasses the cache has to set it from Little's law on the *store read* rate: threads ≈ reads a second × the
latency of one.

**And its value cannot be measured here at all**, which is worth stating rather than leaving as an absence. To
see a pool help, the reads have to block, and the only slow reads available are modelled — but `LatencyStore`
sits *outside* `FileStore`, so it holds completions the pool has already produced and the pool's threads never
block for the modelled time. The composition cannot express "the backing is slow". Same wall as `O_DIRECT`, and
the same answer: a Linux host.

**One measurement was of my own mistake, and it is the reason the table above has no cliff.** The first version
parked each idle thread with a fifty-microsecond timeout, so sixty-four idle threads woke twenty thousand times
a second each — 1.3M wakeups against four cores — and that configuration reported p50 174ms against 1.6ms.
`park` needs no timeout: `unpark` leaves a token when it arrives first, so a thread about to sleep returns
immediately instead. With the timeout gone, sixty-four threads are indistinguishable from sixteen.

### Still unbuilt, and named so it is not read as done

- **Nothing reconciles at startup**, but the worst of it is now refused rather than done. `reclaim` only
  frees a segment it has a block count for and a restart begins with none, so a file a previous life left
  behind is still never removed — a leak. What no longer happens is the damage: `open_with` uses `O_EXCL`, so
  reusing that segment sixty-three days later *fails* instead of writing over the file's front and leaving a
  mix of two days that nothing points into. A refusal seals, which is the safe end of it. The reconcile
  itself is one call at the seam (`existing()`, a bitmap of segments present) and is not built, because
  nothing restarts yet — and `a_segment_file_left_behind_is_refused_rather_than_written_over` is what holds
  the position until it is.
