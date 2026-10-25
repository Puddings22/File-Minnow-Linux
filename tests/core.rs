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
    let mut restored = store::load(&cache).unwrap();
    // The saved copy records when it was saved; the rest round-trips.
    assert!(restored.saved_at >= s.scanned_at);
    let saved_at = std::mem::take(&mut restored.saved_at);
    assert_eq!(
        serde_json::to_value(&s).unwrap(),
        serde_json::to_value(&restored).unwrap()
    );
    restored.saved_at = saved_at;
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
    let r = query::search(&s, &req, None, None).unwrap();
    assert_eq!(r.entries[0].name(), b"b");
    assert_eq!(r.total, 3);
    assert_eq!(r.generation, 4);
    assert!(query::search(&s, &req, Some((&AtomicU64::new(2), 1)), None).is_err());
}
#[test]
fn run_count_sort_and_page_counts() {
    let s = index::Snapshot {
        entries: file_minnow::table::Table::from_entries(vec![e("c"), e("a"), e("b")]),
        generation: 1,
        ..Default::default()
    };
    let temp = tempfile::tempdir().unwrap();
    let counts = file_minnow::run_counts::RunCounts::load(temp.path().join("run_counts.json"));
    for _ in 0..3 {
        counts.record(std::path::Path::new("/fixture/c"));
    }
    counts.record(std::path::Path::new("/fixture/a"));
    let req = SearchRequest {
        sort: Sort::RunCount,
        descending: true,
        limit: 10,
        ..Default::default()
    };
    let r = query::search(&s, &req, None, Some(&counts)).unwrap();
    assert_eq!(r.total, 3);
    // Most-used first; the never-used row keeps path (row) order.
    let names: Vec<Vec<u8>> = r.entries.iter().map(|x| x.name().to_vec()).collect();
    assert_eq!(names, [b"c".to_vec(), b"a".to_vec(), b"b".to_vec()]);
    assert_eq!(r.counts, vec![3, 1, 0]);
    // Ascending: the never-used row comes first.
    let req = SearchRequest {
        sort: Sort::RunCount,
        descending: false,
        limit: 10,
        ..Default::default()
    };
    let r = query::search(&s, &req, None, Some(&counts)).unwrap();
    assert_eq!(r.counts, vec![0, 1, 3]);
    // Without a counter store the page carries no counts.
    let r = query::search(&s, &req, None, None).unwrap();
    assert!(r.counts.is_empty());
}
#[test]
fn everything_style_functions_and_folding() {
    let entry = |path: &str, kind, size, created| {
        Entry::new(path.as_bytes().to_vec(), kind, size, 1_700_000_000).created(created)
    };
    let (f, d) = (index::Kind::File, index::Kind::Folder);
    let none = file_minnow::table::NO_DATE;
    let s = index::Snapshot {
        entries: file_minnow::table::Table::from_entries(vec![
            entry("/home", d, 0, none),
            entry("/home/Documente", d, 0, none),
            entry("/home/Documente/Știință.pdf", f, 3000, 1_600_000_000),
            entry("/home/Documente/raport.pdf", f, 500, 1_650_000_000),
            entry("/home/Documente/Rapoarte", d, 0, none),
            entry("/home/Documente/Rapoarte/raport.pdf", f, 700, 1_700_000_000),
            entry("/home/Documente/gol", d, 0, none),
            entry("/home/Poze", d, 0, none),
            entry("/home/Poze/École.jpg", f, 90_000, none),
            entry("/home/Poze/report-final.txt", f, 10, none),
            entry("/home/report.txt", f, 1, none),
        ]),
        ..Default::default()
    };
    let names = |query: &str, sort: Sort, descending: bool| -> Vec<String> {
        let request = SearchRequest {
            query: query.into(),
            sort,
            descending,
            ..Default::default()
        };
        query::search(&s, &request, None, None)
            .unwrap()
            .entries
            .iter()
            .map(|e| String::from_utf8_lossy(e.name()).into_owned())
            .collect()
    };
    // Accents and case are ignored both ways.
    assert_eq!(names("stiinta", Sort::Name, false), ["Știință.pdf"]);
    assert_eq!(names("ȘTIINȚĂ", Sort::Name, false), ["Știință.pdf"]);
    assert_eq!(names("ecole", Sort::Name, false), ["École.jpg"]);
    assert_eq!(names("path:poze ecole", Sort::Name, false), ["École.jpg"]);
    assert!(names("case:stiinta", Sort::Name, false).is_empty());
    // Duplicates, empty folders, parent and child.
    assert_eq!(
        names("dupe:", Sort::Path, false),
        ["raport.pdf", "raport.pdf"]
    );
    assert_eq!(names("empty:", Sort::Name, false), ["gol"]);
    // Names sort naturally: case and accents do not change the order.
    assert_eq!(
        names("parent:/home/Documente/", Sort::Name, false),
        ["gol", "Rapoarte", "raport.pdf", "Știință.pdf"]
    );
    assert_eq!(
        names("child:raport.pdf", Sort::Name, false),
        ["Documente", "Rapoarte"]
    );
    assert_eq!(names("child:RAPOARTE", Sort::Name, false), ["Documente"]);
    // Whole words: "report" but not "report-final"? A dash is a boundary, so
    // both match; "repor" matches nothing as a whole word.
    assert_eq!(
        names("ww:report", Sort::Name, false),
        ["report-final.txt", "report.txt"]
    );
    assert!(names("ww:repor", Sort::Name, false).is_empty());
    assert_eq!(names("wholeword:gol", Sort::Name, false), ["gol"]);
    // Folder sizes come from the files inside; size: applies to folders too.
    assert_eq!(
        names("folder: size:>=3500", Sort::Size, true),
        ["home", "Poze", "Documente"]
    );
    let documents = query::search(
        &s,
        &SearchRequest {
            query: "path:/home/Documente folder:".into(),
            ..Default::default()
        },
        None,
        None,
    )
    .unwrap();
    let size_of = |name: &str| {
        documents
            .entries
            .iter()
            .find(|e| e.name() == name.as_bytes())
            .unwrap()
            .size
    };
    assert_eq!(
        (size_of("Documente"), size_of("Rapoarte"), size_of("gol")),
        (4200, 700, 0)
    );
    // Creation dates: unknown ones never match and sort first ascending.
    assert_eq!(
        names("dc:<2023-01-01", Sort::Created, false),
        ["Știință.pdf", "raport.pdf"]
    );
    assert_eq!(
        names("dc:>=2000-01-01", Sort::Created, true)[0],
        "raport.pdf"
    );
    assert_eq!(names("dc:>=2000-01-01", Sort::Created, true).len(), 3);
}
#[test]
fn content_search_reads_only_candidate_files() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().join("files");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("notes.txt"), "The Quarterly Report is late").unwrap();
    fs::write(dir.join("other.txt"), "nothing here").unwrap();
    fs::write(dir.join("report.md"), "no keyword").unwrap();
    // The cache directory is never indexed, so it lives beside the files.
    let s = index::scan(std::slice::from_ref(&dir), &t.path().join("cache"), 1);
    assert_eq!(s.entries.len(), 4, "folder and three files");
    let names = |query: &str| -> Vec<String> {
        let request = SearchRequest {
            query: query.into(),
            ..Default::default()
        };
        query::search(&s, &request, None, None)
            .unwrap()
            .entries
            .iter()
            .map(|e| String::from_utf8_lossy(e.name()).into_owned())
            .collect()
    };
    assert_eq!(names("content:quarterly"), ["notes.txt"]);
    assert!(names("case:content:quarterly").is_empty());
    assert_eq!(names("ext:txt content:report"), ["notes.txt"]);
    assert!(names("report content:keyword !md").is_empty());
    // OR binds tighter: report AND (content:keyword OR notes).
    assert_eq!(names("report content:keyword | notes"), ["report.md"]);
    assert_eq!(
        names("<report content:keyword> | notes"),
        ["notes.txt", "report.md"]
    );
    assert!(Query::parse("content:").is_err());
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
        query::search(&s, &request, None, None)
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

#[test]
fn startup_catch_up_reads_only_changed_folders() {
    use std::time::{Duration, SystemTime};
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("files");
    let dir = temp.path().join("cache");
    fs::create_dir_all(&dir).unwrap();
    for folder in ["a", "b/c", "quiet"] {
        fs::create_dir_all(root.join(folder)).unwrap();
    }
    for (file, text) in [
        ("a/1.txt", "1"),
        ("b/2.txt", "22"),
        ("b/c/3.txt", "333"),
        ("quiet/4.txt", "4"),
    ] {
        fs::write(root.join(file), text).unwrap();
    }
    let set_time = |path: &std::path::Path, time: SystemTime| {
        fs::File::open(path).unwrap().set_modified(time).unwrap();
    };
    let long_ago = SystemTime::now() - Duration::from_secs(100);
    for folder in ["", "a", "b", "b/c", "quiet"] {
        set_time(&root.join(folder), long_ago);
    }
    let config = file_minnow::config::IndexConfig::from_roots(std::slice::from_ref(&root));
    let mut saved = index::scan_config(&config, &dir, 1, None, &|| false, &|_| {})
        .unwrap()
        .unwrap();
    saved.saved_at = index::now() - 50;
    // While "not running": a file added, one deleted, a new folder, and a
    // file changed in place (its folder's time does not change).
    fs::write(root.join("b/new.txt"), "n").unwrap();
    fs::remove_file(root.join("a/1.txt")).unwrap();
    fs::create_dir_all(root.join("d/e")).unwrap();
    fs::write(root.join("d/e/deep.txt"), "d").unwrap();
    fs::write(root.join("b/c/3.txt"), "3333333").unwrap();
    set_time(&root.join("b/c"), long_ago);
    let quick = index::quick_scan(&config, &dir, &saved, &|| false, &|_| {}, None)
        .unwrap()
        .unwrap();
    let full = index::scan_config(&config, &dir, 2, None, &|| false, &|_| {})
        .unwrap()
        .unwrap();
    let listing = |s: &index::Snapshot| -> Vec<(String, u64)> {
        s.entries
            .entries()
            .iter()
            .filter(|e| e.kind == index::Kind::File)
            .map(|e| (e.display_path(), e.size))
            .collect()
    };
    let mut expected = listing(&full);
    for (path, size) in &mut expected {
        if path.ends_with("3.txt") {
            *size = 3; // kept from the saved index until the next full walk
        }
    }
    assert_eq!(listing(&quick), expected);
    let paths = |s: &index::Snapshot| -> Vec<Vec<u8>> {
        s.entries
            .entries()
            .iter()
            .map(|e| e.path.to_vec())
            .collect()
    };
    assert_eq!(
        paths(&quick),
        paths(&full),
        "same entries in the same order"
    );
    assert_eq!(quick.config, config.fingerprint());
    assert_eq!(quick.generation, 2);
    // A changed setting is not a catch-up: the fingerprint differs.
    let mut other = config.clone();
    other.exclude_hidden = !other.exclude_hidden;
    assert_ne!(other.fingerprint(), config.fingerprint());
    let mut scheduled = config.clone();
    scheduled.roots[0].schedule = file_minnow::config::Schedule::Never;
    assert_eq!(scheduled.fingerprint(), config.fingerprint());
}

#[test]
fn duplicates_by_name_size_and_contents() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("files");
    let cache = store::prepare(&temp.path().join("cache")).unwrap();
    let big = |tail: u8| {
        let mut data = vec![7_u8; 100_000];
        data[99_999] = tail;
        data
    };
    for (path, data) in [
        ("a/x.bin", big(1)),
        ("b/y.bin", big(1)),
        // Same size and the same first 64 KiB, different end.
        ("c/z.bin", big(2)),
        ("d/x.bin", vec![1, 2, 3]),
        ("e/empty.txt", vec![]),
        ("f/empty.txt", vec![]),
    ] {
        let file = root.join(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, data).unwrap();
    }
    let s = index::scan(std::slice::from_ref(&root), &cache, 1);
    let found = |query: &str| -> Vec<String> {
        let request = SearchRequest {
            query: query.into(),
            sort: Sort::Path,
            ..Default::default()
        };
        query::search(&s, &request, None, None)
            .unwrap()
            .entries
            .iter()
            .map(|e| String::from_utf8_lossy(&e.path[root.as_os_str().len() + 1..]).into_owned())
            .collect()
    };
    assert_eq!(found("dupe:content"), ["a/x.bin", "b/y.bin"]);
    assert_eq!(found("dupe:size"), ["a/x.bin", "b/y.bin", "c/z.bin"]);
    assert_eq!(
        found("dupe:name file:"),
        ["a/x.bin", "d/x.bin", "e/empty.txt", "f/empty.txt"]
    );
    assert!(
        found("dupe:name;size").is_empty(),
        "empty files have no size to compare"
    );
    // Duplicates among the other terms' results only.
    assert_eq!(found("dupe:size !b/"), ["a/x.bin", "c/z.bin"]);
    assert!(found("dupe:content path:a/").is_empty());
    assert!(Query::parse("dupe:colour").is_err());
}

#[test]
fn file_lists_are_indexed_under_a_folder() {
    use file_minnow::config::{IndexConfig, RootConfig};
    let temp = tempfile::tempdir().unwrap();
    let cache = store::prepare(&temp.path().join("cache")).unwrap();
    let list = temp.path().join("nas.efu");
    fs::write(
        &list,
        "Filename,Size,Date Modified,Date Created,Attributes\n\
         \"\\\\nas\\share\\Photos\\\",,,,16\n\
         \"\\\\nas\\share\\Photos\\beach.jpg\",2048,132537600000000000,,32\n\
         \"\\\\nas\\share\\notes.tmp\",1,,,32\n\
         \"\\\\other\\x.txt\",1,,,32\n",
    )
    .unwrap();
    let shown = temp.path().join("nas");
    let config = IndexConfig {
        roots: vec![RootConfig::for_list(&list, "\\\\nas\\share", &shown)],
        ..IndexConfig::default()
    };
    config.validate().unwrap();
    let scan = |previous: Option<&file_minnow::table::Table>| {
        index::scan_config_watched(&config, &cache, 1, None, previous, &|| false, &|_| {}, None)
            .unwrap()
            .unwrap()
    };
    let names = |s: &index::Snapshot| -> Vec<(String, u64)> {
        s.entries
            .entries()
            .iter()
            .map(|e| (e.display_path(), e.size))
            .collect()
    };
    let first = scan(None);
    let root = shown.display().to_string();
    // The default excluded file types apply (notes.tmp), other servers do not belong.
    assert_eq!(
        names(&first),
        [
            (root.clone(), 2048),
            (format!("{root}/Photos"), 2048),
            (format!("{root}/Photos/beach.jpg"), 2048)
        ]
    );
    assert_eq!(first.entries.entry(2).modified, 1_609_286_400);
    // An unchanged list keeps its entries; a changed one is read again.
    let again = scan(Some(&first.entries));
    assert_eq!(names(&again), names(&first));
    fs::write(
        &list,
        "Filename,Size\n\"\\\\nas\\share\\Photos\\sunset.jpg\",10\n",
    )
    .unwrap();
    let changed = scan(Some(&again.entries));
    assert_eq!(
        names(&changed),
        [
            (root.clone(), 10),
            (format!("{root}/Photos/sunset.jpg"), 10)
        ]
    );
    // Searchable like any folder.
    let found = query::search(
        &changed,
        &SearchRequest {
            query: "sunset".into(),
            ..Default::default()
        },
        None,
        None,
    )
    .unwrap();
    assert_eq!(found.total, 1);
}
