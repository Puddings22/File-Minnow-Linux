use file_minnow::{
    query::SearchRequest,
    service::{self, Request},
    store,
};
use std::{
    fs,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[test]
fn cli_and_daemon_end_to_end() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("files");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("invoice.pdf"), b"invoice").unwrap();
    fs::write(root.join("notes.txt"), b"notes").unwrap();
    let cache = store::prepare(&t.path().join("cache")).unwrap();
    let bin = env!("CARGO_BIN_EXE_file-minnow");
    let output = Command::new(bin)
        .args(["--data-dir"])
        .arg(&cache)
        .args(["index", "--root"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = Command::new(bin)
        .arg("--data-dir")
        .arg(&cache)
        .args(["search", "ext:pdf", "--null"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stdout.ends_with(b"invoice.pdf\0"));
    let _daemon = Daemon(
        Command::new(bin)
            .arg("--data-dir")
            .arg(&cache)
            .args(["daemon", "--root"])
            .arg(&root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let start = Instant::now();
    loop {
        if let Ok(r) = service::request(&cache, &Request::Status)
            && r.status.generation > 0
            && !r.status.scanning
        {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(30));
    }
    let response = service::request(
        &cache,
        &Request::Search(SearchRequest {
            query: "ext:pdf".into(),
            ..Default::default()
        }),
    )
    .unwrap();
    assert!(response.error.is_none());
    assert_eq!(response.search.unwrap().total, 1);
    let response = service::request(
        &cache,
        &Request::Search(SearchRequest {
            query: "bad:value".into(),
            ..Default::default()
        }),
    )
    .unwrap();
    assert!(response.error.is_some());
    let second = Command::new(bin)
        .arg("--data-dir")
        .arg(&cache)
        .args(["index", "--root"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(!second.status.success());
    assert!(
        service::request(&cache, &Request::Rescan)
            .unwrap()
            .error
            .is_none()
    );
}
