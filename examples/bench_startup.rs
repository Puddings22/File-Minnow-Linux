//! Start-up cost: a full walk versus the catch-up from a saved index
//! (`index::quick_scan`), on a folder you name. Reads the folder only; the
//! index lives in a temporary directory. Warm cache: run it twice.
use file_minnow::{config::IndexConfig, index, watch::Watcher};
use std::{path::PathBuf, time::Instant};

fn main() {
    let root = PathBuf::from(
        std::env::args()
            .nth(1)
            .expect("usage: bench_startup FOLDER"),
    );
    let data = std::env::temp_dir().join(format!("minnow-bench-startup-{}", std::process::id()));
    std::fs::create_dir_all(&data).unwrap();
    let config = IndexConfig::from_roots(&[root]);
    for round in 0..3 {
        // Both add a folder watch per folder, as the app does.
        let watcher = Watcher::new(|_| {}).expect("inotify");
        let sweep = watcher.begin_sweep();
        let add = |folder: &std::path::Path| watcher.add(folder, sweep);
        let started = Instant::now();
        let mut full = index::scan_config_watched(
            &config,
            &data,
            1,
            None,
            None,
            &|| false,
            &|_| {},
            Some(&add),
        )
        .unwrap()
        .unwrap();
        let full_ms = started.elapsed().as_secs_f64() * 1000.;
        full.saved_at = index::now();
        drop(watcher);
        let watcher = Watcher::new(|_| {}).expect("inotify");
        let sweep = watcher.begin_sweep();
        let add = |folder: &std::path::Path| watcher.add(folder, sweep);
        let started = Instant::now();
        let quick = index::quick_scan(&config, &data, &full, &|| false, &|_| {}, Some(&add))
            .unwrap()
            .unwrap();
        let quick_ms = started.elapsed().as_secs_f64() * 1000.;
        let folders = full
            .entries
            .iter()
            .filter(|e| e.kind == index::Kind::Folder)
            .count();
        println!(
            "{}",
            serde_json::json!({"round": round, "entries": full.entries.len(), "folders": folders,
                "full_walk_ms": full_ms, "catch_up_ms": quick_ms, "same_entries": quick.entries.len() == full.entries.len()})
        );
    }
    let _ = std::fs::remove_dir_all(&data);
}
