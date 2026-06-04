mod alsa_connect;
mod config;
mod lua_api;
mod lua_stdlib_tests;
mod osc;
mod osc_params;
mod route;
mod timer;

use clap::Parser;
use anyhow::{Context as _, Result};
use notify::{Event, EventKind, RecursiveMode, Watcher};
use std::collections::{HashMap, HashSet};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
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

// ── Control socket ────────────────────────────────────────────────────────────

enum ControlCmd {
    Resync { reply: oneshot::Sender<String> },
    Reload { reply: oneshot::Sender<String> },
    Status { reply: oneshot::Sender<String> },
}

/// RAII guard: removes the socket file on drop.
struct ControlSocketFile(PathBuf);

impl Drop for ControlSocketFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Bind the control socket, set permissions (0o660), and spawn the accept loop.
fn start_control_socket(
    path: &Path,
    tx: mpsc::Sender<ControlCmd>,
) -> Result<ControlSocketFile> {
    let _ = std::fs::remove_file(path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create socket dir {}", parent.display()))?;
    }
    let listener = tokio::net::UnixListener::bind(path)
        .with_context(|| format!("bind control socket {}", path.display()))?;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660));
    info!("Control socket: {}", path.display());

    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let tx = tx.clone();
                    tokio::spawn(async move { handle_control_conn(stream, tx).await });
                }
                Err(e) => { error!("Control socket accept: {}", e); break; }
            }
        }
    });

    Ok(ControlSocketFile(path.to_path_buf()))
}

/// Handle one control connection: read a command, send to the main loop, write the reply.
async fn handle_control_conn(stream: tokio::net::UnixStream, tx: mpsc::Sender<ControlCmd>) {
    let (read_half, mut write_half) = stream.into_split();
    let line = match tokio::io::BufReader::new(read_half).lines().next_line().await {
        Ok(Some(l)) => l,
        _ => return,
    };
    let (reply_tx, reply_rx) = oneshot::channel();
    let cmd = match line.trim() {
        "resync" => ControlCmd::Resync { reply: reply_tx },
        "reload" => ControlCmd::Reload { reply: reply_tx },
        "status" => ControlCmd::Status { reply: reply_tx },
        other => {
            let _ = write_half.write_all(format!("error: unknown command '{}'\n", other).as_bytes()).await;
            return;
        }
    };
    if tx.send(cmd).await.is_ok() {
        if let Ok(response) = reply_rx.await {
            let _ = write_half.write_all(response.as_bytes()).await;
        }
    }
}

/// Client-side: connect to the control socket, send a command, print the reply.
async fn do_control_cmd(cmd: &str) -> Result<()> {
    let path = config::control_socket_path();
    let stream = tokio::net::UnixStream::connect(&path).await
        .with_context(|| format!("connect to {}: is midi-daemon running?", path.display()))?;
    let (read_half, mut write_half) = stream.into_split();
    write_half.write_all(format!("{}\n", cmd).as_bytes()).await?;
    let mut lines = tokio::io::BufReader::new(read_half).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        println!("{}", line);
    }
    Ok(())
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

#[derive(Parser)]
#[command(about = "A Lua-scriptable MIDI routing daemon")]
struct Cli {
    /// Send a resync command to the running daemon
    #[arg(long)] resync: bool,
    /// Hot-reload routes in the running daemon
    #[arg(long)] reload: bool,
    /// Print status of the running daemon
    #[arg(long)] status: bool,
    /// Log level (e.g. debug, info, warn, error)
    #[arg(long)] log_level: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    if cli.resync { return do_control_cmd("resync").await; }
    if cli.reload { return do_control_cmd("reload").await; }
    if cli.status { return do_control_cmd("status").await; }

    let log_filter = if let Some(level) = cli.log_level {
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

    // Bind control socket (removed automatically on drop).
    let (ctrl_tx, mut ctrl_rx) = mpsc::channel::<ControlCmd>(8);
    let _ctrl_socket = start_control_socket(&config::control_socket_path(), ctrl_tx)?;

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
            Some(cmd) = ctrl_rx.recv() => {
                match cmd {
                    ControlCmd::Resync { reply } => {
                        info!("Control: resync");
                        let guard = routes.lock().unwrap();
                        for (name, route) in guard.iter() {
                            route.send_resync();
                            debug!("Queued resync for route '{}'", name);
                        }
                        let _ = reply.send("ok\n".to_string());
                    }
                    ControlCmd::Reload { reply } => {
                        info!("Control: reload");
                        handle_config_changed(
                            &routes_dir, &mut config, &routes, &conn_mgr,
                            &osc_dispatch, &mut osc_receivers,
                        );
                        let _ = reply.send("ok\n".to_string());
                    }
                    ControlCmd::Status { reply } => {
                        let mut route_names: Vec<String> =
                            routes.lock().unwrap().keys().cloned().collect();
                        route_names.sort();
                        let mut ports: Vec<u16> = osc_receivers.keys().copied().collect();
                        ports.sort_unstable();
                        let text = format!(
                            "pid: {}\nroutes: {}\nosc_recv: {}\nconfig: {}\ncache: {}\nsocket: {}\n",
                            std::process::id(),
                            if route_names.is_empty() { "(none)".into() } else { route_names.join(", ") },
                            if ports.is_empty() { "(none)".into() } else { ports.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", ") },
                            config.config_path.as_deref()
                                .map(|p| p.display().to_string())
                                .unwrap_or_else(|| "(defaults)".into()),
                            config.cache_dir().display(),
                            config::control_socket_path().display(),
                        );
                        let _ = reply.send(text);
                    }
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
