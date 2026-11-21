//! Parquet output.

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use parquet::basic::Compression;
use parquet::data_type::{ByteArray, ByteArrayType, Int32Type, Int64Type};
use parquet::file::properties::{EnabledStatistics, WriterProperties, WriterVersion};
use parquet::file::writer::SerializedFileWriter;
use parquet::schema::parser::parse_message_type;
use parquet::schema::types::Type;

use crate::WorkerOut;

const USER_SCHEMA: &str = "message user_daily {
  required binary user_id (STRING);
  required int32 day (DATE);
  required int64 sessions;
  required int64 events;
  required int64 revenue_cents;
  optional binary top_category (STRING);
}";

const CAMP_SCHEMA: &str = "message campaign_daily {
  required binary campaign_id (STRING);
  required binary channel (STRING);
  required int32 day (DATE);
  required int64 sessions;
  required int64 purchasing_sessions;
  required int64 revenue_cents;
  required int64 assisted_sessions;
}";

fn props() -> Arc<WriterProperties> {
    Arc::new(
        WriterProperties::builder()
            .set_writer_version(WriterVersion::PARQUET_1_0)
            .set_compression(Compression::UNCOMPRESSED)
            .set_dictionary_enabled(false)
            .set_statistics_enabled(EnabledStatistics::None)
            .set_data_page_size_limit(8 << 20)
            .set_write_batch_size(1 << 20)
            .build(),
    )
}

enum Col<'a> {
    Bytes(Vec<ByteArray>, Option<Vec<i16>>),
    I32(&'a [i32]),
    I64(&'a [i64]),
}

fn write_file(path: &Path, schema: Arc<Type>, cols: Vec<Col<'_>>) {
    let f = BufWriter::with_capacity(4 << 20, File::create(path).expect("create parquet"));
    let mut w = SerializedFileWriter::new(f, schema, props()).expect("writer");
    let mut rg = w.next_row_group().expect("rg");
    let mut it = cols.into_iter();
    while let Some(mut c) = rg.next_column().expect("col") {
        match it.next().expect("column data") {
            Col::Bytes(v, def) => {
                c.typed::<ByteArrayType>().write_batch(&v, def.as_deref(), None).expect("write");
            }
            Col::I32(v) => {
                c.typed::<Int32Type>().write_batch(v, None, None).expect("write");
            }
            Col::I64(v) => {
                c.typed::<Int64Type>().write_batch(v, None, None).expect("write");
            }
        }
        c.close().expect("close col");
    }
    rg.close().expect("close rg");
    w.close().expect("close file");
}

fn write_user(path: &Path, r: &WorkerOut, cat_bytes: &[Bytes]) {
    let names = Bytes::from(r.names.clone());
    let mut ids = Vec::with_capacity(r.name_end.len());
    let mut s = 0usize;
    for &e in &r.name_end {
        ids.push(ByteArray::from(names.slice(s..e as usize)));
        s = e as usize;
    }
    let mut tops = Vec::new();
    let mut def = Vec::with_capacity(r.top.len());
    for &t in &r.top {
        if t == u32::MAX {
            def.push(0);
        } else {
            def.push(1);
            tops.push(ByteArray::from(cat_bytes[t as usize].clone()));
        }
    }
    let schema = Arc::new(parse_message_type(USER_SCHEMA).unwrap());
    write_file(
        path,
        schema,
        vec![
            Col::Bytes(ids, None),
            Col::I32(&r.day),
            Col::I64(&r.sessions),
            Col::I64(&r.events),
            Col::I64(&r.revenue),
            Col::Bytes(tops, Some(def)),
        ],
    );
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
    let cat_bytes: Vec<Bytes> = cat_names.iter().map(|c| Bytes::from(c.clone())).collect();
    let any = results.iter().any(|r| !r.day.is_empty());
    std::thread::scope(|s| {
        for (w, r) in results.iter().enumerate() {
            if r.day.is_empty() && (any || w > 0) {
                continue;
            }
            let ud = &ud;
            let cat_bytes = &cat_bytes;
            s.spawn(move || write_user(&ud.join(format!("part-{w:05}.parquet")), r, cat_bytes));
        }
        // campaign_daily
        let mut ids = Vec::new();
        let mut chans = Vec::new();
        let mut days = Vec::new();
        let mut v = [Vec::new(), Vec::new(), Vec::new(), Vec::new()];
        let organic = Bytes::from_static(b"organic");
        for c in 0..=camp_ids.len() {
            for d in 0..ndays {
                let cell = camp[c * ndays + d];
                if cell[0] == 0 && cell[3] == 0 {
                    continue;
                }
                if c == camp_ids.len() {
                    ids.push(ByteArray::from(organic.clone()));
                    chans.push(ByteArray::from(organic.clone()));
                } else {
                    ids.push(ByteArray::from(camp_ids[c].clone()));
                    chans.push(ByteArray::from(channels[c].clone()));
                }
                days.push((ws_day + d as i64) as i32);
                for k in 0..4 {
                    v[k].push(cell[k]);
                }
            }
        }
        let schema = Arc::new(parse_message_type(CAMP_SCHEMA).unwrap());
        write_file(
            &cd.join("part-00000.parquet"),
            schema,
            vec![
                Col::Bytes(ids, None),
                Col::Bytes(chans, None),
                Col::I32(&days),
                Col::I64(&v[0]),
                Col::I64(&v[1]),
                Col::I64(&v[2]),
                Col::I64(&v[3]),
            ],
        );
    });
}
