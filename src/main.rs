mod arena;
mod keys;
mod out;
mod pq;
mod scan;
mod ts;
mod uf;

use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use arena::{Arena, Blocks, BLOCK};
use keys::{cmp_key, hash, key};
use ts::DAY;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const WEEK: i64 = 7 * DAY;
const GAP: u32 = 1_800_000;
const NO_TS: u32 = u32::MAX;
const CHUNK: usize = 2 << 20;

/// Event candidate: kept for dedup even when invalid (ts == NO_TS). 28 bytes.
#[derive(Clone, Copy)]
#[repr(C, packed(4))]
struct Ev {
    eid: u64,
    user: u64,
    ingest: u32,
    ts: u32,
    /// price (low 24 bits, 0 unless a purchase) | category << 24
    pc: u32,
}

/// A surviving event, keyed by person. 16 bytes.
#[derive(Clone, Copy)]
struct Pe {
    person: u64,
    ts: u32,
    pc: u32,
}

#[inline(always)]
fn price_of(pc: u32) -> u32 {
    pc & 0xFF_FFFF
}

/// A valid click. `user` becomes the person after link resolution.
#[derive(Clone, Copy)]
struct Ck {
    user: u64,
    click: u64,
    ts: u32,
    camp: u32,
}

fn read_small(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_default()
}

fn small_lines<const N: usize>(data: &[u8], mut f: impl FnMut([&[u8]; N])) {
    let mut buf = data.to_vec();
    buf.extend_from_slice(&[0u8; scan::PAD]);
    let start = match memchr::memchr(b'\n', data) {
        Some(p) => p + 1,
        None => data.len(),
    };
    scan::lines::<N>(&buf, start, data.len(), |fs| f(*fs));
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
            s.spawn(move || {
                loop {
                    let t = next.fetch_add(1, Ordering::Relaxed);
                    if t >= n_tasks {
                        break;
                    }
                    f(w, t);
                }
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

struct ParseOut {
    buf: Vec<u8>,
    ev: Vec<Blocks<Ev>>,
    ck: Vec<Blocks<Ck>>,
    links: Vec<(u64, u64)>,
    tsf: ts::Fast,
}


fn cpu_secs() -> (f64, f64) {
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
    let t = |v: libc::timeval| v.tv_sec as f64 + v.tv_usec as f64 * 1e-6;
    (t(u.ru_utime), t(u.ru_stime))
}

fn timing(t0: Instant, what: &str) {
    if std::env::var_os("SESSIONS_QUIET").is_none() {
        let (u, s) = cpu_secs();
        eprintln!("{what:>9} {:>7.1} ms  cpu u {u:6.2} s {s:5.2}", t0.elapsed().as_secs_f64() * 1e3);
    }
}

fn main() {
    let t0 = Instant::now();
    let data = PathBuf::from(std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".into()));
    let outdir = PathBuf::from(std::env::var("OUTPUT_DIR").unwrap_or_else(|_| "/output".into()));
    let threads: usize = std::env::var("NCPU")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(8, |n| n.get()))
        .max(1);

    // ---- lookups
    let manifest = std::fs::read_to_string(data.join("manifest.json")).expect("manifest");
    let ws = manifest_ms(&manifest, "\"window_start\"");
    let we = manifest_ms(&manifest, "\"window_end\"");
    assert!(we > ws && we - ws + WEEK < u32::MAX as i64, "window too long");

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
    assert!(cat_names.len() <= 256, "more than 255 categories");

    let camp_bytes = read_small(&data.join("campaigns.csv"));
    let mut camp_ids: Vec<Vec<u8>> = Vec::new();
    let mut channels: Vec<Vec<u8>> = Vec::new();
    let mut campaigns = NumMap::new(b"c_");
    small_lines::<2>(&camp_bytes, |[c, ch]| {
        if campaigns.get(c).is_none() {
            campaigns.insert(c, camp_ids.len() as u32);
            camp_ids.push(c.to_vec());
            channels.push(ch.to_vec());
        }
    });
    let n_camp = camp_ids.len();
    let ctx = Ctx { ws, we, products, unknown_cat, campaigns };

    // ---- files & tasks (events first: they are the bulk)
    let mut files: Vec<(File, usize)> = Vec::new();
    let mut tasks: Vec<Task> = Vec::new();
    let mut ev_bytes = 0usize;
    let mut ck_bytes = 0usize;
    for (kind, dir) in [(Kind::Event, "events"), (Kind::Link, "links"), (Kind::Click, "clicks")] {
        for p in list_csv(&data.join(dir)) {
            let f = File::open(&p).expect("open");
            let len = f.metadata().map(|m| m.len() as usize).unwrap_or(0);
            if len == 0 {
                continue;
            }
            let fi = files.len();
            files.push((f, len));
            if kind == Kind::Event {
                ev_bytes += len;
            }
            if kind == Kind::Click {
                ck_bytes += len;
            }
            let mut s = 0;
            while s < len {
                let e = (s + CHUNK).min(len);
                tasks.push(Task { kind, file: fi, s, e });
                s = e;
            }
        }
    }
    let est_rows = ev_bytes / 78 + 1;
    let nparts = (est_rows / 12_000).next_power_of_two().clamp(16, 4096);
    let pshift = 64 - nparts.trailing_zeros();
    let part_of = |k: u64| (hash(k) >> pshift) as usize;
    let est_clicks = ck_bytes / 40 + 1;
    let arena_bytes = (est_rows * (32 + 24) + est_clicks * 48) / threads * 5 / 4 + 3 * nparts * BLOCK + (256 << 20);
    let arenas: Vec<Arena> = (0..threads).map(|_| Arena::new(arena_bytes)).collect();

    let ing_base: i64 = ws.div_euclid(1000) - (1i64 << 31);
    let outs: Vec<Mutex<ParseOut>> = (0..threads)
        .map(|_| {
            Mutex::new(ParseOut {
                buf: Vec::new(),
                ev: (0..nparts).map(|_| Blocks::new()).collect(),
                ck: (0..nparts).map(|_| Blocks::new()).collect(),
                links: Vec::new(),
                tsf: ts::Fast::default(),
            })
        })
        .collect();
    timing(t0, "setup");
    if std::env::var_os("EXP_MICRO").is_some() {
        micro(&files, &ctx, ws);
        return;
    }

    // ---- phase A: links (small) -> lock-free union-find; root = byte-wise smallest ID = person name
    let link_tasks: Vec<&Task> = tasks.iter().filter(|t| t.kind == Kind::Link).collect();
    run_parallel(threads, link_tasks.len(), |w, t| {
        let task = link_tasks[t];
        let (file, flen) = &files[task.file];
        let mut guard = outs[w].lock().unwrap();
        let o = &mut *guard;
        let mut buf = std::mem::take(&mut o.buf);
        let (start, end) = scan::load(file, *flen, task.s, task.e, &mut buf);
        scan::lines::<2>(&buf, start, end, |f| {
            if f[0].is_empty() || f[1].is_empty() {
                return;
            }
            o.links.push(unsafe { (keys::key_padded(f[0]), keys::key_padded(f[1])) });
        });
        o.buf = buf;
    });
    let n_links: usize = outs.iter().map(|o| o.lock().unwrap().links.len()).sum();
    let uf = uf::Uf::new(2 * n_links);
    {
        let guards: Vec<_> = outs.iter().map(|o| o.lock().unwrap()).collect();
        let link_vecs: Vec<&Vec<(u64, u64)>> = guards.iter().map(|g| &g.links).collect();
        let per = 1 << 14;
        let mut ltasks: Vec<(usize, usize)> = Vec::new();
        for (i, v) in link_vecs.iter().enumerate() {
            let mut s = 0;
            while s < v.len() {
                ltasks.push((i, s));
                s += per;
            }
        }
        run_parallel(threads, ltasks.len(), |_, t| {
            let (i, s) = ltasks[t];
            let v = link_vecs[i];
            for &(a, b) in &v[s..(s + per).min(v.len())] {
                uf.union(a, b);
            }
        });
    }
    let lmap = uf::LinkMap::build(&uf, &|n, f| run_parallel(threads, n, |_, t| f(t)));
    drop(uf);
    for o in &outs {
        o.lock().unwrap().links = Vec::new();
    }
    let lmap = &lmap;
    timing(t0, "links");

    // ---- phase B: events and clicks. Persons are resolved here, through a small ring so each link-map
    // miss overlaps with parsing the following lines. Clicks need no dedup: they go straight to person
    // partitions; events go to event_id partitions for dedup.
    const R: usize = 16;
    let main_tasks: Vec<&Task> = tasks.iter().filter(|t| t.kind != Kind::Link).collect();
    run_parallel(threads, main_tasks.len(), |w, t| {
        let task = main_tasks[t];
        let (file, flen) = &files[task.file];
        let arena = &arenas[w];
        let mut guard = outs[w].lock().unwrap();
        let o = &mut *guard;
        let mut own = std::mem::take(&mut o.buf);
        let (start, end) = scan::load(file, *flen, task.s, task.e, &mut own);
        let buf: &[u8] = &own;
        match task.kind {
            Kind::Event => {
                let mut ring = [Ev { eid: 0, user: 0, ingest: 0, ts: 0, pc: 0 }; R];
                let mut rn = 0usize;
                scan::lines::<7>(&buf, start, end, |f| {
                    if let Some(ev) = parse_event(&ctx, &mut o.tsf, f, ing_base) {
                        if ev.ts == NO_TS {
                            o.ev[part_of(ev.eid)].push(arena, ev);
                            return;
                        }
                        lmap.prefetch(ev.user);
                        let slot = rn & (R - 1);
                        if rn >= R {
                            let mut old = ring[slot];
                            old.user = lmap.person(old.user);
                            o.ev[part_of(old.eid)].push(arena, old);
                        }
                        ring[slot] = ev;
                        rn += 1;
                    }
                });
                for k in rn.saturating_sub(R)..rn {
                    let mut old = ring[k & (R - 1)];
                    old.user = lmap.person(old.user);
                    o.ev[part_of(old.eid)].push(arena, old);
                }
            }
            Kind::Click => {
                let mut ring = [Ck { user: 0, click: 0, ts: 0, camp: 0 }; R];
                let mut rn = 0usize;
                scan::lines::<4>(&buf, start, end, |f| {
                    if f[0].is_empty() || f[1].is_empty() {
                        return;
                    }
                    let Some(camp) = ctx.campaigns.get(f[2]) else { return };
                    let Some(t) = o.tsf.parse(f[3]) else { return };
                    if t < ws - WEEK || t >= we {
                        return;
                    }
                    let c = Ck {
                        user: unsafe { keys::key_padded(f[1]) },
                        click: unsafe { keys::key_padded(f[0]) },
                        ts: (t - (ws - WEEK)) as u32,
                        camp,
                    };
                    lmap.prefetch(c.user);
                    let slot = rn & (R - 1);
                    if rn >= R {
                        let mut old = ring[slot];
                        old.user = lmap.person(old.user);
                        o.ck[part_of(old.user)].push(arena, old);
                    }
                    ring[slot] = c;
                    rn += 1;
                });
                for k in rn.saturating_sub(R)..rn {
                    let mut old = ring[k & (R - 1)];
                    old.user = lmap.person(old.user);
                    o.ck[part_of(old.user)].push(arena, old);
                }
            }
            Kind::Link => unreachable!(),
        }
        o.buf = own;
    });
    let mut outs: Vec<ParseOut> = outs.into_iter().map(|m| m.into_inner().unwrap()).collect();
    for o in &mut outs {
        o.buf = Vec::new();
    }
    timing(t0, "parse");

    // ---- dedup per eid-partition, then re-partition survivors by person
    struct Stage2 {
        pe: Vec<Blocks<Pe>>,
        gather: Vec<Ev>,
        table: Vec<u32>,
    }
    let st2: Vec<Mutex<Stage2>> = (0..threads)
        .map(|_| {
            Mutex::new(Stage2 {
                pe: (0..nparts).map(|_| Blocks::new()).collect(),
                gather: Vec::new(),
                table: Vec::new(),
            })
        })
        .collect();
    let ck_parts: Vec<Vec<Blocks<Ck>>> = outs.iter_mut().map(|o| std::mem::take(&mut o.ck)).collect();
    // ev_in[t][src]: each partition's lists, taken (and recycled) by whichever worker dedups it
    let mut ev_src: Vec<Vec<Blocks<Ev>>> = outs.into_iter().map(|o| o.ev).collect();
    let ev_in: Vec<Mutex<Vec<Blocks<Ev>>>> = (0..nparts)
        .map(|t| Mutex::new(ev_src.iter_mut().map(|v| std::mem::take(&mut v[t])).collect()))
        .collect();
    drop(ev_src);
    run_parallel(threads, nparts, |w, t| {
        let arena = &arenas[w];
        let mut g = st2[w].lock().unwrap();
        let st = &mut *g;
        st.gather.clear();
        {
            let mut lists = ev_in[t].lock().unwrap();
            for l in lists.iter_mut() {
                l.extend_into(&mut st.gather);
                l.recycle(arena);
            }
        }
        let n = st.gather.len();
        let cap = (n * 2).next_power_of_two().max(16);
        let mask = cap - 1;
        st.table.clear();
        st.table.resize(cap, u32::MAX);
        let gather = &st.gather;
        let table = &mut st.table;
        for (idx, e) in gather.iter().enumerate() {
            let mut i = (hash(e.eid) as usize >> 3) & mask;
            loop {
                let s = unsafe { *table.get_unchecked(i) };
                if s == u32::MAX {
                    table[i] = idx as u32;
                    break;
                }
                let o = unsafe { gather.get_unchecked(s as usize) };
                if o.eid == e.eid {
                    if e.ingest < o.ingest {
                        table[i] = idx as u32;
                    }
                    break;
                }
                i = (i + 1) & mask;
            }
        }
        for &s in table.iter() {
            if s != u32::MAX {
                let e = unsafe { gather.get_unchecked(s as usize) };
                if e.ts != NO_TS {
                    st.pe[part_of(e.user)].push(arena, Pe { person: e.user, ts: e.ts, pc: e.pc });
                }
            }
        }
    });
    drop(ev_in);
    let st2: Vec<Stage2> = st2.into_iter().map(|m| m.into_inner().unwrap()).collect();
    timing(t0, "dedup");

    // ---- sessions per person-partition
    let ws_day = ws.div_euclid(DAY);
    let ndays = ((we - 1).div_euclid(DAY) - ws_day + 1) as usize;
    let ncol = n_camp + 1; // + organic
    let results: Vec<Mutex<WorkerOut>> =
        (0..threads).map(|_| Mutex::new(WorkerOut::new(ncol * ndays, n_camp))).collect();
    let mut order: Vec<(usize, usize)> =
        (0..nparts).map(|t| (st2.iter().map(|s| s.pe[t].len()).sum::<usize>(), t)).collect();
    order.sort_unstable_by(|a, b| b.cmp(a));
    run_parallel(threads, nparts, |w, t| {
        let t = order[t].1;
        let mut wo = results[w].lock().unwrap();
        let wo = &mut *wo;
        let mut evs = std::mem::take(&mut wo.evs);
        let mut cks = std::mem::take(&mut wo.cks);
        let mut eb = std::mem::take(&mut wo.eb);
        let mut cb = std::mem::take(&mut wo.cb);
        // bucket by the next hash bits below the partition bits: persons almost never share a bucket,
        // so each bucket needs only a tiny sort by (person, ts)
        let n_ev: usize = st2.iter().map(|s| s.pe[t].len()).sum();
        let bits = ((n_ev / 8).max(16).next_power_of_two().trailing_zeros()).min(12);
        let sh = pshift - bits;
        let bmask = (1usize << bits) - 1;
        let bucket = |k: u64| (hash(k) >> sh) as usize & bmask;
        radix_events(st2.iter().map(|s| &s.pe[t]), n_ev, &bucket, bmask + 1, &mut eb, &mut evs, &mut wo.scratch);
        bucket_scatter(ck_parts.iter().map(|v| &v[t]), |c: &Ck| bucket(c.user), bmask + 1, &mut cb, &mut cks);
        for b in 0..=bmask {
            let ev = &mut evs[eb[b] as usize..eb[b + 1] as usize];
            if ev.is_empty() {
                continue;
            }
            // already in ts order within the bucket; group persons if several share it (stable)
            if ev.len() > 1 && ev.iter().any(|e| e.person != ev[0].person) {
                ev.sort_by_key(|e| e.person);
            }
            let ck = &mut cks[cb[b] as usize..cb[b + 1] as usize];
            if ck.len() > 1 {
                ck.sort_unstable_by(|a, b| {
                    a.user.cmp(&b.user).then(a.ts.cmp(&b.ts)).then_with(|| cmp_key(a.click, b.click))
                });
            }
            let mut ci = 0;
            let mut i = 0;
            while i < ev.len() {
                let person = ev[i].person;
                let mut j = i + 1;
                while j < ev.len() && ev[j].person == person {
                    j += 1;
                }
                while ci < ck.len() && ck[ci].user < person {
                    ci += 1;
                }
                let mut cj = ci;
                while cj < ck.len() && ck[cj].user == person {
                    cj += 1;
                }
                person_sessions(wo, &ev[i..j], &ck[ci..cj], person, ws, ws_day, ndays, n_camp);
                ci = cj;
                i = j;
            }
        }
        wo.eb = eb;
        wo.cb = cb;
        wo.evs = evs;
        wo.cks = cks;
    });
    let results: Vec<WorkerOut> = results.into_iter().map(|m| m.into_inner().unwrap()).collect();
    timing(t0, "sessions");

    // ---- outputs
    let mut camp_acc = vec![[0i64; 4]; ncol * ndays];
    for r in &results {
        for (a, b) in camp_acc.iter_mut().zip(&r.camp) {
            for k in 0..4 {
                a[k] += b[k];
            }
        }
    }
    out::write_all(&outdir, &results, &camp_acc, &camp_ids, &channels, &cat_names, ws_day, ndays, threads);
    timing(t0, "total");
    std::process::exit(0);
}

/// Counting-sort the elements of several block lists into `out` by bucket; `starts` gets bucket offsets.
/// Partition events -> `out` ordered by (bucket, ts): LSD radix on ts (11+11+10 bits) then a stable
/// pass on the bucket. One read of the blocks builds all four histograms.
fn radix_events<'a>(
    srcs: impl Iterator<Item = &'a Blocks<Pe>> + Clone,
    n: usize,
    bucket: &impl Fn(u64) -> usize,
    nb: usize,
    starts: &mut Vec<u32>,
    out: &mut Vec<Pe>,
    tmp: &mut Vec<Pe>,
) {
    let mut h0 = [0u32; 2048];
    let mut h1 = [0u32; 2048];
    let mut h2 = [0u32; 1024];
    starts.clear();
    starts.resize(nb + 1, 0);
    out.clear();
    out.reserve(n);
    tmp.clear();
    tmp.reserve(n);
    for b in srcs.clone() {
        b.for_each_slice(|sl| {
            for e in sl {
                h0[(e.ts & 2047) as usize] += 1;
                h1[((e.ts >> 11) & 2047) as usize] += 1;
                h2[(e.ts >> 22) as usize & 1023] += 1;
                starts[bucket(e.person) + 1] += 1;
            }
        });
    }
    fn excl(h: &mut [u32]) {
        let mut s = 0;
        for c in h.iter_mut() {
            let x = *c;
            *c = s;
            s += x;
        }
    }
    excl(&mut h0);
    excl(&mut h1);
    excl(&mut h2);
    for i in 0..nb {
        starts[i + 1] += starts[i];
    }
    unsafe {
        let o = out.as_mut_ptr();
        let t = tmp.as_mut_ptr();
        for b in srcs {
            b.for_each_slice(|sl| {
                for e in sl {
                    let k = (e.ts & 2047) as usize;
                    *t.add(h0[k] as usize) = *e;
                    h0[k] += 1;
                }
            });
        }
        for i in 0..n {
            let e = *t.add(i);
            let k = ((e.ts >> 11) & 2047) as usize;
            *o.add(h1[k] as usize) = e;
            h1[k] += 1;
        }
        for i in 0..n {
            let e = *o.add(i);
            let k = (e.ts >> 22) as usize & 1023;
            *t.add(h2[k] as usize) = e;
            h2[k] += 1;
        }
        let mut pos: Vec<u32> = starts[..nb].to_vec();
        for i in 0..n {
            let e = *t.add(i);
            let k = bucket(e.person);
            *o.add(pos[k] as usize) = e;
            pos[k] += 1;
        }
        out.set_len(n);
    }
}

fn bucket_scatter<'a, T: Copy + 'a>(
    srcs: impl Iterator<Item = &'a Blocks<T>> + Clone,
    key: impl Fn(&T) -> usize,
    nb: usize,
    starts: &mut Vec<u32>,
    out: &mut Vec<T>,
) {
    starts.clear();
    starts.resize(nb + 1, 0);
    let mut n = 0usize;
    for b in srcs.clone() {
        b.for_each_slice(|sl| {
            n += sl.len();
            for e in sl {
                starts[key(e) + 1] += 1;
            }
        });
    }
    for i in 0..nb {
        starts[i + 1] += starts[i];
    }
    out.clear();
    out.reserve(n);
    let dst = out.as_mut_ptr();
    let mut pos: Vec<u32> = starts[..nb].to_vec();
    for b in srcs {
        b.for_each_slice(|sl| {
            for e in sl {
                let k = key(e);
                unsafe { *dst.add(pos[k] as usize) = *e };
                pos[k] += 1;
            }
        });
    }
    unsafe { out.set_len(n) };
}


/// An event line -> dedup candidate (None if the event_id is empty).
#[inline(always)]
fn parse_event(ctx: &Ctx, tsf: &mut ts::Fast, f: &[&[u8]; 7], ing_base: i64) -> Option<Ev> {
    if f[0].is_empty() {
        return None;
    }
    let eid = unsafe { keys::key_padded(f[0]) };
    let (ingest, ing32) = match tsf.parse(f[3]) {
        Some(v) => (v, (v.div_euclid(1000) - ing_base).clamp(0, u32::MAX as i64 - 1) as u32),
        None => (i64::MAX, u32::MAX),
    };
    let mut ev = Ev { eid, user: 0, ingest: ing32, ts: NO_TS, pc: 0 };
    if let Some(v) = validate(ctx, tsf, f, ingest) {
        ev.user = unsafe { keys::key_padded(f[1]) };
        ev.ts = v.0;
        ev.pc = v.1 | (v.2 << 24);
    }
    Some(ev)
}

/// Returns (ts_rel, price, cat) when the row is a valid, non-late event.
#[inline(always)]
fn validate(ctx: &Ctx, tsf: &mut ts::Fast, f: &[&[u8]; 7], ingest: i64) -> Option<(u32, u32, u32)> {
    if f[1].is_empty() || f[5].is_empty() {
        return None;
    }
    let t = tsf.parse(f[2])?;
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
    evs: Vec<Pe>,
    cks: Vec<Ck>,
    eb: Vec<u32>,
    cb: Vec<u32>,
    scratch: Vec<Pe>,
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
            evs: Vec::new(),
            cks: Vec::new(),
            eb: Vec::new(),
            cb: Vec::new(),
            scratch: Vec::new(),
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
            let price = price_of(e.pc);
            if price > 0 {
                let cat = e.pc >> 24;
                purchased = true;
                rev += price as i64;
                match wo.cats.iter_mut().find(|x| x.0 == cat) {
                    Some(x) => x.1 += price as u64,
                    None => wo.cats.push((cat, price as u64)),
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

fn micro(files: &[(File, usize)], ctx: &Ctx, _ws: i64) {
    let (f, len) = &files.iter().max_by_key(|x| x.1).unwrap();
    let mut buf = Vec::new();
    let (start, end) = scan::load(f, *len, 0, *len, &mut buf);
    let n = buf[start..end].iter().filter(|&&b| b == b'\n').count();
    let run = |name: &str, g: &dyn Fn() -> u64| {
        let mut best = f64::MAX;
        let mut r = 0;
        for _ in 0..7 {
            let t = Instant::now();
            r += g();
            best = best.min(t.elapsed().as_secs_f64());
        }
        eprintln!("{name:>10}: {:5.1} ns/line ({})", best * 1e9 / n as f64, r % 7);
    };
    run("newlines", &|| memchr::memchr_iter(b'\n', &buf[start..end]).count() as u64);
    run("nl-chain", &|| scan::bench_nl(&buf, start, end));
    run("masks", &|| scan::bench_masks(&buf, start, end));
    run("lines", &|| { let mut c = 0u64; scan::lines::<7>(&buf, start, end, |f| c += f[0].len() as u64); c });
    run("keys", &|| { let mut c = 0u64; scan::lines::<7>(&buf, start, end, |f| c = c.wrapping_add(key(f[0]) ^ key(f[1]))); c });
    run("ingest", &|| { let mut c = 0u64; scan::lines::<7>(&buf, start, end, |f| c = c.wrapping_add(ts::parse(f[3]).unwrap_or(0) as u64)); c });
    run("ingestF", &|| { let mut tf = ts::Fast::default(); let mut c = 0u64; scan::lines::<7>(&buf, start, end, |f| c = c.wrapping_add(tf.parse(f[3]).unwrap_or(0) as u64)); c });
    run("tsF", &|| { let mut tf = ts::Fast::default(); let mut c = 0u64; scan::lines::<7>(&buf, start, end, |f| c = c.wrapping_add(tf.parse(f[2]).unwrap_or(0) as u64)); c });
    run("ts", &|| { let mut c = 0u64; scan::lines::<7>(&buf, start, end, |f| c = c.wrapping_add(ts::parse(f[2]).unwrap_or(0) as u64)); c });
    run("product", &|| { let mut c = 0u64; scan::lines::<7>(&buf, start, end, |f| c += ctx.products.get(f[5]).unwrap_or(0) as u64); c });
    let ib = _ws.div_euclid(1000) - (1i64 << 31);
    run("event", &|| { let mut tf = ts::Fast::default(); let mut c = 0u64; scan::lines::<7>(&buf, start, end, |f| c = c.wrapping_add(parse_event(ctx, &mut tf, f, ib).map_or(0, |v| v.eid ^ v.user))); c });
    run("event+push", &|| {
        let arena = Arena::new(1 << 30);
        let mut parts: Vec<Blocks<Ev>> = (0..2048).map(|_| Blocks::new()).collect();
        let mut tf = ts::Fast::default();
        scan::lines::<7>(&buf, start, end, |f| if let Some(ev) = parse_event(ctx, &mut tf, f, ib) { parts[(hash(ev.eid) >> 53) as usize].push(&arena, ev) });
        parts.iter().map(|p| p.len() as u64).sum()
    });
    run("validate", &|| { let mut tf = ts::Fast::default(); let mut c = 0u64; scan::lines::<7>(&buf, start, end, |f| c += validate(ctx, &mut tf, f, 0).map_or(0, |v| v.0 as u64)); c });
}
