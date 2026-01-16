use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;

const OPS: u64 = 100_000;

/// inictus alloc/free throughput.
fn inictus_malloc_free(size: usize) {
  for _ in 0..OPS {
    unsafe {
      let ptr = inictus::ralloc_malloc(size);
      black_box(ptr);
      inictus::ralloc_free(ptr);
    }
  }
}

/// libc alloc/free throughput.
fn libc_malloc_free(size: usize) {
  for _ in 0..OPS {
    unsafe {
      let ptr = libc::malloc(size);
      black_box(ptr);
      libc::free(ptr);
    }
  }
}

fn benchmark_malloc_throughput(c: &mut Criterion) {
  let mut group = c.benchmark_group("malloc_throughput");

  for size in [16, 64, 256, 1024, 4096] {
    group.throughput(Throughput::Elements(OPS));

    group.bench_with_input(BenchmarkId::new("inictus", size), &size, |b, &size| {
      b.iter(|| inictus_malloc_free(size))
    });

    group.bench_with_input(BenchmarkId::new("libc", size), &size, |b, &size| {
      b.iter(|| libc_malloc_free(size))
    });
  }

  group.finish();
}

criterion_group!(benches, benchmark_malloc_throughput);
criterion_main!(benches);
