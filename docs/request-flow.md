# The life of a request

What happens to one transfer, in the order it happens, and what each step is allowed to touch. The
stage names are the design's: S1 intake, S2 dispatch, S3 judge, S4 propose, S5 apply.

## One tick

`Reactor::tick` runs the stages in a fixed order and never waits for any of them:

1. **drain backlogs** — hand over acks and hold decisions that a full queue refused earlier, and
   retry dispatches that a full external queue refused.
2. **intake (S1) and dispatch (S2)** — admit requests, give each a place in its lane, throw the
   external calls.
3. **judge (S3)** — take whatever external replies have arrived and decide the requests they
   complete.
4. **propose (S4)** — hand a due batch to consensus.
5. **apply (S5)** — apply the batches consensus has committed, in commit order.
6. **housekeeping** — let the pending engine evict what it no longer needs.

Backlogs come first because they are the only things that can block intake, and apply comes last
because it is the only stage that cannot be moved off this core. A tick that finds nothing to do
reports that, and the loop spins.

## Single-phase: the short path

```
client → intake ─ idempotency ─→ judge → propose → consensus → apply → ack
```

- **S1** `prepare` validates the shape, resolves both accounts to handles, refuses a ledger
  mismatch or a quarantined lane, and gives the request its lane seq. A debit on an unconstrained
  account takes no seq: nothing about judging it depends on the lane's earlier requests.
- **S2** `dispatch` sends the idempotency lookup and moves on. Nothing waits. A full queue means the
  slot is deferred, not dropped: it keeps its seq, because dropping it would leave a permanent gap.
- **S3** the idempotency reply clears the request's last dependency, so it is judged: seq continuity
  is checked (a gap means an external component broke contract 1), a duplicate is answered as one,
  and the balance is read as *committed columns + the lane's speculative overlay*. The effect is
  built and the amount it will spend moves into the overlay, so the next request on that lane sees
  the promise rather than a stale balance.
- **S4** the effect joins the open batch. The batch is proposed when it is full or its linger
  expires, and never split inside a linked chain.
- **S5** the commit arrives in order, the account component applies the four-column delta, the
  overlay gives the amount back, and the ack goes out.

## Hold, settle, void: the pending path

A hold takes the short path — creating a hold reads nothing. Settling or voiding one needs to know
how much is left, which only the pending engine knows:

```
S2  overlay has the hold?  ── yes ─→ judge from the overlay (no round trip)
                           ── no  ─→ lookup → engine → reply → judge
    lane already waiting?  ── yes ─→ fence (an ordering token that reads nothing)
```

- The engine's **overlay** answers inline. It holds a copy of what the store last confirmed plus the
  reservations no batch has committed yet, so a resolution is judged against the remainder as of
  now, not as of the last commit.
- A hold the engine has just been told to create is in the overlay already, so the common case —
  settle a hold this ledger created — costs no round trip.
- While a lane has a reply outstanding, every later request of that lane travels the pending path
  too, as a **fence**. Without it a request that reads nothing would overtake one that does, and the
  lane's order would be the reply order instead of the arrival order.
- **S3** checks the accounts and ledger against the hold, refuses an amount above the remainder, and
  applies the budget-group rule. The amount is reserved in the overlay at judge time, so a second
  resolution cannot take the same money.
- **S5** releases the reservation and tells the engine what changed: `Reduce` when something is
  left, `Remove` when nothing is. The engine's copy follows those writes, so it never disagrees with
  the store.

## Linked chain: judged as one

Legs of one submission are held until the last one has its external results, then judged together
in arrival order. Inside that judgment a leg sees what earlier legs decided — availability an
earlier leg brings in, and holds an earlier leg creates — because the chain commits or rolls back as
one. Any leg failing rejects all of them; none of it reaches the batch.

Requests that arrive on a chain's lane while the chain is unjudged wait behind it, in arrival order,
so the barrier the sequencer creates does not itself reorder a lane. A chain still open at a batch
boundary was abandoned by the client and is rejected, rather than gating its lanes forever.

## What can go wrong, and where it is caught

| Failure | Where | Result |
|---|---|---|
| Bad shape, unknown account, ledger mismatch | S1 | rejected before a seq is issued |
| Insufficient balance, amount over the hold remainder, group rule | S3 | rejected; nothing moves |
| Reply out of a lane's seq order | S3 | the lane is quarantined; enough lanes fail-stop the sequencer |
| Consensus refuses the batch | S5 | every effect rolled back: overlay released, reservation returned, requests rejected |
| A queue is full | S2, S4 | deferred and retried; reaching a backlog limit pauses intake |
| No work slot left | S1 | refused as overloaded, which is backpressure reaching the client |

A refused commit can only cause a false reject, never an overdraft: the overlay records what a
request will *take* at judge time and gives it back on failure, so a decision the ledger never
committed cannot have let anyone spend the same money twice.

## Invariants each stage keeps

- Nothing waits. Every stage advances what it can and returns.
- The sequencer never reorders a lane. It issues the order and checks it; reordering external
  replies is the external component's job.
- Effects are applied in commit order, and only after consensus has committed them.
- The judge reads three layers and writes one: committed columns (the account component), the lane's
  speculative overlay, and the pending overlay — plus the chain's own scratch while a chain is being
  judged.
