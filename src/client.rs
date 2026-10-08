//! A GUI may own the index or attach to an existing daemon without copying its database.
use crate::{
    config::{self, Settings},
    query::{self, SearchRequest, SearchResponse},
    runtime::{Handle, Runtime, Status},
    service::{self, Request, Response, Server},
};
use anyhow::{Result, bail};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};

#[derive(Clone)]
pub enum Backend {
    Local(Handle),
    Remote {
        dir: PathBuf,
        view: Arc<RwLock<(Status, Settings)>>,
    },
}
impl Backend {
    fn accept(&self, response: Response) -> Result<Response> {
        if let Self::Remote { view, .. } = self {
            let mut view = view.write().unwrap();
            view.0 = response.status.clone();
            if let Some(settings) = &response.settings {
                view.1 = settings.clone();
            }
        }
        if let Some(error) = &response.error {
            bail!(error.clone());
        }
        Ok(response)
    }
    fn remote(&self, request: Request) -> Result<Response> {
        let Self::Remote { dir, .. } = self else {
            unreachable!()
        };
        self.accept(service::request(dir, &request)?)
    }
    pub fn status(&self) -> Status {
        match self {
            Self::Local(h) => h.status(),
            Self::Remote { view, .. } => view.read().unwrap().0.clone(),
        }
    }
    pub fn settings(&self) -> Settings {
        match self {
            Self::Local(h) => h.settings(),
            Self::Remote { view, .. } => view.read().unwrap().1.clone(),
        }
    }
    pub fn search(
        &self,
        request: &SearchRequest,
        cancel: Option<(&AtomicU64, u64)>,
    ) -> Result<SearchResponse> {
        match self {
            Self::Local(h) => {
                let snapshot = h.state.read().unwrap().snapshot.clone();
                query::search(&snapshot, request, cancel)
            }
            Self::Remote { .. } => {
                if cancel.is_some_and(|(v, id)| v.load(Ordering::Relaxed) != id) {
                    bail!("Query cancelled");
                }
                self.remote(Request::Search(request.clone()))?
                    .search
                    .ok_or_else(|| anyhow::anyhow!("Service returned no results"))
            }
        }
    }
    pub fn apply_settings(&self, settings: Settings) -> Result<Settings> {
        match self {
            Self::Local(h) => h.apply_settings(settings),
            Self::Remote { .. } => self
                .remote(Request::SetSettings {
                    settings: Box::new(settings),
                })?
                .settings
                .ok_or_else(|| anyhow::anyhow!("Service returned no settings")),
        }
    }
    pub fn rescan(&self) -> Result<()> {
        match self {
            Self::Local(h) => h.rescan(None),
            Self::Remote { .. } => self.remote(Request::Rescan).map(|_| ()),
        }
    }
    pub fn pause(&self, value: bool) -> Result<()> {
        match self {
            Self::Local(h) => h.pause(value),
            Self::Remote { .. } => self.remote(Request::Pause { paused: value }).map(|_| ()),
        }
    }
    pub fn on_change(&self, callback: impl Fn() + Send + Sync + 'static) -> Subscription {
        let stop = Arc::new(AtomicBool::new(false));
        let ending = stop.clone();
        let backend = self.clone();
        match self {
            Self::Local(h) => h.on_change(move || {
                if !ending.load(Ordering::Relaxed) {
                    callback();
                }
            }),
            Self::Remote { .. } => {
                thread::spawn(move || {
                    while !ending.load(Ordering::Relaxed) {
                        let result = backend.remote(Request::Watch {
                            after: backend.status().revision,
                        });
                        if ending.load(Ordering::Relaxed) {
                            break;
                        }
                        if result.is_ok() {
                            let _ = backend.remote(Request::Settings);
                        } else if let Self::Remote { view, .. } = &backend {
                            view.write().unwrap().0.message =
                                "Index service disconnected; reconnecting…".into();
                        }
                        callback();
                        if result.is_err() {
                            thread::sleep(Duration::from_millis(500));
                        }
                    }
                });
            }
        }
        Subscription {
            stop,
            remote: match self {
                Self::Remote { dir, .. } => Some(dir.clone()),
                _ => None,
            },
        }
    }
}
pub struct Subscription {
    stop: Arc<AtomicBool>,
    remote: Option<PathBuf>,
}
impl Drop for Subscription {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(dir) = self.remote.take() {
            thread::spawn(move || {
                let _ = service::request(&dir, &Request::Pulse);
            });
        }
    }
}
pub struct Session {
    pub backend: Backend,
    server: Option<Server>,
    runtime: Option<Runtime>,
}
impl Session {
    pub fn connect(roots: &[PathBuf], dir: &Path) -> Result<Self> {
        if crate::store::socket_path(dir, "search.sock").exists()
            && let Ok(response) = service::request(dir, &Request::Settings)
        {
            if let Some(error) = response.error {
                bail!(error);
            }
            let settings = response
                .settings
                .ok_or_else(|| anyhow::anyhow!("Service does not support settings"))?;
            let backend = Backend::Remote {
                dir: dir.to_path_buf(),
                view: Arc::new(RwLock::new((response.status, settings.clone()))),
            };
            if !roots.is_empty() {
                let mut next = settings;
                let roots = crate::index::normalize_roots(roots)?;
                next.index.roots = roots
                    .iter()
                    .map(|p| {
                        next.index
                            .roots
                            .iter()
                            .find(|r| r.path.path() == *p)
                            .cloned()
                            .unwrap_or_else(|| config::RootConfig::new(p))
                    })
                    .collect();
                backend.apply_settings(next)?;
            }
            return Ok(Self {
                backend,
                server: None,
                runtime: None,
            });
        }
        let (config, settings) = config::open_for_roots(roots, dir)?;
        let runtime = Runtime::configured(settings, dir.to_path_buf(), config)?;
        let server = Server::start(runtime.handle.clone(), dir)?;
        Ok(Self {
            backend: Backend::Local(runtime.handle.clone()),
            server: Some(server),
            runtime: Some(runtime),
        })
    }
    pub fn owns_index(&self) -> bool {
        self.runtime.is_some()
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        if let Some(runtime) = &self.runtime {
            runtime.handle.shutdown();
        }
        self.server.take();
        self.runtime.take();
    }
}
