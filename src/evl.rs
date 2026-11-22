//! Event lines: split and field parse fused into one pass per line.
//!
//! A line shorter than 127 bytes is classified from four 32-byte vectors (commas, quotes), its quoting
//! validated with the mask formula of `scan`, its six commas located with pdep, and its fields parsed
//! in place: both timestamps through one vector pass (`ts::Fast::f1_pair`), F3 by SWAR. Anything off
//! the fast shapes falls back to the exact general code (`ts::Fast::parse`, `keys`, `parse_event`).

use crate::{keys, parse_event, parse_price, scan, ts, Ctx, Ev, DAY, NO_TS};

#[cfg(target_feature = "avx2")]
#[inline(always)]
unsafe fn masks128(p: *const u8) -> (u128, u128) {
    use std::arch::x86_64::*;
    unsafe {
        let m = |v: __m256i, ch: u8| -> u64 { (_mm256_movemask_epi8(_mm256_cmpeq_epi8(v, _mm256_set1_epi8(ch as i8))) as u32) as u64 };
        let a = _mm256_loadu_si256(p as *const __m256i);
        let b = _mm256_loadu_si256(p.add(32) as *const __m256i);
        let c = _mm256_loadu_si256(p.add(64) as *const __m256i);
        let d = _mm256_loadu_si256(p.add(96) as *const __m256i);
        let cl = m(a, b',') | (m(b, b',') << 32);
        let ch = m(c, b',') | (m(d, b',') << 32);
        let ql = m(a, b'"') | (m(b, b'"') << 32);
        let qh = m(c, b'"') | (m(d, b'"') << 32);
        (cl as u128 | ((ch as u128) << 64), ql as u128 | ((qh as u128) << 64))
    }
}

#[inline(always)]
fn slow_line(ctx: &Ctx, tsf: &mut ts::Fast, line: &[u8], ing_base: i64, sink: &mut impl FnMut(Ev)) {
    let mut f = [&line[0..0]; 7];
    if scan::split::<7>(line, &mut f) {
        if let Some(ev) = parse_event(ctx, tsf, &f, ing_base) {
            sink(ev);
        }
    }
}

/// A split line: offset and, per field, content start (relative) and length.
#[derive(Clone, Copy)]
#[repr(C, align(32))]
struct Row {
    s: u32,
    st: [u8; 7],
    n: [u8; 7],
}

const NOROW: Row = Row { s: 0, st: [0; 7], n: [0; 7] };

/// Splits line [s, s + len), len < 127, into `row` (a malformed line gets n[0] = 0).
#[inline(always)]
fn split_row(buf: &[u8], s: usize, len: usize, row: &mut Row) {
    use std::arch::x86_64::_pdep_u64;
    let p = unsafe { buf.as_ptr().add(s) };
    let (c, q) = unsafe { masks128(p) };
    let lm: u128 = (1u128 << len) - 1;
    let (c, q) = (c & lm, q & lm);
    // quoting (see scan::split_short)
    let starts = (c << 1) | 1;
    let lasts = (c | (1u128 << len)) >> 1;
    let qa = q & starts;
    let qb = q & lasts;
    let ok = (c.count_ones() == 6) & ((q ^ (qa | qb)) | (qa & qb) | (qb.wrapping_sub(qa) & (c | lasts)) == 0);
    let (mut lo, mut hi) = (c as u64, (c >> 64) as u64);
    let k = lo.count_ones() as usize;
    let mut pp = [0u32; 16];
    for j in 0..6 {
        pp[j] = lo.trailing_zeros();
        lo &= lo.wrapping_sub(1);
    }
    for j in 0..6 {
        unsafe { *pp.get_unchecked_mut(k + j) = 64 + hi.trailing_zeros() };
        hi &= hi.wrapping_sub(1);
    }
    let pos = |i: u32| pp[i as usize] as usize;
    let ends = [pos(0), pos(1), pos(2), pos(3), pos(4), pos(5), len];
    row.s = s as u32;
    let mut fs = 0usize;
    for i in 0..7 {
        let qd = (unsafe { *p.add(fs & 127) } == b'"') as usize;
        row.st[i] = (fs + qd) as u8;
        row.n[i] = (ends[i].wrapping_sub(fs).wrapping_sub(2 * qd)) as u8;
        fs = ends[i] + 1;
    }
    // a malformed line reads as an empty event_id: no candidate
    row.n[0] &= (ok as u8).wrapping_neg();
}

/// Parses one split line; same result as `parse_event` on its fields.
#[inline(always)]
fn parse_row<const M: u32>(buf: &[u8], row: &Row, ctx: &Ctx, tsf: &mut ts::Fast, ing_base: i64, sink: &mut impl FnMut(Ev)) {
    let s = row.s as usize;
    let p = unsafe { buf.as_ptr().add(s) };
    let st = row.st.map(|x| x as usize);
    let n = row.n.map(|x| x as usize);
    if M == 1 {
        sink(Ev { eid: (st[0] ^ n[0] ^ st[1] ^ n[1] ^ st[2] ^ n[2] ^ st[3] ^ n[3]) as u64, ehi: (st[4] ^ n[4] ^ st[5] ^ n[5] ^ st[6] ^ n[6]) as u64, elo: 0, user: 0, uhi: 0, ulo: 0, ingest: 0, ts: 0, pc: 0 });
        return;
    }
    let fld = |i: usize| -> &[u8] { unsafe { buf.get_unchecked(s + st[i]..s + st[i] + n[i]) } };
    if n[0] == 0 {
        return;
    }
    let (eid, ehi, elo) = unsafe { keys::eid_padded(fld(0)) };
    if M == 2 {
        sink(Ev { eid, ehi, elo, user: 0, uhi: 0, ulo: 0, ingest: 0, ts: 0, pc: 0 });
        return;
    }
    let (p2, p3) = unsafe { (p.add(st[2]), p.add(st[3])) };
    let (t1, ok1, im, iok) = unsafe { tsf.f1_pair(p2, n[2], p3, n[3]) };
    if M == 3 {
        sink(Ev { eid, ehi, elo, user: t1 as u64, uhi: im as u64, ulo: 0, ingest: ok1 as u32, ts: iok as u32, pc: 0 });
        return;
    }
    let ingest = if iok { Some(im) } else { tsf.parse(fld(3)) };
    let (ingest, ing32) = match ingest {
        Some(v) => {
            let secs = if v >= 0 { v / 1000 } else { v.div_euclid(1000) };
            let rel = secs - ing_base;
            (v, if (rel as u64) < u32::MAX as u64 { rel as u32 } else { rel.clamp(0, u32::MAX as i64 - 1) as u32 })
        }
        None => (i64::MAX, u32::MAX),
    };
    let mut ev = Ev { eid, ehi, elo, user: 0, uhi: 0, ulo: 0, ingest: ing32, ts: NO_TS, pc: 0 };
    if let Some((t, price, cat)) = valid(ctx, tsf, &fld, &n, p2, t1, ok1, ingest) {
        (ev.user, ev.uhi, ev.ulo) = unsafe { keys::key_deferred(fld(1)) };
        ev.ts = t;
        ev.pc = price | (cat << 24);
    }
    sink(ev);
}

/// `validate` on the located fields, with the F1 / F3 fast results for ts.
#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn valid<'a>(
    ctx: &Ctx,
    tsf: &mut ts::Fast,
    fld: &impl Fn(usize) -> &'a [u8],
    n: &[usize; 7],
    p2: *const u8,
    t1: i64,
    ok1: bool,
    ingest: i64,
) -> Option<(u32, u32, u32)> {
    if n[1] == 0 || n[5] == 0 {
        return None;
    }
    let (t3, ok3) = unsafe { ts::digits13p(p2) };
    let fast = ok1 | ((n[2] == 13) & ok3);
    let tv = if ok1 { t1 } else { t3 };
    let t = if fast { tv } else { tsf.parse(fld(2))? };
    if t < ctx.ws || t >= ctx.we {
        return None;
    }
    let ty = fld(4);
    let price = match ty.len() {
        4 => {
            let w = u32::from_le_bytes([ty[0], ty[1], ty[2], ty[3]]);
            if w == u32::from_le_bytes(*b"view") || w == u32::from_le_bytes(*b"cart") {
                0
            } else {
                return None;
            }
        }
        8 => {
            let w = u64::from_le_bytes([ty[0], ty[1], ty[2], ty[3], ty[4], ty[5], ty[6], ty[7]]);
            if w == u64::from_le_bytes(*b"purchase") {
                parse_price(fld(6))?
            } else {
                return None;
            }
        }
        _ => return None,
    };
    if ingest - t > DAY {
        return None;
    }
    let cat = if price > 0 { unsafe { ctx.products.get_padded(fld(5)) }.unwrap_or(ctx.unknown_cat) } else { 0 };
    Some(((t - ctx.ws) as u32, price, cat))
}

/// Lines split per batch before any field is parsed: the split loop has no data-dependent branch.
const RB: usize = 32;

/// `scan::lines::<7>` + `parse_event` over buf[start..end): every event candidate goes to `sink`.
#[inline(always)]
pub fn event_lines<const M: u32>(buf: &[u8], start: usize, end: usize, ctx: &Ctx, tsf: &mut ts::Fast, ing_base: i64, mut sink: impl FnMut(Ev)) {
    debug_assert!(buf.len() >= end + scan::PAD);
    let mut line_start = start;
    let mut rows = [NOROW; RB + 1];
    let mut cnt = 0usize;
    scan::line_ends(buf, start, end, |e| {
        let s = line_start;
        line_start = e + 1;
        let len = e - s;
        if len < 127 {
            split_row(buf, s, len, &mut rows[cnt]);
            cnt += 1;
            if cnt == RB {
                for r in &rows[..RB] {
                    parse_row::<M>(buf, r, ctx, tsf, ing_base, &mut sink);
                }
                cnt = 0;
            }
        } else {
            slow_line(ctx, tsf, &buf[s..e], ing_base, &mut sink);
        }
    });
    for r in &rows[..cnt] {
        parse_row::<M>(buf, r, ctx, tsf, ing_base, &mut sink);
    }
    if line_start < end {
        // unterminated last line of the file
        slow_line(ctx, tsf, &buf[line_start..end], ing_base, &mut sink);
    }
}

#[inline(never)]
pub fn mb_split<const V: u32>(buf: &[u8], start: usize, end: usize) -> u64 {
    let mut acc = 0u64;
    let mut line_start = start;
    scan::line_ends(buf, start, end, |e| {
        let s = line_start;
        line_start = e + 1;
        let len = e - s;
        if V == 0 {
            acc = acc.wrapping_add(len as u64);
            return;
        }
        if len >= 127 {
            return;
        }
        let p = unsafe { buf.as_ptr().add(s) };
        let (c, q) = unsafe { masks128(p) };
        let lm: u128 = (1u128 << len) - 1;
        let (c, q) = (c & lm, q & lm);
        if V == 1 {
            acc = acc.wrapping_add(c.count_ones() as u64 ^ q.count_ones() as u64);
            return;
        }
        let starts = (c << 1) | 1;
        let lasts = (c | (1u128 << len)) >> 1;
        let qa = q & starts;
        let qb = q & lasts;
        let ok = (c.count_ones() == 6) & ((q ^ (qa | qb)) | (qa & qb) | (qb.wrapping_sub(qa) & (c | lasts)) == 0);
        if V == 2 {
            acc = acc.wrapping_add(ok as u64);
            return;
        }
        let mut r = NOROW;
        split_row(buf, s, len, &mut r);
        if V == 3 {
            acc = acc.wrapping_add(r.st[6] as u64 ^ r.n[0] as u64);
            return;
        }
        acc = acc.wrapping_add(u64::from_le_bytes([r.st[0], r.st[1], r.st[2], r.st[3], r.st[4], r.st[5], r.st[6], 0]) ^ u64::from_le_bytes([r.n[0], r.n[1], r.n[2], r.n[3], r.n[4], r.n[5], r.n[6], 0]));
    });
    acc
}

#[inline(never)]
pub fn mb_fused<const M: u32>(buf: &[u8], start: usize, end: usize, ctx: &Ctx, tsf: &mut ts::Fast, ib: i64) -> u64 {
    let mut c = 0u64;
    event_lines::<M>(buf, start, end, ctx, tsf, ib, #[inline(always)] |v| c = c.wrapping_add(v.eid ^ v.user ^ v.ts as u64 ^ v.pc as u64 ^ v.ingest as u64 ^ v.uhi ^ v.ehi));
    c
}

/// Newline, comma and quote masks of the 128 bytes at p.
#[cfg(target_feature = "avx2")]
#[inline(always)]
unsafe fn masks3(p: *const u8) -> ([u64; 2], [u64; 2], [u64; 2]) {
    use std::arch::x86_64::*;
    unsafe {
        let m = |v: __m256i, ch: u8| -> u64 { (_mm256_movemask_epi8(_mm256_cmpeq_epi8(v, _mm256_set1_epi8(ch as i8))) as u32) as u64 };
        let a = _mm256_loadu_si256(p as *const __m256i);
        let b = _mm256_loadu_si256(p.add(32) as *const __m256i);
        let c = _mm256_loadu_si256(p.add(64) as *const __m256i);
        let d = _mm256_loadu_si256(p.add(96) as *const __m256i);
        (
            [m(a, b'\n') | (m(b, b'\n') << 32), m(c, b'\n') | (m(d, b'\n') << 32)],
            [m(a, b',') | (m(b, b',') << 32), m(c, b',') | (m(d, b',') << 32)],
            [m(a, b'"') | (m(b, b'"') << 32), m(c, b'"') | (m(d, b'"') << 32)],
        )
    }
}

/// Same as `event_lines`, one chained pass: each line's 128-byte window gives its end and its masks.
#[inline(always)]
pub fn event_lines2<const M: u32>(buf: &[u8], start: usize, end: usize, ctx: &Ctx, tsf: &mut ts::Fast, ing_base: i64, mut sink: impl FnMut(Ev)) {
    use std::arch::x86_64::{_bzhi_u64, _blsi_u64};
    debug_assert!(buf.len() >= end + scan::PAD);
    let mut s = start;
    while s < end {
        let p = unsafe { buf.as_ptr().add(s) };
        let (nl, c, q) = unsafe { masks3(p) };
        // line length = first newline; none in 128 bytes (long or unterminated line): general path
        let len = if nl[0] != 0 { nl[0].trailing_zeros() } else { 64 + nl[1].trailing_zeros() } as usize;
        if len >= 127 || s + len > end {
            let e = memchr::memchr(b'\n', &buf[s..end]).map_or(end, |x| s + x);
            slow_line(ctx, tsf, &buf[s..e], ing_base, &mut sink);
            s = e + 1;
            continue;
        }
        let next = s + len + 1;
        let hl = len.max(64) as u32 - 64;
        let (cl, ch) = unsafe { (_bzhi_u64(c[0], len as u32), _bzhi_u64(c[1], hl)) };
        let (ql, qh) = unsafe { (_bzhi_u64(q[0], len as u32), _bzhi_u64(q[1], hl)) };
        let k = cl.count_ones();
        let cnt = k + ch.count_ones();
        // quoting (see scan::split_short), on (lo, hi) words; E = commas + the line end
        let c128 = cl as u128 | ((ch as u128) << 64);
        let q128 = ql as u128 | ((qh as u128) << 64);
        let nb = unsafe { _blsi_u64(nl[0]) as u128 | (((if nl[0] == 0 { _blsi_u64(nl[1]) } else { 0 }) as u128) << 64) };
        let starts = (c128 << 1) | 1;
        let lasts = (c128 | nb) >> 1;
        let qa = q128 & starts;
        let qb = q128 & lasts;
        let bad = (q128 ^ (qa | qb)) | (qa & qb) | (qb.wrapping_sub(qa) & (c128 | lasts));
        s = next;
        if (cnt != 6) | (bad != 0) {
            continue;
        }
        // comma positions: low word first, then the high word's after them
        let mut pos = [0u32; 16];
        let (mut lo, mut hi) = (cl, ch);
        for j in 0..6 {
            pos[j] = lo.trailing_zeros();
            lo &= lo.wrapping_sub(1);
        }
        for j in 0..6 {
            unsafe { *pos.get_unchecked_mut(k as usize + j) = 64 + hi.trailing_zeros() };
            hi &= hi.wrapping_sub(1);
        }
        let mut st = [0usize; 7];
        let mut n = [0usize; 7];
        let mut fs = 0usize;
        for i in 0..7 {
            let fe = if i < 6 { pos[i] as usize } else { len };
            let qd = (unsafe { *p.add(fs) } == b'"') as usize;
            st[i] = fs + qd;
            n[i] = fe - fs - 2 * qd;
            fs = fe + 1;
        }
        let row = Row { s: (next - len - 1) as u32, st: st.map(|x| x as u8), n: n.map(|x| x as u8) };
        parse_row::<M>(buf, &row, ctx, tsf, ing_base, &mut sink);
    }
}

#[inline(never)]
pub fn mb_fused2<const M: u32>(buf: &[u8], start: usize, end: usize, ctx: &Ctx, tsf: &mut ts::Fast, ib: i64) -> u64 {
    let mut c = 0u64;
    event_lines2::<M>(buf, start, end, ctx, tsf, ib, #[inline(always)] |v| c = c.wrapping_add(v.eid ^ v.user ^ v.ts as u64 ^ v.pc as u64 ^ v.ingest as u64 ^ v.uhi ^ v.ehi));
    c
}

/// A line shape: its comma (+ newline) and quote masks, and where its fields are. Lines repeat a few
/// thousand shapes, so a hit skips quote validation and field extraction.
#[derive(Clone, Copy)]
#[repr(C, align(64))]
pub struct Shape {
    key: [u64; 4],
    st: [u8; 8],
    n: [u8; 8],
}

pub const SHAPE_BITS: u32 = 12;

pub fn new_shapes() -> Vec<Shape> {
    vec![Shape { key: [0; 4], st: [0; 8], n: [0; 8] }; 1 << SHAPE_BITS]
}

#[inline(always)]
#[allow(clippy::too_many_arguments)]
pub fn event_lines3<const M: u32>(
    buf: &[u8],
    start: usize,
    end: usize,
    ctx: &Ctx,
    tsf: &mut ts::Fast,
    shapes: &mut [Shape],
    ing_base: i64,
    mut sink: impl FnMut(Ev),
) {
    use std::arch::x86_64::{_blsi_u64, _bzhi_u64};
    debug_assert!(buf.len() >= end + scan::PAD);
    debug_assert!(shapes.len() == 1 << SHAPE_BITS);
    let mut s = start;
    while s < end {
        let p = unsafe { buf.as_ptr().add(s) };
        let (nl, c, q) = unsafe { masks3(p) };
        let len = if nl[0] != 0 { nl[0].trailing_zeros() } else { 64 + nl[1].trailing_zeros() } as usize;
        if len >= 127 || s + len > end {
            let e = memchr::memchr(b'\n', &buf[s..end]).map_or(end, |x| s + x);
            slow_line(ctx, tsf, &buf[s..e], ing_base, &mut sink);
            s = e + 1;
            continue;
        }
        let ls = s;
        s += len + 1;
        let hl = len.max(64) as u32 - 64;
        // key: commas + the line-end bit (so it fixes the length), quotes
        let (nb0, nb1) = unsafe { (_blsi_u64(nl[0]), if nl[0] == 0 { _blsi_u64(nl[1]) } else { 0 }) };
        let k = unsafe { [_bzhi_u64(c[0], len as u32) | nb0, _bzhi_u64(c[1], hl) | nb1, _bzhi_u64(q[0], len as u32), _bzhi_u64(q[1], hl)] };
        let h = ((k[0] ^ k[1].rotate_left(23) ^ k[2].rotate_left(41) ^ k[3].rotate_left(7)).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (64 - SHAPE_BITS)) as usize;
        let e = unsafe { shapes.get_unchecked_mut(h) };
        if (e.key[0] ^ k[0]) | (e.key[1] ^ k[1]) | (e.key[2] ^ k[2]) | (e.key[3] ^ k[3]) != 0 && !new_shape(p, len, k, e) {
            continue;
        }
        if M == 7 {
            let f: [&[u8]; 7] = std::array::from_fn(|i| unsafe { buf.get_unchecked(ls + e.st[i] as usize..ls + e.st[i] as usize + e.n[i] as usize) });
            if let Some(ev) = parse_event(ctx, tsf, &f, ing_base) {
                sink(ev);
            }
            continue;
        }
        if M == 0 && e.st[7] != 0 && fast_row(p, e, ctx, tsf, ing_base, &mut sink) {
            continue;
        }
        let row = Row { s: ls as u32, st: [e.st[0], e.st[1], e.st[2], e.st[3], e.st[4], e.st[5], e.st[6]], n: [e.n[0], e.n[1], e.n[2], e.n[3], e.n[4], e.n[5], e.n[6]] };
        parse_row::<M>(buf, &row, ctx, tsf, ing_base, &mut sink);
    }
}

/// Validates a line of a shape not in the cache (masks `k` as in `event_lines3`) and fills `e`.
/// False if the line is malformed (then `e` is left alone).
pub static MISSES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static FASTMISS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[inline(never)]
fn new_shape(p: *const u8, len: usize, k: [u64; 4], e: &mut Shape) -> bool {
    MISSES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let ce = k[0] as u128 | ((k[1] as u128) << 64);
    let q = k[2] as u128 | ((k[3] as u128) << 64);
    let nb = 1u128 << len;
    let c = ce ^ nb;
    if c.count_ones() != 6 {
        return false;
    }
    let starts = (c << 1) | 1;
    let lasts = ce >> 1;
    let qa = q & starts;
    let qb = q & lasts;
    if (q ^ (qa | qb)) | (qa & qb) | (qb.wrapping_sub(qa) & (c | lasts)) != 0 {
        return false;
    }
    let mut fs = 0usize;
    let mut m = c;
    for i in 0..7 {
        let fe = if i < 6 { m.trailing_zeros() as usize } else { len };
        m &= m.wrapping_sub(1);
        let qd = (unsafe { *p.add(fs) } == b'"') as usize;
        e.st[i] = (fs + qd) as u8;
        e.n[i] = (fe - fs - 2 * qd) as u8;
        fs = fe + 1;
    }
    // fast-path kind: short ids, ingest_ts F1-sized, view/cart-sized type, a product; ts F1 (1) or F3 (2)
    let n = e.n;
    let ids = (1..=10).contains(&n[0]) && (1..=10).contains(&n[1]);
    e.st[7] = if ids && n[3] == 20 && n[4] == 4 && n[5] > 0 {
        match n[2] {
            20 => 1,
            13 => 2,
            _ => 0,
        }
    } else {
        0
    };
    e.key = k;
    true
}

/// The common line, in one straight pass: short ids packed together, both timestamps in one vector
/// pass, a 4-byte event type. False (nothing emitted) when any field is off that shape; the caller
/// then takes the general path.
#[inline(always)]
fn fast_row(p: *const u8, e: &Shape, ctx: &Ctx, tsf: &mut ts::Fast, ing_base: i64, sink: &mut impl FnMut(Ev)) -> bool {
    let st = |i: usize| unsafe { p.add(e.st[i] as usize) };
    let (eid, user, oka, okb) = unsafe { keys::pack2(st(0), st(1), e.n[0], e.n[1]) };
    let ok = oka & okb;
    let (p2, p3) = (st(2), st(3));
    let (t1, ok1, im, iok) = unsafe { tsf.f1_pair(p2, 20, p3, 20) };
    let (t3, ok3) = unsafe { ts::digits13p(p2) };
    let f3 = e.st[7] == 2;
    let t = if f3 { t3 } else { t1 };
    let ty = unsafe { (st(4) as *const u32).read_unaligned() };
    let vc = (ty == u32::from_le_bytes(*b"view")) | (ty == u32::from_le_bytes(*b"cart"));
    // every check of the general path, for this shape: valid ids, both timestamps, type, window, late
    let good = ok & iok & (if f3 { ok3 } else { ok1 }) & vc & (t >= ctx.ws) & (t < ctx.we) & (im - t <= DAY);
    if !good {
        FASTMISS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return false;
    }
    let secs = im / 1000;
    let rel = secs - ing_base;
    let ing32 = if (rel as u64) < u32::MAX as u64 { rel as u32 } else { rel.clamp(0, u32::MAX as i64 - 1) as u32 };
    sink(Ev { eid, ehi: 0, elo: 0, user, uhi: 0, ulo: 0, ingest: ing32, ts: (t - ctx.ws) as u32, pc: 0 });
    true
}

#[inline(never)]
pub fn mb_fused3<const M: u32>(buf: &[u8], start: usize, end: usize, ctx: &Ctx, tsf: &mut ts::Fast, sh: &mut [Shape], ib: i64) -> u64 {
    let mut c = 0u64;
    event_lines3::<M>(buf, start, end, ctx, tsf, sh, ib, #[inline(always)] |v| c = c.wrapping_add(v.eid ^ v.user ^ v.ts as u64 ^ v.pc as u64 ^ v.ingest as u64 ^ v.uhi ^ v.ehi));
    c
}

/// Lines classified per batch before any is parsed (the classify loop's only cross-line dependency
/// is the line start; the shape-table loads of a batch overlap).
const LB: usize = 16;

#[derive(Clone, Copy)]
#[repr(C, align(64))]
struct Pending {
    key: [u64; 4],
    s: u32,
    len: u32,
    h: u32,
}

#[inline(always)]
#[allow(clippy::too_many_arguments)]
pub fn event_lines4<const M: u32>(
    buf: &[u8],
    start: usize,
    end: usize,
    ctx: &Ctx,
    tsf: &mut ts::Fast,
    shapes: &mut [Shape],
    ing_base: i64,
    mut sink: impl FnMut(Ev),
) {
    use std::arch::x86_64::_bzhi_u64;
    debug_assert!(buf.len() >= end + scan::PAD);
    debug_assert!(shapes.len() == 1 << SHAPE_BITS);
    let mut pend = [Pending { key: [0; 4], s: 0, len: 0, h: 0 }; LB];
    let mut nb = 0usize;
    let mut line_start = start;
    scan::line_ends(buf, start, end, |e| {
        let s = line_start;
        line_start = e + 1;
        let len = e - s;
        if len >= 127 {
            slow_line(ctx, tsf, &buf[s..e], ing_base, &mut sink);
            return;
        }
        let (c, q) = unsafe { masks128(unsafe { buf.as_ptr().add(s) }) };
        let hl = len.max(64) as u32 - 64;
        let nb0 = if len < 64 { 1u64 << len } else { 0 };
        let nb1 = if len >= 64 { 1u64 << (len - 64) } else { 0 };
        let k = unsafe { [_bzhi_u64(c as u64, len as u32) | nb0, _bzhi_u64((c >> 64) as u64, hl) | nb1, _bzhi_u64(q as u64, len as u32), _bzhi_u64((q >> 64) as u64, hl)] };
        let h = ((k[0] ^ k[1].rotate_left(23) ^ k[2].rotate_left(41) ^ k[3].rotate_left(7)).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (64 - SHAPE_BITS)) as usize;
        unsafe { std::arch::x86_64::_mm_prefetch(shapes.as_ptr().add(h) as *const i8, std::arch::x86_64::_MM_HINT_T0) };
        pend[nb] = Pending { key: k, s: s as u32, len: len as u32, h: h as u32 };
        nb += 1;
        if nb == LB {
            flush4::<M>(buf, &pend, ctx, tsf, shapes, ing_base, &mut sink);
            nb = 0;
        }
    });
    flush4::<M>(buf, &pend[..nb], ctx, tsf, shapes, ing_base, &mut sink);
    if line_start < end {
        slow_line(ctx, tsf, &buf[line_start..end], ing_base, &mut sink);
    }
}

#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn flush4<const M: u32>(buf: &[u8], pend: &[Pending], ctx: &Ctx, tsf: &mut ts::Fast, shapes: &mut [Shape], ing_base: i64, sink: &mut impl FnMut(Ev)) {
    if M == 5 {
        for pd in pend {
            sink(Ev { eid: pd.key[0] ^ pd.key[1] ^ pd.key[2] ^ pd.key[3], ehi: pd.h as u64, elo: 0, user: 0, uhi: 0, ulo: 0, ingest: 0, ts: 0, pc: 0 });
        }
        return;
    }
    for pd in pend {
        let (ls, len, k) = (pd.s as usize, pd.len as usize, pd.key);
        let p = unsafe { buf.as_ptr().add(ls) };
        let e = unsafe { shapes.get_unchecked_mut(pd.h as usize) };
        if (e.key[0] ^ k[0]) | (e.key[1] ^ k[1]) | (e.key[2] ^ k[2]) | (e.key[3] ^ k[3]) != 0 && !new_shape(p, len, k, e) {
            continue;
        }
        if M == 0 && e.st[7] != 0 && fast_row(p, e, ctx, tsf, ing_base, sink) {
            continue;
        }
        let row = Row { s: ls as u32, st: [e.st[0], e.st[1], e.st[2], e.st[3], e.st[4], e.st[5], e.st[6]], n: [e.n[0], e.n[1], e.n[2], e.n[3], e.n[4], e.n[5], e.n[6]] };
        parse_row::<M>(buf, &row, ctx, tsf, ing_base, sink);
    }
}

#[inline(never)]
pub fn mb_fused4<const M: u32>(buf: &[u8], start: usize, end: usize, ctx: &Ctx, tsf: &mut ts::Fast, sh: &mut [Shape], ib: i64) -> u64 {
    let mut c = 0u64;
    event_lines4::<M>(buf, start, end, ctx, tsf, sh, ib, #[inline(always)] |v| c = c.wrapping_add(v.eid ^ v.user ^ v.ts as u64 ^ v.pc as u64 ^ v.ingest as u64 ^ v.uhi ^ v.ehi));
    c
}

/// The common event line, straight through: short or long hex ids, ts F1 / F2 / F3-13, ingest_ts F1,
/// a view / cart type and a product, every alternative computed and selected without branching. False
/// (nothing emitted) when the line is off these shapes; the caller then takes the general path.
#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn fast_line(p: *const u8, st: &[usize; 7], n: &[usize; 7], ctx: &Ctx, tsf: &ts::Fast, ing_base: i64, sink: &mut impl FnMut(Ev)) -> bool {
    let f = |i: usize| unsafe { p.add(st[i]) };
    // ids: canonical (1..=10 bytes) or long hex (33)
    let (ks0, ks1, oks0, oks1) = unsafe { keys::pack2(f(0), f(1), n[0] as u8, n[1] as u8) };
    let (eh, el, okl0) = unsafe { keys::hex32(f(0).add(1)) };
    let (uh, ul, okl1) = unsafe { keys::hex32(f(1).add(1)) };
    let (ep, up) = unsafe { (*f(0), *f(1)) };
    let short0 = oks0 & (n[0].wrapping_sub(1) < 10);
    let short1 = oks1 & (n[1].wrapping_sub(1) < 10);
    let long0 = okl0 & (n[0] == 33) & ep.is_ascii_lowercase();
    let long1 = okl1 & (n[1] == 33) & up.is_ascii_lowercase();
    // timestamps: ts local part + zone, ingest
    let (tl, okt, im, oki) = unsafe { tsf.local_pair(f(2), f(3)) };
    let (t3, ok3) = unsafe { ts::digits13p(f(2)) };
    let (zoff, okz) = unsafe { ts::zone(f(2).add(19)) };
    let z2 = unsafe { *f(2).add(19) };
    let z3 = unsafe { *f(3).add(19) };
    let ok1 = okt & (n[2] == 20) & (z2 == b'Z');
    let ok2 = okt & (n[2] == 25) & okz;
    let ok3 = ok3 & (n[2] == 13);
    let t = if ok3 { t3 } else if ok2 { tl + zoff } else { tl };
    let ty = unsafe { (f(4) as *const u32).read_unaligned() };
    let vc = (n[4] == 4) & ((ty == u32::from_le_bytes(*b"view")) | (ty == u32::from_le_bytes(*b"cart")));
    let good = (short0 | long0)
        & (short1 | long1)
        & (ok1 | ok2 | ok3)
        & oki
        & (n[3] == 20)
        & (z3 == b'Z')
        & vc
        & (n[5] > 0)
        & (t >= ctx.ws)
        & (t < ctx.we)
        & (im - t <= DAY);
    let mut dummy = 0u8;
    keys::prefetch_long_if(long1 & good, up, uh, ul, &mut dummy as *const u8);
    if !good {
        return false;
    }
    let rel = im / 1000 - ing_base;
    let ing32 = if (rel as u64) < u32::MAX as u64 { rel as u32 } else { rel.clamp(0, u32::MAX as i64 - 1) as u32 };
    let (eid, ehi, elo) = if short0 { (ks0, 0, 0) } else { (keys::LONG_EID | ep as u64, eh, el) };
    let (user, uhi, ulo) = if short1 { (ks1, 0, 0) } else { (keys::LONG_EID | up as u64, uh, ul) };
    sink(Ev { eid, ehi, elo, user, uhi, ulo, ingest: ing32, ts: (t - ctx.ws) as u32, pc: 0 });
    true
}

/// `scan::lines::<7>` + `parse_event` over buf[start..end): every event candidate goes to `sink`.
#[inline(always)]
pub fn event_lines5<const M: u32>(buf: &[u8], start: usize, end: usize, ctx: &Ctx, tsf: &mut ts::Fast, ing_base: i64, mut sink: impl FnMut(Ev)) {
    use std::arch::x86_64::_bzhi_u64;
    debug_assert!(buf.len() >= end + scan::PAD);
    let mut line_start = start;
    scan::line_ends(buf, start, end, |e| {
        let s = line_start;
        line_start = e + 1;
        let len = e - s;
        if len >= 127 {
            slow_line(ctx, tsf, &buf[s..e], ing_base, &mut sink);
            return;
        }
        let p = unsafe { buf.as_ptr().add(s) };
        let (c, q) = unsafe { masks128(p) };
        let hl = len.max(64) as u32 - 64;
        let (cl, ch) = unsafe { (_bzhi_u64(c as u64, len as u32), _bzhi_u64((c >> 64) as u64, hl)) };
        let (ql, qh) = unsafe { (_bzhi_u64(q as u64, len as u32), _bzhi_u64((q >> 64) as u64, hl)) };
        let c = cl as u128 | ((ch as u128) << 64);
        let q = ql as u128 | ((qh as u128) << 64);
        let k = cl.count_ones() as usize;
        // quoting (see scan::split_short)
        let starts = (c << 1) | 1;
        let lasts = (c | (1u128 << len)) >> 1;
        let qa = q & starts;
        let qb = q & lasts;
        if (k + ch.count_ones() as usize != 6) | ((q ^ (qa | qb)) | (qa & qb) | (qb.wrapping_sub(qa) & (c | lasts)) != 0) {
            return;
        }
        let mut pos = [0u32; 16];
        let (mut lo, mut hi) = (cl, ch);
        for j in 0..6 {
            pos[j] = lo.trailing_zeros();
            lo &= lo.wrapping_sub(1);
        }
        for j in 0..6 {
            unsafe { *pos.get_unchecked_mut(k + j) = 64 + hi.trailing_zeros() };
            hi &= hi.wrapping_sub(1);
        }
        pos[6] = len as u32;
        let mut st = [0usize; 7];
        let mut n = [0usize; 7];
        let mut fs = 0usize;
        for i in 0..7 {
            let fe = pos[i] as usize;
            let qd = (unsafe { *p.add(fs) } == b'"') as usize;
            st[i] = fs + qd;
            n[i] = fe - fs - 2 * qd;
            fs = fe + 1;
        }
        if n[0] == 0 {
            return;
        }
        if M == 0 && fast_line(p, &st, &n, ctx, tsf, ing_base, &mut sink) {
            return;
        }
        let row = Row { s: s as u32, st: st.map(|x| x as u8), n: n.map(|x| x as u8) };
        parse_row::<M>(buf, &row, ctx, tsf, ing_base, &mut sink);
    });
    if line_start < end {
        slow_line(ctx, tsf, &buf[line_start..end], ing_base, &mut sink);
    }
}

#[inline(never)]
pub fn mb_fused5<const M: u32>(buf: &[u8], start: usize, end: usize, ctx: &Ctx, tsf: &mut ts::Fast, ib: i64) -> u64 {
    let mut c = 0u64;
    event_lines5::<M>(buf, start, end, ctx, tsf, ib, #[inline(always)] |v| c = c.wrapping_add(v.eid ^ v.user ^ v.ts as u64 ^ v.pc as u64 ^ v.ingest as u64 ^ v.uhi ^ v.ehi ^ v.elo ^ v.ulo));
    c
}

/// Field starts and lengths of a validated line, from its comma positions and quoted-start flags, in
/// two vectors: st[i] = content start, n[i] = content length (lanes 0..7; lane 7 unused).
#[cfg(target_feature = "avx2")]
#[inline(always)]
unsafe fn fields_vec(pos: &[u32; 16], qf: u32, len: usize, st: &mut [u32; 8], n: &mut [u32; 8]) {
    use std::arch::x86_64::*;
    unsafe {
        // ends: p0..p5, len
        let e = _mm256_setr_epi32(pos[0] as i32, pos[1] as i32, pos[2] as i32, pos[3] as i32, pos[4] as i32, pos[5] as i32, len as i32, 0);
        // starts: 0, p0 + 1, ..., p5 + 1
        let s = _mm256_permutevar8x32_epi32(_mm256_add_epi32(e, _mm256_set1_epi32(1)), _mm256_setr_epi32(7, 0, 1, 2, 3, 4, 5, 6));
        let s = _mm256_blend_epi32::<1>(s, _mm256_setzero_si256());
        let qd = _mm256_and_si256(_mm256_srlv_epi32(_mm256_set1_epi32(qf as i32), _mm256_setr_epi32(0, 1, 2, 3, 4, 5, 6, 7)), _mm256_set1_epi32(1));
        _mm256_storeu_si256(st.as_mut_ptr() as *mut __m256i, _mm256_add_epi32(s, qd));
        _mm256_storeu_si256(n.as_mut_ptr() as *mut __m256i, _mm256_sub_epi32(_mm256_sub_epi32(e, s), _mm256_add_epi32(qd, qd)));
    }
}

/// The common event line, straight through: canonical ids (long hex ids on a side branch), ts F1 or
/// F3-13 selected without branching (F2 on a side branch), ingest_ts F1, a view / cart type and a
/// product. False (nothing emitted) when the line is off these shapes: the caller takes the general path.
#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn fast_line6<const M: u32>(p: *const u8, st: &[u32; 8], n: &[u32; 8], ctx: &Ctx, tsf: &ts::Fast, ing_base: i64, sink: &mut impl FnMut(Ev)) -> bool {
    let f = |i: usize| unsafe { p.add(st[i] as usize) };
    let (n0, n1, n2) = (n[0] as usize, n[1] as usize, n[2] as usize);
    if M == 14 {
        let (tl, okt, im, oki) = unsafe { tsf.local_pair(f(2), f(3)) };
        sink(Ev { eid: 0, ehi: 0, elo: 0, user: 0, uhi: 0, ulo: 0, ingest: oki as u32, ts: okt as u32, pc: (tl ^ im) as u32 });
        return true;
    }
    let (mut eid, mut user, oks0, oks1) = unsafe { keys::pack2(f(0), f(1), n0 as u8, n1 as u8) };
    if M == 13 {
        sink(Ev { eid, ehi: 0, elo: 0, user, uhi: 0, ulo: 0, ingest: oks0 as u32, ts: oks1 as u32, pc: 0 });
        return true;
    }
    let (mut ehi, mut elo, mut uhi, mut ulo) = (0u64, 0u64, 0u64, 0u64);
    let mut ids = oks0 & (n0.wrapping_sub(1) < 10) & oks1 & (n1.wrapping_sub(1) < 10);
    if !ids {
        // long hex ids (1 in 8 of each)
        let ok0 = if n0.wrapping_sub(1) < 10 && oks0 {
            true
        } else {
            let (h, l, ok) = unsafe { keys::hex32(f(0).add(1)) };
            let pre = unsafe { *f(0) };
            (eid, ehi, elo) = (keys::LONG_EID | pre as u64, h, l);
            ok & (n0 == 33) & pre.is_ascii_lowercase()
        };
        let ok1 = if n1.wrapping_sub(1) < 10 && oks1 {
            true
        } else {
            let (h, l, ok) = unsafe { keys::hex32(f(1).add(1)) };
            let pre = unsafe { *f(1) };
            (user, uhi, ulo) = (keys::LONG_EID | pre as u64, h, l);
            let ok = ok & (n1 == 33) & pre.is_ascii_lowercase();
            if ok {
                let mut dummy = 0u8;
                keys::prefetch_long_if(true, pre, h, l, &mut dummy as *const u8);
            }
            ok
        };
        ids = ok0 & ok1;
    }
    let (tl, okt, im, oki) = unsafe { tsf.local_pair(f(2), f(3)) };
    if M == 11 {
        sink(Ev { eid, ehi, elo, user, uhi, ulo, ingest: oki as u32, ts: okt as u32 ^ ids as u32, pc: (tl ^ im) as u32 });
        return true;
    }
    let (t3, ok3) = unsafe { ts::digits13p(f(2)) };
    let z2 = unsafe { *f(2).add(19) };
    let mut t = if n2 == 13 { t3 } else { tl };
    let mut tok = ((n2 == 13) & ok3) | ((n2 == 20) & okt & (z2 == b'Z'));
    if n2 == 25 {
        let (zoff, okz) = unsafe { ts::zone(f(2).add(19)) };
        t = tl + zoff;
        tok = okt & okz;
    }
    let ty = unsafe { (f(4) as *const u32).read_unaligned() };
    let vc = (n[4] == 4) & ((ty == u32::from_le_bytes(*b"view")) | (ty == u32::from_le_bytes(*b"cart")));
    let z3 = unsafe { *f(3).add(19) };
    let good = ids & tok & oki & (n[3] == 20) & (z3 == b'Z') & vc & (n[5] > 0) & (t >= ctx.ws) & (t < ctx.we) & (im - t <= DAY);
    if !good {
        return false;
    }
    // F1 values are whole seconds: the division is exact for either sign
    let rel = im / 1000 - ing_base;
    let ing32 = if (rel as u64) < u32::MAX as u64 { rel as u32 } else { rel.clamp(0, u32::MAX as i64 - 1) as u32 };
    sink(Ev { eid, ehi, elo, user, uhi, ulo, ingest: ing32, ts: (t - ctx.ws) as u32, pc: 0 });
    true
}

/// `scan::lines::<7>` + `parse_event` over buf[start..end): every event candidate goes to `sink`.
#[inline(always)]
pub fn event_lines6<const M: u32>(buf: &[u8], start: usize, end: usize, ctx: &Ctx, tsf: &mut ts::Fast, ing_base: i64, mut sink: impl FnMut(Ev)) {
    use std::arch::x86_64::{_bzhi_u64, _pext_u64};
    debug_assert!(buf.len() >= end + scan::PAD);
    let mut line_start = start;
    let mut pos = [0u32; 16];
    let mut st = [0u32; 8];
    let mut n = [0u32; 8];
    scan::line_ends(buf, start, end, |e| {
        let s = line_start;
        line_start = e + 1;
        let len = e - s;
        if len >= 127 {
            slow_line(ctx, tsf, &buf[s..e], ing_base, &mut sink);
            return;
        }
        let p = unsafe { buf.as_ptr().add(s) };
        let (c, q) = unsafe { masks128(p) };
        let hl = len.max(64) as u32 - 64;
        let (cl, ch) = unsafe { (_bzhi_u64(c as u64, len as u32), _bzhi_u64((c >> 64) as u64, hl)) };
        let (ql, qh) = unsafe { (_bzhi_u64(q as u64, len as u32), _bzhi_u64((q >> 64) as u64, hl)) };
        let c = cl as u128 | ((ch as u128) << 64);
        let q = ql as u128 | ((qh as u128) << 64);
        let k = cl.count_ones() as usize;
        // quoting (see scan::split_short)
        let starts = (c << 1) | 1;
        let lasts = (c | (1u128 << len)) >> 1;
        let qa = q & starts;
        let qb = q & lasts;
        if (k + ch.count_ones() as usize != 6) | ((q ^ (qa | qb)) | (qa & qb) | (qb.wrapping_sub(qa) & (c | lasts)) != 0) {
            return;
        }
        // comma positions: the low word's, then the high word's after them
        let (mut lo, mut hi) = (cl, ch);
        for j in 0..6 {
            pos[j] = lo.trailing_zeros();
            lo &= lo.wrapping_sub(1);
        }
        for j in 0..6 {
            unsafe { *pos.get_unchecked_mut(k + j) = 64 + hi.trailing_zeros() };
            hi &= hi.wrapping_sub(1);
        }
        // quoted fields: bit i = field i starts with a quote
        let (sl, sh) = (starts as u64, (starts >> 64) as u64);
        let qf = unsafe { _pext_u64(qa as u64, sl) | (_pext_u64((qa >> 64) as u64, sh) << sl.count_ones()) } as u32;
        unsafe { fields_vec(&pos, qf, len, &mut st, &mut n) };
        if n[0] == 0 {
            return;
        }
        if M == 10 {
            sink(Ev { eid: st[0] as u64 ^ n[1] as u64 ^ st[2] as u64, ehi: n[3] as u64 ^ st[4] as u64 ^ n[5] as u64, elo: st[6] as u64, user: n[0] as u64, uhi: 0, ulo: 0, ingest: 0, ts: 0, pc: 0 });
            return;
        }
        if M == 12 {
            sink(Ev { eid: pos[0] as u64 ^ pos[1] as u64 ^ pos[2] as u64, ehi: pos[3] as u64 ^ pos[4] as u64 ^ pos[5] as u64, elo: qf as u64, user: 0, uhi: 0, ulo: 0, ingest: 0, ts: 0, pc: 0 });
            return;
        }
        if (M == 0 || M == 11 || M == 13 || M == 14) && fast_line6::<M>(p, &st, &n, ctx, tsf, ing_base, &mut sink) {
            return;
        }
        let row = Row { s: s as u32, st: [st[0] as u8, st[1] as u8, st[2] as u8, st[3] as u8, st[4] as u8, st[5] as u8, st[6] as u8], n: [n[0] as u8, n[1] as u8, n[2] as u8, n[3] as u8, n[4] as u8, n[5] as u8, n[6] as u8] };
        parse_row::<M>(buf, &row, ctx, tsf, ing_base, &mut sink);
    });
    if line_start < end {
        slow_line(ctx, tsf, &buf[line_start..end], ing_base, &mut sink);
    }
}

#[inline(never)]
pub fn mb_fused6<const M: u32>(buf: &[u8], start: usize, end: usize, ctx: &Ctx, tsf: &mut ts::Fast, ib: i64) -> u64 {
    let mut c = 0u64;
    event_lines6::<M>(buf, start, end, ctx, tsf, ib, #[inline(always)] |v| c = c.wrapping_add(v.eid ^ v.user ^ v.ts as u64 ^ v.pc as u64 ^ v.ingest as u64 ^ v.uhi ^ v.ehi ^ v.elo ^ v.ulo));
    c
}

const SCALAR_FIELDS: bool = false;

/// A split line waiting for its fields to be parsed (one line behind the splitter).
#[derive(Clone, Copy)]
#[repr(C, align(32))]
struct Split {
    st: [u32; 8],
    n: [u32; 8],
    s: usize,
    live: bool,
}

#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn finish(buf: &[u8], sp: &Split, ctx: &Ctx, tsf: &mut ts::Fast, ing_base: i64, sink: &mut impl FnMut(Ev)) {
    if !sp.live {
        return;
    }
    let p = unsafe { buf.as_ptr().add(sp.s) };
    if fast_line6::<0>(p, &sp.st, &sp.n, ctx, tsf, ing_base, sink) {
        return;
    }
    let (st, n) = (&sp.st, &sp.n);
    let row = Row { s: sp.s as u32, st: [st[0] as u8, st[1] as u8, st[2] as u8, st[3] as u8, st[4] as u8, st[5] as u8, st[6] as u8], n: [n[0] as u8, n[1] as u8, n[2] as u8, n[3] as u8, n[4] as u8, n[5] as u8, n[6] as u8] };
    parse_row::<0>(buf, &row, ctx, tsf, ing_base, sink);
}

/// `scan::lines::<7>` + `parse_event` over buf[start..end): every event candidate goes to `sink`, in
/// line order. Software-pipelined: each step splits line i and parses the fields of line i - 1, so
/// the two dependency chains overlap.
#[inline(always)]
pub fn event_lines7(buf: &[u8], start: usize, end: usize, ctx: &Ctx, tsf: &mut ts::Fast, ing_base: i64, mut sink: impl FnMut(Ev)) {
    use std::arch::x86_64::{_bzhi_u64, _pext_u64};
    debug_assert!(buf.len() >= end + scan::PAD);
    let mut line_start = start;
    let mut pos = [0u32; 16];
    let mut sp = [Split { st: [0; 8], n: [0; 8], s: 0, live: false }; 2];
    let mut cur = 0usize;
    scan::line_ends(buf, start, end, |e| {
        let s = line_start;
        line_start = e + 1;
        let len = e - s;
        if len >= 127 {
            finish(buf, &sp[cur ^ 1], ctx, tsf, ing_base, &mut sink);
            sp[cur ^ 1].live = false;
            slow_line(ctx, tsf, &buf[s..e], ing_base, &mut sink);
            return;
        }
        let p = unsafe { buf.as_ptr().add(s) };
        let (c, q) = unsafe { masks128(p) };
        let hl = len.max(64) as u32 - 64;
        let (cl, ch) = unsafe { (_bzhi_u64(c as u64, len as u32), _bzhi_u64((c >> 64) as u64, hl)) };
        let (ql, qh) = unsafe { (_bzhi_u64(q as u64, len as u32), _bzhi_u64((q >> 64) as u64, hl)) };
        let c = cl as u128 | ((ch as u128) << 64);
        let q = ql as u128 | ((qh as u128) << 64);
        let k = cl.count_ones() as usize;
        // quoting (see scan::split_short)
        let starts = (c << 1) | 1;
        let lasts = (c | (1u128 << len)) >> 1;
        let qa = q & starts;
        let qb = q & lasts;
        let ok = (k + ch.count_ones() as usize == 6) & ((q ^ (qa | qb)) | (qa & qb) | (qb.wrapping_sub(qa) & (c | lasts)) == 0);
        // comma positions: the low word's, then the high word's after them
        let (mut lo, mut hi) = (cl, ch);
        for j in 0..6 {
            let in_lo = lo != 0;
            pos[j] = if in_lo { lo.trailing_zeros() } else { 64 + hi.trailing_zeros() };
            let (l2, h2) = (lo & lo.wrapping_sub(1), hi & hi.wrapping_sub(1));
            lo = if in_lo { l2 } else { lo };
            hi = if in_lo { hi } else { h2 };
        }
        // quoted fields: bit i = field i starts with a quote
        let (sl, sh) = (starts as u64, (starts >> 64) as u64);
        let qf = unsafe { _pext_u64(qa as u64, sl) | (_pext_u64((qa >> 64) as u64, sh) << sl.count_ones()) } as u32;
        let x = &mut sp[cur];
        if SCALAR_FIELDS {
            let mut fs = 0u32;
            for i in 0..7 {
                let fe = if i < 6 { pos[i] } else { len as u32 };
                let qd = (qf >> i) & 1;
                x.st[i] = fs + qd;
                x.n[i] = fe.wrapping_sub(fs).wrapping_sub(2 * qd);
                fs = fe + 1;
            }
        } else {
            unsafe { fields_vec(&pos, qf, len, &mut x.st, &mut x.n) };
        }
        x.s = s;
        x.live = ok & (x.n[0] != 0);
        cur ^= 1;
        finish(buf, &sp[cur], ctx, tsf, ing_base, &mut sink);
    });
    finish(buf, &sp[cur ^ 1], ctx, tsf, ing_base, &mut sink);
    if line_start < end {
        slow_line(ctx, tsf, &buf[line_start..end], ing_base, &mut sink);
    }
}

#[inline(never)]
pub fn mb_fused7(buf: &[u8], start: usize, end: usize, ctx: &Ctx, tsf: &mut ts::Fast, ib: i64) -> u64 {
    let mut c = 0u64;
    event_lines7(buf, start, end, ctx, tsf, ib, #[inline(always)] |v| c = c.wrapping_add(v.eid ^ v.user ^ v.ts as u64 ^ v.pc as u64 ^ v.ingest as u64 ^ v.uhi ^ v.ehi ^ v.elo ^ v.ulo));
    c
}

#[inline(never)]
pub fn mb_pack2(ids: &[(&[u8], &[u8])]) -> u64 {
    let mut c = 0u64;
    for (a, b) in ids {
        let (x, y, o1, o2) = unsafe { keys::pack2(a.as_ptr(), b.as_ptr(), a.len() as u8, b.len() as u8) };
        c = c.wrapping_add(x ^ y ^ (o1 as u64) ^ ((o2 as u64) << 1));
    }
    c
}
#[inline(never)]
pub fn mb_pack16(ids: &[(&[u8], &[u8])]) -> u64 {
    let mut c = 0u64;
    for (a, b) in ids {
        c = c.wrapping_add(unsafe { keys::key_padded(a) ^ keys::key_padded(b) });
    }
    c
}

/// `parse_event` with a straight-line fast path for the common shapes (same result).
#[inline(always)]
pub fn parse_event_fast(ctx: &Ctx, tsf: &mut ts::Fast, f: &[&[u8]; 7], ing_base: i64) -> Option<Ev> {
    let (n0, n1, n2) = (f[0].len(), f[1].len(), f[2].len());
    let (p0, p1, p2, p3) = (f[0].as_ptr(), f[1].as_ptr(), f[2].as_ptr(), f[3].as_ptr());
    let (eid, user, oks0, oks1) = unsafe { keys::pack2(p0, p1, n0 as u8, n1 as u8) };
    let (tl, okt, im, oki) = unsafe { tsf.local_pair(p2, p3) };
    let (t3, ok3) = unsafe { ts::digits13p(p2) };
    let z2 = unsafe { *p2.add(19) };
    let z3 = unsafe { *p3.add(19) };
    let t = if n2 == 13 { t3 } else { tl };
    let tok = ((n2 == 13) & ok3) | ((n2 == 20) & okt & (z2 == b'Z'));
    let ty = f[4];
    let tw = unsafe { (ty.as_ptr() as *const u32).read_unaligned() };
    let vc = (ty.len() == 4) & ((tw == u32::from_le_bytes(*b"view")) | (tw == u32::from_le_bytes(*b"cart")));
    let good = oks0
        & oks1
        & (n0.wrapping_sub(1) < 10)
        & (n1.wrapping_sub(1) < 10)
        & tok
        & oki
        & (f[3].len() == 20)
        & (z3 == b'Z')
        & vc
        & !f[5].is_empty()
        & (t >= ctx.ws)
        & (t < ctx.we)
        & (im - t <= DAY);
    if !good {
        return parse_event(ctx, tsf, f, ing_base);
    }
    // F1 values are whole seconds: the division is exact for either sign
    let rel = im / 1000 - ing_base;
    let ing32 = if (rel as u64) < u32::MAX as u64 { rel as u32 } else { rel.clamp(0, u32::MAX as i64 - 1) as u32 };
    Some(Ev { eid, ehi: 0, elo: 0, user, uhi: 0, ulo: 0, ingest: ing32, ts: (t - ctx.ws) as u32, pc: 0 })
}

#[inline(never)]
pub fn mb_fused8(buf: &[u8], start: usize, end: usize, ctx: &Ctx, tsf: &mut ts::Fast, ib: i64) -> u64 {
    let mut c = 0u64;
    scan::lines_old::<7>(buf, start, end, #[inline(always)] |f| {
        if let Some(v) = parse_event_fast(ctx, tsf, f, ib) {
            c = c.wrapping_add(v.eid ^ v.user ^ v.ts as u64 ^ v.pc as u64 ^ v.ingest as u64 ^ v.uhi ^ v.ehi ^ v.elo ^ v.ulo)
        }
    });
    c
}

/// `parse_event` with both timestamps' F1 local parts read in one vector pass (same result).
#[inline(always)]
pub fn parse_event_b(ctx: &Ctx, tsf: &mut ts::Fast, f: &[&[u8]; 7], ing_base: i64) -> Option<Ev> {
    if f[0].is_empty() {
        return None;
    }
    let (eid, ehi, elo) = unsafe { keys::eid_padded(f[0]) };
    let (p2, p3) = (f[2].as_ptr(), f[3].as_ptr());
    let (tl, okt, im, oki) = unsafe { tsf.local_pair(p2, p3) };
    let ingest = if oki & (f[3].len() == 20) & (unsafe { *p3.add(19) } == b'Z') { Some(im) } else { tsf.parse(f[3]) };
    let (ingest, ing32) = match ingest {
        Some(v) => {
            let secs = if v >= 0 { v / 1000 } else { v.div_euclid(1000) };
            let rel = secs - ing_base;
            (v, if (rel as u64) < u32::MAX as u64 { rel as u32 } else { rel.clamp(0, u32::MAX as i64 - 1) as u32 })
        }
        None => (i64::MAX, u32::MAX),
    };
    let mut ev = Ev { eid, ehi, elo, user: 0, uhi: 0, ulo: 0, ingest: ing32, ts: NO_TS, pc: 0 };
    let n = [f[0].len(), f[1].len(), f[2].len(), f[3].len(), f[4].len(), f[5].len(), f[6].len()];
    let fld = |i: usize| f[i];
    let ok1 = okt & (unsafe { *p2.add(19) } == b'Z');
    if let Some((t, price, cat)) = valid(ctx, tsf, &fld, &n, p2, tl, ok1 & (n[2] == 20), ingest) {
        (ev.user, ev.uhi, ev.ulo) = unsafe { keys::key_deferred(f[1]) };
        ev.ts = t;
        ev.pc = price | (cat << 24);
    }
    Some(ev)
}

#[inline(never)]
pub fn mb_fused9(buf: &[u8], start: usize, end: usize, ctx: &Ctx, tsf: &mut ts::Fast, ib: i64) -> u64 {
    let mut c = 0u64;
    scan::lines_old::<7>(buf, start, end, #[inline(always)] |f| {
        if let Some(v) = parse_event_b(ctx, tsf, f, ib) {
            c = c.wrapping_add(v.eid ^ v.user ^ v.ts as u64 ^ v.pc as u64 ^ v.ingest as u64 ^ v.uhi ^ v.ehi ^ v.elo ^ v.ulo)
        }
    });
    c
}
