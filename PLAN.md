# Shopping Sessions — Implementation Plan

Goal: exact output for every seed, fastest wall time. Target: < 0.6 s on practice (20M events),
< 2 s on final (60M). Hard limits: practice ≤ 2× baseline, final 3 min/run, 24 GB, 8 vCPU (4 cores × HT).

---

## 0. What the investigation found

### Data scale (measured on dev, extrapolated)

| Input  | dev 1M | practice ~20M | final ~60M |
| ------ | ------ | ------------- | ---------- |
| events | 1.07M lines, 82 MB, 30 files | ~21M, 1.65 GB | ~64M, 5 GB |
| clicks | 255k, 12 MB, 40 files | ~5M, 240 MB | ~15M, 720 MB |
| links  | 45k, 0.9 MB, 14 files | ~0.9M, 18 MB | ~2.7M, 55 MB |
| user_daily rows | 117k | ~2.3M | ~7M |
| campaign_daily rows | 27k | ~28k | ~28k (2001 campaigns × 14 days cap) |

Parsing 5 GB of CSV dominates. Second: hash dedup on event_id + grouping by person. Third: writing ~7M parquet rows.

### Value shapes (from generator source `tools-generator/src/*.rs`)

- user IDs: `u` + base36 of a 40-bit scramble → 2–9 chars, `[a-z0-9]`. Event IDs `e…`, click IDs `k…` same shape.
- products `p1..p50000`, campaigns `c_1..c_2000`. Unknown: `P123`, `p50001..p99999`, empty, `C_12`, `c_2001..c_9999`.
- Exotic values are rare in the generator (case variants 5e-6, upper-cased link sides 1/2000) but fixtures use
  spaces, tabs, `\r`, unicode digits. **Every path must be exact for arbitrary bytes; only the fast path may assume shape.**
- Whales: 5 persons × 168 sessions × 1–2% of all events (~1.2M events each at 60M).
- Shared devices (kiosks): 3–6 IDs that merge ~0.4% of people into a few giant persons (thousands of people,
  ~60k events, ~15k clicks each at 60M). Chains up to 400 links.
- Late dup trap: 1/64 of duplicated events name a different user in later copies → dedup **must be global on event_id**,
  never per-user.
- 2% of duplicated events: only the earliest copy has bad ts → event dropped (E5).
- Dup copies are always ingested 1 s–10 min later (strictly greater), so ingest ties never differ.
- Click copies are identical (quoted differently) → **click dedup can be skipped**: identical copies never change
  the credited click (same ts, same click_id) nor the distinct-campaign assist set.
- Windows: random midnight 2025-01-01 … 2028-12-17; Feb 29 2028 occurs. ts relative to `window_start - 7d` fits u32 ms.

### Test assets

- 36 fixtures (`fixtures/`, README lists each trap). 2/36 pass with empty output.
- dev set seed 1 + `dev-answer/` (117k + 27k rows).
- `generate` lets us make any seed/size → used for differential testing against an independent reference.

---

## 1. Correctness spec (condensed checklist, every item gets a test)

### CSV
- [ ] Skip exactly the first line of every file (a later line equal to the header is data — fixture L3).
- [ ] Only `\n` ends a line; `\r` is data. Last line without `\n` still counts if non-empty.
- [ ] Field count must match exactly (7 / 4 / 2); empty line = malformed.
- [ ] Field quoted iff it starts AND ends with `"` and len ≥ 2 and no other `"` inside; any other `"` → malformed line.
- [ ] Empty field (`` or `""`) = null. No trimming, no case folding anywhere.
- [ ] Only read `*.csv` in `events/`, `clicks/`, `links/`. Header-only and empty files are fine.

### Timestamps → i64 ms UTC
- [ ] F1 `YYYY-MM-DDTHH:MM:SSZ` exactly 20 bytes; F2 same + `±HH:MM` exactly 25 bytes, offset h ≤ 23, m ≤ 59, colon required, `-00:00` ok; UTC = local − offset.
- [ ] ASCII digits only; month 1–12, day ≤ month length with Gregorian leap rule, hour ≤ 23, min ≤ 59, sec ≤ 59.
- [ ] F3: all ASCII digits, ≥ 1 digit, leading zeros ok, value > i64 → simply out of window (not a crash).
- [ ] Everything else bad (lowercase t/z, space, fraction, no zone, `+0200`, `-5`, `1.79e12`, empty).
- [ ] Event window `[ws, we)`; click window `[ws − 7d, we)`.

### Links
- [ ] Drop malformed and lines with either side empty. Self-links/repeats harmless.
- [ ] Transitive union; person name = byte-wise min ID of component, including IDs that occur only in links.
- [ ] IDs not in any link are their own person.

### Events
- [ ] Drop malformed + empty event_id at parse. Everything else is a dedup candidate (even empty user, bad ts …).
- [ ] Dedup global on event_id: keep min ingest_ts copy; THEN validate it; invalid kept copy kills the event.
- [ ] Valid: user non-empty; ts valid & in window; type ∈ {view, cart, purchase} exact; product non-empty;
      purchase price ASCII digits, value 1..=10,000,000 (leading zeros ok; long zero-padded strings ok).
- [ ] Late: drop if `ingest − ts > 86_400_000`; equal kept; negative kept.
- [ ] Category from products.csv, else `unknown` (participates in top_category, byte-wise tie-break).

### Sessions & attribution
- [ ] Per person, sort by ts; new session iff `ts − prev_ts > 1_800_000` (equal ts → same session).
- [ ] Session day = UTC date of first event; all its events/purchases count on that day.
- [ ] Clicks: drop malformed / empty click_id; valid iff user non-empty, ts valid & in click window, campaign ∈ campaigns.csv.
- [ ] Credit: person's valid clicks with `start − 7d ≤ ts ≤ start`; max ts, tie → byte-wise min click_id. None → `organic`.
- [ ] Assists: every distinct campaign with a click in that same closed window (incl. the crediting one). organic never assists.

### Outputs
- [ ] `user_daily(user_id string, day DATE, sessions, events, revenue_cents, top_category nullable)`.
- [ ] `campaign_daily(campaign_id, channel, day DATE, sessions, purchasing_sessions, revenue_cents, assisted_sessions)`,
      a row for every (campaign, day) with credit OR assist; organic row only when it credited (its assists = 0).
- [ ] `day` is Parquet INT32 logical DATE. Columns exact names, no extras. Write `*.parquet` directly in each folder.
- [ ] Self-check at end (debug builds): Σsessions and Σrevenue equal across tables.

---

## 2. Engine choice

**Rust, single static binary, custom CSV parser, no dataframe engine.**

- Leaderboard: top 0.30 s, median ~1.3 s; DuckDB/Polars entries cluster at 2–6 s. Engines pay for generic CSV
  parsing (and DuckDB's dialect differs from the rules anyway: `""` ≠ null, malformed-line handling).
- Rust: generator is Rust, toolchain present (1.98), `parquet` crate for output, std threads, `memmap2`.
- Two-step delivery to protect the score: **Step A** correct straightforward Rust version → submit (banks a
  ~1–2 s score), **Step B** optimized version behind the same tests.

---

## 3. Architecture (Step B target; Step A is the same shape without the micro-optimizations)

```
           ┌────────── parse (8 threads, chunks of ~4 MB split on '\n') ──────────┐
 links/*  ─┤ links  → (keyA,keyB) pairs                                            │
 clicks/* ─┤ clicks → valid click recs                                             │
 events/* ─┤ events → recs radix-partitioned by hash(event_id) → P buckets/thread │
           └───────────────────────────────────────────────────────────────────────┘
 links: parallel union-find (hook larger key under smaller ⇒ root = name) → LinkMap key→name
 dedup (per eid-partition, cache-resident hash): keep min ingest; drop invalid/late
        → survivor: person = LinkMap.get(user) or user  → re-partition by hash(person)
 clicks:  person = LinkMap.get(user) or user           → partition by hash(person)
 per person-partition (dynamic scheduling, 512+ parts):
        group by person → sort ts → sessionize → attribute (two-pointer over sorted clicks)
        → user_daily rows + thread-local campaign×day accumulators
 write: user_daily as N parquet files in parallel; campaign_daily one file.
```

### 3.1 Keys (no strings in the hot path)
- `Key = u64`. **Canonical** byte string: len 1..=10, every byte in `[0-9a-z]` (covers `u…`, `e…`, `k…`).
  Packed as 6-bit codes, `'0'..'9'→1..10`, `'a'..'z'→11..36`, zero-padded, big-endian → u64 order == byte order.
- **Exotic** (anything else): bit 63 set + index into a global intern table (`Mutex<HashMap<Vec<u8>,u32>>` + bytes
  arena). Rare in real data; exact for fixtures.
- Compare(Key,Key): both canonical → integer compare; otherwise compare decoded bytes. Used only for the person
  name (UF hook order) and click_id tie-break.
- Decode Key → bytes for output.
- Products: `p` + digits without leading zero → dense `cat_by_num[]` built from products.csv entries of that shape;
  other strings → small HashMap. Same for campaigns (`c_` + num). Absent ⇒ unknown / invalid. Exact.
- Categories: all names from products.csv + `unknown`, sorted byte-wise → cat id order == byte order (tie-break = min id).

### 3.2 Parse
- Enumerate files, `mmap` each (`memmap2`, `MADV_SEQUENTIAL`/`WILLNEED`), cut into ~4 MB tasks at `\n`
  (header skipped only in the task holding file offset 0). Atomic work counter; 8 workers (`NCPU`).
- Line scan: v1 scalar (memchr for `\n`, byte loop for `,`/`"`). v2: AVX2 64-byte blocks → comma/quote/newline
  bitmasks, iterate set bits (simdjson-style). Compile with `-C target-cpu=x86-64-v3` (Cascade Lake ok; local i7 ok).
- Fixed-width SWAR timestamp parser: F1/F2 by length + positional check; F3 by digit run; days_from_civil table.
- Event record (32 B): `eid:u64, user:u64, ts_rel:u32 (ms from ws), ingest_rel:i32 (s), price:u32, cat:u16, flags:u8`
  where flags = {valid, purchase}. Invalid copies keep only eid+ingest+flags=0 (still compete in dedup).
  Late/validity computed at parse time (they depend only on the row itself).
- Click record: `person-to-be user:u64, ts_rel:u32 (from ws−7d), campaign:u16, click:u64`. Only valid clicks kept.

### 3.3 Dedup (shuffle 1)
- P = 1024–4096 partitions by high bits of `hash(eid)` so each partition's table fits L2.
- Per partition: open-addressing table eid → best index (min ingest); single pass; survivors = winners with valid flag.

### 3.4 Links / person names
- Pairs → map distinct keys to dense ids (partitioned hash, parallel), union-find with atomics; union hooks
  the root with the larger key under the smaller ⇒ final root = byte-wise min ID = person name (exotic compare via bytes).
- LinkMap: key → name key, open addressing, ~2× size. Lookups from dedup/click stages done in batches with
  software prefetch (`_mm_prefetch`) to hide misses.

### 3.5 Person partitions (shuffle 2) & sessionize
- Survivor events and valid clicks → partition by `hash(person)`; 512–2048 partitions, work-stealing order by size
  (biggest first: whales/kiosks).
- Inside: local hash person → slot, counting sort, per-person `sort_unstable` on ts (small arrays; whales ~1M).
- Sessionize linear. Per session: start, n_events, revenue, has_purchase, category revenue (sparse — purchases ~3%).
- Clicks per person sorted by `(ts asc, click_id desc)` (exotic-aware compare) → "last with ts ≤ start" is the credit.
- Assists: sliding window `[start−7d, start]` with two pointers; per-campaign counts array (2001) + dense active set
  (swap-remove) → enumerate distinct campaigns O(distinct). Sessions are visited in increasing start ⇒ monotone window.
- Aggregation: user_daily per (person, day) — sessions already ordered, contiguous by day. top_category = argmax over
  summed category revenue for that (person, day), tie → lowest cat id. campaign_daily: thread-local
  `[campaign+organic][day 0..14]` × 4 counters, merged at the end.

### 3.6 Output
- `parquet` crate low-level writer: no compression, plain encoding, dictionary off, statistics off.
  `user_id` BYTE_ARRAY (String), `day` INT32 (Date), counts INT64, `top_category` optional BYTE_ARRAY.
- user_daily: one file per worker written in parallel (`part-00000.parquet` …). campaign_daily: one file.
- If the crate turns out to cost > 50 ms, replace with a hand-rolled minimal parquet writer (footer via thrift compact).

### 3.7 Process & container
- Multi-stage Dockerfile: `rust:1.8x-bookworm` builder (`cargo build --release`, LTO fat, codegen-units=1,
  panic=abort, mimalloc) → `debian:bookworm-slim` (or `gcr.io/distroless/cc`) with only the binary. CMD = binary.
- Reads `DATA_DIR`, `OUTPUT_DIR`, `NCPU`. Writes only to `OUTPUT_DIR`. Logs/timings to stderr only.
- No `Vec` frees at exit: `std::process::exit(0)` right after the last file is flushed (skip destructors).
- Threads: `NCPU` workers (8); never more (4096 limit irrelevant).

---

## 4. Verification ("full proof")

1. **Fixtures**: `python3 scripts/run_fixtures.py fixtures -- target/release/sessions` → 36/36. Native, no docker, < 2 s.
2. **Dev set**: `make check-dev` → MATCH.
3. **Independent reference** (`tools/reference.py`, plain Python, line-by-line from rules.md, no shared code
   with the Rust version): slow but obviously-correct. Must itself pass 36/36 and dev MATCH.
4. **Differential fuzz over seeds**: `scripts/fuzz.sh` — for seeds 1..50 × sizes {2k, 20k, 200k} events:
   `generate` → run both → `compare.py`. Includes windows in 2028 (Feb 29). Any diff → minimize & add a fixture.
5. **Hand-made adversarial fixtures** beyond the 36: header-equal row mid-file, file without trailing `\n`,
   21-digit F3, F3 with 30 leading zeros, IDs longer than 10 chars / with uppercase / unicode / tabs (exotic path),
   exotic vs canonical name ordering (`U1` < `a` < `u1`), click_id tie with exotic id, product `p0`, `p050`,
   price `0000000000000000000000250`, empty files, only-header files, a person whose name only appears in links.
6. **Docker run** with scorer flags (`scripts/docker_run.sh`: read-only rootfs, uid 1000, no network, 8 CPUs, 24 GB)
   on dev and on a 20M and 60M generated set; check MATCH vs reference-on-small / vs Step A on large.
7. **Determinism**: run 60M twice, outputs identical; run under `NCPU=1` and `NCPU=8` → identical (catches races).
8. **Limits**: `/usr/bin/time -v` peak RSS on 60M < 8 GB; stdout empty.

Gate before every submission: steps 1, 2, 4 (≥ 20 seeds), 6 all green.

---

## 5. Work order & milestones

| # | Milestone | Done when |
|---|-----------|-----------|
| M1 | Repo scaffolding: Cargo project `sessions/`, Dockerfile, `run.sh`, generate 20M/60M sets in `/scratch`-like dir | `make build` works |
| M2 | Python reference (`tools/reference.py`) | 36/36 + dev MATCH |
| M3 | Rust Step A: straightforward (std HashMap, single dedup map, per-person sort), parallel parse only | 36/36, dev MATCH, fuzz 50 seeds clean |
| M4 | **Submit #1** (banks a score) | scorer accepts |
| M5 | Profiling harness: per-stage timers to stderr, `perf` on 60M | stage breakdown known |
| M6 | Partitioned dedup + person shuffle, prefetching LinkMap, fast keys | fuzz clean, ≥ 3× faster |
| M7 | SIMD line scanner + SWAR timestamp parse | parse ≥ 1 GB/s/thread |
| M8 | Parallel parquet output, exit without teardown | — |
| M9 | **Submit #2..n**, tune partitions/chunk size/threads (8 vs 16 workers on 8 HT vCPUs) | — |
| M10 | Final pick: fastest commit that is fuzz-clean on ≥ 50 seeds and 60M < 5 s locally | — |

Submission budget: 2/hour; the scorer checks the dev set first (cheap fail), hidden practice reveals only counts →
never submit without the fuzz gate.

---

## 6. Risks & mitigations

| Risk | Mitigation |
|------|-----------|
| Subtle rule misread → hidden data differs while dev matches | Independent reference + many-seed fuzz + adversarial fixtures |
| Exotic IDs break packed-key ordering | Bit-63 exotic tag; all comparisons go through one `cmp_key` tested with mixed cases |
| Cold page cache on scorer (I/O bound) | Parallel mmap readers with `MADV_WILLNEED`; nothing else possible |
| Skew: whale / kiosk partitions dominate | Many partitions, largest-first scheduling; whale sort is ~1M elements (~20 ms) |
| Parquet crate surprises (DATE type, nullable) | Validate with polars loader used by `compare.py` + `pyarrow.parquet.read_schema` |
| Container differences (uid 1000, ro root, no /tmp use) | Always test via `scripts/docker_run.sh` before submit |
| Click-dedup skip wrong if copies ever differ | Rules guarantee identical; reference does dedup, fuzz will catch it |
| Final 60M memory | ~32 B/event × 64M ≈ 2 GB + mmap; measured in step 8 |
| Score noise ±20% | Optimize big-ticket items; don't chase < 5% gains with submissions |
