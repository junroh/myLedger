.PHONY: verify test-all fio fio-hold sim sim-check sizing sizing-check sizing-units help

help:
	@echo "Available targets:"
	@echo "  make verify       - everything docs/status.md claims was run, in order"
	@echo "  make test-all     - cargo test --workspace --quiet"
	@echo "  make fio ARGS='...' - cargo run --release -p ledgerfio -- <args>"
	@echo "  make sim ARGS='...' - cargo run --release -p ledgersim -- <args>"
	@echo "  make fio-hold      - cargo run --release -p ledgerfio -- run --workload hold-settle --duration 3s --accounts 100000"
	@echo "  make sim-check     - cargo run --release -p ledgersim -- check --seeds 16"
	@echo "  make sizing        - python3 sizing/model.py, the memory and disk answer"
	@echo "  make sizing-units  - refresh sizing/units.json after changing a sized struct"

# What `docs/status.md` says it was verified with, as something that can be run rather than
# remembered. A crash in a workload took two commits to notice because the tools were not part of any
# check: `cargo test` runs for milliseconds and the defect needed a second of release build to reach.
#
# Debug for the tests, because rule 6's self-invariants and every debug_assert are compiled out of a
# release build. Release for the tools, because that is where their numbers come from — and where a
# workload that aborts does so soonest.
#
# `workload=all` comes from the workload kinds themselves, so a seventh is covered by adding it.
#
# The `--expiry-days` run is here because no other line crosses a day: every workload above leaves the
# engine on the wall clock, where a run of seconds never reaches a retention boundary and the sweep, the
# per-day counts and the freeing of a day's blocks are all untouched. Six days against the default lifetime
# of three, so the run reaches the expiry of holds it created itself.
#
# Its rate and its `--resolve-after` are what make it exercise the walk rather than only the freeing. Left
# uncapped, the run resolves a day's holds within a tenth of a second and every day is already empty when it
# expires — the sweep then frees each one without reading a block, and reports nothing. So the rate is held
# down and the holds are given an age longer than the run, which leaves them alive to be found.
#
# The same run again with `--residency 1 --store-read-cache 0`, and it covers two things the line above
# does not reach. **The first is the device at all**: at the default residency every record the sweep's
# judgements want is still in memory, so that run reports `reads 0 queued` and the whole read path below
# the engine is untouched by it. An hour of residency is what pushes the reads down to the volume.
#
# **The second is the cache-off arm.** With the cache on, the sweep's own read fills it before any judgement
# asks and coalescing measures zero — so the only protection the miss path has is, on every line here, code
# that nothing runs. Off, it carries fifty-four thousand reads. The two arms are one saving taken at two
# moments and each hides the other; running only the default arm is running only the half that works.
#
# The one run with directories under it is here for the same reason the `--expiry-days` one is: no other
# line reaches those paths. Real files, the write lane and a snapshot destination are all off unless asked
# for, so without this they would be code that only their own unit tests ever run — and the last thing to
# rot unnoticed here was exactly that, a workload nothing outside `cargo test` exercised.
#
# All three in one run on purpose: what they do together is what a deployment does, and a snapshot's
# coverage moving while the barrier that decides it completes on another thread is the interaction none of
# them tests alone. `--resolve-after` keeps the holds alive so blocks are actually written, and the cadence
# is small so a two-second run writes several snapshots. Both directories go with the run.
#
# Then the same run with **one** directory, which is how a deployment declares that the blocks and the
# snapshot are on one disk: one store, one queue, and two writers interleaved on one write lane at a real
# rate. That last part is the reason it is a second line rather than a replacement — the two-directory run
# is the other shape, and neither covers the other. The unit tests reach the shared store but not the lane.
#
# The two lines also cover the two write arrangements now that the lane is the default: the first asks for
# `--store-write-lane 0`, which is the synchronous baseline every older number was taken against, and the
# second takes the default. A baseline nothing runs is a baseline that rots.
#
# Nothing here gates a latency target, and one reason is worth stating: a long run's tail is the dedup
# stand-in's, not the ledger's — see `status.md`.
verify:
	cargo test --workspace --quiet
	cargo build --release --workspace --all-targets
	cargo run --release -p ledgerfio -- run --sweep workload=all --duration 1s
	cargo run --release -p ledgerfio -- run --workload void-heavy --duration 2s --rate 100k --resolve-after 900000 --expiry-days 6
	cargo run --release -p ledgerfio -- run --workload void-heavy --duration 2s --rate 100k --resolve-after 900000 --expiry-days 6 --residency 1 --store-read-cache 0
	blk=$$(mktemp -d); snap=$$(mktemp -d); cargo run --release -p ledgerfio -- run --workload hold-settle --duration 2s --rate 100k --resolve-after 900000 --store-dir $$blk --store-write-lane 0 --snapshot-dir $$snap --snapshot-every 200; status=$$?; rm -rf $$blk $$snap; exit $$status
	one=$$(mktemp -d); cargo run --release -p ledgerfio -- run --workload hold-settle --duration 2s --rate 100k --resolve-after 900000 --store-dir $$one --store-write-lane 1 --snapshot-dir $$one --snapshot-every 200; status=$$?; rm -rf $$one; exit $$status
	cargo run --release -p ledgerfio -- layout
	cargo run --release -p ledgersim -- check --seeds 32
	cargo run --release -p ledgersim -- capacity --duration-ms 200
	cargo run --release -p ledgersim -- require --rate 500000
	$(MAKE) --no-print-directory sizing-check

# The sizing model may not hard-code a unit cost, so it reads them from a cached dump -- and a cache
# nothing checks is the same stale number one step further away. This regenerates from the build in
# front of it and refuses a difference: change a sized struct and `make verify` says so, the same way
# `layout_claim!` fails a build rather than letting a size drift.
#
# The commit is excluded from the comparison on purpose. It records where the numbers came from, and
# demanding it match HEAD would mean refreshing the file on every unrelated commit -- a check that
# fires for something it is not about is one people learn to re-run past.
sizing-check:
	@cargo build --release -p ledgerfio --quiet
	@./target/release/ledgerfio layout --json | (cd sizing && python3 units.py check)
	@python3 sizing/model.py > /dev/null
	@echo "sizing units match the build"

sizing:
	@python3 sizing/model.py

sizing-units:
	@cargo build --release -p ledgerfio --quiet
	@./target/release/ledgerfio layout --json | (cd sizing && python3 units.py write)
	@echo "sizing/units.json refreshed"

test-all:
	cargo test --workspace --quiet

fio:
	cargo run --release -p ledgerfio -- $(ARGS)

sim:
	cargo run --release -p ledgersim -- $(ARGS)

fio-hold:
	cargo run --release -p ledgerfio -- run --workload hold-settle --duration 3s --accounts 100000

sim-check:
	cargo run --release -p ledgersim -- check --seeds 16
