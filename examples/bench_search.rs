//! Synthetic metadata benchmark; does not enumerate or create user files.
use file_minnow::{
    index::{Entry, Snapshot},
    query::{SearchRequest, Sort, search},
};
use std::time::Instant;
fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .unwrap_or("100000".into())
        .parse()
        .unwrap();
    let start = Instant::now();
    let mut entries = Vec::with_capacity(n);
    for i in 0..n {
        let name = format!(
            "{}-{i:08}.{}",
            if i % 100 == 0 { "invoice" } else { "document" },
            if i % 3 == 0 { "pdf" } else { "txt" }
        );
        entries.push(Entry::new(
            format!("/data/projects/{:04}/{name}", i % 1000).into_bytes(),
            "file".into(),
            i as u64 * 100,
            1700000000 + i as i64,
        ));
    }
    let s = Snapshot {
        entries: file_minnow::table::Table::from_entries(entries),
        generation: 1,
        ..Default::default()
    };
    println!(
        "{{\"entries\":{n},\"construction_ms\":{:.3},\"entry_struct_bytes\":{}}}",
        start.elapsed().as_secs_f64() * 1000.,
        std::mem::size_of::<Entry>()
    );
    let cold = search(
        &s,
        &SearchRequest {
            limit: 100,
            ..Default::default()
        },
        None,
    )
    .unwrap();
    println!(
        "{}",
        serde_json::json!({"cold_name_order_build_ms":cold.elapsed_ms})
    );
    for query in [
        "invoice",
        "ext:pdf",
        "document",
        "not-present",
        "",
        "size:>50mb",
    ] {
        let r = SearchRequest {
            query: query.into(),
            limit: 100,
            sort: Sort::Name,
            ..Default::default()
        };
        let mut samples = Vec::new();
        let mut total = 0;
        for _ in 0..20 {
            let result = search(&s, &r, None).unwrap();
            samples.push(result.elapsed_ms);
            total = result.total;
            std::hint::black_box(result);
        }
        samples.sort_by(f64::total_cmp);
        println!(
            "{}",
            serde_json::json!({"query":query,"matches":total,"p50_ms":samples[10],"p95_ms":samples[18]})
        );
    }
    if let Ok(s) = std::fs::read_to_string("/proc/self/status") {
        for line in s
            .lines()
            .filter(|s| s.starts_with("VmRSS:") || s.starts_with("VmHWM:"))
        {
            eprintln!("{line}");
        }
    }
}
