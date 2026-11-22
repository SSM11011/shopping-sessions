//! Minimal Parquet writer: one row group, one PLAIN uncompressed v1 data page per column chunk (split into
//! pages of at most PAGE_ROWS rows), thrift-compact footer. Supports required/optional BYTE_ARRAY (UTF8),
//! INT32 (optionally DATE) and INT64.

use std::fs::File;
use std::io::Write;
use std::path::Path;

const PAGE_ROWS: usize = 1 << 20;

pub enum Data<'a> {
    /// (concatenated bytes, end offsets); optional: present flags (None = required)
    Str { bytes: &'a [u8], ends: &'a [u32], present: Option<&'a [bool]> },
    I32(&'a [i32]),
    I64(&'a [i64]),
    /// i64 values that all fit in i32, stored as INT32 (4 bytes each)
    I64As32(&'a [i64]),
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
    fn varint(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.buf.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.buf.push(v as u8);
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

struct ChunkMeta {
    name: String,
    ty: i32,
    num_values: i64,
    size: i64,
    offset: i64,
    optional: bool,
}

fn page_header(n: usize, size: usize) -> Vec<u8> {
    let mut t = T::new();
    t.i32f(1, 0); // DATA_PAGE
    t.i32f(2, size as i32);
    t.i32f(3, size as i32);
    t.begin(5);
    t.i32f(1, n as i32);
    t.i32f(2, 0); // PLAIN
    t.i32f(3, 3); // RLE
    t.i32f(4, 3); // RLE
    t.end();
    t.buf.push(0);
    t.buf
}

pub fn write(path: &Path, rows: usize, cols: &[Column<'_>]) -> std::io::Result<()> {
    // pages stream straight to the file through one reused buffer (no whole-file buffer)
    let mut out = std::io::BufWriter::with_capacity(1 << 20, File::create(path)?);
    let mut pos: usize = 0;
    out.write_all(b"PAR1")?;
    pos += 4;
    let mut metas = Vec::new();
    let mut page = Vec::new();
    for c in cols {
        let offset = pos as i64;
        let (ty, optional) = match &c.data {
            Data::Str { present, .. } => (6, present.is_some()),
            Data::I32(_) => (1, false),
            Data::I64(_) => (2, false),
            Data::I64As32(_) => (1, false),
        };
        let mut r0 = 0;
        // value cursor for optional strings (non-null values are stored densely)
        let mut v0 = 0usize;
        while r0 < rows || (rows == 0 && r0 == 0) {
            let r1 = (r0 + PAGE_ROWS).min(rows);
            page.clear();
            match &c.data {
                Data::Str { bytes, ends, present } => {
                    let mut v1 = v0;
                    if let Some(p) = present {
                        // definition levels: one bit-packed run, bit width 1, with a 4-byte length prefix
                        let n = r1 - r0;
                        let groups = n.div_ceil(8);
                        let mut lv = T::new();
                        lv.varint(((groups as u64) << 1) | 1);
                        let mut bits = vec![0u8; groups];
                        for (i, &x) in p[r0..r1].iter().enumerate() {
                            if x {
                                bits[i >> 3] |= 1 << (i & 7);
                                v1 += 1;
                            }
                        }
                        lv.buf.extend_from_slice(&bits);
                        page.extend_from_slice(&(lv.buf.len() as u32).to_le_bytes());
                        page.extend_from_slice(&lv.buf);
                    } else {
                        v1 = r1;
                    }
                    let mut s = if v0 == 0 { 0 } else { ends[v0 - 1] as usize };
                    page.reserve((v1 - v0) * 4 + ends.get(v1.wrapping_sub(1)).map_or(0, |&e| e as usize - s));
                    for &e in &ends[v0..v1] {
                        let e = e as usize;
                        page.extend_from_slice(&((e - s) as u32).to_le_bytes());
                        page.extend_from_slice(&bytes[s..e]);
                        s = e;
                    }
                    v0 = v1;
                }
                Data::I32(v) => {
                    for x in &v[r0..r1] {
                        page.extend_from_slice(&x.to_le_bytes());
                    }
                }
                Data::I64As32(v) => {
                    for x in &v[r0..r1] {
                        page.extend_from_slice(&(*x as i32).to_le_bytes());
                    }
                }
                Data::I64(v) => {
                    for x in &v[r0..r1] {
                        page.extend_from_slice(&x.to_le_bytes());
                    }
                }
            }
            let ph = page_header(r1 - r0, page.len());
            out.write_all(&ph)?;
            out.write_all(&page)?;
            pos += ph.len() + page.len();
            r0 = r1;
            if rows == 0 {
                break;
            }
        }
        metas.push(ChunkMeta {
            name: c.name.to_string(),
            ty,
            num_values: rows as i64,
            size: pos as i64 - offset,
            offset,
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
        t.list(2, 5, 2);
        t.zz(0); // PLAIN
        t.zz(3); // RLE
        t.list(3, 8, 1);
        t.varint(m.name.len() as u64);
        t.buf.extend_from_slice(m.name.as_bytes());
        t.i32f(4, 0); // UNCOMPRESSED
        t.i64f(5, m.num_values);
        t.i64f(6, m.size);
        t.i64f(7, m.size);
        t.i64f(9, m.offset);
        t.end();
        t.end();
    }
    t.i64f(2, total);
    t.i64f(3, rows as i64);
    t.end();
    t.strf(6, "sessions");
    t.buf.push(0);
    let footer = t.buf;
    out.write_all(&footer)?;
    out.write_all(&(footer.len() as u32).to_le_bytes())?;
    out.write_all(b"PAR1")?;
    out.flush()
}
