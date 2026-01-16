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

Benchmark comparison using [mimalloc-bench](https://github.com/daanx/mimalloc-bench) suite (8 threads, Docker). Results vary ~10% between runs for both inictus and glibc.

| Test | glibc Time | glibc RSS | inictus Time | inictus RSS | Result |
|------|------------|-----------|--------------|-------------|--------|
| **glibc-simple** | 2.16s | 1.5MB | 1.71s | 2.2MB | **1.26x faster** ✓ |
| **glibc-thread** | 2.00s | 2.7MB | 2.01s | 4.1MB | ~1.0x |
| **cfrac** | 3.37s | 2.6MB | 3.51s | 3.2MB | 0.96x |
| **espresso** | 3.66s | 2.1MB | 3.61s | 3.4MB | ~1.0x |
| **barnes** | 2.66s | 56.6MB | 2.87s | 57.1MB | 0.92x |
| **mstressN** | 0.84s | 256.7MB | 0.73s | 163.7MB | **1.15x faster** ✓ |
| **rptestN** | 16.00s | 23.1MB | 16.08s | 891.3MB | ~1.0x |
| **xmalloc-testN** | 5.01s | 49.7MB | 5.05s | 310.4MB | ~1.0x |
| **alloc-test1** | 3.60s | 13.7MB | 3.44s | 15.5MB | **1.04x faster** ✓ |
| **cache-scratch1** | 0.94s | 3.7MB | 0.98s | 5.9MB | 0.95x |
| **larsonN** | 7.03s | 64.5MB | 7.06s | 363.0MB | ~1.0x |
| **larsonN-sized** | 7.03s | 64.1MB | 7.05s | 363.3MB | ~1.0x |
| **sh6benchN** | 0.95s | 412.0MB | 0.41s | 319.4MB | **2.31x faster** ✓ |
| **sh8benchN** | 2.91s | 158.2MB | 1.48s | 231.2MB | **1.96x faster** ✓ |
| **malloc-large** | 3.66s | 521.2MB | 2.98s | 628.9MB | **1.22x faster** ✓ |

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