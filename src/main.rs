mod alsa_connect;
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
use tracing::{error, info, warn};

use alsa_connect::ConnectionManager;
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

    let config = Config::find_and_load()?;

    info!("Starting midi-daemon");
    info!("Routes directory: {}", config.routes_dir.display());

    let routes_dir = config.routes_dir.clone();
    let mut config = Arc::new(config);

    // Map of route name -> Route handle
    let routes: Arc<Mutex<HashMap<String, Route>>> =
        Arc::new(Mutex::new(HashMap::new()));

    let conn_mgr = Arc::new(ConnectionManager::new());
    Arc::clone(&conn_mgr).spawn_watcher();

    // Initial load of all .lua files
    load_all_routes(&routes_dir, Arc::clone(&config), Arc::clone(&routes), Arc::clone(&conn_mgr)).await?;

    // inotify watcher for hot-reload
    enum WatchEvent {
        RouteChanged(PathBuf),
        ConfigChanged,
    }

    let (tx, mut rx) = mpsc::channel::<WatchEvent>(32);

    let config_path = config.config_path.clone();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
        if let Ok(event) = res {
            match event.kind {
                EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) => {
                    for path in event.paths {
                        if path.extension().map(|e| e == "lua").unwrap_or(false) {
                            let _ = tx.blocking_send(WatchEvent::RouteChanged(path));
                        } else if config_path.as_deref() == Some(path.as_path()) {
                            let _ = tx.blocking_send(WatchEvent::ConfigChanged);
                        }
                    }
                }
                _ => {}
            }
        }
    })?;

    watcher.watch(&routes_dir, RecursiveMode::NonRecursive)?;
    info!("Watching {} for changes", routes_dir.display());

    if let Some(ref cfg_path) = config.config_path {
        let cfg_dir = cfg_path.parent().unwrap_or(cfg_path.as_path());
        if cfg_dir != routes_dir.as_path() {
            watcher.watch(cfg_dir, RecursiveMode::NonRecursive)?;
            info!("Watching {} for changes", cfg_dir.display());
        }
    }

    // Hot-reload loop
    while let Some(event) = rx.recv().await {
        match event {
            WatchEvent::RouteChanged(path) => {
                let name = match path.file_stem().and_then(|s| s.to_str()) {
                    Some(n) => n.to_string(),
                    None => continue,
                };

                info!("Detected change in {}.lua — reloading", name);

                // Extract ports before dropping the old route so ALSA port IDs are preserved.
                let old_ports = routes.lock().unwrap().remove(&name).map(|r| r.take_ports());

                if path.exists() {
                    match Route::start(&path, Arc::clone(&config), old_ports) {
                        Ok(route) => {
                            conn_mgr.register_route(&name, route.port_decl(), &route.connect_decl);
                            conn_mgr.apply_all();
                            routes.lock().unwrap().insert(name.clone(), route);
                            info!("Reloaded route: {}", name);
                        }
                        Err(e) => error!("Failed to load route {}: {}", name, e),
                    }
                } else {
                    conn_mgr.unregister_route(&name);
                    info!("Removed route: {}", name);
                }
            }
            WatchEvent::ConfigChanged => {
                info!("config.toml changed — reloading");
                match config.reload() {
                    Ok(new_cfg) => {
                        if new_cfg.routes_dir != routes_dir {
                            warn!(
                                "routes_dir changed in config.toml — restart the daemon for this to take effect"
                            );
                        }
                        config = Arc::new(new_cfg);
                        reload_all_routes(&routes_dir, Arc::clone(&config), Arc::clone(&routes), Arc::clone(&conn_mgr));
                        info!("Config reloaded");
                    }
                    Err(e) => error!("Failed to reload config.toml: {}", e),
                }
            }
        }
    }

    Ok(())
}

fn reload_all_routes(
    dir: &PathBuf,
    config: Arc<Config>,
    routes: Arc<Mutex<HashMap<String, Route>>>,
    conn_mgr: Arc<ConnectionManager>,
) {
    let names: Vec<String> = routes.lock().unwrap().keys().cloned().collect();
    for name in names {
        let path = dir.join(format!("{}.lua", name));
        let old_ports = routes.lock().unwrap().remove(&name).map(|r| r.take_ports());
        match Route::start(&path, Arc::clone(&config), old_ports) {
            Ok(route) => {
                conn_mgr.register_route(&name, route.port_decl(), &route.connect_decl);
                routes.lock().unwrap().insert(name.clone(), route);
                info!("Reloaded route '{}' with new config", name);
            }
            Err(e) => error!("Failed to reload route '{}': {}", name, e),
        }
    }
    conn_mgr.apply_all();
}

async fn load_all_routes(
    dir: &PathBuf,
    config: Arc<Config>,
    routes: Arc<Mutex<HashMap<String, Route>>>,
    conn_mgr: Arc<ConnectionManager>,
) -> Result<()> {
    if !dir.exists() {
        std::fs::create_dir_all(dir)?;
        info!("Created routes directory: {}", dir.display());
        return Ok(());
    }

    {
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

                match Route::start(&path, Arc::clone(&config), None) {
                    Ok(route) => {
                        conn_mgr.register_route(&name, route.port_decl(), &route.connect_decl);
                        info!("Loaded route: {}", name);
                        map.insert(name, route);
                    }
                    Err(e) => error!("Failed to load route {}: {}", name, e),
                }
            }
        }
    }
    conn_mgr.apply_all();
    Ok(())
}
