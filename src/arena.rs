//! Per-thread bump arenas backed by transparent huge pages, and append-only block lists on top.
//! Cuts page-fault count ~500x versus growing many Vecs (fault cost dominated multi-threaded runs).

use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::marker::PhantomData;

pub struct Arena {
    base: *mut u8,
    cap: usize,
    used: AtomicUsize,
    /// recycled BLOCK-sized blocks (only touched by the arena's own worker thread)
    free: std::cell::UnsafeCell<Vec<*mut u8>>,
}

pub const BLOCK: usize = 16 << 10;

// Each arena is only bumped by its own worker thread; the atomic just makes that Sync-safe.
unsafe impl Send for Arena {}
unsafe impl Sync for Arena {}

impl Arena {
    pub fn new(cap: usize) -> Self {
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
                return Arena { base: std::ptr::null_mut(), cap: 0, used: AtomicUsize::new(0), free: Default::default() };
            }
            if std::env::var_os("SESSIONS_NOTHP").is_none() {
                libc::madvise(p, cap, libc::MADV_HUGEPAGE);
            }
            Arena { base: p as *mut u8, cap, used: AtomicUsize::new(0), free: Default::default() }
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

    pub fn base(&self) -> *mut u8 {
        self.base
    }

    pub fn contains(&self, p: *const u8) -> bool {
        let b = self.base as usize;
        (p as usize) >= b && (p as usize) < b + self.cap
    }

    fn block(&self) -> *mut u8 {
        match unsafe { (*self.free.get()).pop() } {
            Some(p) => p,
            None => self.alloc(BLOCK),
        }
    }

    /// Bytes, 64-aligned. Falls back to a leaked heap allocation when the arena is exhausted.
    fn alloc(&self, bytes: usize) -> *mut u8 {
        let at = (self.used.load(Relaxed) + 63) & !63;
        if at + bytes <= self.cap {
            self.used.store(at + bytes, Relaxed);
            unsafe { self.base.add(at) }
        } else {
            let layout = std::alloc::Layout::from_size_align(bytes.max(1), 64).unwrap();
            unsafe { std::alloc::alloc(layout) }
        }
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
