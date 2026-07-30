# Glossary

One name per concept. Where the design documents use a term, the code uses the same one; where the
code needed a distinction the design blurred, the split is recorded here.

## The two-phase vocabulary

| term | means | in code |
|---|---|---|
| **pending** | the phase, the column and the component. A pending transfer reserves money; the pending column tracks it; the pending engine stores it. | `TransferFlags::PENDING`, `debits_pending`, `PendingPort`, `MemoryPending` |
| **hold** | one reservation created by a pending transfer — the thing a settle or void resolves. | `HoldData`, `HoldView`, `HoldOverlay` |
| **resolve** | the umbrella for finishing a hold, either way. | `resolving_effect`, `PartialResolutionNotAllowed` |
| **settle** | resolve a hold by moving money to the posted column, in whole or in part. | `TransferFlags::POST_PENDING`, `EffectKind::Settle` |
| **void** | resolve a hold by releasing whatever is left. | `TransferFlags::VOID_PENDING`, `EffectKind::Void` |
| **single-phase** | a transfer with no hold at all: posted immediately. | `TransferKind::SinglePhase`, `EffectKind::Post` |

`pending_ref` is the field whose meaning depends on the kind: the hold being resolved for a settle
or void, the budget group being joined for a hold, absent for a single-phase transfer.

## Atomicity and grouping — two different things

| term | means | in code |
|---|---|---|
| **linked transfers** (scenario 2) | the atomicity unit of one submission: these legs commit or roll back together, judged and proposed as one. Lives for one judgment. | module `linked`: `LinkedChain`, `LinkedChains`, `LinkedChainId`, `LinkedScratch`, `LinkedPolicy`, `DepFlags::LINKED_CHAIN`, `LinkedChainTooLong`, `LinkedChainUnterminated`, `TransferFlags::LINKED` |
| **shared budget group** (scenario 1) | a lifetime property of holds: several holds draw on one budget and must be resolved together. Outlives the request that created it. | module `budget`: `BudgetRules`, `BudgetCoverage`; on the wire `BudgetGroup`; errors `SharedBudgetGroupRequired`, `SharedBudgetGroupIncomplete`, `PartialResolutionNotAllowed` |

Every name of the first mechanism contains `Linked`, every name of the second contains `Budget`, so
either scenario is one grep away. "Group" alone never means a chain, and "chain" never means a budget
group. The two meet only where a chain resolves a group, which is the coverage check.

## Order and safety

| term | means | in code |
|---|---|---|
| **lane** | the order the ledger promises for one account, which is the debit side. | `LaneState`, `LaneTable`, `Transfer::lane` |
| **seq** | a request's position in its lane. | `issue_seq`, `accept_seq` |
| **contract 1** | an external component returns a lane's replies in seq order. A gap means it did not. | `LedgerError::SeqGap`, `LogKind::SEQ_GAP` |
| **contract 2** | an external component answers within a bounded time, with no cliffs. | the latency knobs in `ledgerfio` |
| **quarantine** | isolating one lane after a gap. | `LaneState::quarantine`, `Safety` |
| **fail-stop** | halting the sequencer when the fault is not confined to one lane. | `Safety::fail_stop`, `LedgerError::FailStop` |
| **fence** | an ordering token on the pending path for a request that needs no hold data. | `PendingFence`, `Metrics::fences` |
| **order exemption** | a request whose debit account is unconstrained and which reads no hold: it keeps no place in the lane order, so it never fences. | `LaneState::UNORDERED`, `Metrics::order_exempt` |

## The judging view

| term | means | in code |
|---|---|---|
| **committed** | the durable four columns. | `AccountRecord` |
| **speculative overlay** | availability already promised to proposed-but-uncommitted requests. Reducing deltas only. | `LaneState::speculative` |
| **pending overlay** | a copy of each hold's committed remainder plus what proposed-but-uncommitted resolutions have taken from it. Owned by the pending engine, read inline. | `HoldOverlay`, `OverlayState`, `PendingPort::view` |
| **chain scratch** | availability a linked chain's own earlier legs bring in, visible only inside that chain. | `LinkedScratch` |

## Stages and pipeline

| term | means | in code |
|---|---|---|
| **intake** | S1: take a request, resolve accounts, issue its seq. | `Reactor::intake`, `admit`, `prepare` |
| **dispatch** | S2: throw the external calls without waiting. | `Reactor::dispatch` |
| **judge** | S3: check the seq, decide, build the effect. Draining replies is not judging. | `Reactor::judge`, `judge_chain`, `drain_replies` |
| **propose** | S4: hand a batch to consensus. | `Reactor::propose`, `Batcher` |
| **apply** | S5: apply committed effects in order. | `Reactor::apply`, `AccountPort::apply` |
| **effect** | what the leader decided, replicated and applied without re-deciding. | `Effect` |
| **lookup** | asking the pending engine for a hold its overlay does not have. | `PendingLookup`, `begin_lookup`, `admit_lookup`, `OverlayState::LookupSent` |
| **pin** | keeping an overlay entry while a dispatched request is still going to read it, whatever the eviction policy says. | `PendingPort::pin`, `unpin` |
| **log event** | a diagnostic record. Not the ledger's durable log, which is consensus. | `LogEvent`, `LogSink`, `LogKind` |
| **client queue depth** | requests the client has sent and not had answered. `fio` calls it iodepth; throughput is depth over latency, so it bounds what the client can ask for, not what the ledger can do. Always say whose depth — the sequencer's slots and a component's inbox are different bounds. | `Plan::queue_depth`, `ledgersim capacity --qd` |
| **slots** | requests the sequencer can hold at once. Has to cover the queue depth, or the excess is refused as overload. | `Capacity::slots` |
| **inbox depth** | commands a component holds before refusing. Refusing is what makes the sequencer defer a dispatch. | `Faults::inbox_depth`, `Capacity::pending_write_backlog` |
| **proposals in flight** | batches consensus may have outstanding. Times the batch cap, the work one round trip hides. Not the client's queue depth. | `BatchPolicy::in_flight`, `--batches-in-flight` |
