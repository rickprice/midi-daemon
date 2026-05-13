mod config;
mod route;
mod timer;
mod lua_api;

use anyhow::Result;
use notify::{Event, EventKind, RecursiveMode, Watcher};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tracing::{error, info};

use config::Config;
use route::Route;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "midi_daemon=info".to_string()),
        )
        .init();

    let config_path = config::default_config_path();
    let config = Config::load(&config_path)?;

    info!("Starting midi-daemon");
    info!("Routes directory: {}", config.routes_dir.display());

    let routes_dir = config.routes_dir.clone();
    let config = Arc::new(config);

    // Map of route name -> Route handle
    let routes: Arc<Mutex<HashMap<String, Route>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // Initial load of all .lua files
    load_all_routes(&routes_dir, Arc::clone(&config), Arc::clone(&routes)).await?;

    // inotify watcher for hot-reload
    let (tx, mut rx) = mpsc::channel::<PathBuf>(32);
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
        if let Ok(event) = res {
            match event.kind {
                EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) => {
                    for path in event.paths {
                        if path.extension().map(|e| e == "lua").unwrap_or(false) {
                            let _ = tx.blocking_send(path);
                        }
                    }
                }
                _ => {}
            }
        }
    })?;

    watcher.watch(&routes_dir, RecursiveMode::NonRecursive)?;
    info!("Watching {} for changes", routes_dir.display());

    // Hot-reload loop
    while let Some(path) = rx.recv().await {
        let name = match path.file_stem().and_then(|s| s.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };

        info!("Detected change in {}.lua — reloading", name);

        let mut map = routes.lock().unwrap();
        // Drop old route (closes its MIDI ports and stops its thread)
        map.remove(&name);

        if path.exists() {
            match Route::start(&path, Arc::clone(&config)) {
                Ok(route) => {
                    map.insert(name.clone(), route);
                    info!("Reloaded route: {}", name);
                }
                Err(e) => error!("Failed to load route {}: {}", name, e),
            }
        } else {
            info!("Removed route: {}", name);
        }
    }

    Ok(())
}

async fn load_all_routes(
    dir: &PathBuf,
    config: Arc<Config>,
    routes: Arc<Mutex<HashMap<String, Route>>>,
) -> Result<()> {
    if !dir.exists() {
        std::fs::create_dir_all(dir)?;
        info!("Created routes directory: {}", dir.display());
        return Ok(());
    }

    let mut map = routes.lock().unwrap();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().map(|e| e == "lua").unwrap_or(false) {
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
                .to_string();

            match Route::start(&path, Arc::clone(&config)) {
                Ok(route) => {
                    info!("Loaded route: {}", name);
                    map.insert(name, route);
                }
                Err(e) => error!("Failed to load route {}: {}", name, e),
            }
        }
    }
    Ok(())
}
