mod keys;
mod out;
mod ts;

use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use keys::{cmp_key, hash, key, EMPTY};
use memmap2::Mmap;
use ts::DAY;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const WEEK: i64 = 7 * DAY;
const GAP: u32 = 1_800_000;
const NO_TS: u32 = u32::MAX;
const CHUNK: usize = 4 << 20;

/// Event candidate: kept for dedup even when invalid (ts == NO_TS).
#[derive(Clone, Copy)]
struct Ev {
    eid: u64,
    user: u64,
    ingest: i64,
    ts: u32,
    price: u32,
    cat: u32,
}

/// A surviving event, keyed by person.
#[derive(Clone, Copy)]
struct Pe {
    person: u64,
    ts: u32,
    price: u32,
    cat: u32,
}

/// A valid click. `user` becomes the person after link resolution.
#[derive(Clone, Copy)]
struct Ck {
    user: u64,
    click: u64,
    ts: u32,
    camp: u32,
}

// ---------------------------------------------------------------- CSV

#[inline(always)]
fn unquote(f: &[u8]) -> Option<&[u8]> {
    if memchr(b'"', f).is_none() {
        return Some(f);
    }
    let n = f.len();
    if n >= 2 && f[0] == b'"' && f[n - 1] == b'"' && memchr(b'"', &f[1..n - 1]).is_none() {
        Some(&f[1..n - 1])
    } else {
        None
    }
}

#[inline(always)]
fn memchr(b: u8, s: &[u8]) -> Option<usize> {
    s.iter().position(|&x| x == b)
}

/// Split a line into exactly N unquoted fields; false if malformed.
#[inline(always)]
fn split<'a, const N: usize>(line: &'a [u8], out: &mut [&'a [u8]; N]) -> bool {
    let mut n = 0;
    let mut start = 0;
    let len = line.len();
    loop {
        let end = match memchr(b',', &line[start..]) {
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

/// Calls f on every line in [s, e) of a file's bytes (s/e nominal; lines belong to the chunk they start in).
fn for_lines(m: &[u8], s: usize, e: usize, mut f: impl FnMut(&[u8])) {
    let align = |p: usize| -> usize {
        if p == 0 {
            0
        } else if p >= m.len() {
            m.len()
        } else {
            match memchr(b'\n', &m[p - 1..]) {
                Some(q) => p - 1 + q + 1,
                None => m.len(),
            }
        }
    };
    let mut at = align(s);
    let end = align(e);
    if s == 0 {
        // header line
        at = match memchr(b'\n', m) {
            Some(q) => q + 1,
            None => m.len(),
        };
        if at > end {
            return;
        }
    }
    while at < end {
        let nl = match memchr(b'\n', &m[at..]) {
            Some(q) => at + q,
            None => m.len(),
        };
        if nl > at {
            f(&m[at..nl]);
        }
        at = nl + 1;
    }
}

fn read_small(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_default()
}

fn small_lines<const N: usize>(data: &[u8], mut f: impl FnMut([&[u8]; N])) {
    for_lines(data, 0, data.len(), |line| {
        let mut fs = [&b""[..]; N];
        if split(line, &mut fs) {
            f(fs);
        }
    });
}

// ---------------------------------------------------------------- lookups

/// Exact string -> u32 map with a dense fast path for `prefix` + decimal number (no leading zero).
struct NumMap {
    prefix: &'static [u8],
    table: Vec<u32>,
    other: HashMap<Vec<u8>, u32>,
}

impl NumMap {
    fn new(prefix: &'static [u8]) -> Self {
        NumMap { prefix, table: Vec::new(), other: HashMap::new() }
    }
    #[inline(always)]
    fn num(&self, s: &[u8]) -> Option<usize> {
        let p = self.prefix.len();
        if s.len() <= p || s.len() > p + 9 || &s[..p] != self.prefix || s[p] == b'0' {
            return None;
        }
        let mut n = 0usize;
        for &b in &s[p..] {
            let x = b.wrapping_sub(b'0');
            if x >= 10 {
                return None;
            }
            n = n * 10 + x as usize;
        }
        Some(n)
    }
    fn insert(&mut self, s: &[u8], v: u32) {
        match self.num(s) {
            Some(n) if n < 10_000_000 => {
                if self.table.len() <= n {
                    self.table.resize(n + 1, u32::MAX);
                }
                self.table[n] = v;
            }
            _ => {
                self.other.insert(s.to_vec(), v);
            }
        }
    }
    #[inline(always)]
    fn get(&self, s: &[u8]) -> Option<u32> {
        if let Some(n) = self.num(s) {
            if n < self.table.len() {
                let v = self.table[n];
                return if v == u32::MAX { None } else { Some(v) };
            }
        }
        self.other.get(s).copied()
    }
}

// ---------------------------------------------------------------- link map

struct LinkMap {
    mask: usize,
    keys: Vec<u64>,
    vals: Vec<u64>,
}

impl LinkMap {
    fn new(n: usize) -> Self {
        let cap = (n * 2).next_power_of_two().max(16);
        LinkMap { mask: cap - 1, keys: vec![EMPTY; cap], vals: vec![0; cap] }
    }
    fn insert(&mut self, k: u64, v: u64) {
        let mut i = hash(k) as usize & self.mask;
        loop {
            let s = self.keys[i];
            if s == EMPTY || s == k {
                self.keys[i] = k;
                self.vals[i] = v;
                return;
            }
            i = (i + 1) & self.mask;
        }
    }
    #[inline(always)]
    fn person(&self, k: u64) -> u64 {
        let mut i = hash(k) as usize & self.mask;
        loop {
            let s = self.keys[i];
            if s == k {
                return self.vals[i];
            }
            if s == EMPTY {
                return k;
            }
            i = (i + 1) & self.mask;
        }
    }
}

// ---------------------------------------------------------------- setup

struct Ctx {
    ws: i64,
    we: i64,
    products: NumMap,
    unknown_cat: u32,
    campaigns: NumMap,
}

fn manifest_ms(text: &str, field: &str) -> i64 {
    let at = text.find(field).expect("manifest field");
    let rest = &text[at + field.len()..];
    let q1 = rest.find('"').unwrap();
    let rest = &rest[q1 + 1..];
    let q2 = rest.find('"').unwrap();
    ts::parse(rest[..q2].as_bytes()).expect("manifest ts")
}

fn list_csv(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "csv") && p.is_file())
            .collect(),
        Err(_) => Vec::new(),
    };
    v.sort();
    v
}

fn run_parallel(threads: usize, n_tasks: usize, f: impl Fn(usize, usize) + Sync) {
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for w in 0..threads {
            let f = &f;
            let next = &next;
            s.spawn(move || loop {
                let t = next.fetch_add(1, Ordering::Relaxed);
                if t >= n_tasks {
                    break;
                }
                f(w, t);
            });
        }
    });
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Event,
    Click,
    Link,
}

struct Task {
    kind: Kind,
    file: usize,
    s: usize,
    e: usize,
}

#[derive(Default)]
struct ParseOut {
    ev: Vec<Vec<Ev>>,
    ck: Vec<Ck>,
    links: Vec<(u64, u64)>,
}

fn main() {
    let t0 = Instant::now();
    let args: Vec<String> = std::env::args().collect();
    let data = PathBuf::from(std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".into()));
    let outdir = PathBuf::from(std::env::var("OUTPUT_DIR").unwrap_or_else(|_| "/output".into()));
    let _ = args;
    let threads: usize = std::env::var("NCPU")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(8, |n| n.get()));
    let threads = threads.max(1);

    // ---- lookups
    let manifest = std::fs::read_to_string(data.join("manifest.json")).expect("manifest");
    let ws = manifest_ms(&manifest, "\"window_start\"");
    let we = manifest_ms(&manifest, "\"window_end\"");
    assert!(we - ws + WEEK < u32::MAX as i64, "window too long");

    let prod_bytes = read_small(&data.join("products.csv"));
    let mut prod_rows: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    small_lines::<2>(&prod_bytes, |[p, c]| prod_rows.push((p.to_vec(), c.to_vec())));
    let mut cat_names: Vec<Vec<u8>> = prod_rows.iter().map(|r| r.1.clone()).collect();
    cat_names.push(b"unknown".to_vec());
    cat_names.sort();
    cat_names.dedup();
    let cat_id: HashMap<Vec<u8>, u32> =
        cat_names.iter().enumerate().map(|(i, c)| (c.clone(), i as u32)).collect();
    let mut products = NumMap::new(b"p");
    for (p, c) in &prod_rows {
        products.insert(p, cat_id[c]);
    }
    let unknown_cat = cat_id[&b"unknown"[..]];

    let camp_bytes = read_small(&data.join("campaigns.csv"));
    let mut camp_ids: Vec<Vec<u8>> = Vec::new();
    let mut channels: Vec<Vec<u8>> = Vec::new();
    let mut campaigns = NumMap::new(b"c_");
    small_lines::<2>(&camp_bytes, |[c, ch]| {
        if !c.is_empty() && campaigns.get(c).is_none() {
            campaigns.insert(c, camp_ids.len() as u32);
            camp_ids.push(c.to_vec());
            channels.push(ch.to_vec());
        }
    });
    let n_camp = camp_ids.len();
    let ctx = Ctx { ws, we, products, unknown_cat, campaigns };

    // ---- files & tasks
    let mut maps: Vec<Mmap> = Vec::new();
    let mut tasks: Vec<Task> = Vec::new();
    let mut ev_bytes = 0usize;
    for (kind, dir) in [(Kind::Event, "events"), (Kind::Click, "clicks"), (Kind::Link, "links")] {
        for p in list_csv(&data.join(dir)) {
            let f = File::open(&p).expect("open");
            let len = f.metadata().map(|m| m.len() as usize).unwrap_or(0);
            if len == 0 {
                continue;
            }
            let m = unsafe { Mmap::map(&f).expect("mmap") };
            #[cfg(unix)]
            let _ = m.advise(memmap2::Advice::Sequential);
            let fi = maps.len();
            maps.push(m);
            if kind == Kind::Event {
                ev_bytes += len;
            }
            let mut s = 0;
            while s < len {
                let e = (s + CHUNK).min(len);
                tasks.push(Task { kind, file: fi, s, e });
                s = e;
            }
        }
    }
    // events first, biggest chunks first is implicit (all equal size)
    let est_rows = ev_bytes / 78 + 1;
    let nparts = (est_rows / 12_000).next_power_of_two().clamp(16, 8192);
    let pshift = 64 - nparts.trailing_zeros();
    let part_of = |k: u64| (hash(k) >> pshift) as usize;

    let outs: Vec<Mutex<ParseOut>> = (0..threads)
        .map(|_| {
            Mutex::new(ParseOut { ev: (0..nparts).map(|_| Vec::new()).collect(), ..Default::default() })
        })
        .collect();

    run_parallel(threads, tasks.len(), |w, t| {
        let task = &tasks[t];
        let m = &maps[task.file][..];
        let mut o = outs[w].lock().unwrap();
        let o = &mut *o;
        match task.kind {
            Kind::Event => for_lines(m, task.s, task.e, |line| {
                let mut f = [&b""[..]; 7];
                if !split(line, &mut f) || f[0].is_empty() {
                    return;
                }
                let eid = key(f[0]);
                let ingest = ts::parse(f[3]).unwrap_or(i64::MAX);
                let mut ev = Ev { eid, user: 0, ingest, ts: NO_TS, price: 0, cat: 0 };
                if let Some(v) = validate(&ctx, &f, ingest) {
                    ev.user = key(f[1]);
                    ev.ts = v.0;
                    ev.price = v.1;
                    ev.cat = v.2;
                }
                o.ev[part_of(eid)].push(ev);
            }),
            Kind::Click => for_lines(m, task.s, task.e, |line| {
                let mut f = [&b""[..]; 4];
                if !split(line, &mut f) || f[0].is_empty() || f[1].is_empty() {
                    return;
                }
                let Some(camp) = ctx.campaigns.get(f[2]) else { return };
                let Some(t) = ts::parse(f[3]) else { return };
                if t < ws - WEEK || t >= we {
                    return;
                }
                o.ck.push(Ck { user: key(f[1]), click: key(f[0]), ts: (t - (ws - WEEK)) as u32, camp });
            }),
            Kind::Link => for_lines(m, task.s, task.e, |line| {
                let mut f = [&b""[..]; 2];
                if !split(line, &mut f) || f[0].is_empty() || f[1].is_empty() {
                    return;
                }
                o.links.push((key(f[0]), key(f[1])));
            }),
        }
    });
    let mut outs: Vec<ParseOut> = outs.into_iter().map(|m| m.into_inner().unwrap()).collect();
    eprintln!("parse {:?} parts={nparts}", t0.elapsed());

    // ---- links: union-find, root = byte-wise smallest ID
    let mut dense: HashMap<u64, u32> = HashMap::new();
    let mut ids: Vec<u64> = Vec::new();
    let mut parent: Vec<u32> = Vec::new();
    fn find(parent: &mut [u32], mut x: u32) -> u32 {
        while parent[x as usize] != x {
            let p = parent[x as usize];
            parent[x as usize] = parent[p as usize];
            x = p;
        }
        x
    }
    for o in &mut outs {
        for (a, b) in std::mem::take(&mut o.links) {
            let mut id = |k: u64| -> u32 {
                *dense.entry(k).or_insert_with(|| {
                    ids.push(k);
                    parent.push(parent.len() as u32);
                    (ids.len() - 1) as u32
                })
            };
            let (ia, ib) = (id(a), id(b));
            let (ra, rb) = (find(&mut parent, ia), find(&mut parent, ib));
            if ra != rb {
                if cmp_key(ids[ra as usize], ids[rb as usize]).is_lt() {
                    parent[rb as usize] = ra;
                } else {
                    parent[ra as usize] = rb;
                }
            }
        }
    }
    let mut lmap = LinkMap::new(ids.len());
    for i in 0..ids.len() {
        let r = find(&mut parent, i as u32);
        lmap.insert(ids[i], ids[r as usize]);
    }
    eprintln!("links {:?} ids={}", t0.elapsed(), ids.len());

    // ---- dedup per eid-partition, resolve persons, re-partition by person
    let pe_outs: Vec<Mutex<Vec<Vec<Pe>>>> =
        (0..threads).map(|_| Mutex::new((0..nparts).map(|_| Vec::new()).collect())).collect();
    let ck_outs: Vec<Mutex<Vec<Vec<Ck>>>> =
        (0..threads).map(|_| Mutex::new((0..nparts).map(|_| Vec::new()).collect())).collect();
    let ck_in: Vec<Vec<Ck>> = outs.iter_mut().map(|o| std::mem::take(&mut o.ck)).collect();
    let ev_in: Vec<Vec<Vec<Ev>>> = outs.into_iter().map(|o| o.ev).collect();
    let lmap = &lmap;
    run_parallel(threads, nparts + ck_in.len(), |w, t| {
        if t < nparts {
            let n: usize = ev_in.iter().map(|v| v[t].len()).sum();
            let cap = (n * 2).next_power_of_two().max(16);
            let mask = cap - 1;
            let mut table: Vec<Ev> =
                vec![Ev { eid: EMPTY, user: 0, ingest: 0, ts: 0, price: 0, cat: 0 }; cap];
            for src in &ev_in {
                for e in &src[t] {
                    let mut i = (hash(e.eid) as usize >> 3) & mask;
                    loop {
                        let s = &mut table[i];
                        if s.eid == EMPTY {
                            *s = *e;
                            break;
                        }
                        if s.eid == e.eid {
                            if e.ingest < s.ingest {
                                *s = *e;
                            }
                            break;
                        }
                        i = (i + 1) & mask;
                    }
                }
            }
            let mut o = pe_outs[w].lock().unwrap();
            for e in &table {
                if e.eid != EMPTY && e.ts != NO_TS {
                    let person = lmap.person(e.user);
                    o[part_of(person)].push(Pe { person, ts: e.ts, price: e.price, cat: e.cat });
                }
            }
        } else {
            let mut o = ck_outs[w].lock().unwrap();
            for c in &ck_in[t - nparts] {
                let person = lmap.person(c.user);
                o[part_of(person)].push(Ck { user: person, ..*c });
            }
        }
    });
    drop(ev_in);
    drop(ck_in);
    let pe_in: Vec<Vec<Vec<Pe>>> = pe_outs.into_iter().map(|m| m.into_inner().unwrap()).collect();
    let ck_in: Vec<Vec<Vec<Ck>>> = ck_outs.into_iter().map(|m| m.into_inner().unwrap()).collect();
    eprintln!("dedup {:?}", t0.elapsed());

    // ---- sessions per person-partition
    let ws_day = ws.div_euclid(DAY);
    let ndays = ((we - 1).div_euclid(DAY) - ws_day + 1) as usize;
    let ncol = n_camp + 1; // + organic
    let results: Vec<Mutex<WorkerOut>> = (0..threads)
        .map(|_| Mutex::new(WorkerOut::new(ncol * ndays, n_camp)))
        .collect();
    run_parallel(threads, nparts, |w, t| {
        let mut wo = results[w].lock().unwrap();
        let wo = &mut *wo;
        let mut evs: Vec<Pe> = Vec::with_capacity(pe_in.iter().map(|v| v[t].len()).sum());
        for v in &pe_in {
            evs.extend_from_slice(&v[t]);
        }
        let mut cks: Vec<Ck> = Vec::with_capacity(ck_in.iter().map(|v| v[t].len()).sum());
        for v in &ck_in {
            cks.extend_from_slice(&v[t]);
        }
        evs.sort_unstable_by_key(|e| (e.person, e.ts));
        cks.sort_unstable_by(|a, b| {
            a.user.cmp(&b.user).then(a.ts.cmp(&b.ts)).then_with(|| cmp_key(a.click, b.click))
        });
        let mut ci = 0;
        let mut i = 0;
        while i < evs.len() {
            let person = evs[i].person;
            let mut j = i + 1;
            while j < evs.len() && evs[j].person == person {
                j += 1;
            }
            while ci < cks.len() && cks[ci].user < person {
                ci += 1;
            }
            let mut cj = ci;
            while cj < cks.len() && cks[cj].user == person {
                cj += 1;
            }
            person_sessions(wo, &evs[i..j], &cks[ci..cj], person, ws, ws_day, ndays, n_camp);
            ci = cj;
            i = j;
        }
    });
    let results: Vec<WorkerOut> = results.into_iter().map(|m| m.into_inner().unwrap()).collect();
    eprintln!("sessions {:?}", t0.elapsed());

    // ---- campaign_daily
    let mut camp_acc = vec![[0i64; 4]; ncol * ndays];
    for r in &results {
        for (a, b) in camp_acc.iter_mut().zip(&r.camp) {
            for k in 0..4 {
                a[k] += b[k];
            }
        }
    }
    out::write_all(&outdir, &results, &camp_acc, &camp_ids, &channels, &cat_names, ws_day, ndays, threads);
    eprintln!("total {:?}", t0.elapsed());
    std::process::exit(0);
}

/// Returns (ts_rel, price, cat) when the row is a valid, non-late event.
#[inline(always)]
fn validate(ctx: &Ctx, f: &[&[u8]; 7], ingest: i64) -> Option<(u32, u32, u32)> {
    if f[1].is_empty() || f[5].is_empty() {
        return None;
    }
    let t = ts::parse(f[2])?;
    if t < ctx.ws || t >= ctx.we {
        return None;
    }
    let price = match f[4] {
        b"view" | b"cart" => 0,
        b"purchase" => parse_price(f[6])?,
        _ => return None,
    };
    if ingest - t > DAY {
        return None;
    }
    let cat = ctx.products.get(f[5]).unwrap_or(ctx.unknown_cat);
    Some(((t - ctx.ws) as u32, price, cat))
}

#[inline(always)]
fn parse_price(s: &[u8]) -> Option<u32> {
    if s.is_empty() {
        return None;
    }
    let mut v: u64 = 0;
    for &b in s {
        let x = b.wrapping_sub(b'0');
        if x >= 10 {
            return None;
        }
        v = v * 10 + x as u64;
        if v > 10_000_000 {
            // keep scanning for non-digits? value already out of range -> invalid either way
            return None;
        }
    }
    if v == 0 { None } else { Some(v as u32) }
}

pub struct WorkerOut {
    pub names: Vec<u8>,
    pub name_end: Vec<u32>,
    pub day: Vec<i32>,
    pub sessions: Vec<i64>,
    pub events: Vec<i64>,
    pub revenue: Vec<i64>,
    pub top: Vec<u32>, // u32::MAX = null
    pub camp: Vec<[i64; 4]>,
    count: Vec<u32>,
    pos: Vec<u32>,
    active: Vec<u32>,
    cats: Vec<(u32, u64)>,
}

impl WorkerOut {
    fn new(cells: usize, n_camp: usize) -> Self {
        WorkerOut {
            names: Vec::new(),
            name_end: Vec::new(),
            day: Vec::new(),
            sessions: Vec::new(),
            events: Vec::new(),
            revenue: Vec::new(),
            top: Vec::new(),
            camp: vec![[0; 4]; cells],
            count: vec![0; n_camp],
            pos: vec![0; n_camp],
            active: Vec::new(),
            cats: Vec::new(),
        }
    }
    #[inline]
    fn add(&mut self, c: u32) {
        let c = c as usize;
        if self.count[c] == 0 {
            self.pos[c] = self.active.len() as u32;
            self.active.push(c as u32);
        }
        self.count[c] += 1;
    }
    #[inline]
    fn remove(&mut self, c: u32) {
        let c = c as usize;
        self.count[c] -= 1;
        if self.count[c] == 0 {
            let p = self.pos[c] as usize;
            let last = *self.active.last().unwrap();
            self.active[p] = last;
            self.pos[last as usize] = p as u32;
            self.active.pop();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn person_sessions(
    wo: &mut WorkerOut,
    evs: &[Pe],
    cks: &[Ck],
    person: u64,
    ws: i64,
    ws_day: i64,
    ndays: usize,
    n_camp: usize,
) {
    let week = WEEK as u32;
    let (mut lo, mut hi) = (0usize, 0usize);
    // current (person, day) accumulator
    let (mut a_sess, mut a_ev, mut a_rev) = (0i64, 0i64, 0i64);
    wo.cats.clear();
    let flush = |wo: &mut WorkerOut, day: i64, s: i64, e: i64, r: i64| {
        let start = wo.names.len();
        keys::bytes_into(person, &mut wo.names);
        let _ = start;
        wo.name_end.push(wo.names.len() as u32);
        wo.day.push(day as i32);
        wo.sessions.push(s);
        wo.events.push(e);
        wo.revenue.push(r);
        let mut top = u32::MAX;
        let mut best = 0u64;
        for &(c, v) in &wo.cats {
            if v > best || (v == best && c < top) {
                best = v;
                top = c;
            }
        }
        wo.top.push(top);
        wo.cats.clear();
    };
    let mut i = 0;
    while i < evs.len() {
        let start = evs[i].ts;
        let mut last = start;
        let mut j = i;
        let mut rev = 0i64;
        let mut purchased = false;
        while j < evs.len() && evs[j].ts - last <= GAP {
            last = evs[j].ts;
            let e = &evs[j];
            if e.price > 0 {
                purchased = true;
                rev += e.price as i64;
                match wo.cats.iter_mut().find(|x| x.0 == e.cat) {
                    Some(x) => x.1 += e.price as u64,
                    None => wo.cats.push((e.cat, e.price as u64)),
                }
            }
            j += 1;
        }
        let n = (j - i) as i64;
        let day = (ws + start as i64).div_euclid(DAY);
        let di = (day - ws_day) as usize;
        debug_assert!(di < ndays);
        // attribution: clicks in click-relative coords; session start s_c = start + week
        let s_c = start + week;
        while hi < cks.len() && cks[hi].ts <= s_c {
            wo.add(cks[hi].camp);
            hi += 1;
        }
        while lo < hi && cks[lo].ts < start {
            wo.remove(cks[lo].camp);
            lo += 1;
        }
        let credit = if lo < hi {
            let mts = cks[hi - 1].ts;
            let mut k = hi - 1;
            while k > lo && cks[k - 1].ts == mts {
                k -= 1;
            }
            cks[k].camp as usize
        } else {
            n_camp
        };
        let cell = &mut wo.camp[credit * ndays + di];
        cell[0] += 1;
        cell[1] += purchased as i64;
        cell[2] += rev;
        for idx in 0..wo.active.len() {
            let c = wo.active[idx] as usize;
            wo.camp[c * ndays + di][3] += 1;
        }
        // user_daily: sessions are in start order, so a day's sessions are contiguous; flush when the next
        // session starts on another day (cats already hold only this day's purchases).
        a_sess += 1;
        a_ev += n;
        a_rev += rev;
        let next_day = if j < evs.len() { Some((ws + evs[j].ts as i64).div_euclid(DAY)) } else { None };
        if next_day != Some(day) {
            flush(wo, day, a_sess, a_ev, a_rev);
            a_sess = 0;
            a_ev = 0;
            a_rev = 0;
        }
        i = j;
    }
    while lo < hi {
        wo.remove(cks[lo].camp);
        lo += 1;
    }
}
