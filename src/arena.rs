//! Per-thread bump arenas backed by transparent huge pages, and append-only block lists on top.
//! Cuts page-fault count ~500x versus growing many Vecs (fault cost dominated multi-threaded runs).

use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::marker::PhantomData;

pub struct Arena {
    base: *mut u8,
    cap: usize,
    used: AtomicUsize,
    /// end of this arena's huge-page range (0 once switched to normal pages)
    huge_end: AtomicUsize,
    /// recycled BLOCK-sized blocks (only touched by the arena's own worker thread)
    free: std::cell::UnsafeCell<Vec<*mut u8>>,
}

// Each arena is only bumped by its own worker thread; the atomic just makes that Sync-safe.
unsafe impl Send for Arena {}
unsafe impl Sync for Arena {}

pub const BLOCK: usize = 16 << 10;

static THP_BUDGET: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

/// Decide how many bytes of each worker arena ask for transparent huge pages. Huge pages are a big win
/// when they come cheap (clean memory: ~2x on the parse), but with defrag=madvise a huge-page fault on a
/// fragmented machine can stall in direct compaction. So instead of an all-or-nothing switch (which
/// flips between runs near the threshold), each arena gets huge pages for its share of the memory that
/// is *already free* in >= 2 MB blocks (with headroom), and normal pages beyond that.
/// SESSIONS_THP=0/1 forces none/all.
pub fn decide_thp(threads: usize) -> u64 {
    let _ = threads;
    *THP_BUDGET.get_or_init(|| {
        let per = match std::env::var("SESSIONS_THP").ok().as_deref() {
            Some("0") => 0,
            _ => u64::MAX,
        };
        if per > 0 {
            if let Some((a, f)) = thp_counters() {
                THP_BASE_ALLOC.store(a as usize, Relaxed);
                THP_BASE_FALLBACK.store(f as usize, Relaxed);
            }
        }
        per
    })
}

/// (thp_fault_fallback, compact_stall) from /proc/vmstat: huge-page faults that fell back to 4 KB pages,
/// and direct-compaction stalls (allocations that had to stop and migrate pages to build a 2 MB block).
fn thp_counters() -> Option<(u64, u64)> {
    let t = std::fs::read_to_string("/proc/vmstat").ok()?;
    let mut a = None;
    let mut f = None;
    for l in t.lines() {
        if let Some(v) = l.strip_prefix("thp_fault_fallback ") {
            a = v.trim().parse().ok();
        } else if let Some(v) = l.strip_prefix("compact_stall ") {
            f = v.trim().parse().ok();
        }
    }
    Some((a?, f?))
}

static THP_BASE_ALLOC: AtomicUsize = AtomicUsize::new(0);
static THP_BASE_FALLBACK: AtomicUsize = AtomicUsize::new(0);
static THP_PROBES: AtomicUsize = AtomicUsize::new(0);
/// set once huge pages are judged not worth it on this machine (they mostly fall back)
static THP_OFF: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn thp_budget() -> u64 {
    *THP_BUDGET.get().unwrap_or(&u64::MAX)
}

impl Arena {
    pub fn new(cap: usize) -> Self {
        Self::try_new(cap).expect("arena reservation failed")
    }

    /// Reserves `cap` bytes of address space; None if the kernel refuses (e.g. strict overcommit
    /// accounting, which ignores MAP_NORESERVE, or an address-space limit).
    pub fn try_new(cap: usize) -> Option<Self> {
        let cap = (cap + (2 << 20) - 1) & !((2 << 20) - 1);
        unsafe {
            let p = libc::mmap(
                std::ptr::null_mut(),
                cap,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            );
            if p == libc::MAP_FAILED {
                return None;
            }
            let huge = (thp_budget().min(cap as u64)) as usize;
            if huge > 0 {
                libc::madvise(p, huge, libc::MADV_HUGEPAGE);
            }
            Some(Arena { base: p as *mut u8, cap, used: AtomicUsize::new(0), huge_end: AtomicUsize::new(huge), free: Default::default() })
        }
    }

    /// Pre-fault the first `bytes` of the arena in one kernel call (MADV_POPULATE_WRITE, Linux 5.14+);
    /// silently does nothing where unsupported.
    pub fn populate(&self, bytes: usize) {
        let bytes = bytes.min(self.cap) & !((2 << 20) - 1);
        if bytes > 0 && !self.base.is_null() {
            unsafe {
                libc::madvise(self.base as *mut libc::c_void, bytes, 23 /* MADV_POPULATE_WRITE */);
            }
        }
    }

    /// At the first block of each new 2 MB page in the huge-page range. After the first 16 and 64 such
    /// pages (all arenas together), look at the kernel counters since start: on a fragmented machine
    /// huge-page faults trigger direct compaction (compact_stall climbs, often with fallbacks), which
    /// costs far more than huge pages save, so every arena switches to normal pages. On a machine with
    /// free 2 MB blocks neither counter moves and huge pages stay on.
    #[cold]
    fn probe_huge(&self, _p: *mut u8, at: usize) {
        if !THP_OFF.load(Relaxed) {
            let n = THP_PROBES.fetch_add(1, Relaxed) + 1;
            if n == 16 || n == 64 {
                if let Some((fb, cs)) = thp_counters() {
                    let dfb = fb.saturating_sub(THP_BASE_ALLOC.load(Relaxed) as u64);
                    let dcs = cs.saturating_sub(THP_BASE_FALLBACK.load(Relaxed) as u64);
                    if dcs >= 8 || dfb >= 8 {
                        THP_OFF.store(true, Relaxed);
                    }
                    if std::env::var_os("SESSIONS_QUIET").is_none() {
                        eprintln!("thp check @{n}: fallback +{dfb} compact_stall +{dcs} -> {}", if THP_OFF.load(Relaxed) { "off" } else { "on" });
                    }
                }
            }
        }
        if THP_OFF.load(Relaxed) {
            let from = at;
            let end = self.huge_end.swap(0, Relaxed);
            if end > from {
                unsafe { libc::madvise(self.base.add(from) as *mut libc::c_void, end - from, libc::MADV_NOHUGEPAGE) };
            }
        }
    }

    pub fn base(&self) -> *mut u8 {
        self.base
    }

    pub fn contains(&self, p: *const u8) -> bool {
        let b = self.base as usize;
        (p as usize) >= b && (p as usize) < b + self.cap
    }

    /// A BLOCK-sized, BLOCK-aligned block (alignment lets `Parts` detect a full block from the cursor alone).
    fn block_aligned(&self) -> *mut u8 {
        let at = (self.used.load(Relaxed) + BLOCK - 1) & !(BLOCK - 1);
        if at + BLOCK <= self.cap {
            self.used.store(at + BLOCK, Relaxed);
            let p = unsafe { self.base.add(at) };
            if at & ((2 << 20) - 1) == 0 && at < self.huge_end.load(Relaxed) {
                self.probe_huge(p, at);
            }
            p
        } else {
            // rows are addressed by arena offset (dedup refs), so a heap block would corrupt them
            panic!("arena full ({} bytes)", self.cap);
        }
    }

    fn block(&self) -> *mut u8 {
        match unsafe { (*self.free.get()).pop() } {
            Some(p) => p,
            None => self.alloc(BLOCK),
        }
    }

    /// Bytes, 64-aligned.
    fn alloc(&self, bytes: usize) -> *mut u8 {
        let at = (self.used.load(Relaxed) + 63) & !63;
        if at + bytes <= self.cap {
            self.used.store(at + bytes, Relaxed);
            unsafe { self.base.add(at) }
        } else {
            panic!("arena full ({} bytes)", self.cap);
        }
    }
}

impl Arena {
    /// Free every page the arena touched (the arena must not be read afterwards). MADV_DONTNEED only
    /// takes the mmap lock shared, so all workers can free their arenas at once; munmap (or the exit
    /// path) needs it exclusive and frees the pages on one core.
    pub fn release(&self) {
        let used = (self.used.load(Relaxed) + (2 << 20) - 1) & !((2 << 20) - 1);
        unsafe { libc::madvise(self.base as *mut libc::c_void, used.min(self.cap), libc::MADV_DONTNEED) };
    }
}

impl Drop for Arena {
    fn drop(&mut self) {
        // only reached when a reservation set is abandoned; the run itself exits without dropping
        unsafe { libc::munmap(self.base as *mut libc::c_void, self.cap) };
    }
}

/// Append-only list of fixed-size blocks allocated from an Arena.
/// Append-only list of BLOCK-sized blocks. The hot write state (cursor, end) sits first so a push is
/// one compare and one store.
#[repr(C)]
pub struct Blocks<T: Copy> {
    cur: *mut T,
    end: *mut T,
    blocks: Vec<*mut T>,
    _p: PhantomData<T>,
}

unsafe impl<T: Copy + Send> Send for Blocks<T> {}
unsafe impl<T: Copy + Sync> Sync for Blocks<T> {}

impl<T: Copy> Default for Blocks<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Copy> Blocks<T> {
    const PER: usize = BLOCK / std::mem::size_of::<T>();

    pub fn new() -> Self {
        Blocks { cur: std::ptr::null_mut(), end: std::ptr::null_mut(), blocks: Vec::new(), _p: PhantomData }
    }

    /// Expecting about `rows` rows: size the block-pointer list once (no reallocation while pushing).
    pub fn with_rows(rows: usize) -> Self {
        let blocks = rows * std::mem::size_of::<T>() / BLOCK + 2;
        Blocks { cur: std::ptr::null_mut(), end: std::ptr::null_mut(), blocks: Vec::with_capacity(blocks), _p: PhantomData }
    }

    /// Hands every block to `arena`'s free list (the data is dead afterwards).
    pub fn recycle(&mut self, arena: &Arena) {
        let f = unsafe { &mut *arena.free.get() };
        f.extend(self.blocks.drain(..).map(|b| b as *mut u8));
        self.cur = std::ptr::null_mut();
        self.end = std::ptr::null_mut();
    }

    #[inline(always)]
    pub fn push(&mut self, arena: &Arena, v: T) {
        if self.cur == self.end {
            self.grow(arena);
        }
        unsafe {
            prefetch_ahead(self.cur as *const u8);
            self.cur.write_unaligned(v);
            self.cur = self.cur.add(1);
        }
    }

    /// `push` that also returns where the row landed.
    #[inline(always)]
    pub fn push_ptr(&mut self, arena: &Arena, v: T) -> *mut T {
        if self.cur == self.end {
            self.grow(arena);
        }
        unsafe {
            let p = self.cur;
            prefetch_ahead(p as *const u8);
            p.write_unaligned(v);
            self.cur = p.add(1);
            p
        }
    }

    #[cold]
    #[inline(never)]
    fn grow(&mut self, arena: &Arena) {
        let b = arena.block() as *mut T;
        self.blocks.push(b);
        self.cur = b;
        self.end = unsafe { b.add(Self::PER) };
    }

    fn last_len(&self) -> usize {
        match self.blocks.last() {
            Some(&b) => (self.cur as usize - b as usize) / std::mem::size_of::<T>(),
            None => 0,
        }
    }

    pub fn len(&self) -> usize {
        if self.blocks.is_empty() { 0 } else { (self.blocks.len() - 1) * Self::PER + self.last_len() }
    }

    pub fn for_each_slice(&self, mut f: impl FnMut(&[T])) {
        let n = self.blocks.len();
        let last = self.last_len();
        for (i, &b) in self.blocks.iter().enumerate() {
            let len = if i + 1 == n { last } else { Self::PER };
            f(unsafe { std::slice::from_raw_parts(b, len) });
        }
    }

    pub fn extend_into(&self, out: &mut Vec<T>) {
        self.for_each_slice(|s| out.extend_from_slice(s));
    }
}

/// Each partition list is a write stream: fetch (for write) the cache line two lines ahead of the
/// cursor so it is owned by the time rows reach it (hides the read-for-ownership miss). A prefetch
/// past the end of a block is harmless.
#[inline(always)]
unsafe fn prefetch_ahead(p: *const u8) {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        std::arch::x86_64::_mm_prefetch(p.wrapping_add(PF_DIST) as *const i8, std::arch::x86_64::_MM_HINT_ET0);
    }
}

const PF_DIST: usize = 256;

/// Many append-only lists (one per partition) with the hot write state packed densely: one cursor per
/// list, 8 bytes, so a thread's cursors for all partitions stay in L1. Blocks are BLOCK-aligned, so a
/// cursor sitting on a block boundary means "full" (or "no block yet"). size_of::<T>() must divide BLOCK.
pub struct Parts<T: Copy> {
    curs: Vec<*mut T>,
    lists: Vec<Vec<*mut T>>,
}

unsafe impl<T: Copy + Send> Send for Parts<T> {}
unsafe impl<T: Copy + Sync> Sync for Parts<T> {}

impl<T: Copy> Default for Parts<T> {
    fn default() -> Self {
        Parts { curs: Vec::new(), lists: Vec::new() }
    }
}

impl<T: Copy> Parts<T> {
    const PER: usize = BLOCK / std::mem::size_of::<T>();

    pub fn new(n: usize, rows_each: usize) -> Self {
        let cap = rows_each * std::mem::size_of::<T>() / BLOCK + 2;
        Parts { curs: vec![std::ptr::null_mut(); n], lists: (0..n).map(|_| Vec::with_capacity(cap)).collect() }
    }

    #[inline(always)]
    pub fn push_ptr(&mut self, arena: &Arena, part: usize, v: T) -> *mut T {
        let mut c = unsafe { *self.curs.get_unchecked(part) };
        // a block is full when the next row would not fit (offset 0 = full block of a size dividing
        // BLOCK, or no block yet); for sizes dividing BLOCK the second test folds away
        let off = (c as usize) & (BLOCK - 1);
        if off == 0 || off + std::mem::size_of::<T>() > BLOCK {
            c = self.grow(arena, part);
        }
        unsafe {
            prefetch_ahead(c as *const u8);
            c.write(v);
            *self.curs.get_unchecked_mut(part) = c.add(1);
        }
        c
    }

    #[inline(always)]
    pub fn push(&mut self, arena: &Arena, part: usize, v: T) {
        self.push_ptr(arena, part, v);
    }

    #[cold]
    #[inline(never)]
    fn grow(&mut self, arena: &Arena, part: usize) -> *mut T {
        let b = arena.block_aligned() as *mut T;
        self.lists[part].push(b);
        b
    }

    pub fn len(&self, part: usize) -> usize {
        let l = &self.lists[part];
        match l.last() {
            None => 0,
            Some(&last) => (l.len() - 1) * Self::PER + (self.curs[part] as usize - last as usize) / std::mem::size_of::<T>(),
        }
    }

    pub fn for_each_slice(&self, part: usize, mut f: impl FnMut(&[T])) {
        let l = &self.lists[part];
        let n = l.len();
        for (i, &b) in l.iter().enumerate() {
            let len = if i + 1 == n {
                (self.curs[part] as usize - b as usize) / std::mem::size_of::<T>()
            } else {
                Self::PER
            };
            f(unsafe { std::slice::from_raw_parts(b, len) });
        }
    }
}
