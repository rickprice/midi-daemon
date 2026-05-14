use anyhow::{Context, Result};
use midir::os::unix::{VirtualInput, VirtualOutput};
use midir::{MidiInput, MidiInputConnection, MidiOutput, MidiOutputConnection};
use mlua::prelude::*;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

/// Connect patterns for a route's ports: port_name → regex string.
#[derive(Debug, Clone, Default)]
pub struct ConnectDecl {
    pub inputs: HashMap<String, String>,
    pub outputs: HashMap<String, String>,
}

use crate::config::Config;
use crate::lua_api::{lua_to_midi_bytes, midi_bytes_to_lua, toml_table_to_lua};
use crate::timer::{Timer, TimerEvent};

enum RouteEvent {
    Midi { port: String, bytes: Vec<u8> },
    Timer(TimerEvent),
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

/// A running route: owns its MIDI ports, Lua VM, and timer.
/// Dropping this stops everything cleanly.
pub struct Route {
    ports: Arc<RoutePorts>,
    _timer: Arc<Timer>,
    _thread: std::thread::JoinHandle<()>,
    pub connect_decl: ConnectDecl,
}

impl Route {
    /// Consume the route and return its ports so they can be reused on reload.
    pub fn take_ports(self) -> Arc<RoutePorts> {
        self.ports
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

        // Run the script once to extract both port layout and connect patterns.
        let (decl, raw_connect) = extract_all_decls(&script, &name, route_cfg.as_ref())?;

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

        let thread = std::thread::spawn(move || {
            if let Err(e) = run_lua_event_loop(
                &name_for_thread,
                &script,
                rx,
                out_conns_for_thread,
                default_out,
                timer_for_thread,
                route_cfg,
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
            connect_decl,
        })
    }
}

// ── Extraction helpers ────────────────────────────────────────────────────────

/// Install no-op stubs and the config table so init() can safely call globals.
fn setup_extract_lua(lua: &Lua, route_cfg: Option<&toml::Table>) -> Result<()> {
    lua.globals().set("send", lua.create_function(|_, _: LuaMultiValue| Ok(()))?)?;
    lua.globals().set("set_bpm", lua.create_function(|_, _: f64| Ok(()))?)?;
    lua.globals().set("get_bpm", lua.create_function(|_, ()| -> LuaResult<f64> { Ok(120.0) })?)?;
    lua.globals().set("set_ppqn", lua.create_function(|_, _: u32| Ok(()))?)?;
    lua.globals().set("get_ppqn", lua.create_function(|_, ()| -> LuaResult<u32> { Ok(24) })?)?;
    lua.globals().set("log", lua.create_function(|_, _: String| Ok(()))?)?;
    let cfg_table = match route_cfg {
        Some(tbl) => toml_table_to_lua(lua, tbl)
            .map_err(|e| anyhow::anyhow!("Failed to convert config to Lua: {}", e))?,
        None => lua.create_table()?,
    };
    lua.globals().set("config", cfg_table)?;
    Ok(())
}

fn lua_val_to_string(val: Option<LuaValue>) -> Option<String> {
    match val {
        Some(LuaValue::String(s)) => s.to_str().ok().as_deref().map(str::to_string),
        _ => None,
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
        for pair in t.pairs::<String, String>() {
            if let Ok((k, v)) = pair { decl.inputs.insert(k, v); }
        }
    }
    if let Ok(LuaValue::Table(t)) = connect_tbl.get::<LuaValue>("outputs") {
        for pair in t.pairs::<String, String>() {
            if let Ok((k, v)) = pair { decl.outputs.insert(k, v); }
        }
    }
    // Singular patterns stored under "" sentinel so apply_connect_defaults can expand them.
    if let Some(pat) = lua_val_to_string(connect_tbl.get::<LuaValue>("input").ok()) {
        decl.inputs.insert("".into(), pat);
    }
    if let Some(pat) = lua_val_to_string(connect_tbl.get::<LuaValue>("output").ok()) {
        decl.outputs.insert("".into(), pat);
    }
    decl
}

/// Merge `connect_input`/`connect_output` from config.toml into `decl` at lower priority.
fn connect_from_toml(mut decl: ConnectDecl, route_cfg: Option<&toml::Table>) -> ConnectDecl {
    if let Some(cfg) = route_cfg {
        if let Some(toml::Value::String(pat)) = cfg.get("connect_input") {
            decl.inputs.entry("".into()).or_insert_with(|| pat.clone());
        }
        if let Some(toml::Value::String(pat)) = cfg.get("connect_output") {
            decl.outputs.entry("".into()).or_insert_with(|| pat.clone());
        }
    }
    decl
}

/// Run the script once in a temporary Lua VM to extract both port layout and
/// connect patterns. Falls back to config.toml, then to the single default port.
fn extract_all_decls(
    script: &str,
    name: &str,
    route_cfg: Option<&toml::Table>,
) -> Result<(PortDecl, ConnectDecl)> {
    let lua = Lua::new();
    setup_extract_lua(&lua, route_cfg)?;

    if let Err(e) = lua.load(script).set_name(name).exec() {
        tracing::debug!(
            "[{}] extract_all_decls: script error (will be reported by event loop): {}",
            name, e
        );
        return Ok((PortDecl::default(), connect_from_toml(ConnectDecl::default(), route_cfg)));
    }

    if let Ok(Some(f)) = lua.globals().get::<Option<LuaFunction>>("init") {
        match f.call::<LuaValue>(()) {
            Ok(LuaValue::Table(ref tbl)) => {
                let port_decl = parse_port_decl_from_lua(tbl)?;
                let connect_decl = connect_from_toml(connect_from_lua_table(tbl), route_cfg);
                return Ok((port_decl, connect_decl));
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
    Ok((port_decl, connect_decl))
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
    let all_in  = raw.inputs.remove("").or_else(|| global_input.map(str::to_string));
    let all_out = raw.outputs.remove("").or_else(|| global_output.map(str::to_string));
    for port in port_inputs {
        if !raw.inputs.contains_key(port) {
            if let Some(ref pat) = all_in {
                raw.inputs.insert(port.clone(), pat.clone());
            }
        }
    }
    for port in port_outputs {
        if !raw.outputs.contains_key(port) {
            if let Some(ref pat) = all_out {
                raw.outputs.insert(port.clone(), pat.clone());
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

    // --- Expose `config` table ---
    {
        let cfg_table = match route_cfg {
            Some(ref tbl) => toml_table_to_lua(&lua, tbl)
                .map_err(|e| anyhow::anyhow!("Failed to convert route config to Lua: {}", e))?,
            None => lua.create_table()?,
        };
        lua.globals().set("config", cfg_table)?;
    }

    // --- Load the user script ---
    anyhow::Context::with_context(lua.load(script).set_name(name).exec(), || {
        format!("Lua load error in '{}'", name)
    })?;

    // Cache callbacks once — avoids a global-table lookup on every event.
    let on_midi_fn: Option<LuaFunction> = lua.globals().get("on_midi").ok();
    let on_tick_fn: Option<LuaFunction> = lua.globals().get("on_tick").ok();

    // --- Event loop ---
    loop {
        let event = match rx.blocking_recv() {
            Some(e) => e,
            None => break,
        };

        match event {
            RouteEvent::Midi { port, bytes } => {
                if let Some(ref on_midi) = on_midi_fn {
                    match midi_bytes_to_lua(&lua, &bytes) {
                        Ok(msg) => {
                            // Add `port` field so Lua can distinguish which input fired.
                            let _ = msg.set("port", port.as_str());
                            if let Err(e) = on_midi.call::<()>(msg) {
                                warn!("[{}] on_midi error: {}", name, e);
                            }
                        }
                        Err(e) => warn!("[{}] MIDI parse error: {}", name, e),
                    }
                }
            }
            RouteEvent::Timer(TimerEvent::Tick { tick, bpm, ppqn }) => {
                if let Some(ref on_tick) = on_tick_fn {
                    if let Err(e) = on_tick.call::<()>((tick, bpm, ppqn)) {
                        warn!("[{}] on_tick error: {}", name, e);
                    }
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

    #[test]
    fn connect_lua_singular_input_stored_under_sentinel() {
        let c = connect(r#"
            function init()
                return { inputs = {"kbd"}, outputs = {"synth"},
                         connect = { input = ".*KeyLab.*" } }
            end
        "#);
        assert_eq!(c.inputs.get(""), Some(&".*KeyLab.*".to_string()));
    }

    #[test]
    fn connect_lua_singular_output_stored_under_sentinel() {
        let c = connect(r#"
            function init()
                return { inputs = {"kbd"}, outputs = {"synth"},
                         connect = { output = ".*Surge.*" } }
            end
        "#);
        assert_eq!(c.outputs.get(""), Some(&".*Surge.*".to_string()));
    }

    #[test]
    fn connect_lua_per_port_inputs_stored_by_name() {
        let c = connect(r#"
            function init()
                return { inputs = {"kbd", "pad"}, outputs = {"synth"},
                         connect = { inputs = { kbd = ".*KORG.*", pad = ".*Alesis.*" } } }
            end
        "#);
        assert_eq!(c.inputs.get("kbd"), Some(&".*KORG.*".to_string()));
        assert_eq!(c.inputs.get("pad"), Some(&".*Alesis.*".to_string()));
        assert!(!c.inputs.contains_key(""));
    }

    #[test]
    fn connect_lua_per_port_outputs_stored_by_name() {
        let c = connect(r#"
            function init()
                return { inputs = {"kbd"}, outputs = {"synth", "drums"},
                         connect = { outputs = { synth = ".*Surge.*", drums = ".*DrumKit.*" } } }
            end
        "#);
        assert_eq!(c.outputs.get("synth"), Some(&".*Surge.*".to_string()));
        assert_eq!(c.outputs.get("drums"), Some(&".*DrumKit.*".to_string()));
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
        assert_eq!(c.inputs.get("kbd"), Some(&".*KORG.*".to_string()));
        assert_eq!(c.inputs.get(""), Some(&".*Fallback.*".to_string()));
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
        assert_eq!(c.inputs.get(""), Some(&".*KeyLab.*".to_string()));
    }

    #[test]
    fn connect_toml_connect_output_stored_under_sentinel() {
        let c = connect_with_cfg("-- no init", "connect_output = \".*Surge.*\"");
        assert_eq!(c.outputs.get(""), Some(&".*Surge.*".to_string()));
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
        assert_eq!(c.inputs.get(""), Some(&".*LuaPattern.*".to_string()));
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
        assert_eq!(c.inputs.get(""), Some(&".*TomlPattern.*".to_string()));
        assert_eq!(c.outputs.get(""), Some(&".*TomlOut.*".to_string()));
    }

    #[test]
    fn connect_script_error_returns_empty_falls_back_to_toml() {
        let c = connect_with_cfg(
            "this is ][ not valid lua",
            "connect_input = \".*Fallback.*\"",
        );
        assert_eq!(c.inputs.get(""), Some(&".*Fallback.*".to_string()));
    }

    // ── apply_connect_defaults ────────────────────────────────────────────────

    fn ports(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn decl_with_sentinel(sentinel: &str) -> ConnectDecl {
        let mut d = ConnectDecl::default();
        d.inputs.insert("".into(), sentinel.to_string());
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
        assert_eq!(result.inputs.get("kbd").map(String::as_str), Some(".*MyController.*"));
        assert_eq!(result.inputs.get("pad").map(String::as_str), Some(".*MyController.*"));
        assert_eq!(result.outputs.get("synth").map(String::as_str), Some(".*MySynth.*"));
    }

    #[test]
    fn defaults_sentinel_fills_all_ports() {
        let mut raw = ConnectDecl::default();
        raw.inputs.insert("".into(), ".*RouteLevel.*".into());
        let result = apply_connect_defaults(
            raw, &ports(&["kbd", "pad"]), &ports(&["synth"]), None, None,
        );
        assert_eq!(result.inputs.get("kbd").map(String::as_str), Some(".*RouteLevel.*"));
        assert_eq!(result.inputs.get("pad").map(String::as_str), Some(".*RouteLevel.*"));
        assert!(!result.inputs.contains_key(""));
    }

    #[test]
    fn defaults_sentinel_takes_priority_over_global() {
        let raw = decl_with_sentinel(".*RouteLevel.*");
        let result = apply_connect_defaults(
            raw, &ports(&["kbd"]), &ports(&[]), Some(".*Global.*"), None,
        );
        assert_eq!(result.inputs.get("kbd").map(String::as_str), Some(".*RouteLevel.*"));
    }

    #[test]
    fn defaults_per_port_not_overridden_by_global() {
        let mut raw = ConnectDecl::default();
        raw.inputs.insert("kbd".into(), ".*PerPort.*".into());
        let result = apply_connect_defaults(
            raw, &ports(&["kbd"]), &ports(&[]), Some(".*Global.*"), None,
        );
        assert_eq!(result.inputs.get("kbd").map(String::as_str), Some(".*PerPort.*"));
    }

    #[test]
    fn defaults_per_port_not_overridden_by_sentinel() {
        let mut raw = ConnectDecl::default();
        raw.inputs.insert("kbd".into(), ".*PerPort.*".into());
        raw.inputs.insert("".into(), ".*Sentinel.*".into());
        let result = apply_connect_defaults(
            raw, &ports(&["kbd", "pad"]), &ports(&[]), None, None,
        );
        assert_eq!(result.inputs.get("kbd").map(String::as_str), Some(".*PerPort.*"));
        // pad has no per-port entry, falls back to sentinel
        assert_eq!(result.inputs.get("pad").map(String::as_str), Some(".*Sentinel.*"));
    }

    #[test]
    fn defaults_sentinel_removed_from_final_map() {
        let raw = decl_with_sentinel(".*Pat.*");
        let result = apply_connect_defaults(raw, &ports(&["kbd"]), &ports(&[]), None, None);
        assert!(!result.inputs.contains_key(""));
    }
}
