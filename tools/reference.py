#!/usr/bin/env python3
"""Slow, obviously-correct reference for rules.md. Usage: reference.py DATA_DIR OUT_DIR
Writes OUT_DIR/user_daily.csv and OUT_DIR/campaign_daily.csv in the fixtures' expected format."""
import json
import os
import sys
from collections import defaultdict
from pathlib import Path

DAY = 86_400_000
HOUR = 3_600_000
DIGITS = set(b"0123456789")


def parse_line(line: bytes, nfields: int):
    """Returns list of values (None = null) or None if malformed."""
    if line == b"":
        return None
    parts = line.split(b",")
    if len(parts) != nfields:
        return None
    out = []
    for p in parts:
        if b'"' in p:
            if len(p) >= 2 and p[0:1] == b'"' and p[-1:] == b'"' and b'"' not in p[1:-1]:
                p = p[1:-1]
            else:
                return None
        out.append(p if p != b"" else None)
    return out


def read_lines(folder: Path):
    if not folder.is_dir():
        return
    for f in sorted(folder.iterdir()):
        if not f.name.endswith(".csv"):
            continue
        data = f.read_bytes()
        lines = data.split(b"\n")
        if lines and lines[-1] == b"":
            lines.pop()
        for line in lines[1:]:
            yield line


def is_digits(s: bytes) -> bool:
    return len(s) > 0 and all(c in DIGITS for c in s)


def days_from_civil(y, m, d):
    y = y - 1 if m <= 2 else y
    era = (y if y >= 0 else y - 399) // 400
    yoe = y - era * 400
    doy = (153 * ((m + 9) % 12) + 2) // 5 + d - 1
    doe = yoe * 365 + yoe // 4 - yoe // 100 + doy
    return era * 146097 + doe - 719468


def leap(y):
    return y % 4 == 0 and (y % 100 != 0 or y % 400 == 0)


MDAYS = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]


def parse_ts(s):
    """ms since epoch or None."""
    if s is None:
        return None
    if is_digits(s):
        return int(s)
    n = len(s)
    if n not in (20, 25):
        return None
    t = s
    def dig(a, b):
        part = t[a:b]
        return int(part) if is_digits(part) else None
    if t[4:5] != b"-" or t[7:8] != b"-" or t[10:11] != b"T" or t[13:14] != b":" or t[16:17] != b":":
        return None
    y, mo, d, h, mi, se = dig(0, 4), dig(5, 7), dig(8, 10), dig(11, 13), dig(14, 16), dig(17, 19)
    if None in (y, mo, d, h, mi, se):
        return None
    if not (1 <= mo <= 12):
        return None
    md = 29 if (mo == 2 and leap(y)) else MDAYS[mo - 1]
    if not (1 <= d <= md) or h > 23 or mi > 59 or se > 59:
        return None
    base = (days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + se) * 1000
    if n == 20:
        return base if t[19:20] == b"Z" else None
    sign = t[19:20]
    if sign not in (b"+", b"-") or t[22:23] != b":":
        return None
    oh, om = dig(20, 22), dig(23, 25)
    if oh is None or om is None or oh > 23 or om > 59:
        return None
    off = (oh * 60 + om) * 60000
    return base - off if sign == b"+" else base + off


def iso_to_ms(s: str):
    return parse_ts(s.encode())


def main():
    data, out = Path(sys.argv[1]), Path(sys.argv[2])
    man = json.loads((data / "manifest.json").read_text())
    ws, we = iso_to_ms(man["window_start"]), iso_to_ms(man["window_end"])

    products = {}
    for line in read_lines_file(data / "products.csv"):
        r = parse_line(line, 2)
        if r and r[0] is not None:
            products[r[0]] = r[1] or b""
    campaigns = {}
    for line in read_lines_file(data / "campaigns.csv"):
        r = parse_line(line, 2)
        if r and r[0] is not None:
            campaigns[r[0]] = r[1] or b""

    # links: union-find
    parent = {}

    def find(x):
        parent.setdefault(x, x)
        root = x
        while parent[root] != root:
            root = parent[root]
        while parent[x] != root:
            parent[x], x = root, parent[x]
        return root

    for line in read_lines(data / "links"):
        r = parse_line(line, 2)
        if r is None or r[0] is None or r[1] is None:
            continue
        a, b = find(r[0]), find(r[1])
        if a != b:
            if a < b:
                parent[b] = a
            else:
                parent[a] = b

    def person(u):
        return find(u) if u in parent else u

    # events
    best = {}
    for line in read_lines(data / "events"):
        r = parse_line(line, 7)
        if r is None or r[0] is None:
            continue
        ing = parse_ts(r[3])
        cur = best.get(r[0])
        if cur is None or ing < cur[0]:
            best[r[0]] = (ing, r)
    per_person = defaultdict(list)
    for eid, (ing, r) in best.items():
        _, user, ts_s, _, etype, prod, price_s = r
        if user is None or prod is None:
            continue
        ts = parse_ts(ts_s)
        if ts is None or not (ws <= ts < we):
            continue
        if etype not in (b"view", b"cart", b"purchase"):
            continue
        price = 0
        if etype == b"purchase":
            if price_s is None or not is_digits(price_s):
                continue
            price = int(price_s)
            if not (1 <= price <= 10_000_000):
                continue
        if ing - ts > 24 * HOUR:
            continue
        cat = products.get(prod, b"unknown")
        per_person[person(user)].append((ts, etype == b"purchase", price, cat))

    # clicks
    clicks = defaultdict(dict)
    for line in read_lines(data / "clicks"):
        r = parse_line(line, 4)
        if r is None or r[0] is None:
            continue
        cid, user, camp, ts_s = r
        if user is None or camp not in campaigns:
            continue
        ts = parse_ts(ts_s)
        if ts is None or not (ws - 7 * DAY <= ts < we):
            continue
        clicks[person(user)][cid] = (ts, cid, camp)

    ud = {}
    cd = defaultdict(lambda: [0, 0, 0, 0])
    for p, evs in per_person.items():
        evs.sort(key=lambda e: e[0])
        sessions = []
        prev = None
        for e in evs:
            if prev is None or e[0] - prev > 1_800_000:
                sessions.append([])
            sessions[-1].append(e)
            prev = e[0]
        pclicks = list(clicks.get(p, {}).values())
        for s in sessions:
            start = s[0][0]
            day = start // DAY
            rev = sum(e[2] for e in s if e[1])
            purch = any(e[1] for e in s)
            qual = [c for c in pclicks if start - 7 * DAY <= c[0] <= start]
            if qual:
                mts = max(c[0] for c in qual)
                win = min(c[1] for c in qual if c[0] == mts)
                camp = next(c[2] for c in qual if c[1] == win)
            else:
                camp = b"organic"
            row = cd[(camp, day)]
            row[0] += 1
            row[1] += 1 if purch else 0
            row[2] += rev
            for a in {c[2] for c in qual}:
                cd[(a, day)][3] += 1
            u = ud.setdefault((p, day), [0, 0, 0, defaultdict(int)])
            u[0] += 1
            u[1] += len(s)
            u[2] += rev
            for e in s:
                if e[1]:
                    u[3][e[3]] += e[2]

    out.mkdir(parents=True, exist_ok=True)

    def date(day):
        import datetime
        return datetime.date(1970, 1, 1) + datetime.timedelta(days=day)

    import polars as pl
    urows = []
    for (p, day), (s, n, rev, cats) in ud.items():
        top = None
        if cats:
            m = max(cats.values())
            top = min(c for c, v in cats.items() if v == m).decode()
        urows.append((p.decode(), date(day), s, n, rev, top))
    crows = []
    for (c, day), (s, ps, rev, a) in cd.items():
        ch = b"organic" if c == b"organic" else campaigns[c]
        crows.append((c.decode(), ch.decode(), date(day), s, ps, rev, a))
    (out / "user_daily").mkdir(exist_ok=True)
    (out / "campaign_daily").mkdir(exist_ok=True)
    pl.DataFrame(urows, orient="row", schema={"user_id": pl.String, "day": pl.Date, "sessions": pl.Int64,
                 "events": pl.Int64, "revenue_cents": pl.Int64, "top_category": pl.String}
                 ).write_parquet(out / "user_daily" / "part-0.parquet")
    pl.DataFrame(crows, orient="row", schema={"campaign_id": pl.String, "channel": pl.String, "day": pl.Date,
                 "sessions": pl.Int64, "purchasing_sessions": pl.Int64, "revenue_cents": pl.Int64,
                 "assisted_sessions": pl.Int64}).write_parquet(out / "campaign_daily" / "part-0.parquet")


def read_lines_file(path: Path):
    lines = path.read_bytes().split(b"\n")
    if lines and lines[-1] == b"":
        lines.pop()
    return lines[1:]


if __name__ == "__main__":
    main()
