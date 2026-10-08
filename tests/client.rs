use file_minnow::{
    client::Session,
    config::ConfigStore,
    index,
    service::{self, Request, Runtime, Server},
    store,
};
use std::{
    fs,
    time::{Duration, Instant},
};
#[test]
fn client_attaches_without_taking_daemon_ownership() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("files");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("data.txt"), b"x").unwrap();
    let dir = store::prepare(&temp.path().join("cache")).unwrap();
    let runtime =
        Runtime::start(vec![root.clone()], dir.clone(), Duration::from_secs(300)).unwrap();
    let server = Server::start(runtime.handle.clone(), &dir).unwrap();
    let session = Session::connect(&[], &dir).unwrap();
    assert!(!session.owns_index());
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        let result = session.backend.search(&Default::default(), None).unwrap();
        if result.total == 2 {
            break;
        }
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(session);
    assert!(!runtime.handle.stopped());
    assert!(
        service::request(&dir, &Request::Status)
            .unwrap()
            .error
            .is_none()
    );
    drop(server);
}
#[test]
fn session_starts_service_and_shuts_down_only_its_owner() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("files");
    fs::create_dir(&root).unwrap();
    let dir = store::prepare(&temp.path().join("cache")).unwrap();
    let owner = Session::connect(&index::normalize_roots(&[root]).unwrap(), &dir).unwrap();
    assert!(owner.owns_index());
    let second = Session::connect(&[], &dir).unwrap();
    assert!(!second.owns_index());
    drop(second);
    assert!(service::request(&dir, &Request::Settings).is_ok());
    drop(owner);
    assert!(!file_minnow::store::socket_path(&dir, "search.sock").exists());
    assert!(ConfigStore::for_data_dir(&dir).path.exists());
}
