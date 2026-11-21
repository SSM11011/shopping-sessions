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

pub fn bytes_into(k: u64, out: &mut Vec<u8>) {
    if k & EXOTIC == 0 {
        for i in 0..10 {
            let c = (k >> (54 - 6 * i)) & 63;
            if c == 0 {
                break;
            }
            out.push(CHAR[c as usize]);
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
