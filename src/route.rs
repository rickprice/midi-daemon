use anyhow::{Context, Result};
use midir::os::unix::{VirtualInput, VirtualOutput};
use midir::{MidiInput, MidiInputConnection, MidiOutput, MidiOutputConnection};
use mlua::prelude::*;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

/// Connect patterns for a route's ports: port_name → list of regex strings.
#[derive(Debug, Clone, Default)]
pub struct ConnectDecl {
    pub inputs: HashMap<String, Vec<String>>,
    pub outputs: HashMap<String, Vec<String>>,
}

use crate::config::Config;
use crate::lua_api::{
    lua_to_midi_bytes, lua_val_to_osc_type, midi_bytes_to_lua, osc_message_to_lua,
    toml_table_to_lua,
};
use crate::osc::{OscDecl, OscSender};
use crate::timer::{Timer, TimerEvent};

enum RouteEvent {
    Midi { port: String, bytes: Vec<u8> },
    Timer(TimerEvent),
    Osc { from: SocketAddr, address: String, args: Vec<rosc::OscType> },
}

/// Named input and output ports declared by a route.
///
/// Declared from Lua via `init()` or from `config.toml` via `inputs`/`outputs` arrays.
/// When neither is present the route gets a single default port (backward-compatible).
#[derive(Debug, Clone, PartialEq)]
pub struct PortDecl {
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
}

impl Default for PortDecl {
    fn default() -> Self {
        PortDecl {
            inputs: vec!["default".to_string()],
            outputs: vec!["default".to_string()],
        }
    }
}

impl PortDecl {
    /// True when this is the single unnamed default port (uses backward-compat ALSA names).
    pub fn is_default(&self) -> bool {
        self.inputs.len() == 1
            && self.inputs[0] == "default"
            && self.outputs.len() == 1
            && self.outputs[0] == "default"
    }
}

/// Owns the virtual MIDI ports for a route. Kept alive across Lua reloads so
/// the ALSA client and port IDs stay the same.
pub struct RoutePorts {
    out_conns: HashMap<String, Arc<Mutex<MidiOutputConnection>>>,
    midi_fwds: HashMap<String, Arc<Mutex<Option<mpsc::Sender<RouteEvent>>>>>,
    _in_conns: Vec<MidiInputConnection<()>>,
    pub decl: PortDecl,
}

impl RoutePorts {
    fn create(
        route_name: &str,
        decl: &PortDecl,
        initial_tx: mpsc::Sender<RouteEvent>,
    ) -> Result<Arc<Self>> {
        let base = format!("midi-daemon:{}", route_name);
        let is_default = decl.is_default();

        let mut out_conns = HashMap::new();
        let mut midi_fwds = HashMap::new();
        let mut in_conns = Vec::new();

        for port_name in &decl.outputs {
            // Backward-compat: single default port keeps the old ALSA names.
            let (client_name, alsa_port) = if is_default {
                (format!("{}-out", base), base.clone())
            } else {
                (
                    format!("{}/{}-out", base, port_name),
                    format!("{}/{}", base, port_name),
                )
            };
            let midi_out =
                MidiOutput::new(&client_name).context("Failed to create MIDI output")?;
            let conn = midi_out
                .create_virtual(&alsa_port)
                .map_err(|e| anyhow::anyhow!("Failed to create virtual MIDI output '{}': {}", alsa_port, e))?;
            out_conns.insert(port_name.clone(), Arc::new(Mutex::new(conn)));
        }

        for port_name in &decl.inputs {
            let fwd: Arc<Mutex<Option<mpsc::Sender<RouteEvent>>>> =
                Arc::new(Mutex::new(Some(initial_tx.clone())));

            let alsa_name = if is_default {
                format!("{}-in", base)
            } else {
                format!("{}/{}-in", base, port_name)
            };

            let midi_in = MidiInput::new(&alsa_name).context("Failed to create MIDI input")?;
            let fwd_ref = Arc::clone(&fwd);
            let port_name_owned = port_name.clone();

            let in_conn = midi_in
                .create_virtual(
                    &alsa_name,
                    move |_stamp, message, _| {
                        let guard = fwd_ref.lock().unwrap();
                        if let Some(tx) = guard.as_ref() {
                            let _ = tx.blocking_send(RouteEvent::Midi {
                                port: port_name_owned.clone(),
                                bytes: message.to_vec(),
                            });
                        }
                    },
                    (),
                )
                .map_err(|e| anyhow::anyhow!("Failed to create virtual MIDI input '{}': {}", alsa_name, e))?;

            midi_fwds.insert(port_name.clone(), fwd);
            in_conns.push(in_conn);
        }

        Ok(Arc::new(RoutePorts {
            out_conns,
            midi_fwds,
            _in_conns: in_conns,
            decl: decl.clone(),
        }))
    }

    /// Point all input callbacks at a new event channel (used on hot-reload).
    fn redirect_inputs(&self, new_tx: &mpsc::Sender<RouteEvent>) {
        for fwd in self.midi_fwds.values() {
            *fwd.lock().unwrap() = Some(new_tx.clone());
        }
    }
}

/// A running route: owns its MIDI ports, Lua VM, timer, and OSC sockets.
/// Dropping this stops everything cleanly.
pub struct Route {
    ports: Arc<RoutePorts>,
    _timer: Arc<Timer>,
    _thread: std::thread::JoinHandle<()>,
    /// Sender into the route's event channel, used by the global OSC dispatcher.
    osc_tx: mpsc::Sender<RouteEvent>,
    pub connect_decl: ConnectDecl,
    /// OSC receive port declared by this route's init(). The daemon starts exactly
    /// one shared receiver per unique port and dispatches by /route-name/ prefix.
    pub osc_receive_port: Option<u16>,
}

impl Route {
    /// Returns a `Send + 'static` closure that injects OSC events into this
    /// route's event loop. Used by the global listener in main.rs — avoids
    /// sharing the full Route (which is !Send due to ALSA raw pointers).
    pub fn make_osc_injector(
        &self,
    ) -> impl Fn(SocketAddr, String, Vec<rosc::OscType>) + Send + 'static {
        let tx = self.osc_tx.clone();
        move |from: SocketAddr, address: String, args: Vec<rosc::OscType>| {
            let _ = tx.blocking_send(RouteEvent::Osc { from, address, args });
        }
    }
}

impl Route {
    /// Return a clone of the ports Arc so the caller can pass them to a new Route::start
    /// without consuming (and stopping) this route first.
    pub fn ports_arc(&self) -> Arc<RoutePorts> {
        Arc::clone(&self.ports)
    }

    pub fn port_decl(&self) -> &PortDecl {
        &self.ports.decl
    }

    pub fn start(
        lua_path: &Path,
        config: Arc<Config>,
        existing_ports: Option<Arc<RoutePorts>>,
    ) -> Result<Self> {
        let name = lua_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();

        let script = std::fs::read_to_string(lua_path)
            .with_context(|| format!("Failed to read {}", lua_path.display()))?;

        let route_cfg = config.route_config(&name).cloned();

        // Run the script once to extract port layout, connect patterns, and OSC declarations.
        let (decl, raw_connect, osc_decl) =
            extract_all_decls(&script, &name, route_cfg.as_ref())?;

        // Fill missing connect patterns with global defaults from config.
        let connect_decl = apply_connect_defaults(
            raw_connect,
            &decl.inputs,
            &decl.outputs,
            config.default_connect_input.as_deref(),
            config.default_connect_output.as_deref(),
        );

        let (tx, rx) = mpsc::channel::<RouteEvent>(256);

        let ports = match existing_ports {
            Some(p) if p.decl == decl => {
                p.redirect_inputs(&tx);
                p
            }
            other => {
                if other.is_some() {
                    warn!(
                        "Route '{}': port layout changed on reload — ALSA port IDs will change",
                        name
                    );
                }
                RoutePorts::create(&name, &decl, tx.clone())?
            }
        };

        // Set up OSC sender.
        // Priority: per-route init() targets > global config osc_send_addr.
        // When receive is active but no send destination is configured, create a
        // socket-only sender (empty targets) so the route can still send replies
        // and subscriber notifications to dynamic addresses.
        let OscDecl { receive_port: osc_receive_port, send_targets: osc_send_targets } = osc_decl;

        let any_receive = osc_receive_port.is_some() || config.osc_receive_port.is_some();

        let osc_sender = if !osc_send_targets.is_empty() {
            match OscSender::new(osc_send_targets) {
                Ok(s) => Some(s),
                Err(e) => {
                    warn!("Route '{}': failed to create OSC sender: {}", name, e);
                    None
                }
            }
        } else if let Some(addr) = config.osc_send_addr.as_deref().and_then(parse_socket_addr) {
            let mut targets = HashMap::new();
            targets.insert("default".to_string(), addr);
            match OscSender::new(targets) {
                Ok(s) => Some(s),
                Err(e) => {
                    warn!("Route '{}': failed to create OSC sender from global config: {}", name, e);
                    None
                }
            }
        } else if any_receive {
            // Receive-only: socket still needed to send subscribe replies/notifications.
            match OscSender::new(HashMap::new()) {
                Ok(s) => Some(s),
                Err(e) => {
                    warn!("Route '{}': failed to create OSC reply socket: {}", name, e);
                    None
                }
            }
        } else {
            None
        };

        // Keep a sender for the global OSC dispatcher to inject events.
        // OSC receivers are managed centrally by main.rs — routes never bind their own sockets.
        let osc_tx = tx.clone();

        let timer = Arc::new(Timer::new(config.default_bpm, config.default_ppqn));
        let _timer_thread = timer.spawn(tx.clone(), RouteEvent::Timer);

        // Clone only what the event-loop thread needs.
        let out_conns_for_thread: HashMap<String, Arc<Mutex<MidiOutputConnection>>> = ports
            .out_conns
            .iter()
            .map(|(k, v)| (k.clone(), Arc::clone(v)))
            .collect();
        let default_out = ports.decl.outputs.first().cloned().unwrap_or_default();
        let timer_for_thread = Arc::clone(&timer);
        let name_for_thread = name.clone();

        let osc_heartbeat_interval = config.osc_heartbeat_interval;
        let thread = std::thread::spawn(move || {
            if let Err(e) = run_lua_event_loop(
                &name_for_thread,
                &script,
                rx,
                out_conns_for_thread,
                default_out,
                timer_for_thread,
                route_cfg,
                osc_sender,
                osc_heartbeat_interval,
            ) {
                error!("Route '{}' event loop error: {}", name_for_thread, e);
            }
        });

        info!(
            "Started route '{}' — inputs: [{}], outputs: [{}]",
            name,
            ports.decl.inputs.join(", "),
            ports.decl.outputs.join(", "),
        );

        Ok(Route {
            ports,
            _timer: timer,
            _thread: thread,
            osc_tx,
            connect_decl,
            osc_receive_port,
        })
    }
}

// ── Extraction helpers ────────────────────────────────────────────────────────

const LUA_STDLIB: &str = include_str!("lua/stdlib.lua");

/// Install no-op stubs, the config table, and the stdlib so init() can safely
/// call any global the live event loop exposes.
fn setup_extract_lua(lua: &Lua, name: &str, route_cfg: Option<&toml::Table>) -> Result<()> {
    lua.globals().set("send", lua.create_function(|_, _: LuaMultiValue| Ok(()))?)?;
    lua.globals().set("send_osc", lua.create_function(|_, _: LuaMultiValue| Ok(()))?)?;
    lua.globals().set("set_bpm", lua.create_function(|_, _: f64| Ok(()))?)?;
    lua.globals().set("get_bpm", lua.create_function(|_, ()| -> LuaResult<f64> { Ok(120.0) })?)?;
    lua.globals().set("set_ppqn", lua.create_function(|_, _: u32| Ok(()))?)?;
    lua.globals().set("get_ppqn", lua.create_function(|_, ()| -> LuaResult<u32> { Ok(24) })?)?;
    lua.globals().set("log", lua.create_function(|_, _: String| Ok(()))?)?;
    lua.globals().set("ROUTE_NAME", name)?;
    lua.globals().set("OSC_SEND_ENABLED", false)?;
    let cfg_table = match route_cfg {
        Some(tbl) => toml_table_to_lua(lua, tbl)
            .map_err(|e| anyhow::anyhow!("Failed to convert config to Lua: {}", e))?,
        None => lua.create_table()?,
    };
    lua.globals().set("config", cfg_table)?;
    lua.load(LUA_STDLIB).set_name("stdlib").exec()
        .map_err(|e| anyhow::anyhow!("Failed to load Lua stdlib: {}", e))?;
    Ok(())
}

fn lua_val_to_patterns(val: LuaValue) -> Vec<String> {
    match val {
        LuaValue::String(s) => s.to_str().ok().map(|p| vec![p.to_string()]).unwrap_or_default(),
        LuaValue::Table(t) => {
            let mut pats = Vec::new();
            for i in 1u32.. {
                match t.get::<LuaValue>(i) {
                    Ok(LuaValue::String(s)) => {
                        if let Ok(p) = s.to_str() { pats.push(p.to_string()); }
                    }
                    _ => break,
                }
            }
            pats
        }
        _ => vec![],
    }
}

/// Extract `connect` patterns from the table returned by `init()`.
fn connect_from_lua_table(tbl: &LuaTable) -> ConnectDecl {
    let mut decl = ConnectDecl::default();
    let connect_tbl = match tbl.get::<LuaValue>("connect") {
        Ok(LuaValue::Table(t)) => t,
        _ => return decl,
    };
    if let Ok(LuaValue::Table(t)) = connect_tbl.get::<LuaValue>("inputs") {
        for pair in t.pairs::<String, LuaValue>() {
            if let Ok((k, v)) = pair {
                let pats = lua_val_to_patterns(v);
                if !pats.is_empty() { decl.inputs.insert(k, pats); }
            }
        }
    }
    if let Ok(LuaValue::Table(t)) = connect_tbl.get::<LuaValue>("outputs") {
        for pair in t.pairs::<String, LuaValue>() {
            if let Ok((k, v)) = pair {
                let pats = lua_val_to_patterns(v);
                if !pats.is_empty() { decl.outputs.insert(k, pats); }
            }
        }
    }
    // Singular patterns stored under "" sentinel so apply_connect_defaults can expand them.
    if let Ok(v) = connect_tbl.get::<LuaValue>("input") {
        let pats = lua_val_to_patterns(v);
        if !pats.is_empty() { decl.inputs.insert("".into(), pats); }
    }
    if let Ok(v) = connect_tbl.get::<LuaValue>("output") {
        let pats = lua_val_to_patterns(v);
        if !pats.is_empty() { decl.outputs.insert("".into(), pats); }
    }
    decl
}

/// Merge connect patterns from config.toml into `decl` at lower priority.
///
/// Recognised keys (values may be a string or an array of strings):
///   `connect_input`            — pattern(s) applied to every input port (sentinel)
///   `connect_output`           — pattern(s) applied to every output port (sentinel)
///   `connect_{portname}-in`    — pattern(s) for the named input port specifically
///   `connect_{portname}-out`   — pattern(s) for the named output port specifically
///
/// Lua-declared patterns (already in `decl`) take priority via `or_insert_with`.
fn connect_from_toml(mut decl: ConnectDecl, route_cfg: Option<&toml::Table>) -> ConnectDecl {
    if let Some(cfg) = route_cfg {
        for (key, val) in cfg {
            let pats: Vec<String> = match val {
                toml::Value::String(s) => vec![s.clone()],
                toml::Value::Array(arr) => arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect(),
                _ => continue,
            };
            if pats.is_empty() { continue; }
            if key == "connect_input" {
                decl.inputs.entry("".into()).or_insert_with(|| pats);
            } else if key == "connect_output" {
                decl.outputs.entry("".into()).or_insert_with(|| pats);
            } else if let Some(port) = key
                .strip_prefix("connect_")
                .and_then(|s| s.strip_suffix("-in"))
                .filter(|s| !s.is_empty())
            {
                decl.inputs.entry(port.to_string()).or_insert_with(|| pats);
            } else if let Some(port) = key
                .strip_prefix("connect_")
                .and_then(|s| s.strip_suffix("-out"))
                .filter(|s| !s.is_empty())
            {
                decl.outputs.entry(port.to_string()).or_insert_with(|| pats);
            }
        }
    }
    decl
}

/// Run the script once in a temporary Lua VM to extract port layout, connect
/// patterns, and OSC declarations. Falls back to config.toml, then defaults.
fn extract_all_decls(
    script: &str,
    name: &str,
    route_cfg: Option<&toml::Table>,
) -> Result<(PortDecl, ConnectDecl, OscDecl)> {
    let lua = Lua::new();
    setup_extract_lua(&lua, name, route_cfg)?;

    if let Err(e) = lua.load(script).set_name(name).exec() {
        tracing::debug!(
            "[{}] extract_all_decls: script error (will be reported by event loop): {}",
            name, e
        );
        return Ok((
            PortDecl::default(),
            connect_from_toml(ConnectDecl::default(), route_cfg),
            OscDecl::default(),
        ));
    }

    if let Ok(Some(f)) = lua.globals().get::<Option<LuaFunction>>("init") {
        match f.call::<LuaValue>(()) {
            Ok(LuaValue::Table(ref tbl)) => {
                let port_decl = parse_port_decl_from_lua(tbl)?;
                let connect_decl = connect_from_toml(connect_from_lua_table(tbl), route_cfg);
                let osc_decl = osc_from_lua_table(tbl);
                return Ok((port_decl, connect_decl, osc_decl));
            }
            Ok(_) => warn!("[{}] init() did not return a table; using default ports", name),
            Err(e) => warn!("[{}] init() error: {}; using default ports", name, e),
        }
    }

    // No init(), or init() returned a non-table: fall back to config.toml for ports.
    let port_decl = route_cfg
        .and_then(parse_port_decl_from_toml)
        .unwrap_or_default();
    let connect_decl = connect_from_toml(ConnectDecl::default(), route_cfg);
    Ok((port_decl, connect_decl, OscDecl::default()))
}

/// Parse OSC receive/send declarations from the table returned by `init()`.
///
/// ```lua
/// osc = {
///     receive = 9000,                       -- UDP port to listen on
///     send = { synth = "127.0.0.1:9001" },  -- named send targets
/// }
/// ```
fn osc_from_lua_table(tbl: &LuaTable) -> OscDecl {
    let mut decl = OscDecl::default();

    let osc_tbl = match tbl.get::<LuaValue>("osc") {
        Ok(LuaValue::Table(t)) => t,
        _ => return decl,
    };

    let port_num: Option<i64> = match osc_tbl.get::<LuaValue>("receive").unwrap_or(LuaValue::Nil) {
        LuaValue::Integer(n) => Some(n),
        LuaValue::Number(f) if f.fract() == 0.0 => Some(f as i64),
        LuaValue::Number(f) => {
            warn!("OSC receive port must be an integer, got {}", f);
            None
        }
        LuaValue::Nil => None,
        other => {
            warn!("OSC receive port must be an integer, got {}", other.type_name());
            None
        }
    };
    if let Some(n) = port_num {
        if n > 0 && n <= 65535 {
            decl.receive_port = Some(n as u16);
        } else {
            warn!("OSC receive port {} is out of range (1–65535)", n);
        }
    }

    if let Ok(LuaValue::Table(send_tbl)) = osc_tbl.get::<LuaValue>("send") {
        for pair in send_tbl.pairs::<String, LuaValue>() {
            if let Ok((target_name, LuaValue::String(addr_str))) = pair {
                if let Ok(s) = addr_str.to_str() {
                    match parse_socket_addr(&s) {
                        Some(addr) => {
                            decl.send_targets.insert(target_name, addr);
                        }
                        None => warn!("OSC send target '{}': invalid address '{}'", target_name, s),
                    }
                }
            }
        }
    }

    decl
}

fn parse_socket_addr(s: &str) -> Option<SocketAddr> {
    s.parse::<SocketAddr>().ok()
}

// ── Connect default expansion ─────────────────────────────────────────────────

/// Resolve the final per-port connect patterns from a raw `ConnectDecl`.
///
/// `extract_all_decls` stores per-route ("applies to all ports") patterns under
/// the `""` sentinel key. This function:
///   1. Pops the `""` sentinel (route-level pattern).
///   2. Falls back to `global_input`/`global_output` if the sentinel is absent.
///   3. For each named port that has no explicit entry, inserts the resolved pattern.
fn apply_connect_defaults(
    mut raw: ConnectDecl,
    port_inputs: &[String],
    port_outputs: &[String],
    global_input: Option<&str>,
    global_output: Option<&str>,
) -> ConnectDecl {
    // If the route has any input/output patterns (per-port or sentinel), don't use global defaults.
    let has_in  = !raw.inputs.is_empty();
    let has_out = !raw.outputs.is_empty();
    let all_in  = raw.inputs.remove("").or_else(|| if has_in  { None } else { global_input.map(|s| vec![s.to_string()]) });
    let all_out = raw.outputs.remove("").or_else(|| if has_out { None } else { global_output.map(|s| vec![s.to_string()]) });
    for port in port_inputs {
        if !raw.inputs.contains_key(port) {
            if let Some(ref pats) = all_in {
                raw.inputs.insert(port.clone(), pats.clone());
            }
        }
    }
    for port in port_outputs {
        if !raw.outputs.contains_key(port) {
            if let Some(ref pats) = all_out {
                raw.outputs.insert(port.clone(), pats.clone());
            }
        }
    }
    raw
}

fn parse_port_decl_from_lua(tbl: &LuaTable) -> Result<PortDecl> {
    fn extract_names(val: LuaValue) -> Vec<String> {
        match val {
            LuaValue::String(s) => vec![s.to_str().map(|b| b.to_string()).unwrap_or_default()],
            LuaValue::Table(t) => {
                let mut names = Vec::new();
                for i in 1u32.. {
                    match t.get::<LuaValue>(i) {
                        Ok(LuaValue::String(s)) => {
                            names.push(s.to_str().map(|b| b.to_string()).unwrap_or_default())
                        }
                        _ => break,
                    }
                }
                names
            }
            _ => vec![],
        }
    }

    let inputs = extract_names(tbl.get::<LuaValue>("inputs").unwrap_or(LuaValue::Nil));
    let outputs = extract_names(tbl.get::<LuaValue>("outputs").unwrap_or(LuaValue::Nil));

    if inputs.is_empty() || outputs.is_empty() {
        return Ok(PortDecl::default());
    }

    Ok(PortDecl { inputs, outputs })
}

fn parse_port_decl_from_toml(cfg: &toml::Table) -> Option<PortDecl> {
    let to_strings = |arr: &[toml::Value]| -> Vec<String> {
        arr.iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect()
    };

    let inputs = to_strings(cfg.get("inputs")?.as_array()?);
    let outputs = to_strings(cfg.get("outputs")?.as_array()?);

    if inputs.is_empty() || outputs.is_empty() {
        return None;
    }
    Some(PortDecl { inputs, outputs })
}

// ── Lua event loop ────────────────────────────────────────────────────────────

fn run_lua_event_loop(
    name: &str,
    script: &str,
    mut rx: mpsc::Receiver<RouteEvent>,
    out_conns: HashMap<String, Arc<Mutex<MidiOutputConnection>>>,
    default_out: String,
    timer: Arc<Timer>,
    route_cfg: Option<toml::Table>,
    osc_sender: Option<OscSender>,
    osc_heartbeat_interval: f64,
) -> Result<()> {
    let lua = Lua::new();

    // --- Expose `send(msg)` or `send(port_name, msg)` ---
    //
    // One-arg form sends to the first/only output (backward-compatible).
    // Two-arg form selects a named output declared in init().
    {
        let send_fn = lua.create_function(move |_lua, args: LuaMultiValue| -> LuaResult<()> {
            let (port_name, msg_table) = match args.len() {
                1 => {
                    let msg = match args.into_iter().next() {
                        Some(LuaValue::Table(t)) => t,
                        _ => {
                            return Err(LuaError::RuntimeError(
                                "send: expected a message table".into(),
                            ))
                        }
                    };
                    (default_out.clone(), msg)
                }
                2 => {
                    let mut iter = args.into_iter();
                    let port = match iter.next() {
                        Some(LuaValue::String(s)) => {
                            s.to_str().map_err(LuaError::external)?.to_string()
                        }
                        _ => {
                            return Err(LuaError::RuntimeError(
                                "send: first argument must be a port name string".into(),
                            ))
                        }
                    };
                    let msg = match iter.next() {
                        Some(LuaValue::Table(t)) => t,
                        _ => {
                            return Err(LuaError::RuntimeError(
                                "send: second argument must be a message table".into(),
                            ))
                        }
                    };
                    (port, msg)
                }
                n => {
                    return Err(LuaError::RuntimeError(format!(
                        "send: expected 1 or 2 arguments, got {}",
                        n
                    )))
                }
            };

            match out_conns.get(&port_name) {
                Some(conn) => match lua_to_midi_bytes(&msg_table) {
                    Ok(bytes) => {
                        if let Err(e) = conn.lock().unwrap().send(&bytes) {
                            warn!("MIDI send error on port '{}': {}", port_name, e);
                        }
                    }
                    Err(e) => warn!("lua_to_midi_bytes error: {}", e),
                },
                None => warn!("send: unknown output port '{}'", port_name),
            }

            Ok(())
        })?;
        lua.globals().set("send", send_fn)?;
    }

    // --- Expose `set_bpm(bpm)` ---
    {
        let t = Arc::clone(&timer);
        let f = lua.create_function(move |_, bpm: f64| {
            t.set_bpm(bpm);
            Ok(())
        })?;
        lua.globals().set("set_bpm", f)?;
    }

    // --- Expose `get_bpm()` ---
    {
        let t = Arc::clone(&timer);
        let f = lua.create_function(move |_, ()| Ok(t.get_bpm()))?;
        lua.globals().set("get_bpm", f)?;
    }

    // --- Expose `set_ppqn(ppqn)` ---
    {
        let t = Arc::clone(&timer);
        let f = lua.create_function(move |_, ppqn: u32| {
            t.set_ppqn(ppqn);
            Ok(())
        })?;
        lua.globals().set("set_ppqn", f)?;
    }

    // --- Expose `get_ppqn()` ---
    {
        let t = Arc::clone(&timer);
        let f = lua.create_function(move |_, ()| Ok(t.get_ppqn()))?;
        lua.globals().set("get_ppqn", f)?;
    }

    // --- Expose `log(msg)` ---
    {
        let route_name = name.to_string();
        let f = lua.create_function(move |_, msg: String| {
            info!("[{}] {}", route_name, msg);
            Ok(())
        })?;
        lua.globals().set("log", f)?;
    }

    // --- Expose `ROUTE_NAME` and `OSC_SEND_ENABLED` ---
    // Must be set before the send_osc closure captures osc_sender.
    lua.globals().set("ROUTE_NAME", name)?;
    // OSC_SEND_ENABLED: true when the OSC send socket is available — either a named target
    // was declared or a receive port is active (enabling dynamic subscriber sends).
    // Routes can use this to suppress proactive sends when no OSC infrastructure is wired up.
    lua.globals().set("OSC_SEND_ENABLED", osc_sender.is_some())?;

    // Subscriber address cache: updated after every osc_param_set dispatch/tick so the
    // send_osc closure can fan out to subscribers when no named target is configured.
    let subs_cache: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

    // --- Expose `send_osc` ---
    //
    // Three calling forms:
    //   send_osc("/addr", v…)          address-first → named target, or fans out to subscribers
    //   send_osc("name", "/addr", v…)  named target
    //   send_osc("ip:port", "/addr", v…)  ad-hoc address (subscriber replies, notifications)
    {
        let subs_cache_for_send = std::sync::Arc::clone(&subs_cache);
        let f = lua.create_function(move |_, args: LuaMultiValue| -> LuaResult<()> {
            let sender = match &osc_sender {
                Some(s) => s,
                None => {
                    warn!("send_osc: no OSC socket available");
                    return Ok(());
                }
            };

            if args.is_empty() {
                return Err(LuaError::RuntimeError(
                    "send_osc: expected at least an OSC address argument".into(),
                ));
            }

            let first = match &args[0] {
                LuaValue::String(s) => s.to_str().map_err(LuaError::external)?.to_string(),
                _ => return Err(LuaError::RuntimeError(
                    "send_osc: first argument must be a string".into(),
                )),
            };

            // Ad-hoc address form: first arg parses as SocketAddr ("ip:port")
            if let Ok(dest) = first.parse::<SocketAddr>() {
                let address = match args.get(1) {
                    Some(LuaValue::String(s)) => s.to_str().map_err(LuaError::external)?.to_string(),
                    _ => return Err(LuaError::RuntimeError(
                        "send_osc: OSC address (second argument) must be a string".into(),
                    )),
                };
                if !address.starts_with('/') {
                    return Err(LuaError::RuntimeError(format!(
                        "send_osc: OSC address must start with '/', got '{}'", address
                    )));
                }
                let osc_args = args.into_iter().skip(2)
                    .map(|v| lua_val_to_osc_type(&v))
                    .collect::<LuaResult<Vec<_>>>()?;
                if let Err(e) = sender.send_to_addr(dest, address, osc_args) {
                    warn!("OSC send error: {}", e);
                }
                return Ok(());
            }

            let (target, address, arg_start) = if first.starts_with('/') {
                // Address-first: pick the sole named target, fan out to subscribers, or error.
                if sender.targets.len() == 1 {
                    let t = sender.targets.keys().next().unwrap().clone();
                    (t, first, 1usize)
                } else if sender.targets.is_empty() {
                    // No named target: send to all live subscribers instead.
                    let subs: Vec<String> = subs_cache_for_send.lock().unwrap().clone();
                    if !subs.is_empty() {
                        let osc_args = args.into_iter().skip(1)
                            .map(|v| lua_val_to_osc_type(&v))
                            .collect::<LuaResult<Vec<_>>>()?;
                        for sub_addr in &subs {
                            if let Ok(dest) = sub_addr.parse::<SocketAddr>() {
                                if let Err(e) = sender.send_to_addr(dest, first.clone(), osc_args.clone()) {
                                    warn!("OSC send error: {}", e);
                                }
                            }
                        }
                    }
                    return Ok(());
                } else {
                    return Err(LuaError::RuntimeError(
                        "send_osc: multiple targets configured — specify target name as first argument".into(),
                    ));
                }
            } else {
                // Named-target form.
                if !sender.targets.contains_key(&first) {
                    return Err(LuaError::RuntimeError(format!(
                        "send_osc: unknown target '{}'", first
                    )));
                }
                let address = match args.get(1) {
                    Some(LuaValue::String(s)) => s.to_str().map_err(LuaError::external)?.to_string(),
                    _ => return Err(LuaError::RuntimeError(
                        "send_osc: OSC address (second argument) must be a string".into(),
                    )),
                };
                if !address.starts_with('/') {
                    return Err(LuaError::RuntimeError(format!(
                        "send_osc: OSC address must start with '/', got '{}'", address
                    )));
                }
                (first, address, 2usize)
            };

            let osc_args = args.into_iter()
                .skip(arg_start)
                .map(|v| lua_val_to_osc_type(&v))
                .collect::<LuaResult<Vec<_>>>()?;

            if let Err(e) = sender.send(&target, address, osc_args) {
                warn!("OSC send error: {}", e);
            }

            Ok(())
        })?;
        lua.globals().set("send_osc", f)?;
    }

    // --- Expose `config` table ---
    {
        let cfg_table = match route_cfg {
            Some(ref tbl) => toml_table_to_lua(&lua, tbl)
                .map_err(|e| anyhow::anyhow!("Failed to convert route config to Lua: {}", e))?,
            None => lua.create_table()?,
        };
        lua.globals().set("config", cfg_table)?;
    }

    // --- Load the stdlib ---
    lua.load(LUA_STDLIB).set_name("stdlib").exec()
        .map_err(|e| anyhow::anyhow!("Failed to load Lua stdlib: {}", e))?;

    // --- Load the user script ---
    anyhow::Context::with_context(lua.load(script).set_name(name).exec(), || {
        format!("Lua load error in '{}'", name)
    })?;

    // --- Build OscParamSet from init().osc.params if declared ---
    let mut osc_param_set: Option<crate::osc_params::OscParamSet> =
        lua.globals()
            .get::<Option<LuaFunction>>("init")
            .ok()
            .flatten()
            .and_then(|f| f.call::<LuaValue>(()).ok())
            .and_then(|v| if let LuaValue::Table(t) = v { Some(t) } else { None })
            .and_then(|tbl| {
                let prefix = format!("/{}", name);
                match crate::osc_params::from_init_table(&lua, &prefix, &tbl, osc_heartbeat_interval) {
                    Ok(ps) => ps,
                    Err(e) => {
                        warn!("[{}] Failed to build OscParamSet from init(): {}", name, e);
                        None
                    }
                }
            });

    // Cache callbacks once — avoids a global-table lookup on every event.
    let on_midi_fn: Option<LuaFunction> = lua.globals().get("on_midi").ok();
    let on_tick_fn: Option<LuaFunction> = lua.globals().get("on_tick").ok();
    let on_osc_fn: Option<LuaFunction> = lua.globals().get("on_osc").ok();

    // --- Event loop ---
    loop {
        let event = match rx.blocking_recv() {
            Some(e) => e,
            None => break,
        };

        match event {
            RouteEvent::Midi { port, bytes } => {
                let needs_parse = on_midi_fn.is_some() || osc_param_set.is_some();
                if needs_parse {
                    match midi_bytes_to_lua(&lua, &bytes) {
                        Ok(msg) => {
                            let _ = msg.set("port", port.as_str());
                            if let Some(ref mut ps) = osc_param_set {
                                if let Err(e) = ps.dispatch_midi(&lua, &msg) {
                                    warn!("[{}] midi param dispatch error: {}", name, e);
                                }
                            }
                            if let Some(ref on_midi) = on_midi_fn {
                                if let Err(e) = on_midi.call::<()>(msg) {
                                    warn!("[{}] on_midi error: {}", name, e);
                                }
                            }
                        }
                        Err(e) => warn!("[{}] MIDI parse error: {}", name, e),
                    }
                }
            }
            RouteEvent::Timer(TimerEvent::Tick { tick, bpm, ppqn }) => {
                if let Some(ref mut ps) = osc_param_set {
                    if let Err(e) = ps.tick(&lua) {
                        warn!("[{}] osc_params tick error: {}", name, e);
                    }
                    *subs_cache.lock().unwrap() = ps.subscriber_addrs();
                }
                if let Some(ref on_tick) = on_tick_fn {
                    if let Err(e) = on_tick.call::<()>((tick, bpm, ppqn)) {
                        warn!("[{}] on_tick error: {}", name, e);
                    }
                }
            }
            RouteEvent::Osc { from, address, args } => {
                match osc_message_to_lua(&lua, &address, &args) {
                    Ok(msg) => {
                        let _ = msg.set("from", from.to_string());
                        if let Some(ref mut ps) = osc_param_set {
                            if let Err(e) = ps.dispatch(&lua, &msg) {
                                warn!("[{}] osc_params dispatch error: {}", name, e);
                            }
                            *subs_cache.lock().unwrap() = ps.subscriber_addrs();
                        }
                        if let Some(ref on_osc) = on_osc_fn {
                            if let Err(e) = on_osc.call::<()>(msg) {
                                warn!("[{}] on_osc error: {}", name, e);
                            }
                        } else if osc_param_set.is_none() {
                            warn!("[{}] OSC message '{}' received but no on_osc handler defined", name, address);
                        }
                    }
                    Err(e) => warn!("[{}] OSC message parse error: {}", name, e),
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mlua::Lua;

    // ── PortDecl ──────────────────────────────────────────────────────────────

    #[test]
    fn port_decl_default_has_single_default_input_and_output() {
        let d = PortDecl::default();
        assert_eq!(d.inputs, vec!["default"]);
        assert_eq!(d.outputs, vec!["default"]);
    }

    #[test]
    fn port_decl_is_default_true_for_default() {
        assert!(PortDecl::default().is_default());
    }

    #[test]
    fn port_decl_is_default_false_for_custom_input_name() {
        let d = PortDecl {
            inputs: vec!["keyboard".to_string()],
            outputs: vec!["default".to_string()],
        };
        assert!(!d.is_default());
    }

    #[test]
    fn port_decl_is_default_false_for_custom_output_name() {
        let d = PortDecl {
            inputs: vec!["default".to_string()],
            outputs: vec!["synth".to_string()],
        };
        assert!(!d.is_default());
    }

    #[test]
    fn port_decl_is_default_false_for_multiple_inputs() {
        let d = PortDecl {
            inputs: vec!["default".to_string(), "extra".to_string()],
            outputs: vec!["default".to_string()],
        };
        assert!(!d.is_default());
    }

    #[test]
    fn port_decl_is_default_false_for_multiple_outputs() {
        let d = PortDecl {
            inputs: vec!["default".to_string()],
            outputs: vec!["default".to_string(), "extra".to_string()],
        };
        assert!(!d.is_default());
    }

    #[test]
    fn port_decl_is_default_false_for_empty_inputs() {
        let d = PortDecl {
            inputs: vec![],
            outputs: vec!["default".to_string()],
        };
        assert!(!d.is_default());
    }

    #[test]
    fn port_decl_equality_same_ports() {
        let a = PortDecl {
            inputs: vec!["kbd".to_string()],
            outputs: vec!["synth".to_string()],
        };
        let b = a.clone();
        assert_eq!(a, b);
    }

    #[test]
    fn port_decl_inequality_different_input_names() {
        let a = PortDecl {
            inputs: vec!["kbd".to_string()],
            outputs: vec!["synth".to_string()],
        };
        let b = PortDecl {
            inputs: vec!["pad".to_string()],
            outputs: vec!["synth".to_string()],
        };
        assert_ne!(a, b);
    }

    #[test]
    fn port_decl_inequality_different_output_names() {
        let a = PortDecl {
            inputs: vec!["kbd".to_string()],
            outputs: vec!["synth".to_string()],
        };
        let b = PortDecl {
            inputs: vec!["kbd".to_string()],
            outputs: vec!["drums".to_string()],
        };
        assert_ne!(a, b);
    }

    #[test]
    fn port_decl_inequality_input_order_matters() {
        let a = PortDecl {
            inputs: vec!["x".to_string(), "y".to_string()],
            outputs: vec!["z".to_string()],
        };
        let b = PortDecl {
            inputs: vec!["y".to_string(), "x".to_string()],
            outputs: vec!["z".to_string()],
        };
        assert_ne!(a, b);
    }

    #[test]
    fn port_decl_inequality_output_order_matters() {
        let a = PortDecl {
            inputs: vec!["x".to_string()],
            outputs: vec!["a".to_string(), "b".to_string()],
        };
        let b = PortDecl {
            inputs: vec!["x".to_string()],
            outputs: vec!["b".to_string(), "a".to_string()],
        };
        assert_ne!(a, b);
    }

    // ── parse_port_decl_from_toml ─────────────────────────────────────────────

    fn toml_section(s: &str) -> toml::Table {
        toml::from_str(s).unwrap()
    }

    #[test]
    fn toml_single_input_and_output() {
        let tbl = toml_section("inputs = [\"kbd\"]\noutputs = [\"synth\"]");
        let decl = parse_port_decl_from_toml(&tbl).unwrap();
        assert_eq!(decl.inputs, vec!["kbd"]);
        assert_eq!(decl.outputs, vec!["synth"]);
    }

    #[test]
    fn toml_multiple_inputs_and_outputs() {
        let tbl = toml_section("inputs = [\"kbd\", \"pad\"]\noutputs = [\"synth\", \"drums\"]");
        let decl = parse_port_decl_from_toml(&tbl).unwrap();
        assert_eq!(decl.inputs, vec!["kbd", "pad"]);
        assert_eq!(decl.outputs, vec!["synth", "drums"]);
    }

    #[test]
    fn toml_missing_inputs_returns_none() {
        let tbl = toml_section("outputs = [\"synth\"]");
        assert!(parse_port_decl_from_toml(&tbl).is_none());
    }

    #[test]
    fn toml_missing_outputs_returns_none() {
        let tbl = toml_section("inputs = [\"kbd\"]");
        assert!(parse_port_decl_from_toml(&tbl).is_none());
    }

    #[test]
    fn toml_empty_inputs_array_returns_none() {
        let tbl = toml_section("inputs = []\noutputs = [\"synth\"]");
        assert!(parse_port_decl_from_toml(&tbl).is_none());
    }

    #[test]
    fn toml_empty_outputs_array_returns_none() {
        let tbl = toml_section("inputs = [\"kbd\"]\noutputs = []");
        assert!(parse_port_decl_from_toml(&tbl).is_none());
    }

    #[test]
    fn toml_empty_table_returns_none() {
        let tbl = toml_section("");
        assert!(parse_port_decl_from_toml(&tbl).is_none());
    }

    #[test]
    fn toml_non_string_values_only_returns_none() {
        let tbl = toml_section("inputs = [1, 2]\noutputs = [\"synth\"]");
        assert!(parse_port_decl_from_toml(&tbl).is_none());
    }

    #[test]
    fn toml_mixed_string_and_non_string_keeps_strings() {
        let tbl = toml_section("inputs = [\"kbd\", 42]\noutputs = [\"synth\"]");
        let decl = parse_port_decl_from_toml(&tbl).unwrap();
        assert_eq!(decl.inputs, vec!["kbd"]);
    }

    #[test]
    fn toml_preserves_port_order() {
        let tbl = toml_section("inputs = [\"z\", \"a\", \"m\"]\noutputs = [\"out\"]");
        let decl = parse_port_decl_from_toml(&tbl).unwrap();
        assert_eq!(decl.inputs, vec!["z", "a", "m"]);
    }

    #[test]
    fn toml_extra_keys_are_ignored() {
        let tbl = toml_section(
            "inputs = [\"kbd\"]\noutputs = [\"synth\"]\nbpm = 120\nchannel = 1",
        );
        let decl = parse_port_decl_from_toml(&tbl).unwrap();
        assert_eq!(decl.inputs, vec!["kbd"]);
        assert_eq!(decl.outputs, vec!["synth"]);
    }

    // ── parse_port_decl_from_lua ──────────────────────────────────────────────

    fn lua_array(lua: &Lua, items: &[&str]) -> LuaTable {
        let t = lua.create_table().unwrap();
        for (i, s) in items.iter().enumerate() {
            t.set(i + 1, *s).unwrap();
        }
        t
    }

    fn lua_decl_table(lua: &Lua, inputs: &[&str], outputs: &[&str]) -> LuaTable {
        let tbl = lua.create_table().unwrap();
        tbl.set("inputs", lua_array(lua, inputs)).unwrap();
        tbl.set("outputs", lua_array(lua, outputs)).unwrap();
        tbl
    }

    #[test]
    fn lua_single_input_and_output() {
        let lua = Lua::new();
        let tbl = lua_decl_table(&lua, &["kbd"], &["synth"]);
        let decl = parse_port_decl_from_lua(&tbl).unwrap();
        assert_eq!(decl.inputs, vec!["kbd"]);
        assert_eq!(decl.outputs, vec!["synth"]);
    }

    #[test]
    fn lua_multiple_inputs_and_outputs() {
        let lua = Lua::new();
        let tbl = lua_decl_table(&lua, &["kbd", "pad"], &["synth", "drums"]);
        let decl = parse_port_decl_from_lua(&tbl).unwrap();
        assert_eq!(decl.inputs, vec!["kbd", "pad"]);
        assert_eq!(decl.outputs, vec!["synth", "drums"]);
    }

    #[test]
    fn lua_string_shorthand_for_inputs() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        tbl.set("inputs", "kbd").unwrap();
        tbl.set("outputs", lua_array(&lua, &["synth"])).unwrap();
        let decl = parse_port_decl_from_lua(&tbl).unwrap();
        assert_eq!(decl.inputs, vec!["kbd"]);
    }

    #[test]
    fn lua_string_shorthand_for_outputs() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        tbl.set("inputs", lua_array(&lua, &["kbd"])).unwrap();
        tbl.set("outputs", "synth").unwrap();
        let decl = parse_port_decl_from_lua(&tbl).unwrap();
        assert_eq!(decl.outputs, vec!["synth"]);
    }

    #[test]
    fn lua_string_shorthand_for_both() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        tbl.set("inputs", "kbd").unwrap();
        tbl.set("outputs", "synth").unwrap();
        let decl = parse_port_decl_from_lua(&tbl).unwrap();
        assert_eq!(decl.inputs, vec!["kbd"]);
        assert_eq!(decl.outputs, vec!["synth"]);
    }

    #[test]
    fn lua_missing_inputs_returns_default() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        tbl.set("outputs", lua_array(&lua, &["synth"])).unwrap();
        let decl = parse_port_decl_from_lua(&tbl).unwrap();
        assert!(decl.is_default());
    }

    #[test]
    fn lua_missing_outputs_returns_default() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        tbl.set("inputs", lua_array(&lua, &["kbd"])).unwrap();
        let decl = parse_port_decl_from_lua(&tbl).unwrap();
        assert!(decl.is_default());
    }

    #[test]
    fn lua_empty_table_returns_default() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        let decl = parse_port_decl_from_lua(&tbl).unwrap();
        assert!(decl.is_default());
    }

    #[test]
    fn lua_empty_inputs_array_returns_default() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        tbl.set("inputs", lua.create_table().unwrap()).unwrap();
        tbl.set("outputs", lua_array(&lua, &["synth"])).unwrap();
        let decl = parse_port_decl_from_lua(&tbl).unwrap();
        assert!(decl.is_default());
    }

    #[test]
    fn lua_empty_outputs_array_returns_default() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        tbl.set("inputs", lua_array(&lua, &["kbd"])).unwrap();
        tbl.set("outputs", lua.create_table().unwrap()).unwrap();
        let decl = parse_port_decl_from_lua(&tbl).unwrap();
        assert!(decl.is_default());
    }

    #[test]
    fn lua_preserves_port_order() {
        let lua = Lua::new();
        let tbl = lua_decl_table(&lua, &["z", "a", "m"], &["out"]);
        let decl = parse_port_decl_from_lua(&tbl).unwrap();
        assert_eq!(decl.inputs, vec!["z", "a", "m"]);
    }

    #[test]
    fn lua_non_string_in_array_stops_iteration() {
        // The iterator stops at the first non-string entry.
        let lua = Lua::new();
        let inp = lua.create_table().unwrap();
        inp.set(1, "kbd").unwrap();
        inp.set(2, 42i64).unwrap();
        inp.set(3, "pad").unwrap(); // unreachable — iteration stopped at [2]
        let tbl = lua.create_table().unwrap();
        tbl.set("inputs", inp).unwrap();
        tbl.set("outputs", lua_array(&lua, &["synth"])).unwrap();
        let decl = parse_port_decl_from_lua(&tbl).unwrap();
        assert_eq!(decl.inputs, vec!["kbd"]);
    }

    #[test]
    fn lua_nil_value_stops_iteration() {
        let lua = Lua::new();
        let inp = lua.create_table().unwrap();
        inp.set(1, "kbd").unwrap();
        // index 2 is nil (absent) — iteration stops
        inp.set(3, "pad").unwrap();
        let tbl = lua.create_table().unwrap();
        tbl.set("inputs", inp).unwrap();
        tbl.set("outputs", lua_array(&lua, &["synth"])).unwrap();
        let decl = parse_port_decl_from_lua(&tbl).unwrap();
        assert_eq!(decl.inputs, vec!["kbd"]);
    }

    // ── extract_port_decl ─────────────────────────────────────────────────────

    fn extract(script: &str) -> PortDecl {
        extract_all_decls(script, "test", None).unwrap().0
    }

    fn extract_with_cfg(script: &str, cfg_toml: &str) -> PortDecl {
        let tbl: toml::Table = toml::from_str(cfg_toml).unwrap();
        extract_all_decls(script, "test", Some(&tbl)).unwrap().0
    }

    #[test]
    fn extract_no_init_no_config_returns_default() {
        assert!(extract("-- no init").is_default());
    }

    #[test]
    fn extract_init_returns_named_ports() {
        let decl = extract(r#"
            function init()
                return { inputs = {"kbd", "pad"}, outputs = {"synth", "drums"} }
            end
        "#);
        assert_eq!(decl.inputs, vec!["kbd", "pad"]);
        assert_eq!(decl.outputs, vec!["synth", "drums"]);
    }

    #[test]
    fn extract_init_single_port_each() {
        let decl = extract(r#"
            function init()
                return { inputs = {"kbd"}, outputs = {"synth"} }
            end
        "#);
        assert_eq!(decl.inputs, vec!["kbd"]);
        assert_eq!(decl.outputs, vec!["synth"]);
    }

    #[test]
    fn extract_init_string_shorthand_for_both() {
        let decl = extract(r#"
            function init()
                return { inputs = "kbd", outputs = "synth" }
            end
        "#);
        assert_eq!(decl.inputs, vec!["kbd"]);
        assert_eq!(decl.outputs, vec!["synth"]);
    }

    #[test]
    fn extract_init_string_shorthand_for_inputs_only() {
        let decl = extract(r#"
            function init()
                return { inputs = "kbd", outputs = {"synth", "drums"} }
            end
        "#);
        assert_eq!(decl.inputs, vec!["kbd"]);
        assert_eq!(decl.outputs, vec!["synth", "drums"]);
    }

    #[test]
    fn extract_init_many_ports() {
        let decl = extract(r#"
            function init()
                return {
                    inputs  = {"in1", "in2", "in3", "in4"},
                    outputs = {"out1", "out2", "out3"},
                }
            end
        "#);
        assert_eq!(decl.inputs, vec!["in1", "in2", "in3", "in4"]);
        assert_eq!(decl.outputs, vec!["out1", "out2", "out3"]);
    }

    #[test]
    fn extract_init_overrides_config_toml() {
        let decl = extract_with_cfg(
            r#"
            function init()
                return { inputs = {"lua-in"}, outputs = {"lua-out"} }
            end
            "#,
            "inputs = [\"toml-in\"]\noutputs = [\"toml-out\"]",
        );
        assert_eq!(decl.inputs, vec!["lua-in"]);
        assert_eq!(decl.outputs, vec!["lua-out"]);
    }

    #[test]
    fn extract_config_toml_used_when_no_init() {
        let decl = extract_with_cfg(
            "-- no init",
            "inputs = [\"kbd\"]\noutputs = [\"synth\"]",
        );
        assert_eq!(decl.inputs, vec!["kbd"]);
        assert_eq!(decl.outputs, vec!["synth"]);
    }

    #[test]
    fn extract_init_returning_nil_falls_back_to_toml() {
        let decl = extract_with_cfg(
            r#"
            function init()
                return nil
            end
            "#,
            "inputs = [\"kbd\"]\noutputs = [\"synth\"]",
        );
        assert_eq!(decl.inputs, vec!["kbd"]);
        assert_eq!(decl.outputs, vec!["synth"]);
    }

    #[test]
    fn extract_init_returning_non_table_falls_back_to_toml() {
        let decl = extract_with_cfg(
            r#"
            function init()
                return "not a table"
            end
            "#,
            "inputs = [\"kbd\"]\noutputs = [\"synth\"]",
        );
        assert_eq!(decl.inputs, vec!["kbd"]);
        assert_eq!(decl.outputs, vec!["synth"]);
    }

    #[test]
    fn extract_init_returning_non_table_falls_back_to_default_when_no_toml() {
        let decl = extract(r#"
            function init()
                return 42
            end
        "#);
        assert!(decl.is_default());
    }

    #[test]
    fn extract_init_error_falls_back_to_toml() {
        let decl = extract_with_cfg(
            r#"
            function init()
                error("something went wrong")
            end
            "#,
            "inputs = [\"kbd\"]\noutputs = [\"synth\"]",
        );
        assert_eq!(decl.inputs, vec!["kbd"]);
        assert_eq!(decl.outputs, vec!["synth"]);
    }

    #[test]
    fn extract_init_error_falls_back_to_default_when_no_toml() {
        let decl = extract(r#"
            function init()
                error("oops")
            end
        "#);
        assert!(decl.is_default());
    }

    #[test]
    fn extract_script_syntax_error_returns_default() {
        // Pre-run errors are silently swallowed; the real event loop will surface them.
        let decl = extract("this is ][ not valid lua");
        assert!(decl.is_default());
    }

    #[test]
    fn extract_init_empty_inputs_returns_default_not_toml() {
        // init() returns a valid table but empty inputs → default ports.
        // We do NOT fall through to config.toml in this case.
        let decl = extract_with_cfg(
            r#"
            function init()
                return { inputs = {}, outputs = {"synth"} }
            end
            "#,
            "inputs = [\"kbd\"]\noutputs = [\"synth\"]",
        );
        assert!(decl.is_default());
    }

    #[test]
    fn extract_init_empty_outputs_returns_default_not_toml() {
        let decl = extract_with_cfg(
            r#"
            function init()
                return { inputs = {"kbd"}, outputs = {} }
            end
            "#,
            "inputs = [\"kbd\"]\noutputs = [\"synth\"]",
        );
        assert!(decl.is_default());
    }

    #[test]
    fn extract_config_toml_empty_inputs_returns_default() {
        let decl = extract_with_cfg("-- no init", "inputs = []\noutputs = [\"synth\"]");
        assert!(decl.is_default());
    }

    #[test]
    fn extract_config_toml_empty_outputs_returns_default() {
        let decl = extract_with_cfg("-- no init", "inputs = [\"kbd\"]\noutputs = []");
        assert!(decl.is_default());
    }

    #[test]
    fn extract_backward_compat_script_without_init() {
        let decl = extract(r#"
            function on_midi(msg)
                send(msg)
            end
            function on_tick(tick, bpm, ppqn)
            end
        "#);
        assert!(decl.is_default());
    }

    #[test]
    fn extract_script_can_use_config_global_in_init() {
        let decl = extract_with_cfg(
            r#"
            function init()
                local n = config.label or "fallback"
                return { inputs = {n}, outputs = {"out"} }
            end
            "#,
            "label = \"my-input\"\ninputs = [\"toml-in\"]\noutputs = [\"toml-out\"]",
        );
        assert_eq!(decl.inputs, vec!["my-input"]);
        assert_eq!(decl.outputs, vec!["out"]);
    }

    #[test]
    fn extract_script_can_call_log_in_init() {
        let decl = extract(r#"
            function init()
                log("setting up ports")
                return { inputs = {"kbd"}, outputs = {"synth"} }
            end
        "#);
        assert_eq!(decl.inputs, vec!["kbd"]);
        assert_eq!(decl.outputs, vec!["synth"]);
    }

    #[test]
    fn extract_top_level_code_runs_without_real_connections() {
        // Scripts may call send/log/etc at top level; stubs prevent crashes.
        let decl = extract(r#"
            log("top-level init")
            local x = get_bpm()
            function init()
                return { inputs = {"in"}, outputs = {"out"} }
            end
        "#);
        assert_eq!(decl.inputs, vec!["in"]);
        assert_eq!(decl.outputs, vec!["out"]);
    }

    // ── extract_connect_decl ──────────────────────────────────────────────────

    fn connect(script: &str) -> ConnectDecl {
        extract_all_decls(script, "test", None).unwrap().1
    }

    fn connect_with_cfg(script: &str, cfg_toml: &str) -> ConnectDecl {
        let tbl: toml::Table = toml::from_str(cfg_toml).unwrap();
        extract_all_decls(script, "test", Some(&tbl)).unwrap().1
    }

    #[test]
    fn connect_no_init_no_toml_returns_empty() {
        let c = connect("-- no init");
        assert!(c.inputs.is_empty());
        assert!(c.outputs.is_empty());
    }

    fn pats(strs: &[&str]) -> Vec<String> {
        strs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn connect_lua_singular_input_stored_under_sentinel() {
        let c = connect(r#"
            function init()
                return { inputs = {"kbd"}, outputs = {"synth"},
                         connect = { input = ".*KeyLab.*" } }
            end
        "#);
        assert_eq!(c.inputs.get(""), Some(&pats(&[".*KeyLab.*"])));
    }

    #[test]
    fn connect_lua_singular_output_stored_under_sentinel() {
        let c = connect(r#"
            function init()
                return { inputs = {"kbd"}, outputs = {"synth"},
                         connect = { output = ".*Surge.*" } }
            end
        "#);
        assert_eq!(c.outputs.get(""), Some(&pats(&[".*Surge.*"])));
    }

    #[test]
    fn connect_lua_singular_input_array_stored_under_sentinel() {
        let c = connect(r#"
            function init()
                return { inputs = {"kbd"}, outputs = {"synth"},
                         connect = { input = {".*KeyLab.*", ".*A-PRO.*"} } }
            end
        "#);
        assert_eq!(c.inputs.get(""), Some(&pats(&[".*KeyLab.*", ".*A-PRO.*"])));
    }

    #[test]
    fn connect_lua_per_port_inputs_stored_by_name() {
        let c = connect(r#"
            function init()
                return { inputs = {"kbd", "pad"}, outputs = {"synth"},
                         connect = { inputs = { kbd = ".*KORG.*", pad = ".*Alesis.*" } } }
            end
        "#);
        assert_eq!(c.inputs.get("kbd"), Some(&pats(&[".*KORG.*"])));
        assert_eq!(c.inputs.get("pad"), Some(&pats(&[".*Alesis.*"])));
        assert!(!c.inputs.contains_key(""));
    }

    #[test]
    fn connect_lua_per_port_inputs_array_stored_by_name() {
        let c = connect(r#"
            function init()
                return { inputs = {"kbd"}, outputs = {"synth"},
                         connect = { inputs = { kbd = {".*KORG.*", ".*KeyLab.*"} } } }
            end
        "#);
        assert_eq!(c.inputs.get("kbd"), Some(&pats(&[".*KORG.*", ".*KeyLab.*"])));
    }

    #[test]
    fn connect_lua_per_port_outputs_stored_by_name() {
        let c = connect(r#"
            function init()
                return { inputs = {"kbd"}, outputs = {"synth", "drums"},
                         connect = { outputs = { synth = ".*Surge.*", drums = ".*DrumKit.*" } } }
            end
        "#);
        assert_eq!(c.outputs.get("synth"), Some(&pats(&[".*Surge.*"])));
        assert_eq!(c.outputs.get("drums"), Some(&pats(&[".*DrumKit.*"])));
    }

    #[test]
    fn connect_lua_per_port_and_singular_both_stored() {
        // Per-port entries go by name; singular goes under "".
        let c = connect(r#"
            function init()
                return { inputs = {"kbd", "pad"}, outputs = {"synth"},
                         connect = {
                             inputs = { kbd = ".*KORG.*" },
                             input  = ".*Fallback.*",
                         } }
            end
        "#);
        assert_eq!(c.inputs.get("kbd"), Some(&pats(&[".*KORG.*"])));
        assert_eq!(c.inputs.get(""), Some(&pats(&[".*Fallback.*"])));
    }

    #[test]
    fn connect_lua_no_connect_key_returns_empty() {
        let c = connect(r#"
            function init()
                return { inputs = {"kbd"}, outputs = {"synth"} }
            end
        "#);
        assert!(c.inputs.is_empty());
        assert!(c.outputs.is_empty());
    }

    #[test]
    fn connect_toml_connect_input_stored_under_sentinel() {
        let c = connect_with_cfg("-- no init", "connect_input = \".*KeyLab.*\"");
        assert_eq!(c.inputs.get(""), Some(&pats(&[".*KeyLab.*"])));
    }

    #[test]
    fn connect_toml_connect_input_array_stored_under_sentinel() {
        let c = connect_with_cfg("-- no init", "connect_input = [\".*KeyLab.*\", \".*A-PRO.*\"]");
        assert_eq!(c.inputs.get(""), Some(&pats(&[".*KeyLab.*", ".*A-PRO.*"])));
    }

    #[test]
    fn connect_toml_connect_output_stored_under_sentinel() {
        let c = connect_with_cfg("-- no init", "connect_output = \".*Surge.*\"");
        assert_eq!(c.outputs.get(""), Some(&pats(&[".*Surge.*"])));
    }

    #[test]
    fn connect_toml_per_port_input_stored_by_name() {
        let c = connect_with_cfg("-- no init", "\"connect_keyboard-in\" = \".*A-PRO.*\"");
        assert_eq!(c.inputs.get("keyboard"), Some(&pats(&[".*A-PRO.*"])));
        assert!(!c.inputs.contains_key(""));
    }

    #[test]
    fn connect_toml_per_port_input_array_stored_by_name() {
        let c = connect_with_cfg("-- no init", "\"connect_keyboard-in\" = [\".*A-PRO.*\", \".*KeyLab.*\"]");
        assert_eq!(c.inputs.get("keyboard"), Some(&pats(&[".*A-PRO.*", ".*KeyLab.*"])));
    }

    #[test]
    fn connect_toml_per_port_output_stored_by_name() {
        let c = connect_with_cfg("-- no init", "\"connect_synth-out\" = \".*Surge.*\"");
        assert_eq!(c.outputs.get("synth"), Some(&pats(&[".*Surge.*"])));
        assert!(!c.outputs.contains_key(""));
    }

    #[test]
    fn connect_toml_per_port_multiple_inputs() {
        let c = connect_with_cfg(
            "-- no init",
            "\"connect_keyboard-in\" = \".*A-PRO.*\"\n\"connect_metronome-in\" = \".*metronome-out.*\"",
        );
        assert_eq!(c.inputs.get("keyboard"), Some(&pats(&[".*A-PRO.*"])));
        assert_eq!(c.inputs.get("metronome"), Some(&pats(&[".*metronome-out.*"])));
    }

    #[test]
    fn connect_toml_per_port_input_not_overridden_by_sentinel() {
        // per-port key takes priority over the all-inputs sentinel in the same section
        let c = connect_with_cfg(
            "-- no init",
            "connect_input = \".*Fallback.*\"\n\"connect_keyboard-in\" = \".*A-PRO.*\"",
        );
        assert_eq!(c.inputs.get("keyboard"), Some(&pats(&[".*A-PRO.*"])));
        assert_eq!(c.inputs.get(""), Some(&pats(&[".*Fallback.*"])));
    }

    #[test]
    fn connect_lua_per_port_overrides_toml_per_port() {
        // Lua connect pattern wins over TOML connect pattern for the same port.
        let c = connect_with_cfg(
            r#"
            function init()
                return { inputs = {"keyboard"}, outputs = {"pan"},
                         connect = { inputs = { keyboard = ".*LuaDevice.*" } } }
            end
            "#,
            "\"connect_keyboard-in\" = \".*TomlDevice.*\"",
        );
        assert_eq!(c.inputs.get("keyboard"), Some(&pats(&[".*LuaDevice.*"])));
    }

    #[test]
    fn connect_lua_singular_overrides_toml_sentinel() {
        // Lua stores "" first; toml's or_insert won't overwrite.
        let c = connect_with_cfg(
            r#"
            function init()
                return { inputs = {"kbd"}, outputs = {"synth"},
                         connect = { input = ".*LuaPattern.*" } }
            end
            "#,
            "connect_input = \".*TomlPattern.*\"",
        );
        assert_eq!(c.inputs.get(""), Some(&pats(&[".*LuaPattern.*"])));
    }

    #[test]
    fn connect_toml_used_when_no_lua_connect() {
        let c = connect_with_cfg(
            r#"
            function init()
                return { inputs = {"kbd"}, outputs = {"synth"} }
            end
            "#,
            "connect_input = \".*TomlPattern.*\"\nconnect_output = \".*TomlOut.*\"",
        );
        assert_eq!(c.inputs.get(""), Some(&pats(&[".*TomlPattern.*"])));
        assert_eq!(c.outputs.get(""), Some(&pats(&[".*TomlOut.*"])));
    }

    #[test]
    fn connect_script_error_returns_empty_falls_back_to_toml() {
        let c = connect_with_cfg(
            "this is ][ not valid lua",
            "connect_input = \".*Fallback.*\"",
        );
        assert_eq!(c.inputs.get(""), Some(&pats(&[".*Fallback.*"])));
    }

    // ── apply_connect_defaults ────────────────────────────────────────────────

    fn ports(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn decl_with_sentinel(sentinel: &str) -> ConnectDecl {
        let mut d = ConnectDecl::default();
        d.inputs.insert("".into(), vec![sentinel.to_string()]);
        d
    }

    #[test]
    fn defaults_empty_decl_no_globals_stays_empty() {
        let result = apply_connect_defaults(
            ConnectDecl::default(), &ports(&["default"]), &ports(&["default"]), None, None,
        );
        assert!(result.inputs.is_empty());
        assert!(result.outputs.is_empty());
    }

    #[test]
    fn defaults_global_fills_all_ports() {
        let result = apply_connect_defaults(
            ConnectDecl::default(),
            &ports(&["kbd", "pad"]),
            &ports(&["synth"]),
            Some(".*MyController.*"),
            Some(".*MySynth.*"),
        );
        assert_eq!(result.inputs.get("kbd"), Some(&pats(&[".*MyController.*"])));
        assert_eq!(result.inputs.get("pad"), Some(&pats(&[".*MyController.*"])));
        assert_eq!(result.outputs.get("synth"), Some(&pats(&[".*MySynth.*"])));
    }

    #[test]
    fn defaults_sentinel_fills_all_ports() {
        let mut raw = ConnectDecl::default();
        raw.inputs.insert("".into(), vec![".*RouteLevel.*".to_string()]);
        let result = apply_connect_defaults(
            raw, &ports(&["kbd", "pad"]), &ports(&["synth"]), None, None,
        );
        assert_eq!(result.inputs.get("kbd"), Some(&pats(&[".*RouteLevel.*"])));
        assert_eq!(result.inputs.get("pad"), Some(&pats(&[".*RouteLevel.*"])));
        assert!(!result.inputs.contains_key(""));
    }

    #[test]
    fn defaults_sentinel_takes_priority_over_global() {
        let raw = decl_with_sentinel(".*RouteLevel.*");
        let result = apply_connect_defaults(
            raw, &ports(&["kbd"]), &ports(&[]), Some(".*Global.*"), None,
        );
        assert_eq!(result.inputs.get("kbd"), Some(&pats(&[".*RouteLevel.*"])));
    }

    #[test]
    fn defaults_per_port_not_overridden_by_global() {
        let mut raw = ConnectDecl::default();
        raw.inputs.insert("kbd".into(), vec![".*PerPort.*".to_string()]);
        let result = apply_connect_defaults(
            raw, &ports(&["kbd"]), &ports(&[]), Some(".*Global.*"), None,
        );
        assert_eq!(result.inputs.get("kbd"), Some(&pats(&[".*PerPort.*"])));
    }

    #[test]
    fn defaults_per_port_not_overridden_by_sentinel() {
        let mut raw = ConnectDecl::default();
        raw.inputs.insert("kbd".into(), vec![".*PerPort.*".to_string()]);
        raw.inputs.insert("".into(), vec![".*Sentinel.*".to_string()]);
        let result = apply_connect_defaults(
            raw, &ports(&["kbd", "pad"]), &ports(&[]), None, None,
        );
        assert_eq!(result.inputs.get("kbd"), Some(&pats(&[".*PerPort.*"])));
        // pad has no per-port entry, falls back to sentinel
        assert_eq!(result.inputs.get("pad"), Some(&pats(&[".*Sentinel.*"])));
    }

    #[test]
    fn defaults_per_port_on_one_input_suppresses_global_for_other_inputs() {
        // Route has a per-port entry for "kbd" but nothing for "pad".
        // Because the route specified any connect pattern, global must not be applied to "pad".
        let mut raw = ConnectDecl::default();
        raw.inputs.insert("kbd".into(), vec![".*PerPort.*".to_string()]);
        let result = apply_connect_defaults(
            raw, &ports(&["kbd", "pad"]), &ports(&[]), Some(".*Global.*"), None,
        );
        assert_eq!(result.inputs.get("kbd"), Some(&pats(&[".*PerPort.*"])));
        assert!(!result.inputs.contains_key("pad"), "global must not fill 'pad' when route has any connect pattern");
    }

    #[test]
    fn defaults_per_port_on_one_output_suppresses_global_for_other_outputs() {
        let mut raw = ConnectDecl::default();
        raw.outputs.insert("synth".into(), vec![".*PerPort.*".to_string()]);
        let result = apply_connect_defaults(
            raw, &ports(&[]), &ports(&["synth", "drums"]), None, Some(".*Global.*"),
        );
        assert_eq!(result.outputs.get("synth"), Some(&pats(&[".*PerPort.*"])));
        assert!(!result.outputs.contains_key("drums"), "global must not fill 'drums' when route has any connect pattern");
    }

    #[test]
    fn defaults_sentinel_removed_from_final_map() {
        let raw = decl_with_sentinel(".*Pat.*");
        let result = apply_connect_defaults(raw, &ports(&["kbd"]), &ports(&[]), None, None);
        assert!(!result.inputs.contains_key(""));
    }
}
