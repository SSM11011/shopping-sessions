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
    k8: [u64; 64],
    k2: [u16; 64],
    days: [i64; 64],
}

impl Default for Fast {
    fn default() -> Self {
        Fast { k8: [u64::MAX; 64], k2: [0; 64], days: [0; 64] }
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
    let b = le64(s, 5); // bytes 5..13 (overlaps 5..8)
    let ok = |x: u64| (x & 0xF0F0_F0F0_F0F0_F0F0) == 0x3030_3030_3030_3030
        && (x.wrapping_add(0x0606_0606_0606_0606) & 0xF0F0_F0F0_F0F0_F0F0) == 0x3030_3030_3030_3030;
    if !ok(a) || !ok(b) {
        return None;
    }
    // value = first 8 digits * 10^5 + last 5 digits; last 5 = parse8(b) mod 10^5 (b holds digits 5..13)
    let hi = parse8(a);
    let lo = parse8(b) % 100_000;
    Some((hi * 100_000 + lo) as i64)
}

impl Fast {
    #[inline(always)]
    fn date(&mut self, s: &[u8]) -> Option<i64> {
        let k8 = le64(s, 0);
        let k2 = u16::from_le_bytes([s[8], s[9]]);
        let h = ((k8 ^ (k2 as u64).wrapping_mul(0x9E37_79B9)).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 58) as usize;
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
