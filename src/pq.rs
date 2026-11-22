//! Minimal Parquet writer: one row group, v1 data pages (at most PAGE_ROWS rows each), uncompressed,
//! thrift-compact footer. Supports required/optional BYTE_ARRAY (UTF8), INT32 (optionally DATE) and INT64,
//! PLAIN, RLE_DICTIONARY (PLAIN dictionary page) or DELTA_BINARY_PACKED. The whole file is built in memory
//! and written with one call.

use std::fs::File;
use std::io::Write;
use std::path::Path;

const PAGE_ROWS: usize = 1 << 20;

/// Dictionary values (written as one PLAIN dictionary page).
pub enum Dict<'a> {
    Str { bytes: &'a [u8], ends: &'a [u32] },
    I32(&'a [i32]),
}

pub enum Data<'a> {
    /// (concatenated bytes, end offsets); optional: present flags (None = required)
    Str { bytes: &'a [u8], ends: &'a [u32], present: Option<&'a [bool]> },
    I32(&'a [i32]),
    I64(&'a [i64]),
    /// RLE_DICTIONARY: one index (< dictionary size) per present value. `runs`: RLE runs only (for long
    /// runs of equal indices) instead of one bit-packed run.
    Dict { dict: Dict<'a>, idx: &'a [u32], present: Option<&'a [bool]>, runs: bool },
    /// DELTA_BINARY_PACKED (required columns)
    DeltaI32(&'a [i32]),
    DeltaI64(&'a [i64]),
}

pub struct Column<'a> {
    pub name: &'a str,
    pub date: bool,
    pub data: Data<'a>,
}

// ---------------- thrift compact protocol
struct T {
    buf: Vec<u8>,
    last: Vec<i16>,
}

impl T {
    fn new() -> Self {
        T { buf: Vec::new(), last: vec![0] }
    }
    fn varint(&mut self, v: u64) {
        varint(&mut self.buf, v);
    }
    fn zz(&mut self, v: i64) {
        self.varint(((v << 1) ^ (v >> 63)) as u64);
    }
    fn field(&mut self, id: i16, ty: u8) {
        let last = *self.last.last().unwrap();
        let d = id - last;
        if d > 0 && d <= 15 {
            self.buf.push(((d as u8) << 4) | ty);
        } else {
            self.buf.push(ty);
            self.zz(id as i64);
        }
        *self.last.last_mut().unwrap() = id;
    }
    fn i32f(&mut self, id: i16, v: i32) {
        self.field(id, 5);
        self.zz(v as i64);
    }
    fn i64f(&mut self, id: i16, v: i64) {
        self.field(id, 6);
        self.zz(v);
    }
    fn strf(&mut self, id: i16, s: &str) {
        self.field(id, 8);
        self.varint(s.len() as u64);
        self.buf.extend_from_slice(s.as_bytes());
    }
    fn begin(&mut self, id: i16) {
        self.field(id, 12);
        self.last.push(0);
    }
    fn begin_elem(&mut self) {
        self.last.push(0);
    }
    fn end(&mut self) {
        self.buf.push(0);
        self.last.pop();
    }
    fn list(&mut self, id: i16, elem: u8, n: usize) {
        self.field(id, 9);
        if n < 15 {
            self.buf.push(((n as u8) << 4) | elem);
        } else {
            self.buf.push(0xF0 | elem);
            self.varint(n as u64);
        }
    }
}

#[inline(always)]
fn varint(buf: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        buf.push((v as u8) | 0x80);
        v >>= 7;
    }
    buf.push(v as u8);
}

/// LSB-first bit packer (Parquet bit-packing order).
struct Bits<'b> {
    out: &'b mut Vec<u8>,
    acc: u128,
    n: u32,
}

impl Bits<'_> {
    #[inline(always)]
    fn put(&mut self, v: u64, w: u32) {
        self.acc |= (v as u128) << self.n;
        self.n += w;
        if self.n >= 64 {
            self.out.extend_from_slice(&(self.acc as u64).to_le_bytes());
            self.acc >>= 64;
            self.n -= 64;
        }
    }
    fn finish(self) {
        let bytes = self.n.div_ceil(8) as usize;
        self.out.extend_from_slice(&self.acc.to_le_bytes()[..bytes]);
    }
}

fn bit_width(max: u64) -> u32 {
    64 - max.leading_zeros()
}

/// RLE/bit-packed hybrid, all values in bit-packed groups of 8 (zero padded).
fn hybrid_packed(out: &mut Vec<u8>, v: &[u32], w: u32) {
    let groups = v.len().div_ceil(8);
    varint(out, ((groups as u64) << 1) | 1);
    out.reserve(groups * w as usize + 16);
    let mut b = Bits { out, acc: 0, n: 0 };
    for &x in v {
        b.put(x as u64, w);
    }
    for _ in v.len()..groups * 8 {
        b.put(0, w);
    }
    b.finish();
}

/// RLE/bit-packed hybrid, RLE runs only.
fn hybrid_runs(out: &mut Vec<u8>, v: &[u32], w: u32) {
    let vb = w.div_ceil(8) as usize;
    let mut i = 0;
    while i < v.len() {
        let x = v[i];
        let mut j = i + 1;
        while j < v.len() && v[j] == x {
            j += 1;
        }
        varint(out, ((j - i) as u64) << 1);
        out.extend_from_slice(&x.to_le_bytes()[..vb]);
        i = j;
    }
}

/// DELTA_BINARY_PACKED: blocks of 128 values in 4 miniblocks of 32.
fn delta(out: &mut Vec<u8>, v: &[i64]) {
    varint(out, 128);
    varint(out, 4);
    varint(out, v.len() as u64);
    let first = v.first().copied().unwrap_or(0);
    varint(out, ((first << 1) ^ (first >> 63)) as u64);
    let mut d = [0i64; 128];
    let mut i = 1;
    while i < v.len() {
        let n = (v.len() - i).min(128);
        let mut mn = i64::MAX;
        for k in 0..n {
            let x = v[i + k].wrapping_sub(v[i + k - 1]);
            d[k] = x;
            mn = mn.min(x);
        }
        varint(out, ((mn << 1) ^ (mn >> 63)) as u64);
        let mut ws = [0u32; 4];
        for (m, w) in ws.iter_mut().enumerate() {
            let lo = m * 32;
            if lo >= n {
                break;
            }
            let mut or = 0u64;
            for &x in &d[lo..n.min(lo + 32)] {
                or |= x.wrapping_sub(mn) as u64;
            }
            *w = bit_width(or);
        }
        out.extend(ws.iter().map(|&w| w as u8));
        for (m, &w) in ws.iter().enumerate() {
            let lo = m * 32;
            if lo >= n {
                break;
            }
            if w == 0 {
                continue;
            }
            out.reserve(4 * w as usize + 16);
            let mut b = Bits { out, acc: 0, n: 0 };
            for k in lo..lo + 32 {
                let x = if k < n { d[k].wrapping_sub(mn) as u64 } else { 0 };
                b.put(x, w);
            }
            b.finish();
        }
        i += n;
    }
}

/// Definition levels for an optional column (one bit-packed run, bit width 1, 4-byte length prefix).
/// Returns the number of present values.
fn def_levels(out: &mut Vec<u8>, p: &[bool]) -> usize {
    let groups = p.len().div_ceil(8);
    let mut lv = Vec::with_capacity(groups + 8);
    varint(&mut lv, ((groups as u64) << 1) | 1);
    let at = lv.len();
    lv.resize(at + groups, 0);
    let mut n = 0;
    for (i, &x) in p.iter().enumerate() {
        if x {
            lv[at + (i >> 3)] |= 1 << (i & 7);
            n += 1;
        }
    }
    out.extend_from_slice(&(lv.len() as u32).to_le_bytes());
    out.extend_from_slice(&lv);
    n
}

struct ChunkMeta {
    name: String,
    ty: i32,
    num_values: i64,
    size: i64,
    offset: i64,
    data_offset: i64,
    dict: bool,
    enc: i32,
    optional: bool,
}

fn page_header(n: usize, size: usize, enc: i32) -> Vec<u8> {
    let mut t = T::new();
    t.i32f(1, 0); // DATA_PAGE
    t.i32f(2, size as i32);
    t.i32f(3, size as i32);
    t.begin(5);
    t.i32f(1, n as i32);
    t.i32f(2, enc);
    t.i32f(3, 3); // RLE
    t.i32f(4, 3); // RLE
    t.end();
    t.buf.push(0);
    t.buf
}

fn dict_page_header(n: usize, size: usize) -> Vec<u8> {
    let mut t = T::new();
    t.i32f(1, 2); // DICTIONARY_PAGE
    t.i32f(2, size as i32);
    t.i32f(3, size as i32);
    t.begin(7);
    t.i32f(1, n as i32);
    t.i32f(2, 0); // PLAIN
    t.end();
    t.buf.push(0);
    t.buf
}

fn plain_strs(page: &mut Vec<u8>, bytes: &[u8], ends: &[u32], s: usize) {
    let mut s = s;
    page.reserve(ends.len() * 4 + ends.last().map_or(0, |&e| e as usize - s));
    for &e in ends {
        let e = e as usize;
        page.extend_from_slice(&((e - s) as u32).to_le_bytes());
        page.extend_from_slice(&bytes[s..e]);
        s = e;
    }
}

pub fn write(path: &Path, rows: usize, cols: &[Column<'_>]) -> std::io::Result<()> {
    let mut buf: Vec<u8> = Vec::with_capacity(rows * 16 + 4096);
    buf.extend_from_slice(b"PAR1");
    let mut metas = Vec::new();
    let mut page = Vec::new();
    let mut i64s: Vec<i64> = Vec::new();
    for c in cols {
        let offset = buf.len() as i64;
        let (ty, optional, enc) = match &c.data {
            Data::Str { present, .. } => (6, present.is_some(), 0),
            Data::I32(_) => (1, false, 0),
            Data::I64(_) => (2, false, 0),
            Data::Dict { dict: Dict::Str { .. }, present, .. } => (6, present.is_some(), 8),
            Data::Dict { dict: Dict::I32(_), present, .. } => (1, present.is_some(), 8),
            Data::DeltaI32(_) => (1, false, 5),
            Data::DeltaI64(_) => (2, false, 5),
        };
        // dictionary page first
        let mut dict_w = 0u32;
        if let Data::Dict { dict, .. } = &c.data {
            page.clear();
            let n = match dict {
                Dict::Str { bytes, ends } => {
                    plain_strs(&mut page, bytes, ends, 0);
                    ends.len()
                }
                Dict::I32(v) => {
                    for x in v.iter() {
                        page.extend_from_slice(&x.to_le_bytes());
                    }
                    v.len()
                }
            };
            dict_w = bit_width(n.saturating_sub(1) as u64).max(1);
            let ph = dict_page_header(n, page.len());
            buf.extend_from_slice(&ph);
            buf.extend_from_slice(&page);
        }
        let data_offset = buf.len() as i64;
        let mut r0 = 0;
        // value cursor for optional columns (non-null values are stored densely)
        let mut v0 = 0usize;
        while r0 < rows || (rows == 0 && r0 == 0) {
            let r1 = (r0 + PAGE_ROWS).min(rows);
            page.clear();
            match &c.data {
                Data::Str { bytes, ends, present } => {
                    let v1 = match present {
                        Some(p) => v0 + def_levels(&mut page, &p[r0..r1]),
                        None => r1,
                    };
                    let s = if v0 == 0 { 0 } else { ends[v0 - 1] as usize };
                    plain_strs(&mut page, bytes, &ends[v0..v1], s);
                    v0 = v1;
                }
                Data::I32(v) => {
                    for x in &v[r0..r1] {
                        page.extend_from_slice(&x.to_le_bytes());
                    }
                }
                Data::I64(v) => {
                    for x in &v[r0..r1] {
                        page.extend_from_slice(&x.to_le_bytes());
                    }
                }
                Data::Dict { idx, present, runs, .. } => {
                    let v1 = match present {
                        Some(p) => v0 + def_levels(&mut page, &p[r0..r1]),
                        None => r1,
                    };
                    page.push(dict_w as u8);
                    if *runs {
                        hybrid_runs(&mut page, &idx[v0..v1], dict_w);
                    } else {
                        hybrid_packed(&mut page, &idx[v0..v1], dict_w);
                    }
                    v0 = v1;
                }
                Data::DeltaI32(v) => {
                    i64s.clear();
                    i64s.extend(v[r0..r1].iter().map(|&x| x as i64));
                    delta(&mut page, &i64s);
                }
                Data::DeltaI64(v) => delta(&mut page, &v[r0..r1]),
            }
            let ph = page_header(r1 - r0, page.len(), enc);
            buf.extend_from_slice(&ph);
            buf.extend_from_slice(&page);
            r0 = r1;
            if rows == 0 {
                break;
            }
        }
        metas.push(ChunkMeta {
            name: c.name.to_string(),
            ty,
            num_values: rows as i64,
            size: buf.len() as i64 - offset,
            offset,
            data_offset,
            dict: enc == 8,
            enc,
            optional,
        });
    }
    // footer: FileMetaData
    let mut t = T::new();
    t.i32f(1, 1);
    t.list(2, 12, cols.len() + 1);
    t.begin_elem();
    t.strf(4, "schema");
    t.i32f(5, cols.len() as i32);
    t.end();
    for (c, m) in cols.iter().zip(&metas) {
        t.begin_elem();
        t.i32f(1, m.ty);
        t.i32f(3, if m.optional { 1 } else { 0 });
        t.strf(4, &m.name);
        if m.ty == 6 {
            t.i32f(6, 0); // UTF8
            t.begin(10);
            t.begin(1); // STRING
            t.end();
            t.end();
        } else if c.date {
            t.i32f(6, 6); // DATE
            t.begin(10);
            t.begin(6); // DATE
            t.end();
            t.end();
        }
        t.end();
    }
    t.i64f(3, rows as i64);
    t.list(4, 12, 1);
    t.begin_elem();
    t.list(1, 12, metas.len());
    let mut total = 0i64;
    for m in &metas {
        total += m.size;
        t.begin_elem();
        t.i64f(2, m.offset);
        t.begin(3);
        t.i32f(1, m.ty);
        if m.dict {
            t.list(2, 5, 3);
            t.zz(0); // PLAIN (dictionary page)
            t.zz(3); // RLE
            t.zz(8); // RLE_DICTIONARY
        } else {
            t.list(2, 5, 2);
            t.zz(m.enc as i64);
            t.zz(3); // RLE
        }
        t.list(3, 8, 1);
        t.varint(m.name.len() as u64);
        t.buf.extend_from_slice(m.name.as_bytes());
        t.i32f(4, 0); // UNCOMPRESSED
        t.i64f(5, m.num_values);
        t.i64f(6, m.size);
        t.i64f(7, m.size);
        t.i64f(9, m.data_offset);
        if m.dict {
            t.i64f(11, m.offset);
        }
        t.end();
        t.end();
    }
    t.i64f(2, total);
    t.i64f(3, rows as i64);
    t.end();
    t.strf(6, "sessions");
    t.buf.push(0);
    let footer = t.buf;
    buf.extend_from_slice(&footer);
    buf.extend_from_slice(&(footer.len() as u32).to_le_bytes());
    buf.extend_from_slice(b"PAR1");
    File::create(path)?.write_all(&buf)
}
