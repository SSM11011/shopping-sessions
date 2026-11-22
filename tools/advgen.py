#!/usr/bin/env python3
"""Adversarial dataset generator: small, very dirty, tiny ID space. Usage: advgen.py SEED OUT_DIR
Exercises paths the official generator rarely hits: exotic IDs (case, spaces, tabs, unicode, long),
odd quoting, long lines, headers repeated mid-file, missing trailing newline, empty/header-only files,
F3 with many leading zeros, boundary timestamps, many duplicates with conflicting users."""
import datetime
import json
import os
import random
import sys

DAY = 86_400_000


def iso(ms, rnd):
    dt = datetime.datetime.fromtimestamp(ms / 1000, tz=datetime.timezone.utc)
    r = rnd.random()
    if ms % 1000 or r < 0.3:
        z = "0" * rnd.choice([0, 0, 0, 1, 3, 7, 12, 20])  # 7 / 12 zeros: F3 with the F1 / F2 lengths
        return z + str(ms)
    if r < 0.45:
        off = rnd.choice([-600, -330, 0, 345, 720, 23 * 60 + 59])
        loc = dt + datetime.timedelta(minutes=off)
        sign = "+" if off >= 0 else "-"
        if off == 0 and rnd.random() < 0.5:
            sign = "-"
        a = abs(off)
        return loc.strftime("%Y-%m-%dT%H:%M:%S") + f"{sign}{a // 60:02d}:{a % 60:02d}"
    return dt.strftime("%Y-%m-%dT%H:%M:%SZ")


def bad_ts(ms, rnd):
    dt = datetime.datetime.fromtimestamp(ms // 1000, tz=datetime.timezone.utc)
    return rnd.choice([
        "", dt.strftime("%Y-%m-%dT%H:%M:%S") + ".000Z", dt.strftime("%Y-%m-%dt%H:%M:%SZ"),
        dt.strftime("%Y-%m-%d %H:%M:%SZ"), dt.strftime("%Y-%m-%dT%H:%M:%S"), f"{ms}.0", "-5", "1.79e12",
        dt.strftime("%Y-13-%dT%H:%M:%SZ"), dt.strftime("%Y-02-30T%H:%M:%SZ"), dt.strftime("%Y-%m-%dT24:%M:%SZ"),
        dt.strftime("%Y-%m-%dT%H:%M:60Z"), dt.strftime("%Y-%m-%dT%H:%M:%S+24:00"),
        dt.strftime("%Y-%m-%dT%H:%M:%S+05:60"), dt.strftime("%Y-%m-%dT%H:%M:%S+0200"), "9" * 21,
        "٢٥٠", str(ms)[:-1] + "２", dt.strftime("%Y-%m-%dT%H:%M:%SZ") + "\r",
    ])


def main():
    seed, out = int(sys.argv[1]), sys.argv[2]
    rnd = random.Random(seed)
    # window: random midnight, sometimes around Feb 29 2028
    start_day = rnd.choice([
        datetime.date(2028, 2, 20), datetime.date(2026, 10, 1), datetime.date(2025, 12, 25),
        datetime.date(2027, 2, 20),
    ]) + datetime.timedelta(days=rnd.randrange(0, 5))
    ws = int(datetime.datetime(start_day.year, start_day.month, start_day.day, tzinfo=datetime.timezone.utc).timestamp() * 1000)
    we = ws + 14 * DAY
    os.makedirs(out, exist_ok=True)
    for d in ("events", "clicks", "links"):
        os.makedirs(os.path.join(out, d), exist_ok=True)
    with open(os.path.join(out, "manifest.json"), "w") as f:
        json.dump({"window_start": iso_z(ws), "window_end": iso_z(we)}, f)

    cats = ["shoes", "books", "Books", "a b", "unknowm", "zz"]
    prods = [f"p{i}" for i in range(1, 30)] + ["P5", " p3", "p050", "x"]
    with open(os.path.join(out, "products.csv"), "w") as f:
        f.write("product_id,category\n")
        for p in prods:
            f.write(f"{p},{rnd.choice(cats)}\n")
    camps = [f"c_{i}" for i in range(1, 12)] + ["C_3", "c_ 4", "camp-x"]
    with open(os.path.join(out, "campaigns.csv"), "w") as f:
        f.write("campaign_id,channel\n")
        for c in camps:
            f.write(f"{c},{rnd.choice(['search', 'social', 'email', 'Organic'])}\n")

    base_ids = [f"u{i}" for i in range(rnd.randint(5, 60))]
    exotic = ["U1", " u1", "u1 ", "u\t2", "ü3", "averyveryverylongid_1234567890", "u_9", "A", "0", "zzzzzzzzzzz",
              "UU", "u1\r", "user_id"]
    ids = base_ids + rnd.sample(exotic, rnd.randint(0, len(exotic)))

    def q(v):
        # quote some fields; rarely break quoting
        r = rnd.random()
        if r < 0.25:
            return f'"{v}"'
        if r < 0.003:
            return f'"{v}'
        if r < 0.006:
            return f'{v}"x'
        return v

    def line(fields):
        r = rnd.random()
        if r < 0.01:
            fields = fields[:-1]
        elif r < 0.02:
            fields = fields + ["x"]
        return ",".join(q(v) for v in fields)

    def write_parts(folder, header, rows):
        rnd.shuffle(rows)
        nparts = rnd.randint(1, 4)
        parts = [[] for _ in range(nparts)]
        for r in rows:
            parts[rnd.randrange(nparts)].append(r)
        for i, p in enumerate(parts):
            if folder != "events" and rnd.random() < 0.3 and p:
                p.insert(rnd.randrange(len(p)), header)  # header-equal row mid-file is data
            if rnd.random() < 0.1:
                p.insert(rnd.randrange(len(p) + 1), "")
            body = "\n".join(p)
            text = header + "\n" + body + ("\n" if p and rnd.random() < 0.85 else "")
            with open(os.path.join(out, folder, f"part-{i:05d}.csv"), "w", newline="") as f:
                f.write(text)
        if rnd.random() < 0.3:
            with open(os.path.join(out, folder, "part-99999.csv"), "w") as f:
                f.write(header + "\n")
        if rnd.random() < 0.2:
            open(os.path.join(out, folder, "part-99998.csv"), "w").close()
        if rnd.random() < 0.2:
            with open(os.path.join(out, folder, "notes.txt"), "w") as f:
                f.write("junk,junk\n")

    # links
    links = []
    for _ in range(rnd.randint(0, 80)):
        a, b = rnd.choice(ids + ["ghost1", "ghost0", "Aa", "user_id_b"]), rnd.choice(ids)
        if rnd.random() < 0.05:
            a = ""
        links.append(line([a, b]))
    write_parts("links", "user_id_a,user_id_b", links)

    # events: sessions with boundary gaps
    events = []
    eid = 0
    for _ in range(rnd.randint(50, 400)):
        u = rnd.choice(ids)
        t = ws + rnd.randrange(-DAY, 14 * DAY + DAY)
        t -= t % rnd.choice([1, 1000, 1000, 60000])
        for _ in range(rnd.randint(1, 6)):
            eid += 1
            e = rnd.choice([f"e{eid}", f"e{eid}", f"E{eid}", f"e{eid} "]) if rnd.random() < 0.05 else f"e{eid}"
            typ = rnd.choice(["view"] * 6 + ["cart"] * 2 + ["purchase"] * 3 + ["Purchase", "view ", ""])
            prod = rnd.choice(prods + ["p99", "", "P1"])
            price = rnd.choice(["", "100", "0250", "19.99", "-5", "0", "10000000", "10000001", "00", "٢٥٠",
                                "0000000000000000000000250", str(rnd.randint(1, 50000))])
            ing = t + rnd.choice([0, 1000, 600000, DAY, DAY + 1000, -5000, 3 * DAY])
            ing -= ing % 1000
            ts_s = iso(t, rnd) if rnd.random() > 0.05 else bad_ts(t, rnd)
            user = u if rnd.random() > 0.03 else ""
            row = [e, user, ts_s, iso_z(ing), typ, prod, price]
            events.append(line(row))
            ncopies = rnd.choice([0, 0, 0, 1, 2, 3])
            offsets = rnd.sample([-2000, -1000, 1000, 2000, 60000, 0, 0], ncopies)
            for off in offsets:
                row2 = list(row)
                row2[3] = iso_z(ing + off)
                if off != 0:  # only copies with their own ingest time may differ (rules: equal ingest => identical)
                    if rnd.random() < 0.3:
                        row2[1] = rnd.choice(ids)
                    if rnd.random() < 0.2:
                        row2[2] = iso(t + rnd.randrange(0, 3600_000), rnd)
                events.append(line(row2))
            t += rnd.choice([0, 0, 1, 60_000, 1_799_999, 1_800_000, 1_800_001, 3_600_000, 86_400_000])
    write_parts("events", "event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents", events)

    clicks = []
    for k in range(rnd.randint(0, 300)):
        u = rnd.choice(ids)
        t = ws + rnd.randrange(-8 * DAY, 14 * DAY + DAY)
        t -= t % rnd.choice([1, 1000])
        cid = rnd.choice([f"k{k}", f"k{k}", f"K{k}", f"k{k:03d}", f"k_{k}"])
        row = [cid, u if rnd.random() > 0.03 else "", rnd.choice(camps + ["c_999", "C_1"]),
               iso(t, rnd) if rnd.random() > 0.05 else bad_ts(t, rnd)]
        clicks.append(line(row))
        if rnd.random() < 0.1:
            clicks.append(line(row))
        if rnd.random() < 0.05:  # same-ms tie with a different id
            row2 = list(row)
            row2[0] = f"j{k}"
            clicks.append(line(row2))
    # clicks exactly at session starts / 7 days before
    write_parts("clicks", "click_id,user_id,campaign_id,ts", clicks)


def iso_z(ms):
    return datetime.datetime.fromtimestamp(ms // 1000, tz=datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


if __name__ == "__main__":
    main()
