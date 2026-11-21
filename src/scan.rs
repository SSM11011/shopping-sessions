//! Chunk loading (pread) and CSV line splitting with AVX2 bitmasks.

use std::fs::File;
use std::os::unix::fs::FileExt;

/// Zero bytes kept readable after the data, so 128-byte vector loads never leave the buffer.
pub const PAD: usize = 256;

/// Loads the lines that *start* in file range [s, e) into `buf`.
/// Returns (first line start, end of data) as buffer offsets; buf[data_end..data_end+PAD] is zeroed.
pub fn load(file: &File, flen: usize, s: usize, e: usize, buf: &mut Vec<u8>) -> (usize, usize) {
    let from = s.saturating_sub(1);
    let mut want = (e + 4096).min(flen) - from;
    loop {
        if buf.len() < want + PAD {
            buf.resize(want + PAD, 0);
        }
        file.read_exact_at(&mut buf[..want], from as u64).expect("read");
        // the last line starting before e must end inside the buffer (or at EOF)
        let stop = e - from; // lines must start before this offset
        let tail_ok = from + want == flen || memchr::memchr(b'\n', &buf[stop - 1..want]).is_some();
        if tail_ok {
            break;
        }
        want = (want * 2).min(flen - from);
    }
    let stop = e - from;
    let data_end = match memchr::memchr(b'\n', &buf[stop - 1..want]) {
        Some(p) => stop - 1 + p + 1,
        None => want,
    };
    buf[data_end..data_end + PAD].fill(0);
    let start = if s == 0 {
        // skip the header line
        match memchr::memchr(b'\n', &buf[..data_end]) {
            Some(p) => p + 1,
            None => data_end,
        }
    } else {
        // buf[0] is byte s-1: the first line starting at or after s follows the first '\n' at or after s-1
        match memchr::memchr(b'\n', &buf[..data_end]) {
            Some(p) => p + 1,
            None => data_end,
        }
    };
    (start, data_end)
}

#[inline(always)]
fn unquote(f: &[u8]) -> Option<&[u8]> {
    if memchr::memchr(b'"', f).is_none() {
        return Some(f);
    }
    let n = f.len();
    if n >= 2 && f[0] == b'"' && f[n - 1] == b'"' && memchr::memchr(b'"', &f[1..n - 1]).is_none() {
        Some(&f[1..n - 1])
    } else {
        None
    }
}

/// Scalar split of one line into exactly N unquoted fields.
#[inline]
pub fn split<'a, const N: usize>(line: &'a [u8], out: &mut [&'a [u8]; N]) -> bool {
    let mut n = 0;
    let mut start = 0;
    let len = line.len();
    loop {
        let end = match memchr::memchr(b',', &line[start..]) {
            Some(p) => start + p,
            None => len,
        };
        if n == N {
            return false;
        }
        match unquote(&line[start..end]) {
            Some(f) => out[n] = f,
            None => return false,
        }
        n += 1;
        if end == len {
            return n == N;
        }
        start = end + 1;
    }
}

#[cfg(target_feature = "avx2")]
#[inline(always)]
unsafe fn masks64(p: *const u8) -> (u64, u64, u64) {
    use std::arch::x86_64::*;
    unsafe {
        let a = _mm256_loadu_si256(p as *const __m256i);
        let b = _mm256_loadu_si256(p.add(32) as *const __m256i);
        let m = |v: __m256i, c: u8| -> u64 {
            (_mm256_movemask_epi8(_mm256_cmpeq_epi8(v, _mm256_set1_epi8(c as i8))) as u32) as u64
        };
        (
            m(a, b'\n') | (m(b, b'\n') << 32),
            m(a, b',') | (m(b, b',') << 32),
            m(a, b'"') | (m(b, b'"') << 32),
        )
    }
}

#[cfg(not(target_feature = "avx2"))]
#[inline(always)]
unsafe fn masks64(p: *const u8) -> (u64, u64, u64) {
    let (mut n, mut c, mut q) = (0u64, 0u64, 0u64);
    for i in 0..64 {
        let b = unsafe { *p.add(i) };
        n |= ((b == b'\n') as u64) << i;
        c |= ((b == b',') as u64) << i;
        q |= ((b == b'"') as u64) << i;
    }
    (n, c, q)
}

/// Calls `f` with the N fields of every well-formed line starting in buf[start..end).
/// Requires buf.len() >= end + PAD and buf[end..end+PAD] zeroed.
#[inline(always)]
pub fn lines<'a, const N: usize>(buf: &'a [u8], start: usize, end: usize, mut f: impl FnMut(&[&'a [u8]; N])) {
    debug_assert!(buf.len() >= end + PAD);
    let p = buf.as_ptr();
    let mut at = start;
    let mut fields: [&'a [u8]; N] = [&buf[0..0]; N];
    while at < end {
        let (n0, c0, q0) = unsafe { masks64(p.add(at)) };
        let (len, cm, qt): (usize, u128, u128);
        if n0 != 0 {
            len = n0.trailing_zeros() as usize;
            cm = c0 as u128;
            qt = q0 as u128;
        } else {
            let (n1, c1, q1) = unsafe { masks64(p.add(at + 64)) };
            if n1 != 0 {
                len = 64 + n1.trailing_zeros() as usize;
                cm = c0 as u128 | ((c1 as u128) << 64);
                qt = q0 as u128 | ((q1 as u128) << 64);
            } else {
                // long line (or one running into the zero padding at the very end)
                let nl = match memchr::memchr(b'\n', &buf[at..end]) {
                    Some(q) => at + q,
                    None => end,
                };
                let line = &buf[at..nl];
                if !line.is_empty() && split(line, &mut fields) {
                    f(&fields);
                }
                at = nl + 1;
                continue;
            }
        }
        // a line that reaches the end of data without '\n' (EOF) stops at end: padding is zero so the
        // newline search above already stopped at the first padding byte only if it found '\n'; clamp.
        let len = len.min(end - at);
        let lm: u128 = (1u128 << len) - 1; // len < 128 here
        let cm = cm & lm;
        let qt = qt & lm;
        if len == 0 || cm.count_ones() as usize != N - 1 {
            at += len + 1;
            continue;
        }
        let mut ok = true;
        let mut a = 0usize;
        let mut rest = cm;
        for (i, slot) in fields.iter_mut().enumerate() {
            let b = if i + 1 < N {
                let t = rest.trailing_zeros() as usize;
                rest &= rest - 1;
                t
            } else {
                len
            };
            let fq = (qt >> a) & ((1u128 << (b - a)) - 1);
            if fq == 0 {
                *slot = unsafe { buf.get_unchecked(at + a..at + b) };
            } else if b - a >= 2 && fq == (1 | (1u128 << (b - a - 1))) {
                *slot = unsafe { buf.get_unchecked(at + a + 1..at + b - 1) };
            } else {
                ok = false;
                break;
            }
            a = b + 1;
        }
        if ok {
            f(&fields);
        }
        at += len + 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn run<const N: usize>(data: &[u8]) -> Vec<Vec<Vec<u8>>> {
        let mut buf = data.to_vec();
        buf.extend_from_slice(&[0u8; PAD]);
        let mut out = Vec::new();
        lines::<N>(&buf, 0, data.len(), |f| out.push(f.iter().map(|x| x.to_vec()).collect()));
        out
    }
    #[test]
    fn splitting() {
        let long = format!("{},b\n", "x".repeat(300));
        let mid = format!("{},\"{}\"\n", "y".repeat(70), "z".repeat(40));
        let data = format!(
            "a,b\n\"a\",\"\"\na,b,c\n\nab\na\"b,c\n\"a,b\"\n{long}{mid}\"\",x\r\nlast,line"
        );
        let got = run::<2>(data.as_bytes());
        let want: Vec<Vec<&[u8]>> = vec![
            vec![b"a", b"b"],
            vec![b"a", b""],
            vec!["x".repeat(300).as_bytes().to_vec().leak(), b"b"],
            vec!["y".repeat(70).as_bytes().to_vec().leak(), "z".repeat(40).as_bytes().to_vec().leak()],
            vec![b"", b"x\r"],
            vec![b"last", b"line"],
        ];
        let want: Vec<Vec<Vec<u8>>> = want.into_iter().map(|r| r.into_iter().map(|x| x.to_vec()).collect()).collect();
        assert_eq!(got, want);
        // scalar and vector agree on every line
        for line in data.split('\n') {
            let mut f = [&b""[..]; 2];
            let s = split(line.as_bytes(), &mut f);
            let v = run::<2>(format!("{line}\n").as_bytes());
            assert_eq!(s, v.len() == 1, "{line:?}");
        }
    }
}
