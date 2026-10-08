//! Cost of applying one changed folder to an existing index (read-only:
//! the folder is re-read, nothing on disk is modified). Prints timings only.
use file_minnow::{config::IndexConfig, service, store};
use std::{collections::BTreeSet, os::unix::ffi::OsStringExt, path::PathBuf, time::Instant};

fn main() {
    let dir = PathBuf::from(
        std::env::args()
            .nth(1)
            .expect("usage: bench_update DATA_DIR"),
    );
    let snapshot = store::load(&dir).expect("load index");
    let config = IndexConfig::from_roots(&file_minnow::index::root_paths(&snapshot.roots));
    // Pick a folder from the middle of the index as the "changed" one.
    let table = &snapshot.entries;
    let folder = (table.len() / 2..table.len())
        .find(|i| table.row(*i).kind == file_minnow::index::Kind::Folder)
        .expect("a folder");
    let changed = BTreeSet::from([PathBuf::from(std::ffi::OsString::from_vec(
        table.path(folder),
    ))]);
    let _ = table.name_order();
    let path = table.path(folder);
    let start = table.lower_bound(&path);
    let subtree_rows = table.subtree_end(start, &path) - start;
    // Copy-only cost: splice in nothing, i.e. copy every row once.
    let started = Instant::now();
    let mut builder = file_minnow::table::Builder::with_capacity(table.len());
    builder.copy_range(table, 0, table.len());
    let copied = builder.finish();
    copied.splice_name_order(table, &[(0, table.len(), 0)]);
    println!(
        "{}",
        serde_json::json!({"changed_subtree_rows": subtree_rows,
            "copy_all_rows_and_order_ms": started.elapsed().as_secs_f64() * 1000.})
    );
    for round in 0..3 {
        let started = Instant::now();
        let next =
            service::reconcile_config(&snapshot, &changed, &config, &dir, &|| false, &|_| {})
                .unwrap()
                .unwrap();
        let reconcile_ms = started.elapsed().as_secs_f64() * 1000.;
        let started = Instant::now();
        let _ = next.entries.name_order();
        let order_ms = started.elapsed().as_secs_f64() * 1000.;
        let started = Instant::now();
        store::save(&std::env::temp_dir(), &next).ok();
        let save_ms = started.elapsed().as_secs_f64() * 1000.;
        println!(
            "{}",
            serde_json::json!({"round": round, "entries": next.entries.len(), "reconcile_ms": reconcile_ms,
                "rebuild_name_order_ms": order_ms, "full_save_ms": save_ms})
        );
    }
    let _ = std::fs::remove_file(std::env::temp_dir().join("index.bin"));
}
