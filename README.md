# Inictus

A memory allocator for Linux.

Made for educational purpose, not production ready. 

> Work in progress: Miri compliance, debug assertions, and unit tests are still being developed.

## Features

- **Self-contained** — single ~1500 line file, easy to understand and modify
- **Thread-local allocation** — per-thread heaps with no synchronization on the hot path
- **4-tier span caching** — TLS hot block → local span cache → global cache → reuse cache → buddy
- **40 size classes** — 16B to 32KB with low internal fragmentation
- **Reuse cache** — spans freed remotely are recycled without buddy allocator overhead
- **C API compatibility** — drop-in replacement via `LD_PRELOAD`

## Architecture

### Core Components

| Component | Description |
|-----------|-------------|
| **Arena** | Global memory region (1GB virtual address space) |
| **Buddy** | Power-of-two span allocator (15 orders, 64KB - 1GB) |
| **GlobalCache** | CPU-sharded lock-free cache for fresh spans |
| **ReuseCache** | CPU-sharded lock-free cache for spans with remote-freed blocks |
| **ThreadHeap** | Per-thread allocator state (no synchronization) |
| **Span** | 64KB allocation unit with embedded metadata |

### Memory Hierarchy

```
Arena (Global, 1GB VM)
├── Buddy Allocator (15 orders: 64KB to 1GB)
├── Global Span Cache (8 CPU-sharded slots, lock-free Treiber stacks)
└── Reuse Cache (8 CPU-sharded slots, 4 spans/shard/class limit)
         │
         ▼
ThreadHeap (per-thread, no synchronization)
├── spans[40]        ─ Current active span per class
└── cache[40][2]     ─ Local retired spans for reuse
         │
         ▼
Span (64KB, 65536-byte aligned)
├── Header (128B): metadata, free lists, ownership
├── hot_block        ─ MRU: most recently freed block
├── local_free       ─ Local free list (owner thread)
├── remote_free      ─ Remote free list (other threads, atomic)
└── Payload: blocks of uniform size (16B - 32KB)
```

## Allocation Strategy

### Hot Path (most allocations)

1. Check TLS `hot_block` — single pointer swap, no atomics
2. Try span's local free list — linked list pop
3. Reclaim remote free list — atomic swap, rare
4. Bump allocate — increment pointer

### Cold Path (span exhausted)

1. Pop from thread-local cache — no atomics
2. Pop from global cache — lock-free CAS, CPU-sharded
3. Pop from reuse cache — span with remote-freed blocks, reinitialize
4. Allocate from buddy — fresh span

## Benchmarks

Benchmark comparison using [mimalloc-bench](https://github.com/daanx/mimalloc-bench) suite (Docker, static linking). Results vary ~5-10% between runs. Thread count is controlled via `make docker-bench PROCS=N`.

### 1 Thread

| Test | glibc | inictus | Result |
|------|-------|---------|--------|
| glibc-simple | 2.36s (1.5MB) | 1.83s (2.3MB) | **1.28x** ✓ |
| cfrac | 3.50s (2.8MB) | 3.39s (3.0MB) | **1.03x** ✓ |
| espresso | 3.64s (2.1MB) | 3.41s (3.5MB) | **1.06x** ✓ |
| barnes | 2.63s (57.1MB) | 2.61s (58.4MB) | ~1.0x |
| glibc-thread | 2.00s (1.6MB) | 2.00s (2.2MB) | ~1.0x |
| larsonN | 7.00s (16.1MB) | 7.00s (10.2MB) | ~1.0x |
| larsonN-sized | 7.00s (16.2MB) | 7.00s (10.3MB) | ~1.0x |
| mstressN | 0.02s (4.4MB) | 0.02s (6.0MB) | ~1.0x |
| rptestN | 16.00s (4.9MB) | 16.01s (94.0MB) | ~1.0x |
| xmalloc-testN | 5.00s (2.9MB) | 5.01s (31.9MB) | ~1.0x |
| cache-scratch1 | 0.92s (3.8MB) | 0.94s (4.2MB) | 0.97x |
| alloc-test1 | 3.46s (13.6MB) | 3.47s (13.3MB) | ~1.0x |
| sh6benchN | 2.98s (409.9MB) | 2.15s (412.0MB) | **1.38x** ✓ |
| sh8benchN | 12.09s (172.0MB) | 5.99s (118.0MB) | **2.01x** ✓ |
| malloc-large | 3.46s (521.4MB) | 2.83s (628.3MB) | **1.22x** ✓ |

### 4 Threads

| Test | glibc | inictus | Result |
|------|-------|---------|--------|
| glibc-simple | 2.33s (1.5MB) | 1.78s (2.2MB) | **1.30x** ✓ |
| cfrac | 3.44s (2.7MB) | 3.39s (2.6MB) | ~1.0x |
| espresso | 3.66s (2.2MB) | 3.41s (3.3MB) | **1.07x** ✓ |
| barnes | 2.68s (57.0MB) | 2.59s (57.4MB) | **1.03x** ✓ |
| glibc-thread | 2.00s (2.2MB) | 2.00s (4.3MB) | ~1.0x |
| larsonN | 7.01s (39.1MB) | 7.01s (25.2MB) | ~1.0x |
| larsonN-sized | 7.01s (37.8MB) | 7.01s (25.5MB) | ~1.0x |
| mstressN | 0.26s (79.4MB) | 0.20s (51.0MB) | **1.30x** ✓ |
| rptestN | 16.00s (15.9MB) | 16.03s (499.4MB) | ~1.0x |
| xmalloc-testN | 5.02s (44.8MB) | 5.00s (39.4MB) | ~1.0x |
| cache-scratch1 | 0.95s (3.7MB) | 0.92s (6.2MB) | **1.03x** ✓ |
| alloc-test1 | 3.59s (13.7MB) | 3.58s (15.1MB) | ~1.0x |
| sh6benchN | 1.29s (410.3MB) | 0.59s (330.0MB) | **2.18x** ✓ |
| sh8benchN | 4.10s (159.7MB) | 1.87s (120.0MB) | **2.19x** ✓ |
| malloc-large | 3.65s (521.5MB) | 2.87s (628.8MB) | **1.27x** ✓ |

### 8 Threads

| Test | glibc | inictus | Result |
|------|-------|---------|--------|
| glibc-simple | 2.78s (1.5MB) | 1.72s (2.1MB) | **1.61x** ✓ |
| cfrac | 3.93s (2.7MB) | 3.42s (4.8MB) | **1.14x** ✓ |
| espresso | 4.36s (2.0MB) | 3.38s (3.2MB) | **1.28x** ✓ |
| barnes | 2.96s (60.9MB) | 2.61s (57.2MB) | **1.13x** ✓ |
| glibc-thread | 2.00s (2.7MB) | 2.00s (4.0MB) | ~1.0x |
| larsonN | 7.03s (74.8MB) | 7.03s (38.7MB) | ~1.0x |
| larsonN-sized | 7.03s (71.8MB) | 7.03s (39.4MB) | ~1.0x |
| mstressN | 0.78s (228.4MB) | 0.52s (162.6MB) | **1.50x** ✓ |
| rptestN | 16.00s (27.2MB) | 16.08s (913.6MB) | ~1.0x |
| xmalloc-testN | 5.01s (59.7MB) | 5.01s (38.3MB) | ~1.0x |
| cache-scratch1 | 1.03s (3.9MB) | 0.93s (3.9MB) | **1.10x** ✓ |
| alloc-test1 | 3.42s (13.9MB) | 3.54s (17.4MB) | 0.96x |
| sh6benchN | 0.96s (412.3MB) | 0.37s (315.6MB) | **2.59x** ✓ |
| sh8benchN | 3.14s (160.7MB) | 1.32s (123.1MB) | **2.37x** ✓ |
| malloc-large | 3.30s (521.5MB) | 2.89s (628.5MB) | **1.14x** ✓ |

### 16 Threads

| Test | glibc | inictus | Result |
|------|-------|---------|--------|
| glibc-simple | 2.22s (1.5MB) | 1.80s (3.4MB) | **1.23x** ✓ |
| cfrac | 3.58s (2.7MB) | 3.38s (4.9MB) | **1.05x** ✓ |
| espresso | 3.75s (2.2MB) | 3.36s (4.5MB) | **1.11x** ✓ |
| barnes | 2.82s (56.9MB) | 2.57s (58.6MB) | **1.09x** ✓ |
| glibc-thread | 2.00s (3.9MB) | 2.00s (7.5MB) | ~1.0x |
| larsonN | 7.09s (133.0MB) | 7.07s (69.1MB) | ~1.0x |
| larsonN-sized | 7.09s (120.9MB) | 7.07s (69.9MB) | ~1.0x |
| mstressN | 1.44s (482.9MB) | 0.91s (348.5MB) | **1.58x** ✓ |
| rptestN | 16.00s (47.0MB) | 16.13s (1509.6MB) | ~1.0x |
| xmalloc-testN | 5.02s (73.0MB) | 5.01s (43.1MB) | ~1.0x |
| cache-scratch1 | 0.93s (3.7MB) | 0.94s (16.1MB) | ~1.0x |
| alloc-test1 | 3.31s (13.8MB) | 3.60s (24.0MB) | 0.91x |
| sh6benchN | 0.78s (414.6MB) | 0.40s (317.2MB) | **1.95x** ✓ |
| sh8benchN | 2.49s (160.5MB) | 1.18s (120.3MB) | **2.11x** ✓ |
| malloc-large | 3.64s (521.2MB) | 2.85s (629.0MB) | **1.27x** ✓ |

### Scalability Notes

- **1-thread tests** (`cfrac`, `espresso`, `alloc-test1`, `cache-scratch1`, `malloc-large`, `glibc-simple`): Measure single-threaded allocation throughput and cache locality.
- **N-thread tests** (suffix `N`): Scale with thread count. Multi-threaded benchmarks run with the same thread count as `PROCS`.
- **Cross-thread tests** (`larsonN`, `sh8benchN`, `xmalloc-testN`, `mstressN`): Stress test allocators where objects are freed by different threads than allocated them. These are particularly challenging for allocators with thread-local caches.
- **Producer/consumer** (`xmalloc-testN`): 100 purely allocating threads + 100 purely deallocating threads, testing asymmetric workloads.
- **LIFO/reverse-order** (`sh6benchN`): Tests allocation order patterns — some objects freed LIFO, others in reverse order.
## Running Benchmarks

### A note on benchmarking

Custom micro-benchmarks (Criterion-based alloc/free loops) were abandoned early on. They are easy to overfit: several allocator designs scored well on synthetic benchmarks but failed completely under real-world workloads. The mimalloc-bench suite, which runs actual programs (espresso, cfrac, barnes) alongside stress tests, is a much more reliable signal. The Criterion benchmarks in `benches/` were only used for testing specific optimizations during development, most of which were discarded or are incompatible with the current architecture.

### Static vs dynamic linking

Dynamic linking via `LD_PRELOAD` adds overhead: the re-entrancy guard in `with_heap`, safe TLS fallbacks (`try_with` instead of `with`), the inability to inline across the library, and `__tls_get_addr` calls for every TLS access (~8% of cycles in profiling). Expect worse performance with `LD_PRELOAD` compared to static linking.

### mimalloc-bench (Docker, recommended)

[mimalloc-bench](https://github.com/daanx/mimalloc-bench) is included as a git submodule. Clone with `--recursive` or run `git submodule update --init` after cloning.

Runs the full suite inside Docker with isolated CPU pinning. Statically links inictus into the benchmark binaries and compares against glibc baselines.

```bash
# Build and run
make docker-bench
```

### mimalloc-bench (local, LD_PRELOAD)

Runs mimalloc-bench locally using `LD_PRELOAD`. Requires [mimalloc-bench](https://github.com/daanx/mimalloc-bench) built in `./mimalloc-bench/`. Results will be slower than static linking due to dynamic dispatch overhead.

```bash
# Build inictus and run
make local-bench
```

### Profiling with perf

Generate `perf.data` files for each benchmark (Docker or local):

```bash
# Docker (recommended)
make docker-perf

# Local
make local-perf
```


## Cargo Features

| Feature | Default | Description |
|---------|---------|-------------|
| `rdpid` | yes | Use RDPID instruction for CPU ID (Intel Skylake+, AMD Zen+), falls back to `sched_getcpu` |
| `c_api` | no | Enable C API (`malloc`, `free`, etc.) |
| `dynamic` | no | Safe TLS handling for `LD_PRELOAD` use (handles exit during TLS destruction) |
| `bench` | no | Benchmarking mode |

```bash
cargo build --release --features "c_api,dynamic"
```

## Usage

### Rust

```rust
use inictus::Allocator;

#[global_allocator]
static ALLOCATOR: Allocator = Allocator;
```

### C (LD_PRELOAD)

Build the shared library with C API and dynamic TLS handling enabled:

```bash
cargo build --release --features "c_api,dynamic"
```

Preload to replace malloc/free:

```bash
LD_PRELOAD=./target/release/libinictus.so ./your_program
```

## Tree Borrows Compliance (WIP)

```bash
MIRIFLAGS="-Zmiri-tree-borrows" cargo +nightly miri test
```

## Requirements

- Linux (uses `mmap`, `madvise`, `sched_getcpu`)
- Rust 1.85+ (edition 2024)

## Acknowledgments

The allocator core (`src/lib.rs`) was written by me. Due to time constraints, scripts, Dockerfiles, and documentation outside of `src/lib.rs` were written with the help of LLMs.