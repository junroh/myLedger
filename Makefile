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
# Nothing here gates a latency target, and one reason is worth stating: a long run's tail is the dedup
# stand-in's, not the ledger's — see `status.md`.
verify:
	cargo test --workspace --quiet
	cargo build --release --workspace --all-targets
	cargo run --release -p ledgerfio -- run --sweep workload=all --duration 1s
	cargo run --release -p ledgerfio -- run --workload void-heavy --duration 2s --rate 100k --resolve-after 900000 --expiry-days 6
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
