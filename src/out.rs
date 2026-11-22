//! Output tables via the minimal Parquet writer.

use std::path::Path;

use crate::pq::{self, Column, Data};
use crate::WorkerOut;

pub fn write_user(path: &Path, r: &WorkerOut, cat_names: &[Vec<u8>]) {
    let rows = r.day.len();
    let mut tb = Vec::new();
    let mut te = Vec::new();
    let mut present = Vec::with_capacity(rows);
    for &t in &r.top {
        if t == u32::MAX {
            present.push(false);
        } else {
            present.push(true);
            tb.extend_from_slice(&cat_names[t as usize]);
            te.push(tb.len() as u32);
        }
    }
    pq::write(
        path,
        rows,
        &[
            Column { name: "user_id", date: false, data: Data::Str { bytes: &r.names, ends: &r.name_end, present: None } },
            Column { name: "day", date: true, data: Data::I32(&r.day) },
            Column { name: "sessions", date: false, data: Data::I32(&r.sessions) },
            Column { name: "events", date: false, data: Data::I32(&r.events) },
            // always INT64: a per-file choice could give files different schemas, which multi-file
            // readers (polars glob, pyarrow dataset) reject
            Column { name: "revenue_cents", date: false, data: Data::I64(&r.revenue) },
            Column { name: "top_category", date: false, data: Data::Str { bytes: &tb, ends: &te, present: Some(&present) } },
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
        let (mut ib, mut ie, mut cb, mut ce) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let mut days = Vec::new();
        let mut v: [Vec<i64>; 4] = Default::default();
        for c in 0..=camp_ids.len() {
            for d in 0..ndays {
                let cell = camp[c * ndays + d];
                if cell[0] == 0 && cell[3] == 0 {
                    continue;
                }
                let (id, ch): (&[u8], &[u8]) =
                    if c == camp_ids.len() { (b"organic", b"organic") } else { (&camp_ids[c], &channels[c]) };
                ib.extend_from_slice(id);
                ie.push(ib.len() as u32);
                cb.extend_from_slice(ch);
                ce.push(cb.len() as u32);
                days.push((ws_day + d as i64) as i32);
                for k in 0..4 {
                    v[k].push(cell[k]);
                }
            }
        }
        pq::write(
            &cd.join("part-00000.parquet"),
            days.len(),
            &[
                Column { name: "campaign_id", date: false, data: Data::Str { bytes: &ib, ends: &ie, present: None } },
                Column { name: "channel", date: false, data: Data::Str { bytes: &cb, ends: &ce, present: None } },
                Column { name: "day", date: true, data: Data::I32(&days) },
                Column { name: "sessions", date: false, data: Data::I64(&v[0]) },
                Column { name: "purchasing_sessions", date: false, data: Data::I64(&v[1]) },
                Column { name: "revenue_cents", date: false, data: Data::I64(&v[2]) },
                Column { name: "assisted_sessions", date: false, data: Data::I64(&v[3]) },
            ],
        )
        .expect("write campaign_daily");
    }
}
