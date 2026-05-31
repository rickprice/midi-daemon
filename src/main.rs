mod alsa_connect;
mod config;
mod lua_api;
mod lua_stdlib_tests;
mod osc;
mod osc_params;
mod route;
mod timer;

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

/// A map from route name to an OSC injector closure.
///
/// `Route` is `!Send` (ALSA raw pointers), so we can't share the routes map
/// with the receiver thread. Instead, each route hands out a lightweight
/// `Send + 'static` closure that forwards events into its event channel.
type OscDispatch =
    Arc<Mutex<HashMap<String, Box<dyn Fn(std::net::SocketAddr, String, Vec<rosc::OscType>) + Send>>>>;

fn register_route_osc(dispatch: &OscDispatch, name: &str, route: &Route) {
    dispatch
        .lock()
        .unwrap()
        .insert(name.to_string(), Box::new(route.make_osc_injector()));
}

fn unregister_route_osc(dispatch: &OscDispatch, name: &str) {
    dispatch.lock().unwrap().remove(name);
}

/// Bind the global OSC receive port and dispatch incoming packets to routes by
/// address prefix: `/route-name/rest` → the route named `route-name`.
fn start_global_osc_receiver(
    config: &Config,
    dispatch: OscDispatch,
) -> Option<osc::OscReceiver> {
    let port = config.osc_receive_port?;
    match osc::OscReceiver::spawn(port, move |from, address, args| {
        let route_name = address
            .strip_prefix('/')
            .and_then(|s| s.split('/').next())
            .unwrap_or("");
        if route_name.is_empty() {
            return;
        }
        if let Some(inject) = dispatch.lock().unwrap().get(route_name) {
            inject(from, address, args);
        }
    }) {
        Ok(rx) => {
            info!("Global OSC receiver on UDP port {}", port);
            Some(rx)
        }
        Err(e) => {
            warn!("Failed to start global OSC receiver on port {}: {}", port, e);
            None
        }
    }
}

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

    let osc_dispatch: OscDispatch = Arc::new(Mutex::new(HashMap::new()));

    // Initial load of all .lua files
    load_all_routes(
        &routes_dir,
        Arc::clone(&config),
        Arc::clone(&routes),
        Arc::clone(&conn_mgr),
        Arc::clone(&osc_dispatch),
    ).await?;

    // Global OSC receiver (dispatches /route-name/... to the matching route).
    let mut global_osc_rx = start_global_osc_receiver(&config, Arc::clone(&osc_dispatch));

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

                if path.exists() {
                    info!("Detected change in {}.lua — reloading", name);

                    // Borrow the existing ports Arc without removing the old route so that
                    // ALSA port IDs are preserved and the old route stays alive if the
                    // reload fails (e.g. Lua syntax error or ALSA error).
                    let old_ports = routes.lock().unwrap()
                        .get(&name).map(|r| r.ports_arc());

                    match Route::start(&path, Arc::clone(&config), old_ports) {
                        Ok(route) => {
                            conn_mgr.register_route(&name, route.port_decl(), &route.connect_decl);
                            conn_mgr.apply_all();
                            register_route_osc(&osc_dispatch, &name, &route);
                            // Replacing the old entry drops it, which stops its timer and
                            // detaches its event-loop thread (which will drain and exit).
                            routes.lock().unwrap().insert(name.clone(), route);
                            info!("Reloaded route: {}", name);
                        }
                        Err(e) => error!("Failed to reload route {}: {}", name, e),
                    }
                } else {
                    routes.lock().unwrap().remove(&name);
                    conn_mgr.unregister_route(&name);
                    unregister_route_osc(&osc_dispatch, &name);
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
                        let old_osc_port = config.osc_receive_port;
                        config = Arc::new(new_cfg);
                        // Restart global OSC receiver if the port changed.
                        if config.osc_receive_port != old_osc_port {
                            global_osc_rx = start_global_osc_receiver(&config, Arc::clone(&osc_dispatch));
                        }
                        reload_all_routes(
                            &routes_dir,
                            Arc::clone(&config),
                            Arc::clone(&routes),
                            Arc::clone(&conn_mgr),
                            Arc::clone(&osc_dispatch),
                        );
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
    osc_dispatch: OscDispatch,
) {
    let names: Vec<String> = routes.lock().unwrap().keys().cloned().collect();
    for name in names {
        let path = dir.join(format!("{}.lua", name));
        let old_ports = routes.lock().unwrap().get(&name).map(|r| r.ports_arc());
        match Route::start(&path, Arc::clone(&config), old_ports) {
            Ok(route) => {
                conn_mgr.register_route(&name, route.port_decl(), &route.connect_decl);
                register_route_osc(&osc_dispatch, &name, &route);
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
    osc_dispatch: OscDispatch,
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
                        register_route_osc(&osc_dispatch, &name, &route);
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
