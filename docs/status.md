# Status

What is built and verified, and what is not. Written to be read before picking up work: the design
reasoning lives in `design-notes.md`, the terms in `glossary.md`, and how to run things in `tools.md`.

Verified at the time of writing with `cargo test` (debug, 103 tests), `cargo build --release
--workspace --all-targets`, all six `ledgerfio` workloads, all three `ledgersim` modes, `ledgerfio
layout`, `cargo bench -p ledger-pending`, and `ledgerd` starting and draining on SIGTERM.

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

### Waiting on one thing: the engine cannot speak first

Every port method is call-and-reply. Two things need the engine to start a conversation, and neither
can be built without it:

1. **Sealing on index overflow.** An insert the index cannot take is a hold the log says exists and
   the store does not have. It is counted, reported, and fails a run in both tools — but the node
   does not seal, because a write carries no reply to bring the news back. Rule 19 says detect and
   stop; this detects and reports.
2. **Expiry.** The timing wheel and the auto-void the engine proposes to the sequencer. Nothing of
   it exists.

This is the largest structural addition left: a variant on `poll()` or a separate low-priority
channel.

### Retention is not real

`segment` never advances and no block is ever freed, so the 32-day window exists only as a number
the index is sized from. Segment expiry brings with it the split the engine's design asks for
between "resolved or expired" and "never existed" — today there is one negative answer.

### Recovery is not real

There is no checkpoint. The flush window is an hour *because* unflushed records are memory-only and
have to be in one, so the reason for that number cannot be verified yet. What is verified is that
the split exists and that residency keeps IO off resolutions inside it.

### Smaller, and each with a reason

- **The load-factor alarm is reporting only.** The index reports its load against the target and
  its worst cascade, but nothing warns or refuses before an insert fails.
- **The in-chain fallback read is unmeasured.** No workload creates a hold and partially settles it
  inside one chain, so the cost of that path is covered by a unit test and nothing else.
- **`OrderWait` exists twice under one name** — `pending/src/orderer.rs` for the real orderer and
  `ledgersim/src/fakes.rs` for the stub's. Two orderers, one name, which rule 3 forbids.
- **`idempotency` lane-orders even unordered replies**, which is order-wait bought for nothing.
- **The simulator never resolves on an unconstrained account**, so `check` does not exercise the
  order exemption itself — only the data check that covers it.
- **Consensus is an echo**, the dedup map has no expiry, and the log has no compaction. All three
  grow with a run, which the tools say out loud.

## Where the numbers live

No measured number is written into these documents as a fact to be trusted; each one names the
command that reproduces it. The three that decide the most:

- `ledgerfio run --workload partial-settle --duration 10s` — where reads land across the two
  windows, and what compaction saves.
- `ledgerfio run --workload hold-settle --resolve-after 900000 --external-ratio 30` — what order
  exemption is worth, as lane depth.
- `ledgersim check --seeds 64` — every invariant under fault injection, including the store path.
