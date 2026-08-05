.PHONY: test-all fio fio-hold sim sim-check help

help:
	@echo "Available targets:"
	@echo "  make test-all     - cargo test --workspace --quiet"
	@echo "  make fio ARGS='...' - cargo run --release -p ledgerfio -- <args>"
	@echo "  make sim ARGS='...' - cargo run --release -p ledgersim -- <args>"
	@echo "  make fio-hold      - cargo run --release -p ledgerfio -- run --workload hold-settle --duration 3s --accounts 100000"
	@echo "  make sim-check     - cargo run --release -p ledgersim -- check --seeds 16"

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
