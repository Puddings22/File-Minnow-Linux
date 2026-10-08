use file_minnow::{
    config::{ConfigStore, IndexConfig, RawPath, RootConfig, Schedule, Settings},
    index,
    service::{self, Request, Runtime, Server},
    store,
};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
    thread,
    time::{Duration, Instant},
};

fn setup() -> (tempfile::TempDir, PathBuf, PathBuf, ConfigStore, Settings) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("files");
    fs::create_dir_all(root.join("nested")).unwrap();
    let dir = store::prepare(&temp.path().join("cache")).unwrap();
    let config = ConfigStore::new(temp.path().join("preferences/settings.json"));
    let settings = Settings {
        index: IndexConfig::from_roots(std::slice::from_ref(&root)),
        ..Default::default()
    };
    (temp, root, dir, config, settings)
}
fn wait(runtime: &Runtime, f: impl Fn(&index::Snapshot, &service::Status) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let state = runtime.state.read().unwrap();
        if !state.status.scanning && f(&state.snapshot, &state.status) {
            return;
        }
        drop(state);
        assert!(
            Instant::now() < deadline,
            "timed out: {:?}",
            runtime.handle.status()
        );
        thread::sleep(Duration::from_millis(20));
    }
}
fn has(s: &index::Snapshot, name: &[u8]) -> bool {
    s.entries.iter().any(|e| e.name() == name)
}

#[test]
fn initial_and_live_exclusions_match() {
    let (_temp, root, dir, config, mut settings) = setup();
    fs::create_dir(root.join("cache")).unwrap();
    fs::write(root.join("cache/hidden.rs"), b"x").unwrap();
    fs::write(root.join("main.rs"), b"x").unwrap();
    fs::write(root.join("ignore.tmp"), b"x").unwrap();
    fs::write(root.join("readme.txt"), b"x").unwrap();
    settings.index.excluded_folders = vec![RawPath::new(&root.join("cache"))];
    settings.index.excluded_files = vec!["*.tmp".into()];
    settings.index.included_files = vec!["*.rs".into(), "*.tmp".into()];
    settings.index.exclude_hidden = true;
    let runtime = Runtime::configured(settings, dir, config).unwrap();
    wait(&runtime, |s, _| s.generation > 0);
    {
        let s = runtime.state.read().unwrap();
        assert!(has(&s.snapshot, b"main.rs"));
        for n in [b"hidden.rs".as_slice(), b"ignore.tmp", b"readme.txt"] {
            assert!(!has(&s.snapshot, n));
        }
    }
    fs::write(root.join(".secret.rs"), b"x").unwrap();
    fs::write(root.join("fresh.rs"), b"x").unwrap();
    wait(&runtime, |s, _| has(s, b"fresh.rs"));
    assert!(!has(&runtime.state.read().unwrap().snapshot, b".secret.rs"));
    fs::rename(root.join("fresh.rs"), root.join("fresh.tmp")).unwrap();
    wait(&runtime, |s, _| !has(s, b"fresh.rs"));
    assert!(!has(&runtime.state.read().unwrap().snapshot, b"fresh.tmp"));
}

#[test]
fn settings_apply_remove_roots_and_survive_cache_deletion() {
    let (_temp, root, dir, config, settings) = setup();
    fs::write(root.join("first.txt"), b"x").unwrap();
    let runtime = Runtime::configured(settings, dir.clone(), config.clone()).unwrap();
    wait(&runtime, |s, _| has(s, b"first.txt"));
    let mut changed = runtime.handle.settings();
    changed.index.roots.clear();
    let saved = runtime.handle.apply_settings(changed).unwrap();
    wait(&runtime, |s, _| s.entries.is_empty());
    drop(runtime);
    fs::remove_file(dir.join("index.bin")).unwrap();
    assert_eq!(config.load().unwrap(), saved);
    let runtime = Runtime::configured(config.load().unwrap(), dir, config).unwrap();
    wait(&runtime, |s, _| s.generation > 0);
    assert!(runtime.state.read().unwrap().snapshot.entries.is_empty());
}

#[test]
fn pause_defers_changes_and_resume_reconciles() {
    let (_temp, root, dir, config, settings) = setup();
    let runtime = Runtime::configured(settings, dir, config).unwrap();
    wait(&runtime, |s, _| s.generation > 0);
    runtime.handle.pause(true).unwrap();
    wait(&runtime, |_, s| s.paused);
    let before = runtime.handle.status().generation;
    fs::write(root.join("while-paused.txt"), b"x").unwrap();
    thread::sleep(Duration::from_millis(300));
    assert_eq!(runtime.handle.status().generation, before);
    runtime.handle.pause(false).unwrap();
    wait(&runtime, |s, status| {
        !status.paused && has(s, b"while-paused.txt")
    });
}

#[test]
fn nonrecursive_root_and_manual_monitoring() {
    let (_temp, root, dir, config, mut settings) = setup();
    fs::write(root.join("nested/deep.txt"), b"x").unwrap();
    settings.index.roots[0].recursive = false;
    settings.index.roots[0].monitor = false;
    settings.index.roots[0].schedule = Schedule::Never;
    let runtime = Runtime::configured(settings, dir, config).unwrap();
    wait(&runtime, |s, _| s.generation > 0);
    assert!(!has(&runtime.state.read().unwrap().snapshot, b"deep.txt"));
    fs::write(root.join("top.txt"), b"x").unwrap();
    thread::sleep(Duration::from_millis(300));
    assert!(!has(&runtime.state.read().unwrap().snapshot, b"top.txt"));
    runtime.handle.rescan(Some(root)).unwrap();
    wait(&runtime, |s, _| has(s, b"top.txt"));
}

#[test]
fn interval_rescan_detects_unmonitored_updates() {
    let (_temp, root, dir, config, mut settings) = setup();
    settings.index.roots[0].monitor = false;
    settings.index.roots[0].schedule = Schedule::Interval { seconds: 1 };
    let runtime = Runtime::configured(settings, dir, config).unwrap();
    wait(&runtime, |s, _| s.generation > 0);
    fs::write(root.join("scheduled.txt"), b"x").unwrap();
    wait(&runtime, |s, _| has(s, b"scheduled.txt"));
}

#[test]
fn cancelled_scan_never_publishes_partial_result() {
    let (_temp, root, dir, _config, settings) = setup();
    for i in 0..100 {
        fs::write(root.join(format!("{i}.txt")), b"x").unwrap();
    }
    let calls = AtomicUsize::new(0);
    let outcome = index::scan_config(
        &settings.index,
        &dir,
        1,
        None,
        &|| calls.fetch_add(1, Ordering::Relaxed) > 20,
        &|_| {},
    )
    .unwrap();
    assert!(outcome.is_none());
}

#[test]
fn service_settings_watch_and_concurrent_search() {
    let (_temp, root, dir, config, settings) = setup();
    fs::write(root.join("item.txt"), b"x").unwrap();
    let runtime = Runtime::configured(settings, dir.clone(), config).unwrap();
    let server = Server::start(runtime.handle.clone(), &dir).unwrap();
    wait(&runtime, |s, _| s.generation > 0);
    let initial = service::request(&dir, &Request::Settings)
        .unwrap()
        .settings
        .unwrap();
    let revision = runtime.handle.status().revision;
    let watch_dir = dir.clone();
    let waiting = thread::spawn(move || {
        service::request(&watch_dir, &Request::Watch { after: revision }).unwrap()
    });
    let response = service::request(&dir, &Request::Search(Default::default())).unwrap();
    assert_eq!(response.search.unwrap().total, 3);
    let mut next = initial.clone();
    next.index.excluded_files.push("*.txt".into());
    let applied = service::request(
        &dir,
        &Request::SetSettings {
            settings: Box::new(next),
        },
    )
    .unwrap();
    assert!(applied.error.is_none());
    assert!(waiting.join().unwrap().status.revision > revision);
    wait(&runtime, |s, _| !has(s, b"item.txt"));
    let stale = service::request(
        &dir,
        &Request::SetSettings {
            settings: Box::new(initial),
        },
    )
    .unwrap();
    assert!(stale.error.is_some());
    drop(server);
    assert!(!file_minnow::store::socket_path(&dir, "search.sock").exists());
}

#[test]
fn root_validation_preserves_offline_but_rejects_overlap() {
    let mut config = IndexConfig::from_roots(&[PathBuf::from("/media/offline")]);
    assert!(config.validate().is_ok());
    config.roots.push(RootConfig::new(std::path::Path::new(
        "/media/offline/nested",
    )));
    assert!(config.validate().is_err());
}

#[test]
fn own_scans_do_not_trigger_more_scans() {
    // Regression: a watcher that reported folder opens turned every rescan
    // into an event flood, a queue overflow and another rescan, forever.
    let (_temp, root, dir, config, settings) = setup();
    for i in 0..400 {
        fs::create_dir_all(root.join(format!("d{i:03}/inner"))).unwrap();
    }
    let runtime = Runtime::configured(settings, dir, config).unwrap();
    wait(&runtime, |s, status| {
        s.generation > 0 && status.watched >= 801
    });
    let before = runtime.handle.status().generation;
    runtime.handle.rescan(None).unwrap();
    wait(&runtime, |s, _| s.generation == before + 1);
    thread::sleep(Duration::from_millis(1500));
    let status = runtime.handle.status();
    assert_eq!(status.generation, before + 1, "scanning caused more scans");
    assert!(status.watch_errors.is_empty(), "{:?}", status.watch_errors);
    assert!(!status.scanning);
}

#[test]
fn excluded_and_non_recursive_folders_are_not_watched() {
    let (_temp, root, dir, config, mut settings) = setup();
    for sub in ["keep/a", "skip/b/c", ".hidden/d"] {
        fs::create_dir_all(root.join(sub)).unwrap();
    }
    settings.index.excluded_folders = vec![RawPath::new(&root.join("skip"))];
    settings.index.exclude_hidden = true;
    let runtime = Runtime::configured(settings, dir, config).unwrap();
    // root, nested (from setup), keep, keep/a
    wait(&runtime, |s, status| {
        s.generation > 0 && status.watched == 4
    });
    let mut next = runtime.handle.settings();
    next.index.roots[0].recursive = false;
    runtime.handle.apply_settings(next).unwrap();
    wait(&runtime, |_, status| status.watched == 1);
}
