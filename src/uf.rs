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

/// Read-only key -> person name map holding only IDs whose name differs from themselves, behind a
/// one-word-per-key Bloom filter: unlinked IDs and component roots (the common case) skip the table.
pub struct LinkMap {
    mask: usize,
    slots: Vec<(u64, u64)>,
    bloom: Vec<u64>,
    bmask: usize,
}

#[inline(always)]
fn bloom_bits(h: u64) -> u64 {
    (1u64 << (h & 63)) | (1u64 << ((h >> 6) & 63))
}

impl LinkMap {
    /// Builds from a finished Uf. `par` runs closures over index ranges in parallel.
    pub fn build(uf: &Uf, par: &dyn Fn(usize, &(dyn Fn(usize) + Sync))) -> Self {
        let cap_in = uf.cap();
        // count non-roots
        let per = 1 << 16;
        let ntask = cap_in.div_ceil(per);
        let counts: Vec<AtomicU64> = (0..ntask).map(|_| AtomicU64::new(0)).collect();
        par(ntask, &|t| {
            let mut c = 0;
            for i in t * per..((t + 1) * per).min(cap_in) {
                if let Some((k, name)) = uf.resolved(i) {
                    if k != name {
                        c += 1;
                    }
                }
            }
            counts[t].store(c, Relaxed);
        });
        let n: usize = counts.iter().map(|c| c.load(Relaxed) as usize).sum();
        let cap = (n * 2 + 1).next_power_of_two().max(64);
        let keys: Vec<AtomicU64> = (0..cap).map(|_| AtomicU64::new(EMPTY)).collect();
        let vals: Vec<AtomicU64> = (0..cap).map(|_| AtomicU64::new(0)).collect();
        let bwords = (n * 10 / 64 + 1).next_power_of_two().max(64);
        let bloom: Vec<AtomicU64> = (0..bwords).map(|_| AtomicU64::new(0)).collect();
        let mask = cap - 1;
        let bmask = bwords - 1;
        par(ntask, &|t| {
            for i in t * per..((t + 1) * per).min(cap_in) {
                let Some((k, name)) = uf.resolved(i) else { continue };
                if k == name {
                    continue;
                }
                let h = hash(k);
                bloom[(h >> 40) as usize & bmask].fetch_or(bloom_bits(h), Relaxed);
                let mut j = h as usize & mask;
                loop {
                    if keys[j].compare_exchange(EMPTY, k, Relaxed, Relaxed).is_ok() {
                        vals[j].store(name, Relaxed);
                        break;
                    }
                    j = (j + 1) & mask;
                }
            }
        });
        LinkMap {
            mask,
            slots: keys.into_iter().zip(vals).map(|(k, v)| (k.into_inner(), v.into_inner())).collect(),
            bloom: bloom.into_iter().map(|b| b.into_inner()).collect(),
            bmask,
        }
    }

    #[inline(always)]
    pub fn prefetch(&self, k: u64) {
        let h = hash(k);
        #[cfg(target_arch = "x86_64")]
        unsafe {
            use std::arch::x86_64::*;
            let w = *self.bloom.get_unchecked((h >> 40) as usize & self.bmask);
            let b = bloom_bits(h);
            if w & b == b {
                _mm_prefetch(self.slots.as_ptr().add(h as usize & self.mask) as *const i8, _MM_HINT_T0);
            }
        }
    }

    #[inline(always)]
    pub fn person(&self, k: u64) -> u64 {
        let h = hash(k);
        let b = bloom_bits(h);
        if unsafe { *self.bloom.get_unchecked((h >> 40) as usize & self.bmask) } & b != b {
            return k;
        }
        let mut i = h as usize & self.mask;
        loop {
            let (s, v) = unsafe { *self.slots.get_unchecked(i) };
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
