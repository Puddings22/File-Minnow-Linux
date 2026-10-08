//! Measure an existing index read-only: load time, memory and query latency.
//! Prints counts and timings only, never file names.
use file_minnow::{
    query::{SearchRequest, Sort, search},
    store,
};
use std::{path::PathBuf, time::Instant};

fn rss_kib() -> (u64, u64) {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let field = |name: &str| {
        status
            .lines()
            .find(|l| l.starts_with(name))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    };
    (field("VmRSS:"), field("VmHWM:"))
}

fn main() {
    let dir = PathBuf::from(
        std::env::args()
            .nth(1)
            .expect("usage: bench_index DATA_DIR"),
    );
    let before = rss_kib().0;
    let started = Instant::now();
    let snapshot = store::load(&dir).expect("load index");
    let load_ms = started.elapsed().as_secs_f64() * 1000.;
    let (rss, peak) = rss_kib();
    println!(
        "{}",
        serde_json::json!({"entries": snapshot.entries.len(), "load_ms": load_ms,
            "index_rss_mib": (rss - before) as f64 / 1024., "peak_rss_mib": peak as f64 / 1024.})
    );
    let cold = search(
        &snapshot,
        &SearchRequest {
            limit: 100,
            ..Default::default()
        },
        None,
    )
    .unwrap();
    println!(
        "{}",
        serde_json::json!({"cold_name_order_ms": cold.elapsed_ms, "rss_after_order_mib": rss_kib().0 as f64 / 1024.})
    );
    for query in [
        "a",
        "readme",
        "ext:rs",
        "ext:pdf;docx",
        "zzqqxxnotfound",
        "path:src main",
        "size:>100mb",
        "*.json",
        "regex:^test.*\\.py$",
        "",
        "pic:",
        "doc: report",
    ] {
        for sort in [Sort::Name, Sort::Size, Sort::Type] {
            let request = SearchRequest {
                query: query.into(),
                limit: 10_000,
                sort,
                ..Default::default()
            };
            let mut times: Vec<f64> = (0..7)
                .map(|_| search(&snapshot, &request, None).unwrap().elapsed_ms)
                .collect();
            times.sort_by(f64::total_cmp);
            let matches = search(&snapshot, &request, None).unwrap().total;
            println!(
                "{}",
                serde_json::json!({"query": query, "sort": format!("{sort:?}"), "matches": matches, "median_ms": times[3], "max_ms": times[6]})
            );
        }
    }
}
