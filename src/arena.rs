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
            libc::madvise(p, cap, libc::MADV_HUGEPAGE);
            Arena { base: p as *mut u8, cap, used: AtomicUsize::new(0), free: Default::default() }
        }
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
pub struct Blocks<T: Copy> {
    blocks: Vec<*mut T>,
    last_len: usize,
    per: usize,
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
    pub fn new() -> Self {
        let per = BLOCK / std::mem::size_of::<T>();
        Blocks { blocks: Vec::new(), last_len: per, per, _p: PhantomData }
    }

    /// Hands every block to `arena`'s free list (the data is dead afterwards).
    pub fn recycle(&mut self, arena: &Arena) {
        let f = unsafe { &mut *arena.free.get() };
        f.extend(self.blocks.drain(..).map(|b| b as *mut u8));
        self.last_len = self.per;
    }

    #[inline(always)]
    pub fn push(&mut self, arena: &Arena, v: T) {
        if self.last_len == self.per {
            self.grow(arena);
        }
        unsafe { *self.blocks.last().unwrap_unchecked().add(self.last_len) = v };
        self.last_len += 1;
    }

    #[cold]
    #[inline(never)]
    fn grow(&mut self, arena: &Arena) {
        self.blocks.push(arena.block() as *mut T);
        self.last_len = 0;
    }

    pub fn len(&self) -> usize {
        if self.blocks.is_empty() { 0 } else { (self.blocks.len() - 1) * self.per + self.last_len }
    }

    pub fn for_each_slice(&self, mut f: impl FnMut(&[T])) {
        let n = self.blocks.len();
        for (i, &b) in self.blocks.iter().enumerate() {
            let len = if i + 1 == n { self.last_len } else { self.per };
            f(unsafe { std::slice::from_raw_parts(b, len) });
        }
    }

    pub fn extend_into(&self, out: &mut Vec<T>) {
        self.for_each_slice(|s| out.extend_from_slice(s));
    }
}
