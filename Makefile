.PHONY: help build test fmt clippy docker run-detect run-reachability clean

CARGO ?= cargo
DOCKER ?= docker
IMAGE_TAG ?= noetl/doctor:dev

help:
	@echo "Common targets:"
	@echo "  build              cargo build --release"
	@echo "  test               cargo test"
	@echo "  fmt                cargo fmt --check"
	@echo "  clippy             cargo clippy --all-targets -- -D warnings"
	@echo "  docker             docker build -t $(IMAGE_TAG) ."
	@echo "  run-detect         cargo run -- detect"
	@echo "  run-reachability   cargo run -- reachability"
	@echo "  clean              cargo clean + rm -rf target"

build:
	$(CARGO) build --release

test:
	$(CARGO) test --all-targets

fmt:
	$(CARGO) fmt --all -- --check

clippy:
	$(CARGO) clippy --all-targets -- -D warnings

docker:
	$(DOCKER) build -t $(IMAGE_TAG) .

run-detect:
	$(CARGO) run -- detect

run-reachability:
	$(CARGO) run -- reachability

clean:
	$(CARGO) clean
	rm -rf target
