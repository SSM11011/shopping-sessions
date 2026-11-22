//! IDs as u64 keys. A canonical ID (1..=10 bytes, each in [0-9a-z]) packs into 60 bits as 6-bit codes,
//! big-endian and zero-padded, so integer order == byte order. Anything else is "exotic": bit 63 set and
//! an index into a global intern table. Comparisons involving an exotic key go through the bytes.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering::*};
use std::sync::{Mutex, OnceLock};

pub const EXOTIC: u64 = 1 << 63;
/// Exotic sub-space: a long hex ID (`[a-z]` + 32 `[0-9a-f]`) held in the lock-free `Long` table;
/// the low bits are its slot. Without this bit an exotic key indexes the general interner.
pub const LONG: u64 = 1 << 62;
pub const EMPTY: u64 = u64::MAX;

// ---------------- long hex IDs: lock-free open-addressing table of (prefix, hi, lo)
//
// A slot never moves once filled, so its index is a stable, unique key. Insert: CAS hi 0 -> BUSY,
// write lo and the prefix, publish hi (release). hi values 0 and BUSY are reserved: such an ID goes to
// the general interner, as does any ID whose probe sequence finds no match and no free slot within
// MAX_PROBE (the table only fills, so that outcome is the same for every later lookup of that ID).
const BUSY: u64 = 1;
const MAX_PROBE: usize = 128;

/// One slot: hi (state word), lo, prefix -- adjacent, so a lookup touches one place.
struct Slot {
    hi: AtomicU64,
    lo: AtomicU64,
    pre: AtomicU64,
}

struct Long {
    mask: usize,
    slots: std::mem::ManuallyDrop<Vec<Slot>>,
}

static LONG_TABLE: OnceLock<Long> = OnceLock::new();

/// Sizes the long-ID table for about `expected` distinct long IDs (load <= ~2/3). Call once, before
/// any key is made; without it long IDs use the general interner.
pub fn init_long(expected: usize) {
    let cap = (expected * 3 / 2 + 1024).next_power_of_two();
    // fresh anonymous mapping: zero pages (all-zero = empty slot), no fill pass. Huge pages: 4 KB
    // first-touch faults from 8 threads at random slots contend on the mm lock (0.3 s sys at 20M).
    const HUGE: usize = 2 << 20;
    let bytes = (cap * std::mem::size_of::<Slot>() + HUGE - 1) & !(HUGE - 1);
    let slots = unsafe {
        // over-map by 2 MB and start at a 2 MB boundary so every page of the table can be huge
        let raw = libc::mmap(
            std::ptr::null_mut(),
            bytes + HUGE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
            -1,
            0,
        );
        assert!(raw != libc::MAP_FAILED, "long-id table allocation failed");
        let p = ((raw as usize + HUGE - 1) & !(HUGE - 1)) as *mut libc::c_void;
        libc::madvise(p, bytes, libc::MADV_HUGEPAGE);
        // never freed: the table lives until exit (ManuallyDrop keeps Vec from freeing mmap memory)
        std::mem::ManuallyDrop::new(Vec::from_raw_parts(p as *mut Slot, cap, cap))
    };
    let _ = LONG_TABLE.set(Long { mask: cap - 1, slots });
}

/// (filled slots, capacity, general-interner entries): diagnostics.
pub fn long_stats() -> (usize, usize, usize) {
    let (f, c) = LONG_TABLE.get().map_or((0, 0), |t| (t.slots.iter().filter(|s| s.hi.load(Relaxed) != 0).count(), t.mask + 1));
    (f, c, interner().lock().unwrap().strs.len())
}

/// Write-touches part `t` of `n` of the table (call for every t, before any lookup). Lookups read a
/// slot before writing it; a read fault maps the zero page, after which the write fault installs 4 KB
/// pages (12k faults at 20M) instead of 2 MB ones. Writing first gets huge pages.
pub fn touch_long(t: usize, n: usize) {
    let Some(tb) = LONG_TABLE.get() else { return };
    const HUGE: usize = 2 << 20;
    let base = tb.slots.as_ptr() as *mut u8;
    let bytes = tb.slots.len() * std::mem::size_of::<Slot>();
    let chunks = bytes.div_ceil(HUGE);
    for c in (t * chunks / n)..((t + 1) * chunks / n) {
        unsafe { std::ptr::write_volatile(base.add(c * HUGE), 0) };
    }
}

#[inline(always)]
fn long_slot0(pre: u8, hi: u64, lo: u64) -> usize {
    let mut x = lo ^ hi.rotate_left(29) ^ (pre as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^= x >> 32;
    x = x.wrapping_mul(0xd6e8_feb8_6659_fd93);
    (x ^ (x >> 32)) as usize
}

/// Key for a long hex ID given as (prefix byte, 128-bit value), or None -> general interner.
#[inline]
fn long_key(pre: u8, hi: u64, lo: u64) -> Option<u64> {
    let t = LONG_TABLE.get()?;
    if hi <= BUSY {
        return None;
    }
    let mut i = long_slot0(pre, hi, lo) & t.mask;
    for _ in 0..MAX_PROBE {
        let sl = unsafe { t.slots.get_unchecked(i) };
        let mut h = sl.hi.load(Acquire);
        if h == 0 {
            match sl.hi.compare_exchange(0, BUSY, Acquire, Acquire) {
                Ok(_) => {
                    sl.lo.store(lo, Relaxed);
                    sl.pre.store(pre as u64, Relaxed);
                    sl.hi.store(hi, Release);
                    return Some(EXOTIC | LONG | i as u64);
                }
                Err(x) => h = x,
            }
        }
        while h == BUSY {
            std::hint::spin_loop();
            h = sl.hi.load(Acquire);
        }
        if h == hi && sl.lo.load(Relaxed) == lo && sl.pre.load(Relaxed) == pre as u64 {
            return Some(EXOTIC | LONG | i as u64);
        }
        i = (i + 1) & t.mask;
    }
    None
}

/// Long hex click_id carried by value (never in the table): EXOTIC | LONG | LV | (prefix - 'a') << 56 |
/// worker << 48 | index into that worker's click store, which holds (hi, lo). Click ids only order
/// clicks; they are never linked, so they need no table slot.
pub const LV: u64 = 1 << 61;
static CLICK_STORE: OnceLock<Vec<Vec<[u64; 2]>>> = OnceLock::new();

/// Publishes the per-worker long click_id stores (index = worker). Call once, after parsing and before
/// any comparison of click keys.
pub fn set_click_store(v: Vec<Vec<[u64; 2]>>) {
    let _ = CLICK_STORE.set(v);
}

/// Click-id key: a long hex click_id goes by value into `store` (worker `w` < 256), anything else as
/// `key`. Reads 16 bytes like `key_padded`.
#[inline(always)]
pub unsafe fn click_padded(s: &[u8], w: usize, store: &mut Vec<[u64; 2]>) -> u64 {
    if let Some(k) = unsafe { pack16(s) } {
        return k;
    }
    if let Some((p, hi, lo)) = parse_long(s) {
        let k = EXOTIC | LONG | LV | ((p - b'a') as u64) << 56 | (w as u64) << 48 | store.len() as u64;
        store.push([hi, lo]);
        return k;
    }
    intern(s)
}

#[inline(always)]
fn long_parts(k: u64) -> (u8, u64, u64) {
    if k & LV != 0 {
        let st = CLICK_STORE.get().expect("long click key without store");
        let [hi, lo] = st[((k >> 48) & 0xFF) as usize][(k & ((1 << 48) - 1)) as usize];
        return (b'a' + ((k >> 56) & 31) as u8, hi, lo);
    }
    let t = LONG_TABLE.get().expect("long key without table");
    let sl = &t.slots[(k & !(EXOTIC | LONG)) as usize];
    (sl.pre.load(Relaxed) as u8, sl.hi.load(Relaxed), sl.lo.load(Relaxed))
}

/// 33 bytes `[a-z][0-9a-f]{32}` -> (prefix, hi, lo); None for any other shape.
#[cfg(target_feature = "avx2")]
#[inline(always)]
pub fn parse_long(s: &[u8]) -> Option<(u8, u64, u64)> {
    use std::arch::x86_64::*;
    if s.len() != 33 || !s[0].is_ascii_lowercase() {
        return None;
    }
    unsafe {
        let v = _mm256_loadu_si256(s.as_ptr().add(1) as *const __m256i);
        let d = _mm256_sub_epi8(v, _mm256_set1_epi8(b'0' as i8));
        let l = _mm256_sub_epi8(v, _mm256_set1_epi8(b'a' as i8));
        let is_d = _mm256_cmpeq_epi8(_mm256_min_epu8(d, _mm256_set1_epi8(9)), d);
        let is_l = _mm256_cmpeq_epi8(_mm256_min_epu8(l, _mm256_set1_epi8(5)), l);
        if _mm256_movemask_epi8(_mm256_or_si256(is_d, is_l)) != -1 {
            return None;
        }
        // nibble = digit value, or letter value + 10
        let nib = _mm256_or_si256(_mm256_and_si256(is_d, d), _mm256_andnot_si256(is_d, _mm256_add_epi8(l, _mm256_set1_epi8(10))));
        // pairs (high nibble first) -> bytes in u16 lanes, then pack to 16 bytes in order
        let w = _mm256_maddubs_epi16(nib, _mm256_set1_epi16(0x0110));
        let packed = _mm256_packus_epi16(w, w); // per 128-bit lane: 8 bytes, repeated
        let a = _mm256_extract_epi64::<0>(packed) as u64; // bytes 0..8 of the value (lane 0)
        let b = _mm256_extract_epi64::<2>(packed) as u64; // bytes 8..16 (lane 1)
        Some((s[0], u64::from_be(a.to_le()), u64::from_be(b.to_le())))
    }
}

#[cfg(not(target_feature = "avx2"))]
pub fn parse_long(s: &[u8]) -> Option<(u8, u64, u64)> {
    if s.len() != 33 || !s[0].is_ascii_lowercase() {
        return None;
    }
    let mut v: u128 = 0;
    for &c in &s[1..] {
        let n = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            _ => return None,
        };
        v = (v << 4) | n as u128;
    }
    Some((s[0], (v >> 64) as u64, v as u64))
}

#[inline(always)]
fn exotic_key(s: &[u8]) -> u64 {
    if let Some((p, hi, lo)) = parse_long(s) {
        if let Some(k) = long_key(p, hi, lo) {
            return k;
        }
    }
    intern(s)
}

static CODE: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 10 {
        t[b'0' as usize + i] = 1 + i as u8;
        i += 1;
    }
    let mut i = 0;
    while i < 26 {
        t[b'a' as usize + i] = 11 + i as u8;
        i += 1;
    }
    t
};
static CHAR: [u8; 64] = {
    let mut t = [0u8; 64];
    let mut i = 0;
    while i < 10 {
        t[1 + i] = b'0' + i as u8;
        i += 1;
    }
    let mut i = 0;
    while i < 26 {
        t[11 + i] = b'a' + i as u8;
        i += 1;
    }
    t
};

struct Interner {
    map: HashMap<Vec<u8>, u64>,
    strs: Vec<Vec<u8>>,
}

fn interner() -> &'static Mutex<Interner> {
    static I: OnceLock<Mutex<Interner>> = OnceLock::new();
    I.get_or_init(|| Mutex::new(Interner { map: HashMap::new(), strs: Vec::new() }))
}

#[inline(always)]
pub fn pack(s: &[u8]) -> Option<u64> {
    let n = s.len();
    if n == 0 || n > 10 {
        return None;
    }
    let mut v: u64 = 0;
    for &b in s {
        let c = CODE[b as usize];
        if c == 0 {
            return None;
        }
        v = (v << 6) | c as u64;
    }
    Some(v << (6 * (10 - n)))
}

#[cold]
fn intern(s: &[u8]) -> u64 {
    let mut g = interner().lock().unwrap();
    if let Some(&k) = g.map.get(s) {
        return k;
    }
    let k = EXOTIC | g.strs.len() as u64;
    g.strs.push(s.to_vec());
    g.map.insert(s.to_vec(), k);
    k
}

/// Key of a non-empty byte string.
#[inline(always)]
pub fn key(s: &[u8]) -> u64 {
    match pack(s) {
        Some(k) => k,
        None => exotic_key(s),
    }
}

/// Marker key for a long hex event_id carried by value (prefix in the low byte; hi/lo travel beside
/// it). Never produced by `key`: interner indices and table slots stay far below bit 61.
pub const LONG_EID: u64 = EXOTIC | LONG | (1 << 61);

/// Event-id key: a long hex event_id is returned by value as (LONG_EID | prefix, hi, lo) instead of
/// going through the shared table (event_ids are only compared for equality, inside their dedup
/// partition). Reads 16 bytes like `key_padded`.
#[inline(always)]
pub unsafe fn eid_padded(s: &[u8]) -> (u64, u64, u64) {
    if let Some(k) = unsafe { pack16(s) } {
        return (k, 0, 0);
    }
    if let Some((p, hi, lo)) = parse_long(s) {
        return (LONG_EID | p as u64, hi, lo);
    }
    (intern(s), 0, 0)
}

/// Same as `key`, but reads 16 bytes from s.as_ptr(): the caller guarantees they are readable
/// (fields live in a buffer with zero padding after the data).
#[inline(always)]
pub unsafe fn key_padded(s: &[u8]) -> u64 {
    match unsafe { pack16(s) } {
        Some(k) => k,
        None => exotic_key(s),
    }
}

#[cfg(target_feature = "avx2")]
#[inline(always)]
unsafe fn pack16(s: &[u8]) -> Option<u64> {
    use std::arch::x86_64::*;
    let n = s.len();
    if n == 0 || n > 10 {
        return None;
    }
    unsafe {
        let v = _mm_loadu_si128(s.as_ptr() as *const __m128i);
        let idx = _mm_setr_epi8(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);
        let live = _mm_cmpgt_epi8(_mm_set1_epi8(n as i8), idx);
        // digit: v-'0' in 0..=9 ; letter: v-'a' in 0..=25 (unsigned compare via min)
        let d = _mm_sub_epi8(v, _mm_set1_epi8(b'0' as i8));
        let l = _mm_sub_epi8(v, _mm_set1_epi8(b'a' as i8));
        let is_d = _mm_cmpeq_epi8(_mm_min_epu8(d, _mm_set1_epi8(9)), d);
        let is_l = _mm_cmpeq_epi8(_mm_min_epu8(l, _mm_set1_epi8(25)), l);
        let ok = _mm_or_si128(is_d, is_l);
        if _mm_movemask_epi8(_mm_andnot_si128(ok, live)) != 0 {
            return None;
        }
        let code = _mm_or_si128(
            _mm_and_si128(is_d, _mm_add_epi8(d, _mm_set1_epi8(1))),
            _mm_andnot_si128(is_d, _mm_add_epi8(l, _mm_set1_epi8(11))),
        );
        let code = _mm_and_si128(code, live);
        let p = _mm_maddubs_epi16(code, _mm_setr_epi8(64, 1, 64, 1, 64, 1, 64, 1, 64, 1, 64, 1, 64, 1, 64, 1));
        let q = _mm_madd_epi16(p, _mm_setr_epi16(4096, 1, 4096, 1, 4096, 1, 4096, 1));
        let q0 = _mm_cvtsi128_si32(q) as u32 as u64;
        let q1 = _mm_extract_epi32::<1>(q) as u32 as u64;
        let q2 = _mm_extract_epi32::<2>(q) as u32 as u64;
        Some((q0 << 36) | (q1 << 12) | (q2 >> 12))
    }
}

#[cfg(not(target_feature = "avx2"))]
#[inline(always)]
unsafe fn pack16(s: &[u8]) -> Option<u64> {
    pack(s)
}

#[inline(always)]
pub fn bytes_into(k: u64, out: &mut Vec<u8>) {
    if k & EXOTIC == 0 {
        // canonical: non-zero codes then zero padding; length from the trailing zero 6-bit groups
        let len = if k == 0 { 0 } else { 10 - (k.trailing_zeros() / 6) as usize };
        out.reserve(10);
        unsafe {
            let p = out.as_mut_ptr().add(out.len());
            for i in 0..10 {
                *p.add(i) = CHAR[((k >> (54 - 6 * i)) & 63) as usize];
            }
            out.set_len(out.len() + len);
        }
    } else if k & LONG != 0 {
        let (p, hi, lo) = long_parts(k);
        out.push(p);
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for w in [hi, lo] {
            for i in (0..16).rev() {
                out.push(HEX[((w >> (4 * i)) & 15) as usize]);
            }
        }
    } else {
        let g = interner().lock().unwrap();
        out.extend_from_slice(&g.strs[(k & !EXOTIC) as usize]);
    }
}

#[inline]
pub fn cmp_key(a: u64, b: u64) -> Ordering {
    if (a | b) & EXOTIC == 0 {
        return a.cmp(&b);
    }
    cmp_slow(a, b)
}

fn cmp_slow(a: u64, b: u64) -> Ordering {
    // two long hex IDs: hex digits sort like their values, so (prefix, hi, lo) is byte order
    if a & b & LONG != 0 {
        return long_parts(a).cmp(&long_parts(b));
    }
    let (mut x, mut y) = (Vec::with_capacity(40), Vec::with_capacity(40));
    bytes_into(a, &mut x);
    bytes_into(b, &mut y);
    x.cmp(&y)
}

#[inline(always)]
pub fn hash(k: u64) -> u64 {
    let mut x = k;
    x ^= x >> 31;
    x = x.wrapping_mul(0x7fb5_d329_728e_a185);
    x ^= x >> 27;
    x = x.wrapping_mul(0x81da_def4_bc2d_d44d);
    x ^ (x >> 33)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn simd_pack_matches() {
        let mut x: u64 = 7;
        let mut rnd = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let alpha = b"0123456789abcdefghijklmnopqrstuvwxyzAZ_ /:`{\x00\xff";
        for _ in 0..500_000 {
            let n = (rnd() % 13) as usize;
            let mut buf = vec![0u8; 32];
            for i in 0..16 {
                buf[i] = alpha[(rnd() % alpha.len() as u64) as usize];
            }
            for i in 0..n {
                if rnd() % 4 != 0 {
                    buf[i] = alpha[(rnd() % 36) as usize];
                }
            }
            let s = &buf[..n];
            assert_eq!(unsafe { pack16(s) }, pack(s), "{:?}", s);
        }
    }
    fn scalar_long(s: &[u8]) -> Option<(u8, u64, u64)> {
        if s.len() != 33 || !s[0].is_ascii_lowercase() {
            return None;
        }
        let mut v: u128 = 0;
        for &c in &s[1..] {
            let n = match c {
                b'0'..=b'9' => c - b'0',
                b'a'..=b'f' => c - b'a' + 10,
                _ => return None,
            };
            v = (v << 4) | n as u128;
        }
        Some((s[0], (v >> 64) as u64, v as u64))
    }

    #[test]
    fn long_ids() {
        init_long(1 << 16);
        let mut x: u64 = 11;
        let mut rnd = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let alpha = b"0123456789abcdefABCDEFgz_ ";
        let mut words: Vec<Vec<u8>> = Vec::new();
        for _ in 0..20_000 {
            let n = if rnd() % 4 == 0 { (rnd() % 40) as usize } else { 33 };
            let mut w = vec![b"eukEz9"[(rnd() % 6) as usize]];
            for _ in 1..n.max(1) {
                let c = if rnd() % 50 == 0 { alpha[(rnd() % alpha.len() as u64) as usize] } else { b"0123456789abcdef"[(rnd() % 16) as usize] };
                w.push(c);
            }
            assert_eq!(parse_long(&w), scalar_long(&w), "{:?}", w);
            words.push(w);
        }
        words.push(b"u00000000000000000000000000000000".to_vec());
        words.push(b"u0000000000000000ffffffffffffffff".to_vec());
        words.push(b"u4k2nn2cj".to_vec());
        words.push(b"uffffffffffffffffffffffffffffffff".to_vec());
        let keys: Vec<u64> = words.iter().map(|w| key(w)).collect();
        for (w, &k) in words.iter().zip(&keys) {
            assert_eq!(key(w), k);
            let mut out = Vec::new();
            bytes_into(k, &mut out);
            assert_eq!(&out, w);
        }
        for i in 0..2000 {
            for j in 0..60 {
                let (a, b) = (i, (i * 7 + j * 13) % words.len());
                assert_eq!(cmp_key(keys[a], keys[b]), words[a].cmp(&words[b]));
                assert_eq!(keys[a] == keys[b], words[a] == words[b]);
            }
        }
    }

    #[test]
    fn order_and_roundtrip() {
        let words: Vec<&[u8]> = vec![
            b"u1", b"u10", b"u", b"a", b"zzzzzzzzzz", b"U1", b" u1", b"u1 ", b"A\tB", b"user_id_a", b"0", b"u2",
            b"abcdefghijk", b"e00", b"e0",
        ];
        for a in &words {
            let ka = key(a);
            let mut out = Vec::new();
            bytes_into(ka, &mut out);
            assert_eq!(&out[..], *a);
            for b in &words {
                assert_eq!(cmp_key(ka, key(b)), a.cmp(b), "{:?} {:?}", a, b);
            }
        }
    }
}
