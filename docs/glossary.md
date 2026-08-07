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
| **void** | resolve a hold by releasing whatever is left. Never used unqualified in code: there are two, below. | `EffectKind::Void` |
| **client void** | a void someone submitted and is waiting for. | `TransferKind::VoidClient` |
| **expiry void** | a void the ledger proposed itself, because the hold outlived its retention. | `TransferKind::VoidExpiry`, `TxId::expiry_void_of` |
| **single-phase** | a transfer with no hold at all: posted immediately. | `TransferKind::SinglePhase`, `EffectKind::Post` |

`pending_ref` is the field whose meaning depends on the kind: the hold being resolved for a settle
or void, the budget group being joined for a hold, absent for a single-phase transfer.

**The two voids are the same money and different work**, and this is the distinction to keep straight
because for a while the code did not: one word covered both and three readers told them apart by a bit of
the transaction id. They share `EffectKind::Void`, one delta rule, and one branch in the judge and the
apply — money is where they are identical, so a second branch there would be one rule in two places.
Everything else differs, and it differs in the same direction every time: an expiry void is nobody's
request. No ack leaves for one. Idempotency records nothing (`IdemAsk::Serialize`), because its id is
derived from the hold rather than chosen, so a refused one must stay offerable. And a refusal tells no
one, which is why the sweep offering it again is the only retry there is.

They are two `TransferKind` variants rather than a kind plus an origin, because origin does not vary
independently of kind: a hold, a settle and a single-phase transfer are always a client's. An orthogonal
axis would name eight combinations of which five cannot exist. Design notes §14.

## Atomicity and grouping — two different things

| term | means | in code |
|---|---|---|
| **linked transfers** (scenario 2) | the atomicity unit of one submission: these legs commit or roll back together, judged and proposed as one. Lives for one judgment. | module `linked`: `LinkedChain`, `LinkedChains`, `LinkedChainId`, `LinkedScratch`, `LinkedPolicy`, `DepFlags::LINKED_CHAIN`, `LinkedChainTooLong`, `LinkedChainUnterminated`, `TransferFlags::LINKED` |
| **shared budget group** (scenario 1) | a lifetime property of holds: several holds draw on one budget and must be resolved together. Outlives the request that created it. | module `budget`: `BudgetRules`, `BudgetCoverage`; on the wire `BudgetGroup`; errors `SharedBudgetGroupRequired`, `SharedBudgetGroupIncomplete`, `PartialResolutionNotAllowed` |

Every name of the first mechanism contains `Linked`, every name of the second contains `Budget`, so
either scenario is one grep away. "Group" alone never means a chain, and "chain" never means a budget
group. The two meet only where a chain resolves a group, which is the coverage check.

## The four components

| term | means | in code |
|---|---|---|
| **account** | the durable four columns, one record per account, held in DRAM and persisted by the component itself. | `MemoryAccounts`, `AccountPort`, `AccountRecord` |
| **pending engine** | where holds live: the index, the two memory windows, the blocks. | `MemoryPending`, `PendingPort`, `PendingEngine` |
| **idem** | the component that records transaction ids so a resend is answered as a duplicate — **and** returns a lane's replies in seq order. Only the second is why anything calls it (rule 21). | `MemoryIdem`, `IdempotencyPort`, `IdemAsk`, `IdemVerdict` |
| **consensus** | the replicated log. An echo today. | `EchoRaft`, `RaftPort` |
| **checkpoint** | the pending engine's own state written down so recovery replays the log's tail instead of all of it — and so the log can be truncated at all (`truncate_index <= checkpoint_covered_index`). Raft literature calls this a **snapshot**; one word here, and it is this one. Not durability, which the log already has: this state is a deterministic function of the log, so what a checkpoint buys is recovery time and the right to throw log away. Not built. | design §6.2, and *Recovery is not real* in `status.md` |

**One name for the third one, and it took a rename to get there.** The crate, the port and every message
said `Idem`; the type said `Dedup`, so every assembler that constructed it — the load driver, `ledgerd`, the
test harness, two benches, the simulator — said `dedup` too, and so did the prose. Two words for one
component is what rule 3 forbids, and this is what it looks like when the offender is a *type* name: the
minority word spreads to everything that mentions the type. `idem` is the one word now.


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
| **order exemption** | a request whose debit account is unconstrained: it keeps no place in the lane order, so it never fences and nothing queues behind it. One clause, because the lane exists to protect a balance. A resolution is included, and what it gives up is covered by **stale answer**. | `ledger_base::UNORDERED`, `WorkItem::keeps_lane_place`, `Metrics::order_exempt` |

## The judging view

| term | means | in code |
|---|---|---|
| **committed** | the durable four columns. | `AccountRecord` |
| **speculative overlay** | availability already promised to proposed-but-uncommitted requests. Reducing deltas only. | `LaneState::speculative` |
| **pending overlay** | what the sequencer has decided about a hold and not handed over: the remainder it last told the engine, and what proposed-but-uncommitted resolutions have taken of that. Never a copy of a record — see **hold record**. Bounded by requests in flight. | `HoldOverlay`, `OverlayState`, `PendingOverlay::overlay` |
| **hold record** | what a hold *is*: its two accounts, its ledger, its group and the group's totals. The engine owns it; a lookup carries it to the request that asked, which keeps it in its slot until it is answered. | `HoldData`, `SlotPool::record` |
| **hold view** | the two put together, which is what the judge decides on. The overlay wins on the remainder, because a decision cannot be older than an answer that was in flight across it. | `HoldView::compose` |
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
| **stale answer** | a reply reflecting fewer committed decisions than the engine had already been handed. The data check that stands in for the lane's order on a request keeping no place in it; treated as a contract-1 violation. | `Metrics::stale_answers`, `PendingReply::applied` |
| **lookup** | asking the pending engine for the record a resolution is judged by. Every resolution sends one, bar those of a hold the engine has already said is not there. | `PendingLookup`, `begin_lookup`, `admit_lookup`, `hold_is_missing` |
| **pin** | keeping an overlay entry while a dispatched request is still going to read it, whatever the eviction policy says. | `PendingPort::pin`, `unpin` |
| **log event** | a diagnostic record. Not the ledger's durable log, which is consensus. | `LogEvent`, `LogSink`, `LogKind` |
| **client queue depth** | requests the client has sent and not had answered. `fio` calls it iodepth; throughput is depth over latency, so it bounds what the client can ask for, not what the ledger can do. Always say whose depth — the sequencer's slots and a component's inbox are different bounds. | `Plan::queue_depth`, `ledgersim capacity --qd` |
| **slots** | requests the sequencer can hold at once. Has to cover the queue depth, or the excess is refused as overload. | `Capacity::slots` |
| **inbox depth** | commands a component holds before refusing. Refusing is what makes the sequencer defer a dispatch. | `Faults::inbox_depth`, `Capacity::pending_write_backlog` |
| **proposals in flight** | batches consensus may have outstanding. Times the batch cap, the work one round trip hides. Not the client's queue depth. | `BatchPolicy::in_flight`, `--batches-in-flight` |

## Inside the pending engine

The engine's own structures, which nothing outside it names. The sequencer knows two contracts and
neither mentions any of these.

| term | means | in code |
|---|---|---|
| **inline contract** | the half of the pending port answered on the caller's thread, immediately, and unable to refuse. It is the overlay and nothing else: the sequencer's own decisions, which need no round trip because they are already here. | `PendingOverlay` |
| **queued contract** | the other half: send and move on, a full queue is backpressure, replies come back in each lane's seq order. | `PendingPort`, `PendingCommand`, `PendingReply` |
| **notice** | the engine speaking first: news that answers no command and names no request, so it travels its own channel rather than as a reply. Two exist: a committed hold the index could not take, and a hold that outlived its retention. | `PendingNotice`, `PendingPort::notices` |
| **hold index** | where a hold is, by transaction id. Fingerprints and addresses only, so a shared fingerprint has to be told apart by reading a record — and the index says when that is necessary. | `HoldTable`, `Candidates` |
| **ambiguity bit** | a slot saying its fingerprint is shared with another live key in the same bucket. Set when the second of the pair is inserted, which is the one moment it can be noticed for free. | `insert_new`, `HoldTable::ambiguous` |
| **cascade cap** | the most relocations one insert may make. A hop is a random read and an insert is on the apply path, so this is a latency budget rather than a dial. | `MAX_HOPS` |
| **declared maximum** | the live holds the configuration says the worst case reaches: arrivals x worst-case survivor fraction x retention. The index is sized from it and never grows, so passing it is a hold that cannot be stored. | `LOAD_TARGET`, `DEFAULT_SLOTS`, `PendingCapacity::declared_maximum` |
| **not stored** | a committed hold the index could not take. The log says it exists, so the pending column it reserved can never come back down — this node's state has stopped following the log, and the apply path seals. | `NotStored`, `PendingNotice::HoldNotStored` |
| **segment** | a day of records, and the unit space is reclaimed in. Its number is the day modulo the segments an address has room for, so the day itself needs no storage. | `RecordAddr::segment`, `RecordLog::open_day`, `SEGMENTS` |
| **retention** | how long a hold's record is kept: a promise to the customer, so expiry rounds late and never early. | `PendingCapacity::retention_days` |
| **grace** | days of slack added before deletion. The one number covering every source of *early* deletion — a segment's coarseness, a clock jumping forward, a sweep behind — priced in days of capacity. | `PendingCapacity::grace_days`, `lifetime_days` |
| **expiry** | releasing what is left of a hold whose retention ran out. The engine proposes it and the sequencer judges it, because a client's resolution may be in flight for the same hold. | `PendingNotice::HoldExpired`, `PendingEngine::expiring`, `Reactor::admit_expiry` |
| **reclaim** | hand back the blocks of a segment the index has no entry in. No clock, no cursor, no leadership, and no notion of retention: a segment with no entries holds only dead records. Every node does it for itself. | `PendingEngine::reclaim` |
| **propose expiry** | walk an expired day's blocks and offer an **expiry void** for each hold still alive. Needs the leader's clock, because which day has run out is a judgment. | `PendingEngine::propose_expiry` |
| **throttle** | how expiry stays behind live traffic: a bounded number of the expiring day's **blocks** per round, nothing more offered while the last slice is unanswered, and no second walk of the day until it has emptied a little. Blocks, because a bound on the voids collected is no bound on the work done to collect them. It refuses nothing and tells nobody, which is what separates it from a **rate limiter** — that word belongs to the client edge, where a refusal has someone to hear it and becomes an error to retry. Falling behind deletes late, which is the safe direction. | `expiry_blocks_per_round`, `PendingWorker::sweep_expiry`, `Sweep::waiting_at` |
| **ledger-origin id** | a transaction the ledger made up rather than a client: the top bit of the id. Derived from the hold it resolves, so two leaders propose the same one — which is why clients are refused the bit, and why no stage needs a flag saying whose work a request was. | `TxId::ledger_resolution_of`, `is_ledger_origin`, `LedgerError::ReservedTransactionId` |
| **record** | what a hold is, on a block: its key and the hold, packed and little-endian. | `encode`, `decode`, `RECORD_BYTES` |
| **block** | what one read fetches, and so the unit the speed contract is written against. Written once, never rewritten. Its buffer is aligned to its own size, which is what direct IO requires of the address as well as the offset. | `Block`, `BLOCK_BYTES`, `BlockStore` |
| **address** | segment, block, and which record of the block, in the bits an index slot has spare. It names a **record**, not a block — the third field is what makes it one — so the type is `RecordAddr`; it was `BlockAddr`, and the two places that wanted a block alone said so by zeroing that field. | `RecordAddr` |
| **location token** | where the engine last said a hold's record was, carried back with the decision so applying it needs no record read. Opaque, leader-local, and always safe to be stale — a token that matches no slot falls back to the probe. | `HoldLocation`, `PendingEffect::Remove::location` |
| **record log** | the block being filled and the sealed ones behind it. Append-only: a changed remainder is a new record, not an edited one. | `RecordLog` |
| **durable store** | what answers for blocks: bytes at a segment and an offset, a segment brought into being by its first block and removed whole, and a read that can fail. Named for its purpose rather than its unit — durable space is the point and memory is the stand-in — which is why the trait says `Durable` and the implementation says `Memory`, the same way `PendingPort` and `MemoryPending` do. It was `BlockStore`. | `DurableStore`, `MemoryStore`, `LatencyStore`, `StoreFault` |
| **block checksum** | a CRC32C over a block's records, in the sixteen bytes fifty-one eighty-byte records leave spare — so integrity is free in space. Stamped when a block is sealed, which is the one moment its bytes stop changing, and verified by every path that reads one back. It exists to catch the device that *answers* wrongly rather than refusing, which no accounting identity can: a corrupted remainder moves both sides of the ledger by the same wrong amount. | `Block::stamp`, `Block::intact`, `LogTraffic::store_corruptions` |
| **file store** | the durable store backed by one file per segment in one directory: `seg-NN.blk`, created by its first block, `pwrite`/`pread` at derived offsets, `fsync` on the file then the directory, `unlink` to free. No `unsafe` — `std`'s unix extensions have a safe form for all of it and only the *value* of `O_DIRECT` comes from `libc`. Its `submit`/`poll` reads synchronously, which is `SE-OQ-4`'s baseline rather than its answer. | `FileStore`, `OpenBacking::Files`, `--store-dir` |
| **read pool** | N threads issuing the store's `pread`s, one SPSC pair each, so the engine keeps working while a read is outstanding. The portable third of the design's read backends, inside the backing because io_uring — the mainline — owns the descriptors and could not be a decorator. Zero is the synchronous baseline and the default — not because a pool is worthless, but because the right count follows from the cores an assembly has spare, which no constant knows. | `ReadPool`, `--store-read-threads` |
| **store fault** | the store refusing a call. Two kinds and one reaction: `Missing` is this node's own record of where blocks are disagreeing with the store, `Device` is an `EIO` or an `ENOSPC` — either way a record the log says exists cannot be read, so the apply path seals. Counted apart from **not stored** because one cause is a table sized too small and the other a device. | `StoreFault`, `PendingNotice::StoreFailed`, `Metrics::store_failures` |
| **written** / **durable** | two events, not one. A block handed to the store is written; it is durable once a `sync` has covered it, and only then would a crash still find it. What a sync covered is remembered above the seam, because on a filesystem durability is a fact about the store at a moment rather than a watermark per file. **Coverage** stops at the oldest block that is not durable. | `DurableStore::sync`, `RecordLog::durable_through`, `RecordLog::is_durable` |
| **writeback buffer** | the recent blocks not written to the store yet. A record resolved while its block is here never reaches the store. Its length is the **flush window**, which bounds recovery. | `RecordLog`, `flush_blocks` |
| **residency** | blocks already written to the store and kept in memory anyway, so a resolution inside the window costs no IO. Independent of the flush window in both directions: a day long where flushing is an hour, and holding only survivors, because what is resident has been compacted. | `RecordLog::resident`, `resident_blocks` |
| **window** | how many blocks the buffer holds before its oldest is compacted. A count, not a duration — the engine has no clock. | `DEFAULT_WINDOW_BLOCKS` |
| **compaction** | carrying a block's survivors on and dropping the rest. A record is alive exactly when the index points at it. | `PendingEngine::compact`, `HoldTable::points_at` |
| **orderer** | the engine's side of contract 1: replies leave in the order their commands arrived, however they finished. | `Orderer`, `OrderWait` |
| **place** | a reply's spot in its lane, reserved when the command is dequeued and filled when the work finishes. | `Orderer::expect`, `Orderer::fill` |
