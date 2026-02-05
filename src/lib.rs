#![allow(clippy::missing_safety_doc)]

use core::{
  alloc::{GlobalAlloc, Layout},
  cell::Cell,
  hint,
  mem::size_of,
  ptr::{self, NonNull, null_mut},
  sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering},
};
use std::{cell::UnsafeCell, sync::OnceLock};

// =============================================================================
// Constants
// =============================================================================

const ARENA_SIZE: usize = 1 << 30; // 1GB

const SPAN_SIZE_BITS: usize = 16;
const SPAN_SIZE: usize = 1 << SPAN_SIZE_BITS; // 64KB
const SPAN_ALIGN_MASK: usize = !(SPAN_SIZE - 1);
const SPAN_HEADER_SIZE: usize = size_of::<SpanHeader>();
/// Owner ID for orphaned spans (no owning thread).
const SPAN_OWNER_ORPHAN: u32 = 0;
/// Magic number to identify valid SpanHeaders
const SPAN_MAGIC: u64 = 0x494E_4943_5455_5321; // "INICTUS!"

const SPANS_PER_ARENA: usize = ARENA_SIZE / SPAN_SIZE;

/// Largest allocation, that is equal to the whole arena.
const BUDDY_MAX_ORDER: usize = SPANS_PER_ARENA.trailing_zeros() as usize;

/// Number of linear size classes (16...128).
const CLASSES_LINEAR: usize = 8;
const CLASSES_LINEAR_STEP: usize = 16;

/// Classes per doubling in geometric progression
const CLASSES_PER_DOUBLING: usize = 4;
const CLASSES_MAX_SIZE: usize = (SPAN_SIZE - SPAN_HEADER_SIZE) / 2;

/// Returns the total number of size classes for small allocations. Computed at compile-time.
const fn count_size_classes() -> usize {
  let mut class = 0;
  loop {
    if class_to_size(class) >= CLASSES_MAX_SIZE {
      return class + 1;
    }
    class += 1;
    if class > 64 {
      return class;
    }
  }
}
/// Total number of size classes for small allocations.
const CLASSES_COUNT: usize = count_size_classes();

/// Number of shards for global span caches (reuse + bounded).
/// N shards matches typical CPU count (8 cores) for good cache locality.
const SHARD_COUNT: usize = 8;

/// Cached spans per size class per thread. More = faster, but uses more memory.
const THREAD_LOCAL_CACHE_SIZE: usize = 2;

/// Maximum spans per shard per class in the reuse cache.
const REUSE_CACHE_LIMIT: usize = 4;

/// Maximum total active spans across all threads. Balance between throughput and RSS.
const MAX_GLOBAL_ACTIVE_SPANS: usize = 4096; // 64KB * 4096 = 256MB

/// Global counter of spans currently in use (not in buddy allocator).
/// Incremented when span allocated from buddy, decremented when returned.
static GLOBAL_ACTIVE_SPAN_COUNTER: AtomicUsize = AtomicUsize::new(0);

// =============================================================================
// Compile-Time Assertions
// =============================================================================

const _: () = assert!(ARENA_SIZE.is_power_of_two());
const _: () = assert!(SPAN_SIZE.is_power_of_two());
const _: () = assert!(SPANS_PER_ARENA.is_power_of_two());
const _: () = assert!(ARENA_SIZE % SPAN_SIZE == 0);
const _: () = assert!(class_to_size(CLASSES_COUNT - 1) == CLASSES_MAX_SIZE);
const _: () = assert!(class_to_size(0) == 16);
const _: () = assert!(CLASSES_MAX_SIZE >= 16);
const _: () = assert!(SHARD_COUNT.is_power_of_two());
const _: () = assert!(SPAN_HEADER_SIZE < SPAN_SIZE / 2);
const _: () = assert!(THREAD_LOCAL_CACHE_SIZE >= 1);
const _: () = assert!(core::mem::offset_of!(SpanHeader, remote_free) >= 64);
const _: () = assert!(SPAN_HEADER_SIZE == 128); // 2 cache lines

// =============================================================================
// Types
// =============================================================================

/// `SpanKind` determines allocation strategy.
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SpanKind {
  Small = 0,
  Large = 1,
  Huge = 2,
}

#[repr(C)]
struct FreeBlock {
  next: *mut FreeBlock,
}

/// Span metadata. Sits at offset 0 of each 64KB span.
#[repr(C, align(128))]
struct SpanHeader {
  // === Cache line 0: Owner-thread hot path (no cross-thread writes) ===
  /// Next bump allocation address.
  bump: *mut u8,
  /// End of bump region.
  bump_end: *mut u8,
  /// Most recently freed block (fastest path).
  hot_block: *mut u8,
  /// Free blocks list (owner-thread only).
  local_free: *mut FreeBlock,
  block_size: u32,
  class: u8,
  kind: SpanKind,
  /// Buddy order (0 = 1 span, 1 = 2 spans, ...).
  order: u8,
  /// Padding to 64 bytes (39 bytes used, need 25 more).
  _pad0: [u8; 25],

  // === Cache line 1: Cross-thread contended fields ===
  /// Free blocks from non-owner threads (lock-free Treiber stack).
  remote_free: AtomicPtr<FreeBlock>,
  /// Outstanding allocations (`alloc++`, `free--`).
  used: AtomicU32,
  /// Owning tid. 0 = orphan.
  owner: AtomicU32,
  /// In reuse cache (prevents double-enqueue).
  in_reuse: AtomicBool,
  /// Intrusive list pointer (cache management).
  cache_next: *mut SpanHeader,
  /// Original mmap base (for munmap).
  huge_base: *mut u8,
  /// Total mmap size.
  huge_size: usize,
  /// Magic number for validation.
  magic: u64,
}

// =============================================================================
// Platform
// =============================================================================

unsafe fn os_mmap(size: usize) -> *mut u8 {
  let ptr = unsafe {
    libc::mmap(
      null_mut(),
      size,
      libc::PROT_READ | libc::PROT_WRITE,
      // TODO: Check the performance impact of MAP_NORESERVE
      libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
      -1,
      0,
    )
  };

  if ptr == libc::MAP_FAILED {
    null_mut()
  } else {
    ptr as *mut u8
  }
}

unsafe fn os_munmap(ptr: *mut u8, size: usize) {
  unsafe { libc::munmap(ptr.cast(), size) };
}

// Each thread gets a different ID
fn thread_id_u32() -> u32 {
  thread_local! {
    static TID: u32 = {
      static CTR: AtomicU32 = AtomicU32::new(1); // Start at 1; 0 = SPAN_OWNER_ORPHAN
      CTR.fetch_add(1, Ordering::Relaxed) // We only need uniqueness, not synchronization
    };
  }
  TID.with(|&id| id)
}

// Only supported in the latest x86 CPUs. Seems to be the fastest way to access CPU ID
#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "rdpid"))]
fn cpu_id() -> usize {
  let cpu: u64;
  unsafe {
    // nomem - Doesn't read/write memory
    // nostack - Doesn't touch the stack pointer
    // preserves_flags - Doesn't modify CPU flags (EFLAGS/RFLAGS)
    std::arch::asm!("rdpid {}", out(reg) cpu, options(nomem, nostack, preserves_flags));
  }
  (cpu & 0xFFF) as usize
}

#[cfg(all(target_os = "linux", not(feature = "rdpid")))]
fn cpu_id() -> usize {
  unsafe { libc::sched_getcpu() as usize }
}

#[cfg(not(target_os = "linux"))]
fn cpu_id() -> usize {
  (thread_id_u32() as usize) & 7
}

// =============================================================================
// Thread Heap
// =============================================================================

struct ThreadHeap {
  spans: [*mut SpanHeader; CLASSES_COUNT],
  cache: [[*mut SpanHeader; THREAD_LOCAL_CACHE_SIZE]; CLASSES_COUNT],
  cache_len: [usize; CLASSES_COUNT],
  tid: u32,
  cpu: usize,
}

impl ThreadHeap {
  fn new() -> Self {
    Self {
      spans: [null_mut(); CLASSES_COUNT],
      cache: [[null_mut(); THREAD_LOCAL_CACHE_SIZE]; CLASSES_COUNT],
      cache_len: [0; CLASSES_COUNT],
      tid: thread_id_u32(),
      cpu: cpu_id(),
    }
  }

  fn cache_pop(&mut self, class: usize) -> *mut SpanHeader {
    let len = self.cache_len[class];
    if len > 0 {
      self.cache_len[class] = len - 1;
      self.cache[class][len - 1]
    } else {
      null_mut()
    }
  }

  fn cache_push(&mut self, class: usize, span: *mut SpanHeader) -> bool {
    let len = self.cache_len[class];
    if len < THREAD_LOCAL_CACHE_SIZE {
      self.cache[class][len] = span;
      self.cache_len[class] = len + 1;
      true
    } else {
      false
    }
  }
}

impl Drop for ThreadHeap {
  fn drop(&mut self) {
    let Some(arena) = ARENA.get() else {
      return;
    };

    // Retire active spans.
    for class in 0..CLASSES_COUNT {
      let span = self.spans[class];
      if !span.is_null() {
        unsafe { arena.retire_small_span(self, span) };
      }

      // Flush local cache to global.
      for i in 0..self.cache_len[class] {
        let cached_span = self.cache[class][i];
        if !cached_span.is_null() {
          unsafe {
            // Ensure spans in cache are orphaned/free and not in reuse.
            (*cached_span)
              .owner
              .store(SPAN_OWNER_ORPHAN, Ordering::Release);
            (*cached_span).in_reuse.store(false, Ordering::Release);
            (*cached_span)
              .remote_free
              .store(null_mut(), Ordering::Relaxed);
          }
          arena.global_push(self.cpu, class, cached_span);
        }
      }
    }
  }
}

// =============================================================================
// Buddy Allocator (spinlock-based with list-traversal coalescing)
// =============================================================================

struct SpinLock {
  locked: AtomicBool,
}

impl SpinLock {
  const fn new() -> Self {
    Self {
      locked: AtomicBool::new(false),
    }
  }

  #[inline]
  fn lock(&self) {
    while self
      .locked
      .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
      .is_err()
    {
      while self.locked.load(Ordering::Relaxed) {
        hint::spin_loop();
      }
    }
  }

  #[inline]
  fn unlock(&self) {
    self.locked.store(false, Ordering::Release);
  }
}

/// Intrusive free list for buddy allocator.
struct FreeList {
  head: *mut SpanHeader,
  count: usize,
}

impl FreeList {
  const fn new() -> Self {
    Self {
      head: null_mut(),
      count: 0,
    }
  }
}

/// Lock-protected free list per buddy order.
struct LockedFreeList {
  lock: SpinLock,
  list: UnsafeCell<FreeList>,
}

impl LockedFreeList {
  const fn new() -> Self {
    Self {
      lock: SpinLock::new(),
      list: UnsafeCell::new(FreeList::new()),
    }
  }
}

struct Buddy {
  /// Free lists per order, each with its own lock.
  orders: [LockedFreeList; BUDDY_MAX_ORDER + 1],
}

unsafe impl Sync for Buddy {}

impl Buddy {
  const fn new() -> Self {
    const LOCKED_LIST: LockedFreeList = LockedFreeList::new();
    Self {
      orders: [LOCKED_LIST; BUDDY_MAX_ORDER + 1],
    }
  }

  // Initialize `Buddy`. Should be called only once.
  fn init(&self, base: *mut u8) {
    // Push entire arena as one MAX_ORDER span
    self.orders[BUDDY_MAX_ORDER].lock.lock();
    unsafe {
      let list = &mut *self.orders[BUDDY_MAX_ORDER].list.get();
      let span = base as *mut SpanHeader;
      (*span).cache_next = null_mut();
      list.head = span;
      list.count = 1;
    }
    self.orders[BUDDY_MAX_ORDER].lock.unlock();
  }

  /// Push span to free list at given order (caller must hold lock).
  #[inline]
  unsafe fn push_locked(&self, arena: &Arena, idx: usize, order: usize) {
    let list = unsafe { &mut *self.orders[order].list.get() };
    let span = arena.idx_to_span(idx);
    unsafe { (*span).cache_next = list.head };
    list.head = span;
    list.count += 1;
  }

  /// Pop span from free list at given order (caller must hold lock).
  /// Returns span index or None if list is empty.
  #[inline]
  unsafe fn pop_locked(&self, arena: &Arena, order: usize) -> Option<usize> {
    let list = unsafe { &mut *self.orders[order].list.get() };
    let span = list.head;
    if span.is_null() {
      return None;
    }
    list.head = unsafe { (*span).cache_next };
    list.count -= 1;
    Some(arena.span_to_idx(span))
  }

  /// Try to remove buddy span from free list (caller must hold lock).
  /// Returns true if buddy was found and removed.
  #[inline]
  unsafe fn try_remove_buddy(&self, arena: &Arena, buddy_idx: usize, order: usize) -> bool {
    let list = unsafe { &mut *self.orders[order].list.get() };
    let buddy_span = arena.idx_to_span(buddy_idx);

    if list.head == buddy_span {
      list.head = unsafe { (*buddy_span).cache_next };
      list.count -= 1;
      return true;
    }

    // Walk the list to find buddy
    let mut prev = list.head;
    while !prev.is_null() {
      let next = unsafe { (*prev).cache_next };
      if next == buddy_span {
        unsafe { (*prev).cache_next = (*buddy_span).cache_next };
        list.count -= 1;
        return true;
      }
      prev = next;
    }

    false
  }

  /// Allocate span of given order, splitting larger spans if needed.
  fn alloc(&self, arena: &Arena, order: usize) -> Option<usize> {
    self.orders[order].lock.lock();
    let result = unsafe { self.pop_locked(arena, order) };
    self.orders[order].lock.unlock();

    if let Some(idx) = result {
      GLOBAL_ACTIVE_SPAN_COUNTER.fetch_add(1 << order, Ordering::Relaxed);
      return Some(idx);
    }

    for o in (order + 1)..=BUDDY_MAX_ORDER {
      self.orders[o].lock.lock();
      let result = unsafe { self.pop_locked(arena, o) };
      self.orders[o].lock.unlock();

      if let Some(idx) = result {
        for split in (order..o).rev() {
          let buddy_idx = idx + (1 << split);
          self.orders[split].lock.lock();
          unsafe { self.push_locked(arena, buddy_idx, split) };
          self.orders[split].lock.unlock();
        }
        GLOBAL_ACTIVE_SPAN_COUNTER.fetch_add(1 << order, Ordering::Relaxed);
        return Some(idx);
      }
    }

    None
  }

  /// Free span with buddy coalescing.
  fn free(&self, arena: &Arena, mut idx: usize, mut order: usize) {
    GLOBAL_ACTIVE_SPAN_COUNTER.fetch_sub(1 << order, Ordering::Relaxed);

    // Coalesce with buddy, climbing orders
    while order < BUDDY_MAX_ORDER {
      let buddy_idx = idx ^ (1 << order);
      if buddy_idx >= SPANS_PER_ARENA {
        break;
      }

      self.orders[order].lock.lock();
      let removed = unsafe { self.try_remove_buddy(arena, buddy_idx, order) };
      self.orders[order].lock.unlock();

      if removed {
        idx = idx.min(buddy_idx);
        order += 1;
      } else {
        break;
      }
    }

    self.orders[order].lock.lock();
    unsafe { self.push_locked(arena, idx, order) };
    self.orders[order].lock.unlock();
  }
}

// =============================================================================
// Global Cache (per-shard, per-class)
// =============================================================================

struct GlobalCache {
  heads: [[AtomicU64; CLASSES_COUNT]; SHARD_COUNT],
}

impl GlobalCache {
  const fn new() -> Self {
    const ROW: [AtomicU64; CLASSES_COUNT] = [const { AtomicU64::new(0) }; CLASSES_COUNT];
    Self {
      heads: [ROW; SHARD_COUNT],
    }
  }

  fn pop(&self, shard: usize, class: usize) -> *mut SpanHeader {
    let head = &self.heads[shard & (SHARD_COUNT - 1)][class];
    loop {
      let packed_head = head.load(Ordering::Acquire);
      let ptr = (packed_head & !0xFFFF) as *mut SpanHeader;
      if ptr.is_null() {
        return null_mut();
      }
      let next = unsafe { (*ptr).cache_next };
      let new_packed = (next as u64) | (((packed_head as u16).wrapping_add(1)) as u64);
      if head
        .compare_exchange_weak(packed_head, new_packed, Ordering::AcqRel, Ordering::Relaxed)
        .is_ok()
      {
        return ptr;
      }
    }
  }

  fn push(&self, shard: usize, class: usize, span: *mut SpanHeader) {
    let head = &self.heads[shard & (SHARD_COUNT - 1)][class];
    loop {
      let packed_head = head.load(Ordering::Relaxed);
      unsafe { (*span).cache_next = (packed_head & !0xFFFF) as *mut SpanHeader };
      let new_packed = (span as u64) | (((packed_head as u16).wrapping_add(1)) as u64);
      if head
        .compare_exchange_weak(
          packed_head,
          new_packed,
          Ordering::Release,
          Ordering::Relaxed,
        )
        .is_ok()
      {
        return;
      }
    }
  }
}

// =============================================================================
// Reuse Cache (orphan spans with remote frees)
// =============================================================================

struct ReuseCache {
  heads: [[AtomicU64; CLASSES_COUNT]; SHARD_COUNT],
  counts: [[AtomicUsize; CLASSES_COUNT]; SHARD_COUNT],
}

impl ReuseCache {
  const fn new() -> Self {
    const HEADS_ROW: [AtomicU64; CLASSES_COUNT] = [const { AtomicU64::new(0) }; CLASSES_COUNT];
    const COUNTS_ROW: [AtomicUsize; CLASSES_COUNT] = [const { AtomicUsize::new(0) }; CLASSES_COUNT];
    Self {
      heads: [HEADS_ROW; SHARD_COUNT],
      counts: [COUNTS_ROW; SHARD_COUNT],
    }
  }

  fn pop(&self, shard: usize, class: usize) -> *mut SpanHeader {
    let shard_idx = shard & (SHARD_COUNT - 1);
    let head = &self.heads[shard_idx][class];
    loop {
      let packed_head = head.load(Ordering::Acquire);
      let ptr = (packed_head & !0xFFFF) as *mut SpanHeader;
      if ptr.is_null() {
        return null_mut();
      }
      let next = unsafe { (*ptr).cache_next };
      let new_packed = (next as u64) | (((packed_head as u16).wrapping_add(1)) as u64);
      if head
        .compare_exchange_weak(packed_head, new_packed, Ordering::AcqRel, Ordering::Relaxed)
        .is_ok()
      {
        self.counts[shard_idx][class].fetch_sub(1, Ordering::Relaxed);
        return ptr;
      }
    }
  }

  fn push(&self, shard: usize, class: usize, span: *mut SpanHeader) -> bool {
    let shard_idx = shard & (SHARD_COUNT - 1);
    let count = &self.counts[shard_idx][class];

    if count.load(Ordering::Relaxed) >= REUSE_CACHE_LIMIT {
      return false;
    }

    let head = &self.heads[shard_idx][class];
    loop {
      let packed_head = head.load(Ordering::Relaxed);
      unsafe { (*span).cache_next = (packed_head & !0xFFFF) as *mut SpanHeader };
      let new_packed = (span as u64) | (((packed_head as u16).wrapping_add(1)) as u64);
      if head
        .compare_exchange_weak(
          packed_head,
          new_packed,
          Ordering::Release,
          Ordering::Relaxed,
        )
        .is_ok()
      {
        count.fetch_add(1, Ordering::Relaxed);
        return true;
      }
    }
  }
}

// =============================================================================
// Arena
// =============================================================================

struct Arena {
  base: AtomicPtr<u8>,
  buddy: Buddy,
  cache: GlobalCache,
  reuse: ReuseCache,
}

unsafe impl Sync for Arena {}
unsafe impl Send for Arena {}

static ARENA: OnceLock<Arena> = OnceLock::new();

impl Arena {
  const fn new() -> Self {
    Self {
      base: AtomicPtr::new(null_mut()),
      buddy: Buddy::new(),
      cache: GlobalCache::new(),
      reuse: ReuseCache::new(),
    }
  }

  fn get() -> Option<&'static Self> {
    Some(ARENA.get_or_init(|| {
      // Over-allocate for alignment padding.
      let raw = unsafe { os_mmap(ARENA_SIZE + SPAN_SIZE) };
      if raw.is_null() {
        panic!("Arena mmap failed");
      }

      let aligned = align_up(raw as usize, SPAN_SIZE) as *mut u8;

      let arena = Arena::new();
      arena.base.store(aligned, Ordering::Release);
      arena.buddy.init(aligned);
      arena
    }))
  }

  #[inline]
  fn idx_to_span(&self, idx: usize) -> *mut SpanHeader {
    unsafe { self.base.load(Ordering::Relaxed).add(idx << SPAN_SIZE_BITS) as *mut SpanHeader }
  }

  #[inline]
  fn span_to_idx(&self, span: *mut SpanHeader) -> usize {
    (span as usize - self.base.load(Ordering::Relaxed) as usize) >> SPAN_SIZE_BITS
  }

  #[inline]
  fn ptr_to_span(&self, ptr: *mut u8) -> *mut SpanHeader {
    (ptr as usize & SPAN_ALIGN_MASK) as *mut SpanHeader
  }

  #[inline]
  fn contains(&self, ptr: *mut u8) -> bool {
    let base = self.base.load(Ordering::Relaxed) as usize;
    let addr = ptr as usize;
    addr >= base && addr < base + ARENA_SIZE
  }

  #[inline]
  #[cfg(debug_assertions)]
  fn is_valid_block_ptr(&self, ptr: *mut FreeBlock) -> bool {
    if ptr.is_null() {
      return true; // null is valid
    }
    let base = self.base.load(Ordering::Relaxed) as usize;
    let addr = ptr as usize;
    ptr.is_aligned() && addr >= base && addr < base + ARENA_SIZE
  }

  #[inline(never)]
  fn global_pop(&self, cpu: usize, class: usize) -> *mut SpanHeader {
    let start = cpu & (SHARD_COUNT - 1);
    for i in 0..hint::black_box(SHARD_COUNT) {
      let span_ptr = self.cache.pop((start + i) & (SHARD_COUNT - 1), class);
      if !span_ptr.is_null() {
        return span_ptr;
      }
    }
    null_mut()
  }

  fn global_push(&self, cpu: usize, class: usize, span: *mut SpanHeader) {
    self.cache.push(cpu & (SHARD_COUNT - 1), class, span);
  }

  #[inline(never)]
  fn reuse_pop(&self, cpu: usize, class: usize) -> *mut SpanHeader {
    let start = cpu & (SHARD_COUNT - 1);
    for i in 0..hint::black_box(SHARD_COUNT) {
      let span_ptr = self.reuse.pop((start + i) & (SHARD_COUNT - 1), class);
      if !span_ptr.is_null() {
        return span_ptr;
      }
    }
    null_mut()
  }

  fn reuse_push(&self, cpu: usize, class: usize, span: *mut SpanHeader) {
    if GLOBAL_ACTIVE_SPAN_COUNTER.load(Ordering::Relaxed) > MAX_GLOBAL_ACTIVE_SPANS {
      return;
    }

    let already = unsafe { (*span).in_reuse.swap(true, Ordering::AcqRel) };
    if already {
      return; // Someone else pushed it.
    }

    // Re-verify owner after acquiring lock. If changed, we raced
    if unsafe { (*span).owner.load(Ordering::Acquire) } != SPAN_OWNER_ORPHAN {
      unsafe { (*span).in_reuse.store(false, Ordering::Release) };
      return;
    }

    if !self.reuse.push(cpu & (SHARD_COUNT - 1), class, span) {
      unsafe { (*span).in_reuse.store(false, Ordering::Release) };
    }
  }

  /// Get a small `Span` prepared for allocation:
  #[inline(never)]
  fn get_span_small(&self, heap: &mut ThreadHeap, class: usize) -> *mut SpanHeader {
    // 1) Local cache
    let span_ptr = heap.cache_pop(class);
    if !span_ptr.is_null() {
      unsafe { init_span(span_ptr, class, heap.tid) };
      return span_ptr;
    }

    // 2) Global cache
    heap.cpu = cpu_id();
    let span_ptr = self.global_pop(heap.cpu, class);
    if !span_ptr.is_null() {
      unsafe { init_span(span_ptr, class, heap.tid) };
      return span_ptr;
    }

    // 3) Reuse cache (orphan spans with remote frees)
    loop {
      let span_ptr = self.reuse_pop(heap.cpu, class);
      if span_ptr.is_null() {
        break;
      }

      let claimed = unsafe {
        (*span_ptr)
          .owner
          .compare_exchange(
            SPAN_OWNER_ORPHAN,
            heap.tid,
            Ordering::AcqRel,
            Ordering::Relaxed,
          )
          .is_ok()
      };
      if !claimed {
        continue;
      }

      unsafe {
        (*span_ptr).in_reuse.store(false, Ordering::Release);

        // If fully free, fully reinitialize.
        if (*span_ptr).used.load(Ordering::Acquire) == 0 {
          init_span(span_ptr, class, heap.tid);
        } else {
          // Otherwise, just drain remote frees for immediate reuse.
          let remote = (*span_ptr).remote_free.swap(null_mut(), Ordering::Acquire);
          #[cfg(debug_assertions)]
          {
            debug_assert!(
              self.is_valid_block_ptr(remote),
              "reuse_pop: remote_free {:p} is invalid! span={:p} class={} used={} owner={}",
              remote,
              span_ptr,
              (*span_ptr).class,
              (*span_ptr).used.load(Ordering::Relaxed),
              (*span_ptr).owner.load(Ordering::Relaxed)
            );
          }
          (*span_ptr).local_free = remote;
          (*span_ptr).hot_block = null_mut();
          (*span_ptr).owner.store(heap.tid, Ordering::Release);
          (*span_ptr).in_reuse.store(false, Ordering::Relaxed);
        }
      }

      return span_ptr;
    }

    // 4) Buddy
    self
      .buddy
      .alloc(self, 0)
      .map(|idx| self.idx_to_span(idx))
      .map(|span_ptr| {
        // Fresh buddy spans need used=0 (cached spans already verified used==0)
        unsafe { (*span_ptr).used.store(0, Ordering::Relaxed) };
        unsafe { init_span(span_ptr, class, heap.tid) };
        span_ptr
      })
      .unwrap_or(null_mut())
  }

  /// Retire a small span: publish freelists, mark orphan, cache or return to buddy.
  unsafe fn retire_small_span(&self, heap: &mut ThreadHeap, span: *mut SpanHeader) {
    debug_assert!(!span.is_null());
    debug_assert!(unsafe { (*span).kind } == SpanKind::Small);

    let class = unsafe { (*span).class as usize };

    // Publish local freelists to remote_free.
    unsafe {
      let mut list = (*span).local_free;
      let hot = (*span).hot_block;
      if !hot.is_null() {
        let block = hot as *mut FreeBlock;
        (*block).next = list;
        list = block;
      }

      (*span).hot_block = null_mut();
      (*span).local_free = null_mut();

      if !list.is_null() {
        push_remote_list(&(*span).remote_free, list);
      }

      (*span).owner.store(SPAN_OWNER_ORPHAN, Ordering::Release);
    }

    // Fully free: try local cache, else return to global cache.
    if unsafe { (*span).used.load(Ordering::Acquire) } == 0 {
      unsafe {
        (*span).in_reuse.store(false, Ordering::Release);
        (*span).remote_free.store(null_mut(), Ordering::Relaxed);
      }

      let active = GLOBAL_ACTIVE_SPAN_COUNTER.load(Ordering::Relaxed);
      if active <= MAX_GLOBAL_ACTIVE_SPANS && heap.cache_push(class, span) {
        return;
      }

      self.global_push(heap.cpu, class, span);
      return;
    }

    // Partially used with remote frees
    if unsafe { !(*span).remote_free.load(Ordering::Acquire).is_null() } {
      self.reuse_push(heap.cpu, class, span);
    }
  }
}

/// Atomically prepend a linked list to an AtomicPtr Treiber stack.
/// `list` must be a valid singly-linked list of FreeBlock.
unsafe fn push_remote_list(head: &AtomicPtr<FreeBlock>, list: *mut FreeBlock) {
  // Find list tail so we can splice in O(n) (retire path is cold).
  let mut tail = list;
  while !unsafe { (*tail).next }.is_null() {
    tail = unsafe { (*tail).next };
  }

  loop {
    let cur = head.load(Ordering::Relaxed);
    unsafe { (*tail).next = cur };
    if head
      .compare_exchange_weak(cur, list, Ordering::Release, Ordering::Relaxed)
      .is_ok()
    {
      return;
    }
  }
}

// =============================================================================
// TLS
// =============================================================================

thread_local! {
  static HEAP: UnsafeCell<ThreadHeap> = UnsafeCell::new(ThreadHeap::new());
  static IN_ALLOC: Cell<bool> = const { Cell::new(false) };
}

fn with_heap<R: Default, F: FnOnce(&mut ThreadHeap, &Arena) -> R>(f: F) -> R {
  // For dynamic linking (LD_PRELOAD), TLS may be destroyed during exit.
  // We use try_with to avoid panicking when TLS is being destroyed.
  #[cfg(feature = "dynamic")]
  {
    // Try to access IN_ALLOC; if TLS is destroyed, return default
    let Ok(in_alloc) = IN_ALLOC.try_with(|flag| flag.get()) else {
      return R::default();
    };

    if in_alloc {
      return R::default();
    }

    // Set re-entrancy guard
    let _ = IN_ALLOC.try_with(|flag| flag.set(true));

    let result = HEAP
      .try_with(|h| {
        let heap = unsafe { &mut *h.get() };
        Arena::get().map(|a| f(heap, a)).unwrap_or_default()
      })
      .unwrap_or_default();

    let _ = IN_ALLOC.try_with(|flag| flag.set(false));
    result
  }

  #[cfg(not(feature = "dynamic"))]
  {
    IN_ALLOC.with(|flag| {
      if flag.get() {
        return R::default();
      }
      flag.set(true);

      let result = HEAP.with(|h| {
        let heap = unsafe { &mut *h.get() };
        Arena::get().map(|a| f(heap, a)).unwrap_or_default()
      });

      flag.set(false);
      result
    })
  }
}

// =============================================================================
// Small allocation / free
// =============================================================================

// NOTE: Do NOT reset `used` here. In-flight frees may still be pending.
// Callers must verify used==0 before calling init_span.
unsafe fn init_span(span: *mut SpanHeader, class: usize, tid: u32) {
  let block_size = class_to_size(class);
  let capacity = (SPAN_SIZE - SPAN_HEADER_SIZE) / block_size;
  let base = unsafe { (span as *mut u8).add(SPAN_HEADER_SIZE) };
  let header = unsafe { &mut *span };
  header.bump = base;
  header.bump_end = unsafe { base.add(capacity * block_size) };
  header.hot_block = null_mut();
  header.local_free = null_mut();
  header.remote_free.store(null_mut(), Ordering::Relaxed);
  header.owner.store(tid, Ordering::Release);
  header.in_reuse.store(false, Ordering::Relaxed);
  header.block_size = block_size as u32;
  header.class = class as u8;
  header.kind = SpanKind::Small;
  header.order = 0;
  header.cache_next = null_mut();
  header.huge_base = null_mut();
  header.huge_size = 0;
  header.magic = SPAN_MAGIC;
}

fn alloc_small(heap: &mut ThreadHeap, arena: &Arena, size: usize) -> Option<NonNull<u8>> {
  let class = size_to_class(size);

  loop {
    let mut span = heap.spans[class];
    if span.is_null() {
      span = arena.get_span_small(heap, class);
      if span.is_null() {
        return None;
      }
      heap.spans[class] = span;
    }

    // Verify we own this span before using it
    #[cfg(debug_assertions)]
    {
      let owner = unsafe { (*span).owner.load(Ordering::Relaxed) };
      debug_assert!(
        owner == heap.tid,
        "alloc_small: span {:p} owner {} != our tid {}! class={}",
        span,
        owner,
        heap.tid,
        unsafe { (*span).class }
      );
    }

    unsafe {
      // Fast path: hot block (MRU)
      let hot = (*span).hot_block;
      if !hot.is_null() {
        (*span).hot_block = null_mut();
        (*span).used.fetch_add(1, Ordering::Relaxed);
        return NonNull::new(hot);
      }

      // Local free list
      let block = (*span).local_free;
      if !block.is_null() {
        #[cfg(debug_assertions)]
        {
          debug_assert!(
            arena.is_valid_block_ptr(block),
            "alloc_small: local_free {:p} is invalid! span={:p} class={} used={} owner={}",
            block,
            span,
            (*span).class,
            (*span).used.load(Ordering::Relaxed),
            (*span).owner.load(Ordering::Relaxed)
          );
        }
        (*span).local_free = (*block).next;
        (*span).used.fetch_add(1, Ordering::Relaxed);
        return NonNull::new(block as *mut u8);
      }

      let remote = (*span).remote_free.swap(null_mut(), Ordering::Acquire);
      if !remote.is_null() {
        #[cfg(debug_assertions)]
        {
          debug_assert!(
            arena.is_valid_block_ptr(remote),
            "alloc_small drain: remote_free {:p} is invalid! span={:p} class={} used={} owner={}",
            remote,
            span,
            (*span).class,
            (*span).used.load(Ordering::Relaxed),
            (*span).owner.load(Ordering::Relaxed)
          );
        }
        (*span).local_free = remote;
        continue;
      }

      // Bump allocate
      let bs = (*span).block_size as usize;
      let bump = (*span).bump;
      if bump.add(bs) <= (*span).bump_end {
        (*span).bump = bump.add(bs);
        (*span).used.fetch_add(1, Ordering::Relaxed);
        return NonNull::new(bump);
      }

      // Retire span (no blocks available)
      heap.spans[class] = null_mut();
      arena.retire_small_span(heap, span);
    }
  }
}

fn free_small(arena: &Arena, ptr: *mut u8, span: *mut SpanHeader) {
  unsafe {
    let tid = thread_id_u32();
    let owner = (*span).owner.load(Ordering::Acquire);

    if owner == tid {
      // Local free: hot_block to local_free chain
      let old_hot = (*span).hot_block;
      (*span).hot_block = ptr;
      if !old_hot.is_null() {
        let block = old_hot as *mut FreeBlock;
        (*block).next = (*span).local_free;
        (*span).local_free = block;
      }
    } else {
      // Remote free: push to Treiber stack
      let block = ptr as *mut FreeBlock;
      loop {
        let head = (*span).remote_free.load(Ordering::Relaxed);
        (*block).next = head;
        if (*span)
          .remote_free
          .compare_exchange_weak(head, block, Ordering::Release, Ordering::Relaxed)
          .is_ok()
        {
          break;
        }
      }

      // Orphan span: try reuse cache
      if (*span).owner.load(Ordering::Acquire) == SPAN_OWNER_ORPHAN {
        let class = (*span).class as usize;
        if class < CLASSES_COUNT {
          arena.reuse_push(cpu_id(), class, span);
        }
      }
    }

    // Decrement used AFTER completing the free operation
    let prev = (*span).used.fetch_sub(1, Ordering::Release);
    debug_assert!(prev != 0, "free_small: used underflow");

    if prev == 1 {
      core::sync::atomic::fence(Ordering::Acquire);
      let owner = (*span).owner.load(Ordering::Acquire);
      if owner == SPAN_OWNER_ORPHAN {
        // This prevents double-enqueue and ensures only one thread handles cleanup.
        if !(*span).in_reuse.swap(true, Ordering::AcqRel) {
          // Re-verify owner after acquiring lock. If changed, we raced.
          if (*span).owner.load(Ordering::Acquire) != SPAN_OWNER_ORPHAN {
            // Span was claimed. Restore and abort.
            (*span).in_reuse.store(false, Ordering::Release);
            return;
          }

          let class = (*span).class as usize;
          let cpu = cpu_id();
          // Try reuse cache first or fallback to global cache.
          if !arena.reuse.push(cpu & (SHARD_COUNT - 1), class, span) {
            arena.global_push(cpu, class, span);
          }
        }
      }
      // If owner != ORPHAN, the owning thread will handle via retire_small_span
    }
  }
}

// =============================================================================
// Large / Huge allocation
// =============================================================================

fn alloc_large(arena: &Arena, size: usize) -> *mut u8 {
  let total = match size.checked_add(SPAN_HEADER_SIZE) {
    Some(v) => v,
    None => return null_mut(),
  };

  let spans = total.div_ceil(SPAN_SIZE);
  let order = spans.next_power_of_two().trailing_zeros() as usize;

  if order > BUDDY_MAX_ORDER {
    return alloc_huge(size);
  }

  let Some(idx) = arena.buddy.alloc(arena, order) else {
    return alloc_huge(size);
  };

  let span = arena.idx_to_span(idx);

  unsafe {
    (*span).kind = SpanKind::Large;
    (*span).order = order as u8;
    (*span).class = 255;

    (*span).owner.store(SPAN_OWNER_ORPHAN, Ordering::Relaxed);
    (*span).in_reuse.store(false, Ordering::Relaxed);
    (*span).used.store(0, Ordering::Relaxed);
    (*span).remote_free.store(null_mut(), Ordering::Relaxed);
    (*span).cache_next = null_mut();
    (*span).magic = SPAN_MAGIC;

    (span as *mut u8).add(SPAN_HEADER_SIZE)
  }
}

fn alloc_huge(size: usize) -> *mut u8 {
  let total = match size
    .checked_add(SPAN_HEADER_SIZE)
    .and_then(|v| v.checked_add(64))
  {
    Some(v) => v,
    None => return null_mut(),
  };

  let raw = unsafe { os_mmap(total) };
  if raw.is_null() {
    return null_mut();
  }

  // Place header so that returned pointer is 64-aligned.
  let header_addr = align_up(raw as usize + SPAN_HEADER_SIZE, 64) - SPAN_HEADER_SIZE;
  let span = header_addr as *mut SpanHeader;

  unsafe {
    (*span).kind = SpanKind::Huge;
    (*span).huge_base = raw;
    (*span).huge_size = total;

    (*span).owner.store(SPAN_OWNER_ORPHAN, Ordering::Relaxed);
    (*span).in_reuse.store(false, Ordering::Relaxed);
    (*span).used.store(0, Ordering::Relaxed);
    (*span).remote_free.store(null_mut(), Ordering::Relaxed);
    (*span).magic = SPAN_MAGIC;

    (span as *mut u8).add(SPAN_HEADER_SIZE)
  }
}

fn free_large(arena: &Arena, span: *mut SpanHeader) {
  let order = unsafe { (*span).order as usize };
  arena.buddy.free(arena, arena.span_to_idx(span), order);
}

fn free_huge(span: *mut SpanHeader) {
  unsafe {
    if !(*span).huge_base.is_null() && (*span).huge_size != 0 {
      os_munmap((*span).huge_base, (*span).huge_size);
    }
  }
}

// =============================================================================
// GlobalAlloc
// =============================================================================

pub struct Allocator;

unsafe impl GlobalAlloc for Allocator {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    let size = layout.size().max(1);

    // Route high alignment to huge.
    if layout.align() > 16 {
      return alloc_huge(size);
    }

    if size <= CLASSES_MAX_SIZE
      && let Some(p) = with_heap(|heap, arena| alloc_small(heap, arena, size))
    {
      return p.as_ptr();
    }

    Arena::get()
      .map(|a| {
        if size <= ARENA_SIZE / 2 {
          alloc_large(a, size)
        } else {
          alloc_huge(size)
        }
      })
      .unwrap_or(null_mut())
  }

  unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
    if ptr.is_null() {
      return;
    }

    if let Some(arena) = ARENA.get() {
      if arena.contains(ptr) {
        let span = arena.ptr_to_span(ptr);
        match unsafe { (*span).kind } {
          SpanKind::Small => free_small(arena, ptr, span),
          SpanKind::Large => free_large(arena, span),
          SpanKind::Huge => free_huge(span),
        }
        return;
      }
    }

    // Pointer is outside arena. Check if it's a huge allocation via magic number.
    let span = (ptr as usize - SPAN_HEADER_SIZE) as *mut SpanHeader;
    unsafe {
      if (*span).magic == SPAN_MAGIC && (*span).kind == SpanKind::Huge {
        free_huge(span);
      }
    }

    // It's a foreign pointer, is ignored.
  }

  unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    if ptr.is_null() {
      return unsafe {
        self.alloc(Layout::from_size_align_unchecked(
          new_size.max(1),
          layout.align(),
        ))
      };
    }

    if new_size == 0 {
      unsafe { self.dealloc(ptr, layout) };
      return null_mut();
    }

    // Same size class optimization (small only)
    let old_size = layout.size();
    if old_size <= CLASSES_MAX_SIZE
      && new_size <= CLASSES_MAX_SIZE
      && size_to_class(old_size) == size_to_class(new_size)
    {
      return ptr;
    }

    let new_ptr =
      unsafe { self.alloc(Layout::from_size_align_unchecked(new_size, layout.align())) };

    if !new_ptr.is_null() {
      unsafe { ptr::copy_nonoverlapping(ptr, new_ptr, old_size.min(new_size)) };
      unsafe { self.dealloc(ptr, layout) };
    }

    new_ptr
  }

  unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
    let ptr = unsafe { self.alloc(layout) };
    if !ptr.is_null() {
      unsafe { ptr::write_bytes(ptr, 0, layout.size()) }
    }
    ptr
  }
}

// =============================================================================
// C API (enabled with --features c_api)
// =============================================================================

#[cfg(feature = "c_api")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn malloc(size: usize) -> *mut u8 {
  static A: Allocator = Allocator;
  unsafe { A.alloc(Layout::from_size_align_unchecked(size.max(1), 16)) }
}

#[cfg(feature = "c_api")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn free(ptr: *mut u8) {
  if ptr.is_null() {
    return;
  }
  static A: Allocator = Allocator;
  unsafe { A.dealloc(ptr, Layout::from_size_align_unchecked(1, 1)) }
}

#[cfg(feature = "c_api")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn calloc(nmemb: usize, size: usize) -> *mut u8 {
  let total = nmemb.saturating_mul(size);
  if total == 0 {
    return null_mut();
  }
  static A: Allocator = Allocator;
  unsafe { A.alloc_zeroed(Layout::from_size_align_unchecked(total, 8)) }
}

#[cfg(feature = "c_api")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn realloc(ptr: *mut u8, size: usize) -> *mut u8 {
  static A: Allocator = Allocator;

  if ptr.is_null() {
    return unsafe { A.alloc(Layout::from_size_align_unchecked(size.max(1), 8)) };
  }

  if size == 0 {
    unsafe { A.dealloc(ptr, Layout::from_size_align_unchecked(1, 1)) };
    return null_mut();
  }

  // C realloc lacks old-size: conservatively copy `size` bytes.
  let new_ptr = unsafe { A.alloc(Layout::from_size_align_unchecked(size, 8)) };

  if !new_ptr.is_null() {
    unsafe { ptr::copy_nonoverlapping(ptr, new_ptr, size) };
    unsafe { A.dealloc(ptr, Layout::from_size_align_unchecked(1, 1)) };
  }

  new_ptr
}

#[cfg(feature = "c_api")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn posix_memalign(
  memptr: *mut *mut u8,
  alignment: usize,
  size: usize,
) -> i32 {
  if !alignment.is_power_of_two() || alignment < core::mem::size_of::<*mut u8>() {
    return 22; // EINVAL
  }

  static A: Allocator = Allocator;
  let ptr = unsafe { A.alloc(Layout::from_size_align_unchecked(size.max(1), alignment)) };

  if ptr.is_null() {
    return 12; // ENOMEM
  }

  unsafe { *memptr = ptr };
  0
}

#[cfg(feature = "c_api")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn malloc_usable_size(ptr: *mut u8) -> usize {
  if ptr.is_null() {
    return 0;
  }

  if let Some(arena) = ARENA.get() {
    if arena.contains(ptr) {
      let span = arena.ptr_to_span(ptr);
      return match unsafe { (*span).kind } {
        SpanKind::Small => unsafe { (*span).block_size as usize },
        SpanKind::Large => {
          let order = unsafe { (*span).order as usize };
          (SPAN_SIZE << order) - SPAN_HEADER_SIZE
        }
        SpanKind::Huge => unsafe { (*span).huge_size.saturating_sub(SPAN_HEADER_SIZE + 64) },
      };
    }
  }

  // Foreign pointer: conservative fallback
  0
}

pub unsafe fn ralloc_malloc(size: usize) -> *mut u8 {
  static A: Allocator = Allocator;
  unsafe { A.alloc(Layout::from_size_align_unchecked(size.max(1), 8)) }
}

pub unsafe fn ralloc_free(ptr: *mut u8) {
  static A: Allocator = Allocator;
  unsafe { A.dealloc(ptr, Layout::from_size_align_unchecked(1, 1)) }
}

// =============================================================================
// Utils
// =============================================================================

/// Rounds `x` up to the next multiple of alignment `align`. Alignment must be a power of 2.
#[inline(always)]
const fn align_up(x: usize, align: usize) -> usize {
  let mask = align - 1;
  (x + mask) & !mask
}

// =============================================================================
// Size Classes
// =============================================================================

/// Sub-class multipliers for geometric range (scaled by 16 for integer math).
/// Each represents a fraction of the doubling: 1.0, 1.19, 1.44, 1.69
const GEO_MULTIPLIERS: [usize; 4] = [16, 19, 23, 27];

/// Convert class index to allocation size (inverse of `size_to_class`).
#[inline(always)]
const fn class_to_size(class: usize) -> usize {
  if class < CLASSES_LINEAR {
    (class + 1) * CLASSES_LINEAR_STEP
  } else {
    let geo_index = class - CLASSES_LINEAR + 1;
    let order = geo_index / CLASSES_PER_DOUBLING;
    let sub = geo_index % CLASSES_PER_DOUBLING;

    let base = 128 << order;
    let size = align_up((base * GEO_MULTIPLIERS[sub]) / 16, 16);

    if size > CLASSES_MAX_SIZE {
      CLASSES_MAX_SIZE
    } else {
      size
    }
  }
}

/// Convert allocation size to class index (inverse of `class_to_size`).
#[inline(always)]
fn size_to_class(size: usize) -> usize {
  if size == 0 {
    return 0;
  }
  if size > CLASSES_MAX_SIZE {
    return CLASSES_COUNT - 1;
  }

  // Linear range: ceil(size / 16) - 1, clamped
  if size <= 128 {
    return (size - 1) / CLASSES_LINEAR_STEP;
  }

  // Geometric range: find order via log2, then sub-class via thresholds
  let log2 = (usize::BITS as usize - 1) - size.leading_zeros() as usize;
  let order = log2.saturating_sub(7);
  let base = 128usize << order;

  // Compute thresholds for each sub-class at this order
  let t0 = base;
  let t1 = align_up((base * 19) >> 4, 16);
  let t2 = align_up((base * 23) >> 4, 16);
  let t3 = align_up((base * 27) >> 4, 16);

  // Count exceeded thresholds
  let exceeded =
    (size > t0) as usize + (size > t1) as usize + (size > t2) as usize + (size > t3) as usize;

  // If exceeded == 4, bump to next order
  let order_bump = exceeded >> 2;
  let sub = exceeded & 3;

  let final_order = order + order_bump;
  let geo_index = final_order * CLASSES_PER_DOUBLING + sub;
  CLASSES_LINEAR + geo_index - 1
}
