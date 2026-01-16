.PHONY: build test miri clean clean-cargo clean-docker clean-perf cov bench \
        docker-bench-build docker-bench local-bench release perf \
        local-perf docker-perf-build docker-perf \
        flamegraph flamegraphs

# =============================================================================
# Build & Test
# =============================================================================

all: build

build:
	cargo build

release:
	cargo build --release --features c_api

perf:
	RUSTFLAGS="-C force-frame-pointers=yes" cargo build --profile perf --features "c_api,dynamic"

test:
	cargo test

miri:
	MIRIFLAGS="-Zmiri-tree-borrows" cargo +nightly miri test

cov:
	cargo llvm-cov --html --open --branch

clean: clean-cargo clean-docker clean-perf

clean-cargo:
	cargo clean

clean-docker:
	-docker rmi $(DOCKER_IMAGE) $(DOCKER_IMAGE_PERF) 2>/dev/null || true

clean-perf:
	rm -rf perf-data-local perf-data-docker

# =============================================================================
# Local Benchmarks (Criterion)
# =============================================================================

bench:
ifndef BENCH
	$(error BENCH is required. Usage: make bench BENCH=false_sharing)
endif
	cargo bench --bench $(BENCH) --features bench

bench-html: bench
	xdg-open target/criterion/report/index.html

# =============================================================================
# Local Benchmarks (mimalloc-bench with LD_PRELOAD)
# =============================================================================

local-bench:
	cargo build --release --features "c_api,dynamic"
	./scripts/local_bench.sh $(PROCS)

local-perf: perf
	./scripts/local_perf.sh $(PROCS)

# Flamegraphs (requires: cargo install inferno)
flamegraph:
ifndef PERF
	$(error Usage: make flamegraph PERF=perf-data-local/larsonN.perf.data)
endif
	perf script -i $(PERF) | inferno-collapse-perf | inferno-flamegraph > $(PERF:.perf.data=.svg)
	@echo "Created: $(PERF:.perf.data=.svg)"

flamegraphs:
	@echo "Generating flamegraphs..."
	@find perf-data-local perf-data-docker -name '*.perf.data' 2>/dev/null | while read f; do \
		echo "  $$f -> $${f%.perf.data}.svg"; \
		perf script -i "$$f" | inferno-collapse-perf | inferno-flamegraph > "$${f%.perf.data}.svg" 2>/dev/null; \
	done || echo "No perf data files found in perf-data-local/ or perf-data-docker/"

# =============================================================================
# Docker Benchmarks (mimalloc-bench with static inictus)
# =============================================================================

DOCKER_IMAGE := inictus-bench
DOCKER_IMAGE_PERF := inictus-perf
# or $(shell nproc)
PROCS ?= 8 
PERF_OUTPUT_DOCKER ?= ./perf-data-docker

docker-bench-build:
	docker build -f scripts/Dockerfile.mimalloc-bench --build-arg PROCS=$(PROCS) -t $(DOCKER_IMAGE) --target runtime .

docker-bench: docker-bench-build
	docker run --rm -t --cpus=$(PROCS) $(DOCKER_IMAGE) $(PROCS)

docker-perf-build:
	docker build -f scripts/Dockerfile.mimalloc-bench --build-arg PROCS=$(PROCS) -t $(DOCKER_IMAGE_PERF) --target runtime-perf .

docker-perf: docker-perf-build
	@mkdir -p $(PERF_OUTPUT_DOCKER)
	@mkdir -p ./target/perf/docker
	docker run --rm -t --cpus=$(PROCS) \
		--privileged \
		-v $(PWD)/$(PERF_OUTPUT_DOCKER):/perf-output \
		-v $(PWD)/target/perf/docker:/binaries-output \
		$(DOCKER_IMAGE_PERF) $(PROCS)
	@echo ""
	@echo "Perf data: $(PERF_OUTPUT_DOCKER)/"
	@echo "Binaries:  target/perf/docker/"
