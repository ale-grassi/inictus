use criterion::{Criterion, Throughput, black_box, criterion_group, criterion_main};

#[inline(always)]
fn cpu_id_rdtscp() -> usize {
  let cpu: u32;
  unsafe {
    std::arch::asm!("rdtscp", out("ecx") cpu, out("eax") _, out("edx") _, options(nostack, nomem));
  }
  (cpu & 0xFF) as usize
}

#[inline(always)]
fn cpu_id_rdpid() -> usize {
  let cpu: u64;
  unsafe {
    std::arch::asm!(
        "rdpid {}",
        out(reg) cpu,
        options(nomem, nostack, preserves_flags)
    );
  }
  (cpu & 0xFFF) as usize
}

#[inline(always)]
fn cpu_id_sched() -> usize {
  unsafe { libc::sched_getcpu() as usize }
}

fn bench_cpu_id(c: &mut Criterion) {
  let mut group = c.benchmark_group("cpu_id");
  group.throughput(Throughput::Elements(1));
  group.bench_function("rdtscp", |b| b.iter(|| black_box(cpu_id_rdtscp())));
  group.bench_function("rdpid", |b| b.iter(|| black_box(cpu_id_rdpid())));
  group.bench_function("sched_getcpu", |b| b.iter(|| black_box(cpu_id_sched())));
  group.finish();
}

fn bench_cpu_id_batch(c: &mut Criterion) {
  let mut group = c.benchmark_group("cpu_id_batch");

  const ITERATIONS: u64 = 1000;
  group.throughput(Throughput::Elements(ITERATIONS));

  group.bench_function("rdtscp_x1000", |b| {
    b.iter(|| {
      let mut sum = 0usize;
      for _ in 0..ITERATIONS {
        sum = sum.wrapping_add(cpu_id_rdtscp());
      }
      black_box(sum)
    })
  });

  group.bench_function("rdpid_x1000", |b| {
    b.iter(|| {
      let mut sum = 0usize;
      for _ in 0..ITERATIONS {
        sum = sum.wrapping_add(cpu_id_rdpid());
      }
      black_box(sum)
    })
  });

  group.bench_function("sched_getcpu_x1000", |b| {
    b.iter(|| {
      let mut sum = 0usize;
      for _ in 0..ITERATIONS {
        sum = sum.wrapping_add(cpu_id_sched());
      }
      black_box(sum)
    })
  });

  group.finish();
}

criterion_group!(benches, bench_cpu_id, bench_cpu_id_batch);
criterion_main!(benches);
