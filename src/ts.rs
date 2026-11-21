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
        25 => {
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
}
