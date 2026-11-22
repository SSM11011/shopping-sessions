//! Timestamp forms F1 / F2 / F3 -> epoch milliseconds. None = bad.

pub const SECOND: i64 = 1_000;
pub const DAY: i64 = 86_400_000;

#[inline(always)]
fn d(b: u8) -> Option<i64> {
    let v = b.wrapping_sub(b'0');
    if v < 10 { Some(v as i64) } else { None }
}

#[inline(always)]
fn d2(s: &[u8], i: usize) -> Option<i64> {
    Some(d(s[i])? * 10 + d(s[i + 1])?)
}

pub fn days_from_civil(y: i64, m: i64, day: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + day - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

#[inline(always)]
fn is_leap(y: i64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

const MDAYS: [i64; 13] = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

/// F3: all ASCII digits (non-empty). Values beyond i64 -> None (outside any window anyway).
#[inline]
fn epoch_digits(s: &[u8]) -> Option<Option<i64>> {
    let mut v: u64 = 0;
    let mut overflow = false;
    for &b in s {
        let x = b.wrapping_sub(b'0');
        if x >= 10 {
            return None;
        }
        if !overflow {
            match v.checked_mul(10).and_then(|m| m.checked_add(x as u64)) {
                Some(n) => v = n,
                None => overflow = true,
            }
        }
    }
    if overflow || v > i64::MAX as u64 { Some(None) } else { Some(Some(v as i64)) }
}

#[inline(always)]
fn civil(s: &[u8]) -> Option<i64> {
    if s[4] != b'-' || s[7] != b'-' || s[10] != b'T' || s[13] != b':' || s[16] != b':' {
        return None;
    }
    let y = d2(s, 0)? * 100 + d2(s, 2)?;
    let mo = d2(s, 5)?;
    let day = d2(s, 8)?;
    let h = d2(s, 11)?;
    let mi = d2(s, 14)?;
    let se = d2(s, 17)?;
    if !(1..=12).contains(&mo) || h > 23 || mi > 59 || se > 59 || day < 1 {
        return None;
    }
    let md = if mo == 2 && is_leap(y) { 29 } else { MDAYS[mo as usize] };
    if day > md {
        return None;
    }
    Some(((days_from_civil(y, mo, day) * 86_400) + h * 3_600 + mi * 60 + se) * SECOND)
}

#[inline]
pub fn parse(s: &[u8]) -> Option<i64> {
    match s.len() {
        20 if s[19] == b'Z' => civil(s),
        25 if s[10] == b'T' => {
            let base = civil(s)?;
            let sign = s[19];
            if (sign != b'+' && sign != b'-') || s[22] != b':' {
                return None;
            }
            let oh = d2(s, 20)?;
            let om = d2(s, 23)?;
            if oh > 23 || om > 59 {
                return None;
            }
            let off = (oh * 60 + om) * 60_000;
            Some(if sign == b'+' { base - off } else { base + off })
        }
        0 => None,
        _ => epoch_digits(s)?,
    }
}

/// Per-thread cache of validated calendar dates (`YYYY-MM-DD` bytes -> days since epoch). A window spans
/// only a few dozen distinct dates, so after warm-up the date part costs one compare.
pub struct Fast {
    k8: [u64; 128],
    k2: [u16; 128],
    days: [i64; 128],
}

impl Default for Fast {
    fn default() -> Self {
        // every slot starts as the valid pair 1970-01-01 -> 0, so a hit is always a correct answer
        Fast { k8: [u64::from_le_bytes(*b"1970-01-"); 128], k2: [u16::from_le_bytes(*b"01"); 128], days: [0; 128] }
    }
}

#[inline(always)]
fn le64(s: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(s[at..at + 8].try_into().unwrap())
}

/// "hh:mm:ss" at s[11..19] -> seconds of day, validated. One SWAR pass checks the colons, that every
/// other byte is a digit, and the tens digits' ranges (h <= 2, m <= 5, s <= 5); only h <= 23 remains.
#[inline(always)]
fn time_of_day(s: &[u8]) -> Option<i64> {
    const T: u64 = 0x3030_3a30_303a_3030;
    const DIG: u64 = 0xF0F0_00F0_F000_F0F0;
    const COL: u64 = 0x0000_FF00_00FF_0000;
    // per byte (low first): h tens +13, h ones +6, -, m tens +10, m ones +6, -, s tens +10, s ones +6
    const ADJ: u64 = 0x060A_0006_0A00_060D;
    let t = le64(s, 11);
    if ((t ^ T) & COL) | ((t & DIG) ^ (T & DIG)) | ((t.wrapping_add(ADJ) & DIG) ^ (T & DIG)) != 0 {
        return None;
    }
    let d = t.wrapping_sub(T);
    // pairs: byte 0 = h, byte 3 = m, byte 6 = s (no carries: each byte <= 99)
    let x = d.wrapping_mul(10).wrapping_add(d >> 8);
    let h = (x & 0xFF) as i64;
    let m = ((x >> 24) & 0xFF) as i64;
    let sec = ((x >> 48) & 0xFF) as i64;
    if h > 23 {
        return None;
    }
    Some(h * 3600 + m * 60 + sec)
}

/// 8 ASCII digits, first digit in the lowest byte -> value.
#[inline(always)]
pub fn parse8(x: u64) -> u64 {
    let x = x.wrapping_sub(0x3030_3030_3030_3030);
    let x = x.wrapping_mul(10).wrapping_add(x >> 8);
    (((x & 0x0000_00FF_0000_00FF).wrapping_mul(100 + (1_000_000 << 32)))
        .wrapping_add(((x >> 16) & 0x0000_00FF_0000_00FF).wrapping_mul(1 + (10_000 << 32))))
        >> 32
}

/// Exactly 13 ASCII digits (epoch ms for years 2001..2286) -> value.
#[inline(always)]
fn digits13(s: &[u8]) -> Option<i64> {
    let a = le64(s, 0);
    // bytes 5..13 with the overlap (digits 5..8, already covered by `a`) replaced by '0': the last
    // 5 digits as an 8-digit number
    let b = (le64(s, 5) & !0xFF_FFFF) | 0x30_3030;
    const D: u64 = 0xF0F0_F0F0_F0F0_F0F0;
    const Z: u64 = 0x3030_3030_3030_3030;
    if ((a & D) ^ Z) | ((b & D) ^ Z) | ((a.wrapping_add(0x0606_0606_0606_0606) & D) ^ Z) | ((b.wrapping_add(0x0606_0606_0606_0606) & D) ^ Z) != 0 {
        return None;
    }
    Some((parse8(a) * 100_000 + parse8(b)) as i64)
}

impl Fast {
    #[inline(always)]
    fn date(&mut self, s: &[u8]) -> Option<i64> {
        let k8 = le64(s, 0);
        let k2 = u16::from_le_bytes([s[8], s[9]]);
        // direct index: day-of-month digits (2 + 4 low bits) and the month's parity; any two adjacent
        // months differ in parity, so a window's dates never collide
        let h = (((k2 & 3) | ((k2 >> 6) & 0x3C)) as usize) | ((k8 >> 42) & 0x40) as usize;
        if self.k8[h] == k8 && self.k2[h] == k2 {
            return Some(self.days[h]);
        }
        let d = date_slow(s)?;
        self.k8[h] = k8;
        self.k2[h] = k2;
        self.days[h] = d;
        Some(d)
    }

    /// Same result as `parse`, faster on the common shapes.
    #[inline(always)]
    pub fn parse(&mut self, s: &[u8]) -> Option<i64> {
        match s.len() {
            20 => {
                if s[10] != b'T' || s[19] != b'Z' {
                    return parse(s);
                }
                let days = self.date(s)?;
                let tod = time_of_day(s)?;
                Some((days * 86_400 + tod) * SECOND)
            }
            13 => match digits13(s) {
                Some(v) => Some(v),
                None => parse(s),
            },
            25 => {
                if s[10] != b'T' {
                    return parse(s);
                }
                let days = self.date(s)?;
                let tod = time_of_day(s)?;
                let base = (days * 86_400 + tod) * SECOND;
                let sign = s[19];
                if (sign != b'+' && sign != b'-') || s[22] != b':' {
                    return None;
                }
                let oh = d2(s, 20)?;
                let om = d2(s, 23)?;
                if oh > 23 || om > 59 {
                    return None;
                }
                let off = (oh * 60 + om) * 60_000;
                Some(if sign == b'+' { base - off } else { base + off })
            }
            _ => parse(s),
        }
    }
}

/// Validated `YYYY-MM-DD` at s[0..10] -> days since epoch.
fn date_slow(s: &[u8]) -> Option<i64> {
    if s[4] != b'-' || s[7] != b'-' {
        return None;
    }
    let y = d2(s, 0)? * 100 + d2(s, 2)?;
    let mo = d2(s, 5)?;
    let day = d2(s, 8)?;
    if !(1..=12).contains(&mo) || day < 1 {
        return None;
    }
    let md = if mo == 2 && is_leap(y) { 29 } else { MDAYS[mo as usize] };
    if day > md {
        return None;
    }
    Some(days_from_civil(y, mo, day))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn forms() {
        let f1 = parse(b"2026-10-03T23:58:10Z").unwrap();
        assert_eq!(parse(b"2026-10-03T19:58:10-04:00"), Some(f1));
        assert_eq!(parse(b"1791071890000"), Some(1791071890000));
        assert_eq!(parse(b"0001791071890000"), Some(1791071890000));
        assert_eq!(f1, 1791071890000);
        for bad in [
            &b""[..], b"2026-10-03T23:58:10z", b"2026-10-03t23:58:10Z", b"2026-10-03 23:58:10Z",
            b"2026-10-03T23:58:10", b"2026-10-03T23:58:10.000Z", b"1791071890000.0", b"1.79e12", b"-5",
            b"2026-13-03T23:58:10Z", b"2026-02-30T23:58:10Z", b"2027-02-29T23:58:10Z", b"2026-10-03T24:00:00Z",
            b"2026-10-03T23:59:60Z", b"2026-10-03T23:58:10+24:00", b"2026-10-03T23:58:10+05:60",
            b"2026-10-03T23:58:10+0200", b"999999999999999999999",
        ] {
            assert_eq!(parse(bad), None, "{:?}", std::str::from_utf8(bad));
        }
        assert!(parse(b"2028-02-29T00:00:00Z").is_some());
        assert_eq!(parse(b"2026-10-03T23:58:10-00:00"), Some(f1));
        assert_eq!(parse(b"86400000"), Some(86_400_000));
    }

    #[test]
    fn f3_with_f1_f2_lengths() {
        // 20 and 25 characters are also the F1/F2 lengths; all-digit values of those lengths are F3
        let mut fast = Fast::default();
        for s in [&b"00000001791071890000"[..], b"0000000000001791071890000"] {
            assert_eq!(parse(s), Some(1791071890000));
            assert_eq!(fast.parse(s), Some(1791071890000));
        }
        assert_eq!(fast.parse(b"0000000179107189000x"), None);
        assert_eq!(fast.parse(b"000000000000179107189000x"), None);
    }

    #[test]
    fn fast_matches_slow() {
        let mut fast = Fast::default();
        let mut x: u64 = 99;
        let mut rnd = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let base: Vec<&[u8]> = vec![
            b"2026-10-03T23:58:10Z", b"2028-02-29T00:00:00Z", b"2027-02-28T23:59:59Z", b"2026-10-03T19:58:10-04:00",
            b"1791071890000", b"0001791071890000", b"2026-12-31T00:00:00+12:00",
        ];
        let alphabet = b"0123456789:-TZtz+ .\x00\xff";
        for _ in 0..300_000 {
            let mut v = base[(rnd() % base.len() as u64) as usize].to_vec();
            for _ in 0..(rnd() % 3) {
                let i = (rnd() % v.len() as u64) as usize;
                v[i] = alphabet[(rnd() % alphabet.len() as u64) as usize];
            }
            assert_eq!(fast.parse(&v), parse(&v), "{:?}", String::from_utf8_lossy(&v));
        }
        for s in base {
            assert_eq!(fast.parse(s), parse(s));
        }
    }
}

#[inline(never)]
pub fn mb_parse_all(v: &[&[u8]], tf: &mut Fast) -> u64 {
    let mut c = 0u64;
    for s in v {
        c = c.wrapping_add(tf.parse(s).unwrap_or(0) as u64);
    }
    c
}

/// 13 ASCII digits at p (16 bytes readable) -> (value, all digits).
#[inline(always)]
pub unsafe fn digits13p(p: *const u8) -> (i64, bool) {
    let a = unsafe { (p as *const u64).read_unaligned() };
    let b = (unsafe { (p.add(5) as *const u64).read_unaligned() } & !0xFF_FFFF) | 0x30_3030;
    const D: u64 = 0xF0F0_F0F0_F0F0_F0F0;
    const Z: u64 = 0x3030_3030_3030_3030;
    let bad = ((a & D) ^ Z) | ((b & D) ^ Z) | ((a.wrapping_add(0x0606_0606_0606_0606) & D) ^ Z) | ((b.wrapping_add(0x0606_0606_0606_0606) & D) ^ Z);
    ((parse8(a).wrapping_mul(100_000).wrapping_add(parse8(b))) as i64, bad == 0)
}

/// F2 zone at p (`+HH:MM` / `-HH:MM`, 8 bytes readable) -> (offset to add to local time in ms, valid).
#[inline(always)]
pub unsafe fn zone(p: *const u8) -> (i64, bool) {
    let o = unsafe { (p as *const u64).read_unaligned() };
    let b = |i: u32| ((o >> (8 * i)) & 0xFF) as u8;
    let (h1, h0, m1, m0) = (b(1).wrapping_sub(b'0'), b(2).wrapping_sub(b'0'), b(4).wrapping_sub(b'0'), b(5).wrapping_sub(b'0'));
    let oh = h1 as i64 * 10 + h0 as i64;
    let om = m1 as i64 * 10 + m0 as i64;
    let sign = b(0);
    let ok = ((sign == b'+') | (sign == b'-')) & (b(3) == b':') & (h1 <= 2) & (h0 <= 9) & (m1 <= 5) & (m0 <= 9) & (oh <= 23);
    let off = (oh * 60 + om) * 60_000;
    (if sign == b'-' { off } else { -off }, ok)
}

impl Fast {
    /// Local `YYYY-MM-DDTHH:MM:SS` of two fields at once (32 bytes readable at a and b), both in one
    /// vector pass: (ms_a, ok_a, ms_b, ok_b). ok: valid time, date in the cache; byte 19 (zone) and the
    /// length are not looked at. Then the value equals `parse`'s base before the zone.
    #[cfg(target_feature = "avx2")]
    #[inline(always)]
    pub unsafe fn local_pair(&self, a: *const u8, b: *const u8) -> (i64, bool, i64, bool) {
        use std::arch::x86_64::*;
        unsafe {
            let v = _mm256_inserti128_si256::<1>(
                _mm256_castsi128_si256(_mm_loadu_si128(a.add(4) as *const __m128i)),
                _mm_loadu_si128(b.add(4) as *const __m128i),
            );
            let c8 = |x: u8| x as i8;
            let base = _mm256_broadcastsi128_si256(_mm_setr_epi8(
                0, 0, 0, 0, 0, 0, c8(b'T'), c8(b'0'), c8(b'0'), c8(b':'), c8(b'0'), c8(b'0'), c8(b':'), c8(b'0'), c8(b'0'), 0,
            ));
            let lim = _mm256_broadcastsi128_si256(_mm_setr_epi8(-1, -1, -1, -1, -1, -1, 0, 2, 9, 0, 5, 9, 0, 5, 9, -1));
            let x = _mm256_sub_epi8(v, base);
            let okm = _mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_min_epu8(x, lim), x)) as u32;
            let sh = _mm256_broadcastsi128_si256(_mm_setr_epi8(7, 8, 10, 11, 13, 14, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1));
            let dg = _mm256_shuffle_epi8(x, sh);
            let w = _mm256_maddubs_epi16(dg, _mm256_broadcastsi128_si256(_mm_setr_epi8(10, 1, 10, 1, 10, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0)));
            let m = _mm256_madd_epi16(w, _mm256_broadcastsi128_si256(_mm_setr_epi16(60, 1, 1, 0, 0, 0, 0, 0)));
            let ta = _mm256_extract_epi64::<0>(m) as u64;
            let tb = _mm256_extract_epi64::<2>(m) as u64;
            let (da, ha) = self.date_hit(a);
            let (db, hb) = self.date_hit(b);
            let (mina, minb) = (ta & 0xFFFF_FFFF, tb & 0xFFFF_FFFF);
            let ok_a = (okm & 0xFFFF == 0xFFFF) & ha & (mina < 1440);
            let ok_b = (okm >> 16 == 0xFFFF) & hb & (minb < 1440);
            let ms_a = (da * 86_400 + (mina * 60 + (ta >> 32)) as i64) * SECOND;
            let ms_b = (db * 86_400 + (minb * 60 + (tb >> 32)) as i64) * SECOND;
            (ms_a, ok_a, ms_b, ok_b)
        }
    }

    /// Days of a cached `YYYY-MM-DD` at p (10 bytes readable): (days, hit). Never inserts.
    #[inline(always)]
    unsafe fn date_hit(&self, p: *const u8) -> (i64, bool) {
        let k8 = unsafe { (p as *const u64).read_unaligned() };
        let k2 = unsafe { (p.add(8) as *const u16).read_unaligned() };
        let h = (((k2 & 3) | ((k2 >> 6) & 0x3C)) as usize) | ((k8 >> 42) & 0x40) as usize;
        let hit = unsafe { (*self.k8.get_unchecked(h) == k8) & (*self.k2.get_unchecked(h) == k2) };
        (unsafe { *self.days.get_unchecked(h) }, hit)
    }

    /// F1 values of two fields at once (32 bytes readable at a and b), both in one vector pass.
    /// (ms_a, ok_a, ms_b, ok_b): ok means the field is F1 with a cached date and a valid time, and then
    /// the value equals `parse`'s; otherwise the caller must use `parse`.
    #[cfg(target_feature = "avx2")]
    #[inline(always)]
    pub unsafe fn f1_pair(&self, a: *const u8, na: usize, b: *const u8, nb: usize) -> (i64, bool, i64, bool) {
        use std::arch::x86_64::*;
        unsafe {
            // bytes 4..20 of each field: date tail (unchecked here: a cache hit validates the date),
            // 'T', HH, ':', MM, ':', SS, 'Z'
            let v = _mm256_inserti128_si256::<1>(
                _mm256_castsi128_si256(_mm_loadu_si128(a.add(4) as *const __m128i)),
                _mm_loadu_si128(b.add(4) as *const __m128i),
            );
            let c8 = |x: u8| x as i8;
            let base = _mm256_broadcastsi128_si256(_mm_setr_epi8(
                0, 0, 0, 0, 0, 0, c8(b'T'), c8(b'0'), c8(b'0'), c8(b':'), c8(b'0'), c8(b'0'), c8(b':'), c8(b'0'), c8(b'0'), c8(b'Z'),
            ));
            let lim = _mm256_broadcastsi128_si256(_mm_setr_epi8(-1, -1, -1, -1, -1, -1, 0, 2, 9, 0, 5, 9, 0, 5, 9, 0));
            let x = _mm256_sub_epi8(v, base);
            let okm = _mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_min_epu8(x, lim), x)) as u32;
            let sh = _mm256_broadcastsi128_si256(_mm_setr_epi8(7, 8, 10, 11, 13, 14, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1));
            let dg = _mm256_shuffle_epi8(x, sh);
            let w = _mm256_maddubs_epi16(dg, _mm256_broadcastsi128_si256(_mm_setr_epi8(10, 1, 10, 1, 10, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0)));
            // dwords: HH * 60 + MM, SS
            let m = _mm256_madd_epi16(w, _mm256_broadcastsi128_si256(_mm_setr_epi16(60, 1, 1, 0, 0, 0, 0, 0)));
            let ta = _mm256_extract_epi64::<0>(m) as u64;
            let tb = _mm256_extract_epi64::<2>(m) as u64;
            let (da, ha) = self.date_hit(a);
            let (db, hb) = self.date_hit(b);
            let (mina, minb) = (ta & 0xFFFF_FFFF, tb & 0xFFFF_FFFF);
            let ok_a = (na == 20) & (okm & 0xFFFF == 0xFFFF) & ha & (mina < 1440);
            let ok_b = (nb == 20) & (okm >> 16 == 0xFFFF) & hb & (minb < 1440);
            let ms_a = (da * 86_400 + (mina * 60 + (ta >> 32)) as i64) * SECOND;
            let ms_b = (db * 86_400 + (minb * 60 + (tb >> 32)) as i64) * SECOND;
            (ms_a, ok_a, ms_b, ok_b)
        }
    }
}
