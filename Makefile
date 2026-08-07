.PHONY: verify test-all fio fio-hold sim sim-check help

help:
	@echo "Available targets:"
	@echo "  make verify       - everything docs/status.md claims was run, in order"
	@echo "  make test-all     - cargo test --workspace --quiet"
	@echo "  make fio ARGS='...' - cargo run --release -p ledgerfio -- <args>"
	@echo "  make sim ARGS='...' - cargo run --release -p ledgersim -- <args>"
	@echo "  make fio-hold      - cargo run --release -p ledgerfio -- run --workload hold-settle --duration 3s --accounts 100000"
	@echo "  make sim-check     - cargo run --release -p ledgersim -- check --seeds 16"

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
# Nothing here gates a latency target, and one reason is worth stating: a long run's tail is the dedup
# stand-in's, not the ledger's — see `status.md`.
verify:
	cargo test --workspace --quiet
	cargo build --release --workspace --all-targets
	cargo run --release -p ledgerfio -- run --sweep workload=all --duration 1s
	cargo run --release -p ledgerfio -- run --workload void-heavy --duration 2s --rate 100k --resolve-after 900000 --expiry-days 6
	blk=$$(mktemp -d); snap=$$(mktemp -d); cargo run --release -p ledgerfio -- run --workload hold-settle --duration 2s --rate 100k --resolve-after 900000 --store-dir $$blk --store-write-lane 1 --snapshot-dir $$snap --snapshot-every 200; status=$$?; rm -rf $$blk $$snap; exit $$status
	one=$$(mktemp -d); cargo run --release -p ledgerfio -- run --workload hold-settle --duration 2s --rate 100k --resolve-after 900000 --store-dir $$one --store-write-lane 1 --snapshot-dir $$one --snapshot-every 200; status=$$?; rm -rf $$one; exit $$status
	cargo run --release -p ledgerfio -- layout
	cargo run --release -p ledgersim -- check --seeds 32
	cargo run --release -p ledgersim -- capacity --duration-ms 200
	cargo run --release -p ledgersim -- require --rate 500000

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
