use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use file_minnow::{
    config, index,
    query::{self, SearchRequest, Sort},
    service::{self, Request, Runtime},
    store,
};
use std::{io::Write, path::PathBuf};

#[derive(Parser)]
#[command(version, about = "Fast native Linux filename search")]
struct Args {
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}
#[derive(Subcommand)]
enum Command {
    /// Scan selected folders and atomically save an index
    Index {
        #[arg(long, required = true)]
        root: Vec<PathBuf>,
    },
    /// Search the saved index, or a running daemon with --live
    Search {
        #[arg(default_value = "")]
        query: String,
        #[arg(long, default_value_t = 100)]
        limit: usize,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long,value_enum,default_value_t=Sort::Name)]
        sort: Sort,
        #[arg(long)]
        descending: bool,
        #[arg(long)]
        json: bool,
        #[arg(long, conflicts_with = "json")]
        null: bool,
        #[arg(long)]
        live: bool,
    },
    /// Maintain a live index and serve a local Unix socket
    Daemon {
        #[arg(long)]
        root: Vec<PathBuf>,
        #[arg(long,value_parser=clap::value_parser!(u64).range(1..))]
        rescan_seconds: Option<u64>,
    },
    /// Show saved-index or daemon status
    Status {
        #[arg(long)]
        live: bool,
    },
    /// Ask the daemon to reconcile all folders
    Rescan,
    /// Pause background indexing (search remains available)
    Pause,
    /// Resume indexing and reconcile changes
    Resume,
    /// Stop the background index service
    Stop,
    /// Read preferences or apply a validated JSON preferences file
    Config {
        #[arg(long)]
        import: Option<PathBuf>,
        #[arg(long)]
        live: bool,
    },
    /// Open the native desktop interface (default command)
    Gui {
        #[arg(long)]
        root: Vec<PathBuf>,
        #[arg(long)]
        hidden: bool,
        #[arg(long)]
        search: Option<String>,
    },
    /// Show the existing window, or start it
    Show { query: Option<String> },
    /// Toggle the existing window, or start it
    Toggle,
    /// Quit the desktop app (leaves a separately started daemon running)
    Quit,
    /// Report desktop integration state
    WindowStatus,
    /// Print where this data directory's index, settings and sockets are
    Paths,
}
fn main() -> Result<()> {
    let args = Args::parse();
    let dir = store::prepare(&args.data_dir.unwrap_or_else(store::default_dir))?;
    match args.command.unwrap_or(Command::Gui {
        root: Vec::new(),
        hidden: false,
        search: None,
    }) {
        Command::Index { root } => {
            let (preferences, settings) = config::open_for_roots(&root, &dir)?;
            let _lock = store::writer_lock(&dir)?;
            let settings = preferences.save(settings.revision, settings)?;
            let generation = match store::load(&dir) {
                Ok(s) => s.generation + 1,
                Err(e) => {
                    eprintln!("Preserving unreadable index before rebuild: {e}");
                    store::quarantine(&dir)?;
                    1
                }
            };
            let s =
                index::scan_config(&settings.index, &dir, generation, None, &|| false, &|_| {})?
                    .expect("uncancellable scan");
            store::save(&dir, &s)?;
            println!(
                "Indexed {} entries, {} traversal errors. Saved to {}",
                s.entries.len(),
                s.error_count,
                dir.display()
            );
            for e in s.errors {
                eprintln!("{e}");
            }
        }
        Command::Search {
            query,
            limit,
            offset,
            sort,
            descending,
            json,
            null,
            live,
        } => {
            let req = SearchRequest {
                query,
                limit,
                offset,
                sort,
                descending,
            };
            let response = if live {
                let r = service::request(&dir, &Request::Search(req))?;
                if let Some(e) = r.error {
                    bail!(e);
                }
                r.search
                    .ok_or_else(|| anyhow::anyhow!("Missing response"))?
            } else {
                let s = store::load(&dir)?;
                if s.generation == 0 {
                    bail!("No index yet. Run: file-minnow index --root /your/folder");
                }
                query::search(&s, &req, None)?
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&response)?);
            } else {
                let stdout = std::io::stdout();
                let mut out = stdout.lock();
                for e in &response.entries {
                    out.write_all(&e.path)?;
                    out.write_all(if null { b"\0" } else { b"\n" })?;
                }
                eprintln!(
                    "{} matches · {:.2} ms · generation {}{}",
                    response.total,
                    response.elapsed_ms,
                    response.generation,
                    if live {
                        ""
                    } else {
                        " · saved snapshot (use --live for daemon)"
                    }
                );
            }
        }
        Command::Daemon {
            root,
            rescan_seconds,
        } => {
            let (preferences, mut settings) = config::open_for_roots(&root, &dir)?;
            if let Some(seconds) = rescan_seconds {
                for root in &mut settings.index.roots {
                    root.schedule = config::Schedule::Interval { seconds };
                }
            }
            let runtime = Runtime::configured(settings, dir.clone(), preferences)?;
            service::serve(&runtime, &dir)?;
        }
        Command::Status { live } => {
            if live {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &service::request(&dir, &Request::Status)?.status
                    )?
                );
            } else {
                let s = store::load(&dir)?;
                println!(
                    "{} entries · generation {} · scanned {} · {} traversal errors",
                    s.entries.len(),
                    s.generation,
                    s.scanned_at,
                    s.error_count
                );
                for root in index::root_paths(&s.roots) {
                    println!("{}", root.display());
                }
            }
        }
        Command::Rescan => {
            let r = service::request(&dir, &Request::Rescan)?;
            if let Some(e) = r.error {
                bail!(e);
            }
            println!("Rescan requested");
        }
        command @ (Command::Pause | Command::Resume | Command::Stop) => {
            let request = match command {
                Command::Pause => Request::Pause { paused: true },
                Command::Resume => Request::Pause { paused: false },
                _ => Request::Shutdown,
            };
            let response = service::request(&dir, &request)?;
            if let Some(error) = response.error {
                bail!(error);
            }
            println!("Command accepted");
        }
        Command::Config { import, live } => {
            if live {
                let request = if let Some(path) = import {
                    Request::SetSettings {
                        settings: Box::new(serde_json::from_slice(&std::fs::read(path)?)?),
                    }
                } else {
                    Request::Settings
                };
                let response = service::request(&dir, &request)?;
                if let Some(error) = response.error {
                    bail!(error);
                }
                println!("{}", serde_json::to_string_pretty(&response.settings)?);
            } else {
                let preferences = config::ConfigStore::for_data_dir(&dir);
                let settings = if let Some(path) = import {
                    let _lock = store::writer_lock(&dir)?;
                    let settings: config::Settings = serde_json::from_slice(&std::fs::read(path)?)?;
                    preferences.save(settings.revision, settings)?
                } else {
                    preferences.load()?
                };
                println!("{}", serde_json::to_string_pretty(&settings)?);
            }
        }
        Command::Show { query } => {
            #[cfg(feature = "gui")]
            file_minnow::ui::run_with_options(
                Vec::new(),
                dir,
                file_minnow::ui::LaunchOptions {
                    query,
                    ..Default::default()
                },
            )?;
            #[cfg(not(feature = "gui"))]
            {
                let _ = query;
                bail!("Built without GUI support");
            }
        }
        Command::Toggle => {
            #[cfg(feature = "gui")]
            file_minnow::ui::run_with_options(
                Vec::new(),
                dir,
                file_minnow::ui::LaunchOptions {
                    toggle: true,
                    ..Default::default()
                },
            )?;
            #[cfg(not(feature = "gui"))]
            bail!("Built without GUI support");
        }
        Command::Paths => {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "data_dir": dir,
                    "index": dir.join("index.bin"),
                    "settings": config::ConfigStore::for_data_dir(&dir).path,
                    "search_socket": store::socket_path(&dir, "search.sock"),
                    "ui_socket": store::socket_path(&dir, "ui.sock"),
                }))?
            );
        }
        command @ (Command::Quit | Command::WindowStatus) => {
            #[cfg(feature = "gui")]
            {
                let action = if matches!(command, Command::Quit) {
                    file_minnow::desktop::WindowAction::Quit
                } else {
                    file_minnow::desktop::WindowAction::Status
                };
                let state = file_minnow::desktop::send_window(&dir, &action)?;
                println!("{}", serde_json::to_string_pretty(&state)?);
            }
            #[cfg(not(feature = "gui"))]
            {
                let _ = command;
                bail!("Built without GUI support");
            }
        }
        Command::Gui {
            root,
            hidden,
            search,
        } => {
            #[cfg(feature = "gui")]
            {
                file_minnow::ui::run_with_options(
                    root,
                    dir,
                    file_minnow::ui::LaunchOptions {
                        query: search,
                        hidden,
                        ..Default::default()
                    },
                )?;
            }
            #[cfg(not(feature = "gui"))]
            {
                let _ = (root, hidden, search);
                bail!("Built without GUI support; rebuild with default features");
            }
        }
    }
    Ok(())
}
