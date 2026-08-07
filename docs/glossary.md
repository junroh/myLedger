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
| **snapshot** | the pending engine's index written down so recovery replays the log's tail instead of all of it — and so the log can be truncated at all (`truncate_index <= coverage`). Not durability, which the log already has: this state is a deterministic function of the log, so what a snapshot buys is recovery time and the right to throw log away. The format, coverage, replay, the stable read and a destination are built; starting a node from one is not. | `SnapshotWriter`, `SnapshotReader`, `Snapshots`, design notes §15, §19 and §20 |
| **coverage** | the log position everything up to which a snapshot carries: the oldest block no `sync` has covered, minus one. Replay starts *after* it. Not the last effect applied — the writeback buffer holds records from batches after it, whose slots a snapshot deliberately leaves out. | `PendingEngine::coverage`, design notes §15 |
| **hot buffer** | the design's word (§4.3) for one in-memory tier that both answers reads and holds what write-back has not written yet, sized `peak × writeback`. **The code splits it in two** — the two rows below — because the two halves are sized by different questions. Recorded here rather than reconciled, because which is right depends on a survivor share a device has yet to measure | — |
| **writeback buffer** | the first tier: every record appended, **dead and alive**, for the flush window. Nothing here is on a device, so its size bounds **recovery** — what is in it is only in the log — and losing it costs a replay | `RecordLog::buffer`, `PendingCapacity::flush_window_hours` |
| **residency** | the second tier: blocks that **have** been written and are kept in memory anyway, so a resolution costs no IO. Only survivors, because compaction has already run and repacked them — which is what lets this window be a day while the first is an hour. Its size bounds **latency**, and losing it costs nothing: the store has every block in it. **The invariant the read path rests on is the other direction — a block that is *not* here has already been written** — which is why a miss may go straight to the device without asking anything else | `RecordLog::resident`, `PendingCapacity::residency_hours` |
| **flush** | a record reaching the **device**. `flush_window_hours` is how long one may go unwritten, and that is the whole of what the word means here now. What used to be called `flushed` is **carried on** below. | `PendingCapacity::flush_window_hours` |
| **carried on** | a survivor leaving the writeback buffer for the block being packed — **memory, not the device**. It was the `flushed` counter, which is why the load driver printed it under this name to begin with. | `LogTraffic::carried_on` |
| **seal** | a block being **closed**: no more records go into it, so its bytes stop changing and a whole-block checksum becomes possible. Closing and writing are two calls, and residency takes the block at the first of them — see **residency** | `RecordLog::seal_block`, `RecordLog::submit_writes` |
| **drain** | the whole job of emptying the writeback buffer: compaction, then packing the survivors, closing a full block and submitting it. **Compaction is one step of it**, and today one function is both — which is part of what §20 separates. Today the drain is the last thing apply does; §20 moves it to the worker's round on a declared budget | `PendingEngine::compact`, for now |
| **volume** | a disk, as a deployment **declares** it — not as `st_dev` reports it, which is wrong in both directions (two partitions of one NVMe have different ids and one queue; LVM and network volumes have one id across several devices). One `DurableStore` instance per volume, so whatever shares a volume shares its queue, whoever asked for the IO. ⚠ the declaration is not built: what exists is the case that cannot be wrong, the same directory. Design notes §20 | `OpenBacking::same_volume` |
| **compaction** | the survivor test alone: dropping what the index no longer points at, on a block's way out of the writeback buffer. A record is alive exactly when the index points at it, so this reads nothing | inside `PendingEngine::compact` |
| **answer gate** | a test's hold on what a stand-in answers with: nothing leaves until it is let through, and it says how many are waiting. **What replaces a latency in a test that has to see a request still waiting** — a duration is a guess about the scheduler, and a busy machine invalidates it. Permits rather than a switch, because two answers released together are taken in whatever order the tick takes them. One type for both stand-ins that need it, in `stubkit` because that is where machinery a real component brings none of lives. | `AnswerGate`, `MemoryPending::replies`, `EchoRaft::commits` |
| **read cache** | a volume's cold read cache: blocks kept by how recently they were **read**, which is a different set from residency's, kept by how recently they were **written** — a block residency still has never reaches a volume at all. A store of its own, above the device model so a hit costs no modelled device time. What it is for is a burst of reads into one block, which expiry is by construction. | `Cached`, `MemoryPendingConfig::read_cache_blocks` |
| **volume stats** | what one volume did, counted by the volume: reads queued and answered and how deep the queue got, reads done inline, writes and bytes, barriers, removes, renames, refusals by side, and faults. **The only numbers here that answer for a disk rather than for a caller** — every other IO figure is somebody's tally of what it asked for, which cannot say what the disk was doing when two callers shared it. Also the accounting a watchdog needs. | `VolumeStats`, `DurableStore::stats` |
| **queue share** | blocks of a volume's queue a snapshot dump may hold at once. Half, derived from the depth rather than set beside it. Within a round the blocks already ask first, but a slot the dump takes it holds until the device answers — so on a stalled device a chunk a round would grow into the whole queue, and the ledger would wait on a background job. | `MemoryPendingConfig::snapshot_queue_share` |
| **shadow** | the buckets a snapshot in progress holds aside so its read is stable while the engine keeps writing — copy-on-write, and the kick cascade rather than the effects is why it has to exist. Declared in buckets, and a dump that breaches its budget is abandoned. | `HoldTable::begin_snapshot`, `SnapshotPolicy::shadow_budget` |

**One name for the third one, and it took a rename to get there.** The crate, the port and every message
said `Idem`; the type said `Dedup`, so every assembler that constructed it — the load driver, `ledgerd`, the
test harness, two benches, the simulator — said `dedup` too, and so did the prose. Two words for one
component is what rule 3 forbids, and this is what it looks like when the offender is a *type* name: the
minority word spreads to everything that mentions the type. `idem` is the one word now.

**The row above it was the same fault the other way round, and it lasted longer.** This file declared
`checkpoint` the one word and said Raft's `snapshot` was the outsider — while the code, design notes §15 and
`status.md` had all settled on `snapshot`, in every type name and every heading. So the glossary was the
minority, which is the harder case to notice: nothing a compiler sees disagrees, and the one place a reader
goes to settle a name was the place that was wrong. `snapshot` is the word, because it is the one the code
already speaks; `checkpoint` survives in this file only in the sentence you are reading.


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
| **pause cause** | which backlog stopped intake, published for the client's own thread. A refused submission sees a full queue and nothing else, and that symptom is the same whether the store, consensus or the client's own unread acks caused it — so the refusal carries the reason with it. Read after a refusal; `None` beside one means the client outran a sequencer that was still admitting. | `PauseCause`, `PressureView`, `Refused`, `Backpressure::paused_by` |
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
| **expiry decline** | the sequencer telling the engine it would not take an expiry void, named by the hold. **The only answer such a void gets** — no client asked for it, so no ack leaves — and without it the sweep could not tell a void it had been refused from one still travelling, so it retried both every round at a lookup apiece. | `PendingCommand::ExpiryDeclined`, `PendingEngine::expiry_declined` |
| **reconcile** | what a restart does after reading a snapshot: the day ranges and the next block number come from the restored slots and the volume's own lengths, and the days with a file and nothing alive in them are handed back. Part of `restore` rather than a call beside it, because the second half deletes files and doing it before the index exists deletes all of them. | `PendingEngine::restore`, `RecordLog::reconcile` |
| **reclaim** | hand back the blocks of a segment the index has no entry in. No clock, no cursor, no leadership, and no notion of retention: a segment with no entries holds only dead records. Every node does it for itself. | `PendingEngine::reclaim` |
| **propose expiry** | walk an expired day's blocks and offer an **expiry void** for each hold still alive. Needs the leader's clock, because which day has run out is a judgment. | `PendingEngine::propose_expiry` |
| **throttle** | how expiry stays behind live traffic: a bounded number of the expiring day's **blocks** per round, nothing more offered while the last slice is unanswered, and no second walk of the day until it has emptied a little. Blocks, because a bound on the voids collected is no bound on the work done to collect them. It refuses nothing and tells nobody, which is what separates it from a **rate limiter** — that word belongs to the client edge, where a refusal has someone to hear it and becomes an error to retry. Falling behind deletes late, which is the safe direction. | `expiry_blocks_per_round`, `PendingWorker::sweep_expiry`, `Sweep::waiting_at` |
| **ledger-origin id** | a transaction the ledger made up rather than a client: the top bit of the id. Derived from the hold it resolves, so two leaders propose the same one — which is why clients are refused the bit, and why no stage needs a flag saying whose work a request was. | `TxId::ledger_resolution_of`, `is_ledger_origin`, `LedgerError::ReservedTransactionId` |
| **record** | what a hold is, on a block: its key and the hold, packed and little-endian. | `encode`, `decode`, `RECORD_BYTES` |
| **block** | what one read fetches, and so the unit the speed contract is written against. Written once, never rewritten. Its buffer is aligned to its own size, which is what direct IO requires of the address as well as the offset. | `Block`, `BLOCK_BYTES`, `BlockStore` |
| **address** | segment, block, and which record of the block, in the bits an index slot has spare. It names a **record**, not a block — the third field is what makes it one — so the type is `RecordAddr`; it was `BlockAddr`, and the two places that wanted a block alone said so by zeroing that field. | `RecordAddr` |
| **location token** | where the engine last said a hold's record was, carried back with the decision so applying it needs no record read. Opaque, leader-local, and always safe to be stale — a token that matches no slot falls back to the probe. | `HoldLocation`, `PendingEffect::Remove::location` |
| **record log** | the block being filled and the sealed ones behind it. Append-only: a changed remainder is a new record, not an edited one. | `RecordLog` |
| **durable store** | what answers for blocks: bytes at an **object** and an offset, an object brought into being by its first block, renamed, removed whole, and a read that can fail. Everything that *changes* the volume — write, barrier, rename, remove — is submitted on one queue, because the order between them is what the queue is for. One instance per **volume**, and every IO into that disk goes through it — which is what makes it the one place a queue, an in-flight count and (later) a watchdog can live. Named for its purpose rather than its unit — durable space is the point and memory is the stand-in — which is why the trait says `Durable` and the implementation says `Memory`, the same way `PendingPort` and `MemoryPending` do. It was `BlockStore`. | `DurableStore`, `MemoryStore`, `LatencyStore`, `StoreFault` |
| **object** | what a store names: a **segment**'s blocks, or one of the snapshot's two files. One namespace because they share a disk. The day ↔ segment mapping stays above the store, which is the point of the type — while a file was named by `segment: u8` there was nowhere for a snapshot to be written that was not a day. | `ObjectId`, `ObjectId::SNAPSHOT_CURRENT` |
| **io owner** | who a completion belongs to, in the top bits of its handle — the blocks, a snapshot, or the expiry sweep. Callers draw handles from counters of their own and a completion queue is one queue, so the poller has to be told rather than left to infer it from whether it recognises the number. | `IoOwner`, `RecordLog::take_foreign`, `RecordLog::harvest` |
| **block checksum** | a CRC32C over a block's records, in the sixteen bytes fifty-one eighty-byte records leave spare — so integrity is free in space. Stamped when a block is sealed, which is the one moment its bytes stop changing, and verified by every path that reads one back. It exists to catch the device that *answers* wrongly rather than refusing, which no accounting identity can: a corrupted remainder moves both sides of the ledger by the same wrong amount. | `Block::stamp`, `Block::intact`, `LogTraffic::store_corruptions` |
| **file store** | the durable store backed by one file per object in one directory: `seg-NN.blk` for a day and `pending.snapshot`(`.part`) for the snapshot, created by its first block, `pwrite`/`pread` at derived offsets, `fsync` on the file then the directory, `rename` to publish, `unlink` to free. No `unsafe` — `std`'s unix extensions have a safe form for all of it and only the *value* of `O_DIRECT` comes from `libc`. Its `submit`/`poll` reads synchronously, which is `SE-OQ-4`'s baseline rather than its answer. | `FileStore`, `OpenBacking::Files`, `--store-dir` |
| **read pool** | N threads issuing the store's `pread`s, one SPSC pair each, so the engine keeps working while a read is outstanding. The portable third of the design's read backends, inside the backing because io_uring — the mainline — owns the descriptors and could not be a decorator. Zero is the synchronous baseline and the default — not because a pool is worthless, but because the right count follows from the cores an assembly has spare, which no constant knows. Design notes §18. | `ReadPool`, `--store-read-threads` |
| **store fault** | the store refusing a call. Two kinds and one reaction: `Missing` is this node's own record of where blocks are disagreeing with the store, `Device` is an `EIO` or an `ENOSPC` — either way a record the log says exists cannot be read, so the apply path seals. Counted apart from **not stored** because one cause is a table sized too small and the other a device. | `StoreFault`, `PendingNotice::StoreFailed`, `Metrics::store_failures`; design notes §17 |
| **written** / **durable** | two events, not one. A block handed to the store is written; it is durable once a **barrier** has covered it, and only then would a crash still find it. What a barrier covered is remembered above the seam, because on a filesystem durability is a fact about the store at a moment rather than a watermark per file — and because a barrier is submitted and answered later, so only its completion says what is durable. **Coverage** stops at the oldest block that is not. | `DurableStore::submit_barrier`, `RecordLog::collect_writes`, `RecordLog::durable_through`, `RecordLog::is_durable` |
| **writeback buffer** | the recent blocks not written to the store yet. A record resolved while its block is here never reaches the store. Its length is the **flush window**, which bounds recovery. | `RecordLog`, `flush_blocks` |
| **residency** | blocks already written to the store and kept in memory anyway, so a resolution inside the window costs no IO. Independent of the flush window in both directions: a day long where flushing is an hour, and holding only survivors, because what is resident has been compacted. | `RecordLog::resident`, `resident_blocks` |
| **window** | how many blocks the buffer holds before its oldest is compacted. A count, not a duration — the engine has no clock. | `DEFAULT_WINDOW_BLOCKS` |
| **compaction** | carrying a block's survivors on and dropping the rest. A record is alive exactly when the index points at it. | `PendingEngine::compact`, `HoldTable::points_at` |
| **orderer** | the engine's side of contract 1: replies leave in the order their commands arrived, however they finished. | `Orderer`, `OrderWait` |
| **place** | a reply's spot in its lane, reserved when the command is dequeued and filled when the work finishes. | `Orderer::expect`, `Orderer::fill` |

## Sizing

What one structure costs and how many of it there are — two different owners, and naming them apart is
what keeps a sizing answer from being half remembered. `sizing/`, and design notes §10 for why only one
half of it is arithmetic.

| term | means | in code |
|---|---|---|
| **sized part** | one structure a sizing answer prices, named the same by the component that reports it and the crate that declares it. A component's `Footprint` says how many it holds now; its `SIZING` says what one costs. Matching by name is the whole mechanism, so the names are a vocabulary rather than labels. | `SizedPart`, `Footprint::parts`, each crate's `SIZING` |
| **unit** | what the count is counted in, because each reaches bytes by a different route: a **slot** is linear, a **bucket** is a staircase, a **block** is 4KB whether or not its records are live, an **account** is the working set, and an **effect**, **batch** or **entry** is one item in a buffer. | `Unit` |
| **bucket** | one `hashbrown` slot-and-control-byte, and the count is `next_pow2(entries x 8/7)` — so one percent more entries can double a table. Not the cuckoo index's bucket, which is four eight-byte slots and steps four times more coarsely. | `bucket_bytes`, `buckets_for`, `index_slots_for` |
| **derived** | a count that follows from demand — a rate, a lifetime, a retention, a working set. | `Line.kind` |
| **dial** | a count that is a configured ceiling. **An output of a sizing exercise, not an input**: a dial below what demand requires is where the node refuses work. | `Dials` |
| **writeback buffer / resident blocks / stored blocks** | the engine's three words for one set of records, answering three questions: what a restart would replay, what memory keeps so a resolution need not read a device, and what the store holds — the last being the disk figure. | `RecordLog::blocks`, `MemoryPending::footprint` |
| **peak / busiest hour / daily volume** | the three rates a deployment brings, and they are not one rate. The peak sizes what is held in flight, the busiest hour sizes the hour-wide windows, and the day sizes retention. A deployment whose peak is eighty-six times its mean is sized eighty-six times wrong by whichever one it picks. | `Demand` |
| **lifetime curve** | how long holds live, as *(hours, share resolved by then)* rather than as a mean. Read at the flush window it says what reaches the disk, at residency what a resolution costs, and at retention what expiry has to void — three answers that were three separate guesses before, free to disagree. | `Lifetimes`, `Sizing.resolved_by_flush` / `_residency` / `_retention` |
| **written share** | the records that outlive the flush window and so reach the store. Its complement is what compaction drops — the `died in buffer` line, 98% in a measured run with an hour-wide window. **Not the survivor share**, which is a different point on the same curve and larger by the whole width of residency. | `Sizing.written_share`, `LogTraffic::died_in_buffer` |
| **residency, as an answer** | the window is chosen by what it buys: resolutions landing between residency ending and expiry are device reads, so widening it trades blocks for reads. Printed as a curve rather than solved, because what a read is worth is a property of the device and the tail, neither of which the model has. | `residency_curve`, `Sizing.resolution_reads_per_second` |
