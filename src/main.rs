mod alsa_connect;
mod config;
mod lua_api;
mod lua_stdlib_tests;
mod obs;
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
use obs::ObsManager;
use route::Route;

// ── OSC dispatch ──────────────────────────────────────────────────────────────

/// A map from route name to an OSC injector closure.
///
/// `Route` is `!Send` (ALSA raw pointers), so we can't share the routes map
/// with the receiver thread. Instead, each route hands out a lightweight
/// `Send + 'static` closure that forwards events into its event channel.
type OscDispatch =
    Arc<Mutex<HashMap<String, Box<dyn Fn(std::net::SocketAddr, String, Vec<rosc::OscType>) + Send>>>>;

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
        let guard = dispatch.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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

// ── Daemon state ──────────────────────────────────────────────────────────────

/// All mutable daemon state: config, loaded routes, ALSA connection manager,
/// and the OSC receive side (dispatch table + one UDP socket per needed port).
/// Owned entirely by the main event loop, so plain fields (no `Rc<RefCell<_>>`)
/// suffice — nothing else holds a reference to this state.
struct Daemon {
    routes_dir: PathBuf,
    config: Arc<Config>,
    routes: HashMap<String, Route>,
    conn_mgr: Arc<ConnectionManager>,
    osc_dispatch: OscDispatch,
    osc_receivers: HashMap<u16, osc::OscReceiver>,
    obs_manager: Arc<ObsManager>,
}

impl Daemon {
    fn new(
        routes_dir: PathBuf,
        config: Arc<Config>,
        conn_mgr: Arc<ConnectionManager>,
        obs_manager: Arc<ObsManager>,
    ) -> Self {
        Daemon {
            routes_dir,
            config,
            routes: HashMap::new(),
            conn_mgr,
            osc_dispatch: Arc::new(Mutex::new(HashMap::new())),
            osc_receivers: HashMap::new(),
            obs_manager,
        }
    }

    fn register_route_osc(&self, name: &str, route: &Route) {
        self.osc_dispatch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(name.to_string(), Box::new(route.make_osc_injector()));
    }

    fn unregister_route_osc(&self, name: &str) {
        self.osc_dispatch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(name);
    }

    fn register_route_obs(&self, name: &str, route: &Route) {
        let injector: obs::EventInjector = Arc::new(route.make_obs_injector());
        self.obs_manager.register_route(name, &route.obs_connections, &injector);
    }

    fn unregister_route_obs(&self, name: &str) {
        self.obs_manager.unregister_route(name);
    }

    /// Collect the set of UDP ports that need a running receiver: the global
    /// config port (if set) plus every per-route declared receive port.
    fn needed_osc_ports(&self) -> HashSet<u16> {
        let mut ports = HashSet::new();
        if let Some(p) = self.config.osc_receive_port {
            ports.insert(p);
        }
        for route in self.routes.values() {
            if let Some(p) = route.osc_receive_port {
                ports.insert(p);
            }
        }
        ports
    }

    /// Ensure exactly one receiver is running for each needed port.
    fn sync_osc_receivers(&mut self) {
        let needed = self.needed_osc_ports();
        for &port in &needed {
            if let std::collections::hash_map::Entry::Vacant(e) = self.osc_receivers.entry(port)
                && let Some(rx) = start_osc_receiver(port, Arc::clone(&self.osc_dispatch)) {
                e.insert(rx);
            }
        }
        self.osc_receivers.retain(|p, _| needed.contains(p));
    }

    fn load_all_routes(&mut self) -> Result<()> {
        let entries = match std::fs::read_dir(&self.routes_dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir_all(&self.routes_dir)
                    .with_context(|| format!("create routes directory {}", self.routes_dir.display()))?;
                info!("Created routes directory: {}", self.routes_dir.display());
                return Ok(());
            }
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("open routes directory {}", self.routes_dir.display()));
            }
        };

        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "lua") {
                let name = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("unknown")
                    .to_string();

                match Route::start(&path, &self.config, None, &self.obs_manager) {
                    Ok(route) => {
                        self.conn_mgr.register_route(&name, route.port_decl(), &route.connect_decl);
                        self.register_route_osc(&name, &route);
                        self.register_route_obs(&name, &route);
                        info!("Loaded route: {}", name);
                        self.routes.insert(name, route);
                    }
                    Err(e) => error!("Failed to load route {}: {}", name, e),
                }
            }
        }
        self.conn_mgr.apply_all();
        Ok(())
    }

    fn reload_all_routes(&mut self) {
        let names: Vec<String> = self.routes.keys().cloned().collect();
        for name in names {
            let path = self.routes_dir.join(format!("{name}.lua"));
            let old_ports = self.routes.get(&name).map(route::Route::ports_rc);
            match Route::start(&path, &self.config, old_ports, &self.obs_manager) {
                Ok(route) => {
                    self.conn_mgr.register_route(&name, route.port_decl(), &route.connect_decl);
                    self.register_route_osc(&name, &route);
                    self.register_route_obs(&name, &route);
                    self.routes.insert(name.clone(), route);
                    info!("Reloaded route '{}' with new config", name);
                }
                Err(e) => error!("Failed to reload route '{}': {}", name, e),
            }
        }
        self.conn_mgr.apply_all();
    }

    fn handle_route_changed(&mut self, path: &Path) {
        let name = match path.file_stem().and_then(|s| s.to_str()) {
            Some(n) => n.to_string(),
            None => return,
        };

        if path.exists() {
            info!("Detected change in {}.lua — reloading", name);
            let old_ports = self.routes.get(&name).map(route::Route::ports_rc);
            match Route::start(path, &self.config, old_ports, &self.obs_manager) {
                Ok(route) => {
                    self.conn_mgr.register_route(&name, route.port_decl(), &route.connect_decl);
                    self.conn_mgr.apply_all();
                    self.register_route_osc(&name, &route);
                    self.register_route_obs(&name, &route);
                    self.routes.insert(name.clone(), route);
                    self.sync_osc_receivers();
                    info!("Reloaded route: {}", name);
                }
                Err(e) => error!("Failed to reload route {}: {}", name, e),
            }
        } else {
            self.routes.remove(&name);
            self.conn_mgr.unregister_route(&name);
            self.unregister_route_osc(&name);
            self.unregister_route_obs(&name);
            self.sync_osc_receivers();
            info!("Removed route: {}", name);
        }
    }

    fn handle_config_changed(&mut self) {
        info!("config.toml changed — reloading");
        match self.config.reload() {
            Ok(new_cfg) => {
                if new_cfg.routes_dir != self.routes_dir {
                    warn!(
                        "routes_dir changed in config.toml — restart the daemon for this to take effect"
                    );
                }
                self.obs_manager.sync(&new_cfg.obs);
                self.config = Arc::new(new_cfg);
                self.reload_all_routes();
                self.sync_osc_receivers();
                info!("Config reloaded");
            }
            Err(e) => error!("Failed to reload config.toml: {}", e),
        }
    }

    fn send_resync_all(&self) {
        for (name, route) in &self.routes {
            route.send_resync();
            debug!("Queued resync for route '{}'", name);
        }
    }

    fn status_text(&self) -> String {
        let mut route_names: Vec<&String> = self.routes.keys().collect();
        route_names.sort();
        let mut ports: Vec<u16> = self.osc_receivers.keys().copied().collect();
        ports.sort_unstable();
        format!(
            "pid: {}\nroutes: {}\nosc_recv: {}\nconfig: {}\ncache: {}\nsocket: {}\n",
            std::process::id(),
            if route_names.is_empty() {
                "(none)".into()
            } else {
                route_names.into_iter().cloned().collect::<Vec<_>>().join(", ")
            },
            if ports.is_empty() { "(none)".into() } else { ports.iter().map(std::string::ToString::to_string).collect::<Vec<_>>().join(", ") },
            self.config.config_path.as_deref().map_or_else(|| "(defaults)".into(), |p| p.display().to_string()),
            self.config.cache_dir().display(),
            config::control_socket_path().display(),
        )
    }

    /// Send a `Shutdown` command to every route and wait for their event-loop
    /// threads to finish (which includes saving persisted state).
    fn graceful_shutdown(&mut self) {
        let to_shutdown: Vec<Route> = std::mem::take(&mut self.routes).into_values().collect();
        let handles: Vec<std::thread::JoinHandle<()>> = to_shutdown
            .into_iter()
            .filter_map(route::Route::shutdown)
            .collect();
        for h in handles {
            if let Err(e) = h.join() {
                let msg = e.downcast_ref::<&str>().copied()
                    .or_else(|| e.downcast_ref::<String>().map(String::as_str))
                    .unwrap_or("(unknown panic payload)");
                warn!("Route thread panicked during shutdown: {}", msg);
            }
        }
        info!("All routes shut down");
    }
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
    let Ok(Some(line)) = tokio::io::BufReader::new(read_half).lines().next_line().await else {
        return;
    };
    let (reply_tx, reply_rx) = oneshot::channel();
    let cmd = match line.trim() {
        "resync" => ControlCmd::Resync { reply: reply_tx },
        "reload" => ControlCmd::Reload { reply: reply_tx },
        "status" => ControlCmd::Status { reply: reply_tx },
        other => {
            let _ = write_half.write_all(format!("error: unknown command '{other}'\n").as_bytes()).await;
            return;
        }
    };
    if tx.send(cmd).await.is_ok()
        && let Ok(response) = reply_rx.await {
        let _ = write_half.write_all(response.as_bytes()).await;
    }
}

/// Client-side: connect to the control socket, send a command, print the reply.
async fn do_control_cmd(cmd: &str) -> Result<()> {
    let path = config::control_socket_path();
    let stream = tokio::net::UnixStream::connect(&path).await
        .with_context(|| format!("connect to {}: is midi-daemon running?", path.display()))?;
    let (read_half, mut write_half) = stream.into_split();
    write_half.write_all(format!("{cmd}\n").as_bytes()).await?;
    let mut lines = tokio::io::BufReader::new(read_half).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        println!("{line}");
    }
    Ok(())
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[derive(clap::Subcommand)]
enum Cmd {
    /// Re-apply all current param values to connected OSC clients
    Resync,
    /// Hot-reload all route scripts in the running daemon
    Reload,
    /// Print status of the running daemon
    Status,
}

#[derive(Parser)]
#[command(about = "A Lua-scriptable MIDI routing daemon")]
struct Cli {
    /// Log level (e.g. debug, info, warn, error)
    #[arg(long)] log_level: Option<String>,
    /// Path to config file (overrides $`MIDI_DAEMON_CONFIG` and default search)
    #[arg(long)] config: Option<PathBuf>,
    /// Path to routes directory (overrides `routes_dir` in config file)
    #[arg(long)] routes: Option<PathBuf>,
    /// Control command to send to a running daemon (omit to start the daemon)
    #[command(subcommand)] command: Option<Cmd>,
}

// Top-level daemon wiring (config, control socket, routes, watchers, signal
// handlers, main select loop) — inherently a flat sequence of one-time setup.
#[allow(clippy::too_many_lines)]
#[tokio::main]
async fn main() -> Result<()> {
    enum WatchEvent {
        RouteChanged(PathBuf),
        ConfigChanged,
    }
    use tokio::signal::unix::{signal, SignalKind};

    let cli = Cli::parse();

    match cli.command {
        Some(Cmd::Resync) => return do_control_cmd("resync").await,
        Some(Cmd::Reload) => return do_control_cmd("reload").await,
        Some(Cmd::Status) => return do_control_cmd("status").await,
        None => {}
    }

    let log_filter = if let Some(level) = cli.log_level {
        format!("midi_daemon={level}")
    } else {
        std::env::var("RUST_LOG").unwrap_or_else(|_| "midi_daemon=info".to_string())
    };

    tracing_subscriber::fmt()
        .with_env_filter(log_filter)
        .init();

    let config = Config::find_and_load_with_overrides(
        cli.config.as_deref(),
        cli.routes.as_deref(),
    )?;

    info!("Starting midi-daemon");
    info!("Routes directory: {}", config.routes_dir.display());

    let routes_dir = config.routes_dir.clone();
    let config = Arc::new(config);

    // Bind control socket (removed automatically on drop).
    let (ctrl_tx, mut ctrl_rx) = mpsc::channel::<ControlCmd>(8);
    let _ctrl_socket = start_control_socket(&config::control_socket_path(), ctrl_tx)?;

    let conn_mgr = Arc::new(ConnectionManager::new());
    Arc::clone(&conn_mgr).spawn_watcher();

    let obs_manager = ObsManager::new(&config.obs);

    let mut daemon = Daemon::new(routes_dir.clone(), config, conn_mgr, obs_manager);
    daemon.load_all_routes()?;
    daemon.sync_osc_receivers();

    // inotify watcher for hot-reload
    let (tx, mut rx) = mpsc::channel::<WatchEvent>(32);

    let config_path = daemon.config.config_path.clone();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
        if let Ok(event) = res {
            match event.kind {
                EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) => {
                    for path in event.paths {
                        if path.extension().is_some_and(|e| e == "lua") {
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

    if let Some(ref cfg_path) = daemon.config.config_path {
        let cfg_dir = cfg_path.parent().unwrap_or(cfg_path.as_path());
        if cfg_dir != routes_dir.as_path() {
            watcher.watch(cfg_dir, RecursiveMode::NonRecursive)?;
            info!("Watching {} for changes", cfg_dir.display());
        }
    }

    // Signal handlers
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigusr1 = signal(SignalKind::user_defined1())?;

    // Main event loop
    loop {
        tokio::select! {
            biased;
            _ = sigterm.recv() => {
                info!("Received SIGTERM — shutting down");
                break;
            }
            _ = sigint.recv() => {
                info!("Received SIGINT — shutting down");
                break;
            }
            _ = sigusr1.recv() => {
                info!("Received SIGUSR1 — resyncing all route params");
                daemon.send_resync_all();
            }
            Some(cmd) = ctrl_rx.recv() => {
                match cmd {
                    ControlCmd::Resync { reply } => {
                        info!("Control: resync");
                        daemon.send_resync_all();
                        let _ = reply.send("ok\n".to_string());
                    }
                    ControlCmd::Reload { reply } => {
                        info!("Control: reload");
                        daemon.handle_config_changed();
                        let _ = reply.send("ok\n".to_string());
                    }
                    ControlCmd::Status { reply } => {
                        let _ = reply.send(daemon.status_text());
                    }
                }
            }
            event = rx.recv() => {
                match event {
                    Some(WatchEvent::RouteChanged(path)) => {
                        daemon.handle_route_changed(&path);
                    }
                    Some(WatchEvent::ConfigChanged) => {
                        daemon.handle_config_changed();
                    }
                    None => {
                        info!("Watch channel closed — shutting down");
                        break;
                    }
                }
            }
        }
    }

    daemon.graceful_shutdown();
    Ok(())
}
