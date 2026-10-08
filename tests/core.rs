use file_minnow::{
    index::{self, Entry},
    query::{self, Query, SearchRequest, Sort},
    service, store,
};
use std::{
    collections::BTreeSet,
    fs,
    os::unix::{
        ffi::OsStringExt,
        fs::{PermissionsExt, symlink},
    },
    path::PathBuf,
    sync::atomic::AtomicU64,
    time::{Duration, Instant},
};

fn e(name: &str) -> Entry {
    Entry::new(
        format!("/fixture/{name}").into_bytes(),
        "file".into(),
        1024,
        chrono::NaiveDate::from_ymd_opt(2026, 10, 7)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp(),
    )
}
fn matches(q: &str, name: &str) -> bool {
    Query::parse(q).unwrap().matches(&e(name))
}

#[test]
fn everything_precedence() {
    assert!(matches("apple banana | orange", "apple orange"));
    assert!(!matches("apple banana | orange", "orange"));
    assert!(matches("<apple banana> | orange", "orange"));
    assert!(!matches("!<draft | temp> pdf", "draft.pdf"));
    assert!(matches("!<draft | temp> pdf", "invoice.pdf"));
}
#[test]
fn quotes_protect_operators() {
    assert!(matches("\"a|b !c\"", "a|b !c.txt"));
    assert!(matches("\"file:note\"", "file:note"));
    assert!(matches("path:\"/fixture/hello world\"", "hello world.txt"));
}
#[test]
fn wildcard_whole_basename() {
    assert!(matches("foo*", "foobar.rs"));
    assert!(!matches("foo*", "myfoobar.rs"));
    assert!(matches("*.rs", "main.RS"));
    assert!(matches("a?c", "abc"));
    assert!(!matches("a?c", "abbc"));
}
#[test]
fn modifiers() {
    assert!(matches("ext:rs;toml", "main.RS"));
    assert!(!matches("ext:rs", "main.rss"));
    assert!(matches("case:README", "README.md"));
    assert!(!matches("case:README", "readme.md"));
    assert!(matches("path:/fixture", "x"));
    assert!(matches("regex:\"^a(b|c)\\.rs$\"", "ab.rs"));
}
#[test]
fn size_and_date_boundaries() {
    assert!(matches("size:>=1kb", "x"));
    assert!(!matches("size:>1kb", "x"));
    assert!(matches("size:1..1024", "x"));
    assert!(matches("dm:2026-10-07", "x"));
    assert!(!matches("dm:>2026-10-07", "x"));
    assert!(matches("dm:<2026-10-08", "x"));
}
#[test]
fn bad_queries_fail() {
    for q in [
        "a |",
        "<a",
        "a>",
        "\"x",
        "unknown:x",
        "size:bad",
        "size:9..1",
        "dm:garbage",
        "regex:\"[\"",
        "!",
        "case:",
    ] {
        assert!(Query::parse(q).is_err(), "{q}");
    }
    assert!(Query::parse(&"!".repeat(100)).is_err());
    assert!(Query::parse(&"a".repeat(4097)).is_err());
}

#[test]
fn scan_links_names_exclusions_and_persistence() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("root");
    fs::create_dir_all(root.join("dir")).unwrap();
    let cache = store::prepare(&root.join("cache")).unwrap();
    fs::write(root.join("alpha.rs"), b"abc").unwrap();
    fs::hard_link(root.join("alpha.rs"), root.join("alias.rs")).unwrap();
    symlink("dir", root.join("dir-link")).unwrap();
    symlink("missing", root.join("broken")).unwrap();
    let raw = std::ffi::OsString::from_vec(b"bad-\xff.txt".to_vec());
    fs::write(root.join(&raw), b"raw").unwrap();
    let roots = index::normalize_roots(&[root.clone(), root.join("dir")]).unwrap();
    assert_eq!(roots.len(), 1);
    let s = index::scan(&roots, &cache, 1);
    assert_eq!(s.error_count, 0);
    assert_eq!(s.entries.len(), 7);
    assert!(!s.entries.iter().any(|e| e.path_buf().starts_with(&cache)));
    assert_eq!(s.entries.iter().filter(|e| e.kind == "link").count(), 2);
    let alpha = s.entries.iter().find(|e| e.name() == b"alpha.rs").unwrap();
    let alias = s.entries.iter().find(|e| e.name() == b"alias.rs").unwrap();
    // Hard links stay separate entries under each name.
    assert_ne!(alpha.path(), alias.path());
    let _lock = store::writer_lock(&cache).unwrap();
    store::save(&cache, &s).unwrap();
    let restored = store::load(&cache).unwrap();
    assert_eq!(
        serde_json::to_value(&s).unwrap(),
        serde_json::to_value(&restored).unwrap()
    );
    assert_eq!(s.entries.entries(), restored.entries.entries());
    assert_eq!(
        fs::metadata(cache.join("index.bin"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(&cache).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert!(store::writer_lock(&cache).is_err());
    let q = Query::parse("*.txt").unwrap();
    assert!(
        restored
            .entries
            .entries()
            .iter()
            .any(|e| e.name().contains(&0xff) && q.matches(e))
    );
}
#[test]
fn deterministic_pagination_and_cancellation() {
    let s = index::Snapshot {
        entries: file_minnow::table::Table::from_entries(vec![e("c"), e("a"), e("b")]),
        generation: 4,
        ..Default::default()
    };
    let req = SearchRequest {
        limit: 1,
        offset: 1,
        sort: Sort::Name,
        ..Default::default()
    };
    let r = query::search(&s, &req, None).unwrap();
    assert_eq!(r.entries[0].name(), b"b");
    assert_eq!(r.total, 3);
    assert_eq!(r.generation, 4);
    assert!(query::search(&s, &req, Some((&AtomicU64::new(2), 1))).is_err());
}
#[test]
fn type_sort_and_type_filters() {
    let entry = |path: &str, kind| Entry::new(path.as_bytes().to_vec(), kind, 1, 1);
    let s = index::Snapshot {
        entries: file_minnow::table::Table::from_entries(vec![
            entry("/d", index::Kind::Folder),
            entry("/d/b.SQL", index::Kind::File),
            entry("/d/a.txt", index::Kind::File),
            entry("/d/z.sql", index::Kind::File),
            entry("/d/photos", index::Kind::Folder),
            entry("/d/photos/x.JPG", index::Kind::File),
            entry("/d/.hidden", index::Kind::File),
            entry("/d/notes.md", index::Kind::File),
        ]),
        ..Default::default()
    };
    let names = |query: &str, sort, descending| -> Vec<String> {
        let request = SearchRequest {
            query: query.into(),
            sort,
            descending,
            ..Default::default()
        };
        query::search(&s, &request, None)
            .unwrap()
            .entries
            .iter()
            .map(|e| String::from_utf8_lossy(e.name()).into_owned())
            .collect()
    };
    // Folders, then no extension, then by extension ignoring case, then name.
    assert_eq!(
        names("", Sort::Type, false),
        [
            "d", "photos", ".hidden", "x.JPG", "notes.md", "b.SQL", "z.sql", "a.txt"
        ]
    );
    assert_eq!(names("", Sort::Type, true)[0], "a.txt");
    assert_eq!(names("pic:", Sort::Name, false), ["x.JPG"]);
    assert_eq!(names("doc:", Sort::Name, false), ["a.txt", "notes.md"]);
    assert_eq!(names("t doc:", Sort::Name, false), ["a.txt", "notes.md"]);
    assert_eq!(
        names("o | x doc:", Sort::Name, false),
        ["a.txt", "notes.md"]
    );
}
#[test]
fn subtree_reconciliation_matches_fresh_walk() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("files");
    fs::create_dir_all(root.join("before/sub")).unwrap();
    fs::write(root.join("before/sub/a"), b"a").unwrap();
    let cache = t.path().join("cache");
    let roots = vec![root.clone()];
    let old = index::scan(&roots, &cache, 1);
    fs::rename(root.join("before"), root.join("after")).unwrap();
    fs::write(root.join("after/sub/b"), b"b").unwrap();
    let changed = BTreeSet::from([root.join("before"), root.join("after")]);
    let new = service::reconcile(&old, &changed, &roots, &cache);
    let fresh = index::scan(&roots, &cache, 2);
    assert_eq!(new.entries.entries(), fresh.entries.entries());
}
fn wait_for(rt: &service::Runtime, predicate: impl Fn(&index::Snapshot) -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        let s = rt.state.read().unwrap();
        if !s.status.scanning && predicate(&s.snapshot) {
            return;
        }
        drop(s);
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!(
        "index did not converge: {:?}",
        rt.state.read().unwrap().status
    );
}
#[test]
fn live_create_move_delete_and_restart() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("files");
    fs::create_dir(&root).unwrap();
    let cache = store::prepare(&t.path().join("cache")).unwrap();
    let rt = service::Runtime::start(vec![root.clone()], cache.clone(), Duration::from_secs(60))
        .unwrap();
    wait_for(&rt, |s| s.generation > 0);
    fs::write(root.join("first.rs"), b"one").unwrap();
    wait_for(&rt, |s| s.entries.iter().any(|e| e.name() == b"first.rs"));
    fs::rename(root.join("first.rs"), root.join("second.rs")).unwrap();
    wait_for(&rt, |s| {
        s.entries.iter().any(|e| e.name() == b"second.rs")
            && !s.entries.iter().any(|e| e.name() == b"first.rs")
    });
    let outside = t.path().join("incoming");
    fs::create_dir_all(outside.join("sub")).unwrap();
    fs::write(outside.join("sub/deep.txt"), b"data").unwrap();
    fs::rename(outside, root.join("incoming")).unwrap();
    wait_for(&rt, |s| s.entries.iter().any(|e| e.name() == b"deep.txt"));
    fs::remove_dir_all(root.join("incoming")).unwrap();
    wait_for(&rt, |s| !s.entries.iter().any(|e| e.name() == b"deep.txt"));
    let prior_generation = rt.state.read().unwrap().snapshot.generation;
    rt.rescan();
    wait_for(&rt, |s| s.generation > prior_generation);
    drop(rt);
    fs::remove_file(root.join("second.rs")).unwrap();
    fs::write(root.join("offline.txt"), b"offline change").unwrap();
    let rt = service::Runtime::start(vec![root], cache, Duration::from_secs(60)).unwrap();
    wait_for(&rt, |s| {
        s.entries.iter().any(|e| e.name() == b"offline.txt")
            && !s.entries.iter().any(|e| e.name() == b"second.rs")
    });
}
#[test]
fn damaged_cache_preserved_and_rebuilt() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("root");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("ok.txt"), b"ok").unwrap();
    let cache = store::prepare(&t.path().join("cache")).unwrap();
    fs::write(cache.join("index.bin"), b"this is not an index").unwrap();
    let rt = service::Runtime::start(vec![root], cache.clone(), Duration::from_secs(60)).unwrap();
    wait_for(&rt, |s| s.entries.iter().any(|e| e.name() == b"ok.txt"));
    drop(rt);
    assert_eq!(store::load(&cache).unwrap().entries.len(), 2);
    assert!(fs::read_dir(&cache).unwrap().any(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .contains("corrupt-")
    }));
}
#[test]
fn root_updates_are_persisted() {
    let t = tempfile::tempdir().unwrap();
    let a = t.path().join("a");
    let b = t.path().join("b");
    fs::create_dir(&a).unwrap();
    fs::create_dir(&b).unwrap();
    fs::write(b.join("added.txt"), b"ok").unwrap();
    let cache = store::prepare(&t.path().join("cache")).unwrap();
    let rt =
        service::Runtime::start(vec![a.clone()], cache.clone(), Duration::from_secs(60)).unwrap();
    wait_for(&rt, |s| s.generation > 0);
    rt.set_roots(vec![a, b]);
    wait_for(&rt, |s| s.entries.iter().any(|e| e.name() == b"added.txt"));
    drop(rt);
    assert_eq!(store::load(&cache).unwrap().roots.len(), 2);
}
#[test]
fn relative_root_and_pseudo_filesystems() {
    assert!(index::normalize_roots(&[PathBuf::from("/proc")]).is_err());
    assert!(index::normalize_roots(&[]).is_err());
}
