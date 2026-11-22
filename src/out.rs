//! Output tables via the minimal Parquet writer.

use std::path::Path;

use crate::pq::{self, Column, Data, Dict};
use crate::WorkerOut;

pub fn write_user(path: &Path, r: &WorkerOut, cat_names: &[Vec<u8>], ws_day: i64) {
    let rows = r.day.len();
    // top_category: dictionary of every category name (they are few), indices of the non-null rows
    let mut cb = Vec::new();
    let mut ce = Vec::with_capacity(cat_names.len());
    for c in cat_names {
        cb.extend_from_slice(c);
        ce.push(cb.len() as u32);
    }
    let mut present = Vec::with_capacity(rows);
    let mut tidx = Vec::new();
    for &t in &r.top {
        present.push(t != u32::MAX);
        if t != u32::MAX {
            tidx.push(t);
        }
    }
    let ndays = r.day.iter().max().map_or(1, |&d| d as usize + 1);
    let days: Vec<i32> = (0..ndays).map(|d| (ws_day + d as i64) as i32).collect();
    pq::write(
        path,
        rows,
        &[
            Column {
                name: "user_id",
                date: false,
                data: Data::Dict { dict: Dict::Str { bytes: &r.names, ends: &r.name_end }, idx: &r.uidx, present: None, runs: true },
            },
            Column { name: "day", date: true, data: Data::Dict { dict: Dict::I32(&days), idx: &r.day, present: None, runs: false } },
            Column { name: "sessions", date: false, data: Data::DeltaI32(&r.sessions) },
            Column { name: "events", date: false, data: Data::DeltaI32(&r.events) },
            // always INT64: a per-file choice could give files different schemas, which multi-file
            // readers (polars glob, pyarrow dataset) reject
            Column { name: "revenue_cents", date: false, data: Data::DeltaI64(&r.revenue) },
            Column {
                name: "top_category",
                date: false,
                data: Data::Dict { dict: Dict::Str { bytes: &cb, ends: &ce }, idx: &tidx, present: Some(&present), runs: false },
            },
        ],
    )
    .expect("write user_daily");
}

#[allow(clippy::too_many_arguments)]
pub fn write_all(
    outdir: &Path,
    results: &[WorkerOut],
    camp: &[[i64; 4]],
    camp_ids: &[Vec<u8>],
    channels: &[Vec<u8>],
    cat_names: &[Vec<u8>],
    ws_day: i64,
    ndays: usize,
    _threads: usize,
) {
    let ud = outdir.join("user_daily");
    let cd = outdir.join("campaign_daily");
    std::fs::create_dir_all(&ud).expect("mkdir");
    std::fs::create_dir_all(&cd).expect("mkdir");
    let _ = (results, cat_names, ud);
    {
        // campaign_id: dictionary of the campaigns with rows (their rows are contiguous: RLE runs);
        // channel and day: small dictionaries; counts: DELTA_BINARY_PACKED
        let (mut ib, mut ie, mut cb, mut ce) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let mut chans: Vec<&[u8]> = Vec::new();
        let (mut cidx, mut hidx, mut didx) = (Vec::new(), Vec::new(), Vec::new());
        let mut v: [Vec<i64>; 4] = Default::default();
        for c in 0..=camp_ids.len() {
            let mut named = false;
            for d in 0..ndays {
                let cell = camp[c * ndays + d];
                if cell[0] == 0 && cell[3] == 0 {
                    continue;
                }
                let (id, ch): (&[u8], &[u8]) =
                    if c == camp_ids.len() { (b"organic", b"organic") } else { (&camp_ids[c], &channels[c]) };
                if !named {
                    named = true;
                    ib.extend_from_slice(id);
                    ie.push(ib.len() as u32);
                }
                cidx.push(ie.len() as u32 - 1);
                let h = match chans.iter().position(|&x| x == ch) {
                    Some(h) => h,
                    None => {
                        chans.push(ch);
                        cb.extend_from_slice(ch);
                        ce.push(cb.len() as u32);
                        chans.len() - 1
                    }
                };
                hidx.push(h as u32);
                didx.push(d as u32);
                for k in 0..4 {
                    v[k].push(cell[k]);
                }
            }
        }
        let days: Vec<i32> = (0..ndays.max(1)).map(|d| (ws_day + d as i64) as i32).collect();
        pq::write(
            &cd.join("part-00000.parquet"),
            didx.len(),
            &[
                Column {
                    name: "campaign_id",
                    date: false,
                    data: Data::Dict { dict: Dict::Str { bytes: &ib, ends: &ie }, idx: &cidx, present: None, runs: true },
                },
                Column {
                    name: "channel",
                    date: false,
                    data: Data::Dict { dict: Dict::Str { bytes: &cb, ends: &ce }, idx: &hidx, present: None, runs: false },
                },
                Column { name: "day", date: true, data: Data::Dict { dict: Dict::I32(&days), idx: &didx, present: None, runs: false } },
                Column { name: "sessions", date: false, data: Data::DeltaI64(&v[0]) },
                Column { name: "purchasing_sessions", date: false, data: Data::DeltaI64(&v[1]) },
                Column { name: "revenue_cents", date: false, data: Data::DeltaI64(&v[2]) },
                Column { name: "assisted_sessions", date: false, data: Data::DeltaI64(&v[3]) },
            ],
        )
        .expect("write campaign_daily");
    }
}
