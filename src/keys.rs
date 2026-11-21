//! IDs as u64 keys. A canonical ID (1..=10 bytes, each in [0-9a-z]) packs into 60 bits as 6-bit codes,
//! big-endian and zero-padded, so integer order == byte order. Anything else is "exotic": bit 63 set and
//! an index into a global intern table. Comparisons involving an exotic key go through the bytes.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

pub const EXOTIC: u64 = 1 << 63;
pub const EMPTY: u64 = u64::MAX;

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
        None => intern(s),
    }
}

/// Same as `key`, but reads 16 bytes from s.as_ptr(): the caller guarantees they are readable
/// (fields live in a buffer with zero padding after the data).
#[inline(always)]
pub unsafe fn key_padded(s: &[u8]) -> u64 {
    match unsafe { pack16(s) } {
        Some(k) => k,
        None => intern(s),
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

#[cold]
fn cmp_slow(a: u64, b: u64) -> Ordering {
    let (mut x, mut y) = (Vec::new(), Vec::new());
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
