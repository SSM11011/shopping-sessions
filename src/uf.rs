//! Lock-free union-find over u64 ID keys in an open-addressing table. A union hooks the root with the
//! larger key (byte order) under the smaller, so every root is its component's byte-wise smallest ID.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::*};

use crate::keys::{cmp_key, hash, EMPTY};

pub struct Uf {
    mask: usize,
    keys: Vec<AtomicU64>,
    /// 0 = root, else parent slot + 1
    parent: Vec<AtomicU32>,
}

impl Uf {
    pub fn new(max_ids: usize) -> Self {
        let cap = (max_ids + 1).next_power_of_two().max(64);
        let mut keys = Vec::with_capacity(cap);
        keys.resize_with(cap, || AtomicU64::new(EMPTY));
        let mut parent = Vec::with_capacity(cap);
        parent.resize_with(cap, || AtomicU32::new(0));
        Uf { mask: cap - 1, keys, parent }
    }

    pub fn cap(&self) -> usize {
        self.mask + 1
    }

    #[inline]
    fn slot(&self, k: u64) -> usize {
        let mut i = hash(k) as usize & self.mask;
        loop {
            let cur = self.keys[i].load(Acquire);
            if cur == k {
                return i;
            }
            if cur == EMPTY {
                match self.keys[i].compare_exchange(EMPTY, k, AcqRel, Acquire) {
                    Ok(_) => return i,
                    Err(x) if x == k => return i,
                    Err(_) => {}
                }
            }
            i = (i + 1) & self.mask;
        }
    }

    #[inline]
    fn parent_of(&self, x: usize) -> usize {
        let p = self.parent[x].load(Acquire);
        if p == 0 { x } else { p as usize - 1 }
    }

    #[inline]
    pub fn find(&self, mut x: usize) -> usize {
        loop {
            let p = self.parent_of(x);
            if p == x {
                return x;
            }
            let gp = self.parent_of(p);
            if gp != p {
                let _ = self.parent[x].compare_exchange(p as u32 + 1, gp as u32 + 1, AcqRel, Relaxed);
            }
            x = p;
        }
    }

    pub fn union(&self, a: u64, b: u64) {
        let (sa, sb) = (self.slot(a), self.slot(b));
        if sa == sb {
            return;
        }
        loop {
            let (ra, rb) = (self.find(sa), self.find(sb));
            if ra == rb {
                return;
            }
            let ka = self.keys[ra].load(Relaxed);
            let kb = self.keys[rb].load(Relaxed);
            let (small, big) = if cmp_key(ka, kb).is_lt() { (ra, rb) } else { (rb, ra) };
            if self.parent[big].compare_exchange(0, small as u32 + 1, AcqRel, Acquire).is_ok() {
                return;
            }
        }
    }

    /// (key, name) for slot i, or None if empty. Call after all unions.
    #[inline]
    pub fn resolved(&self, i: usize) -> Option<(u64, u64)> {
        let k = self.keys[i].load(Relaxed);
        if k == EMPTY {
            return None;
        }
        Some((k, self.keys[self.find(i)].load(Relaxed)))
    }
}

/// Read-only key -> person name map, slots aligned with the Uf table.
pub struct LinkMap {
    mask: usize,
    pub slots: Vec<(u64, u64)>,
}

impl LinkMap {
    pub fn empty(cap: usize) -> Self {
        LinkMap { mask: cap - 1, slots: vec![(EMPTY, 0); cap] }
    }
    #[inline(always)]
    pub fn prefetch(&self, k: u64) {
        let i = hash(k) as usize & self.mask;
        #[cfg(target_arch = "x86_64")]
        unsafe {
            use std::arch::x86_64::*;
            _mm_prefetch(self.slots.as_ptr().add(i) as *const i8, _MM_HINT_T0);
        }
    }
    #[inline(always)]
    pub fn person(&self, k: u64) -> u64 {
        let mut i = hash(k) as usize & self.mask;
        loop {
            let (s, v) = self.slots[i];
            if s == k {
                return v;
            }
            if s == EMPTY {
                return k;
            }
            i = (i + 1) & self.mask;
        }
    }
}
