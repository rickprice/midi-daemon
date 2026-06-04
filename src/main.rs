mod alsa_connect;
mod config;
mod lua_api;
mod lua_stdlib_tests;
mod osc;
mod osc_params;
mod route;
mod timer;

use anyhow::{Context as _, Result};
use notify::{Event, EventKind, RecursiveMode, Watcher};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use alsa_connect::ConnectionManager;
use config::Config;
use route::Route;

// ── OSC dispatch ──────────────────────────────────────────────────────────────

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

/// Bind a UDP port and dispatch incoming packets to routes by address prefix
/// (`/route-name/rest` → the route named `route-name`).
fn start_osc_receiver(port: u16, dispatch: OscDispatch) -> Option<osc::OscReceiver> {
    match osc::OscReceiver::spawn(port, move |from, address, args| {
        debug!("OSC received: address='{}' from={}", address, from);
        let route_name = address
            .strip_prefix('/')
            .and_then(|s| s.split('/').next())
            .unwrap_or("");
        if route_name.is_empty() {
            warn!("OSC: ignoring message with empty route prefix: '{}'", address);
            return;
        }
        let guard = dispatch.lock().unwrap();
        if let Some(inject) = guard.get(route_name) {
            inject(from, address, args);
        } else {
            warn!("OSC: no route for address '{}' (prefix '{}')", address, route_name);
        }
    }) {
        Ok(rx) => {
            info!("OSC receiver on UDP port {}", port);
            Some(rx)
        }
        Err(e) => {
            warn!("Failed to start OSC receiver on port {}: {}", port, e);
            None
        }
    }
}

/// Collect the set of UDP ports that need a running receiver: the global
/// config port (if set) plus every per-route declared receive port.
fn needed_osc_ports(config: &Config, routes: &HashMap<String, Route>) -> HashSet<u16> {
    let mut ports = HashSet::new();
    if let Some(p) = config.osc_receive_port {
        ports.insert(p);
    }
    for route in routes.values() {
        if let Some(p) = route.osc_receive_port {
            ports.insert(p);
        }
    }
    ports
}

/// Ensure exactly one receiver is running for each needed port.
fn sync_osc_receivers(
    config: &Config,
    routes: &Arc<Mutex<HashMap<String, Route>>>,
    receivers: &mut HashMap<u16, osc::OscReceiver>,
    dispatch: &OscDispatch,
) {
    let needed = needed_osc_ports(config, &routes.lock().unwrap());
    for &port in &needed {
        if !receivers.contains_key(&port) {
            if let Some(rx) = start_osc_receiver(port, Arc::clone(dispatch)) {
                receivers.insert(port, rx);
            }
        }
    }
    receivers.retain(|p, _| needed.contains(p));
}

// ── PID file ──────────────────────────────────────────────────────────────────

/// RAII PID file: written on creation, removed on drop.
struct PidFile(PathBuf);

impl PidFile {
    fn write(config: &Config) -> Result<Self> {
        let path = pid_file_path(config);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create PID dir {}", parent.display()))?;
        }
        std::fs::write(&path, format!("{}\n", std::process::id()))
            .with_context(|| format!("write PID file {}", path.display()))?;
        info!("PID file: {}", path.display());
        Ok(PidFile(path))
    }
}

impl Drop for PidFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn pid_file_path(config: &Config) -> PathBuf {
    config.cache_dir().join("midi-daemon.pid")
}

/// Try to locate the PID file of a running daemon (user cache first, then system).
fn find_pid_file() -> Option<PathBuf> {
    // User cache (most common)
    if let Some(d) = dirs::cache_dir() {
        let p = d.join("midi-daemon/midi-daemon.pid");
        if p.exists() { return Some(p); }
    }
    // System cache
    let p = PathBuf::from("/var/cache/midi-daemon/midi-daemon.pid");
    if p.exists() { return Some(p); }
    None
}

// ── Graceful shutdown ─────────────────────────────────────────────────────────

/// Send a `Shutdown` command to every route and wait for their event-loop
/// threads to finish (which includes saving persisted state).
fn graceful_shutdown(routes: &Arc<Mutex<HashMap<String, Route>>>) {
    let to_shutdown: Vec<Route> = {
        let mut guard = routes.lock().unwrap();
        guard.drain().map(|(_, r)| r).collect()
    };
    let handles: Vec<std::thread::JoinHandle<()>> = to_shutdown
        .into_iter()
        .filter_map(|r| r.shutdown())
        .collect();
    for h in handles {
        let _ = h.join();
    }
    info!("All routes shut down");
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    // --resync: send SIGUSR1 to the running daemon then exit.
    if args.iter().any(|a| a == "--resync") {
        return do_resync();
    }

    let cli_level = args.windows(2)
        .find(|w| w[0] == "--log-level")
        .map(|w| w[1].clone());

    let log_filter = if let Some(level) = cli_level {
        format!("midi_daemon={}", level)
    } else {
        std::env::var("RUST_LOG").unwrap_or_else(|_| "midi_daemon=info".to_string())
    };

    tracing_subscriber::fmt()
        .with_env_filter(log_filter)
        .init();

    let config = Config::find_and_load()?;

    info!("Starting midi-daemon");
    info!("Routes directory: {}", config.routes_dir.display());

    let routes_dir = config.routes_dir.clone();
    let mut config = Arc::new(config);

    // Write PID file (removed automatically on drop).
    let _pid_file = PidFile::write(&config)?;

    // Map of route name -> Route handle.
    let routes: Arc<Mutex<HashMap<String, Route>>> =
        Arc::new(Mutex::new(HashMap::new()));

    let conn_mgr = Arc::new(ConnectionManager::new());
    Arc::clone(&conn_mgr).spawn_watcher();

    let osc_dispatch: OscDispatch = Arc::new(Mutex::new(HashMap::new()));

    load_all_routes(
        &routes_dir,
        Arc::clone(&config),
        Arc::clone(&routes),
        Arc::clone(&conn_mgr),
        Arc::clone(&osc_dispatch),
    ).await?;

    let mut osc_receivers: HashMap<u16, osc::OscReceiver> = HashMap::new();
    sync_osc_receivers(&config, &routes, &mut osc_receivers, &osc_dispatch);

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

    // Signal handlers
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigusr1 = signal(SignalKind::user_defined1())?;

    // Main event loop
    loop {
        tokio::select! {
            biased;
            _ = sigterm.recv() => {
                info!("Received SIGTERM — shutting down");
                break;
            }
            _ = sigusr1.recv() => {
                info!("Received SIGUSR1 — resyncing all route params");
                let guard = routes.lock().unwrap();
                for (name, route) in guard.iter() {
                    route.send_resync();
                    debug!("Queued resync for route '{}'", name);
                }
            }
            event = rx.recv() => {
                match event {
                    Some(WatchEvent::RouteChanged(path)) => {
                        handle_route_changed(
                            &path, &config, &routes, &conn_mgr, &osc_dispatch, &mut osc_receivers,
                        );
                    }
                    Some(WatchEvent::ConfigChanged) => {
                        handle_config_changed(
                            &routes_dir, &mut config, &routes, &conn_mgr, &osc_dispatch,
                            &mut osc_receivers,
                        );
                    }
                    None => {
                        info!("Watch channel closed — shutting down");
                        break;
                    }
                }
            }
        }
    }

    graceful_shutdown(&routes);
    Ok(())
}

// ── --resync subcommand ───────────────────────────────────────────────────────

fn do_resync() -> Result<()> {
    let pid_path = find_pid_file()
        .context("No midi-daemon PID file found. Is the daemon running?")?;

    let pid_str = std::fs::read_to_string(&pid_path)
        .with_context(|| format!("read PID file {}", pid_path.display()))?;
    let pid: libc::pid_t = pid_str.trim().parse()
        .context("PID file contains invalid PID")?;

    let rc = unsafe { libc::kill(pid, libc::SIGUSR1) };
    if rc != 0 {
        let errno = std::io::Error::last_os_error();
        anyhow::bail!("Failed to send SIGUSR1 to PID {}: {}", pid, errno);
    }

    println!("Sent resync signal to midi-daemon (PID {})", pid);
    Ok(())
}

// ── Hot-reload helpers ────────────────────────────────────────────────────────

fn handle_route_changed(
    path: &Path,
    config: &Arc<Config>,
    routes: &Arc<Mutex<HashMap<String, Route>>>,
    conn_mgr: &Arc<ConnectionManager>,
    osc_dispatch: &OscDispatch,
    osc_receivers: &mut HashMap<u16, osc::OscReceiver>,
) {
    let name = match path.file_stem().and_then(|s| s.to_str()) {
        Some(n) => n.to_string(),
        None => return,
    };

    if path.exists() {
        info!("Detected change in {}.lua — reloading", name);
        let old_ports = routes.lock().unwrap().get(&name).map(|r| r.ports_arc());
        match Route::start(path, Arc::clone(config), old_ports) {
            Ok(route) => {
                conn_mgr.register_route(&name, route.port_decl(), &route.connect_decl);
                conn_mgr.apply_all();
                register_route_osc(osc_dispatch, &name, &route);
                routes.lock().unwrap().insert(name.clone(), route);
                sync_osc_receivers(config, routes, osc_receivers, osc_dispatch);
                info!("Reloaded route: {}", name);
            }
            Err(e) => error!("Failed to reload route {}: {}", name, e),
        }
    } else {
        routes.lock().unwrap().remove(&name);
        conn_mgr.unregister_route(&name);
        unregister_route_osc(osc_dispatch, &name);
        sync_osc_receivers(config, routes, osc_receivers, osc_dispatch);
        info!("Removed route: {}", name);
    }
}

fn handle_config_changed(
    routes_dir: &PathBuf,
    config: &mut Arc<Config>,
    routes: &Arc<Mutex<HashMap<String, Route>>>,
    conn_mgr: &Arc<ConnectionManager>,
    osc_dispatch: &OscDispatch,
    osc_receivers: &mut HashMap<u16, osc::OscReceiver>,
) {
    info!("config.toml changed — reloading");
    match config.reload() {
        Ok(new_cfg) => {
            if new_cfg.routes_dir != *routes_dir {
                warn!(
                    "routes_dir changed in config.toml — restart the daemon for this to take effect"
                );
            }
            *config = Arc::new(new_cfg);
            reload_all_routes(routes_dir, Arc::clone(config), Arc::clone(routes),
                               Arc::clone(conn_mgr), Arc::clone(osc_dispatch));
            sync_osc_receivers(config, routes, osc_receivers, osc_dispatch);
            info!("Config reloaded");
        }
        Err(e) => error!("Failed to reload config.toml: {}", e),
    }
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
