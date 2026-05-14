use anyhow::{Context, Result};
use midir::os::unix::{VirtualInput, VirtualOutput};
use midir::{MidiInput, MidiInputConnection, MidiOutput, MidiOutputConnection};
use mlua::prelude::*;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::config::Config;
use crate::lua_api::{lua_to_midi_bytes, midi_bytes_to_lua, toml_table_to_lua};
use crate::timer::{Timer, TimerEvent};

enum RouteEvent {
    Midi(Vec<u8>),
    Timer(TimerEvent),
}

/// Owns the virtual MIDI ports for a route. Kept alive across Lua reloads so
/// the ALSA client and port IDs stay the same.
pub struct RoutePorts {
    out_conn: Arc<Mutex<MidiOutputConnection>>,
    /// The MIDI input callback sends here; swapped on each reload to forward
    /// to the new route's event channel.
    midi_fwd: Arc<Mutex<Option<mpsc::Sender<RouteEvent>>>>,
    _in_conn: MidiInputConnection<()>,
}

impl RoutePorts {
    fn create(port_name: &str, initial_tx: mpsc::Sender<RouteEvent>) -> Result<Arc<Self>> {
        let midi_fwd: Arc<Mutex<Option<mpsc::Sender<RouteEvent>>>> =
            Arc::new(Mutex::new(Some(initial_tx)));

        let midi_in = MidiInput::new(&format!("{}-in", port_name))
            .context("Failed to create MIDI input")?;
        let fwd_ref = Arc::clone(&midi_fwd);
        let in_conn = midi_in
            .create_virtual(
                &format!("{}-in", port_name),
                move |_stamp, message, _| {
                    let guard = fwd_ref.lock().unwrap();
                    if let Some(tx) = guard.as_ref() {
                        let _ = tx.blocking_send(RouteEvent::Midi(message.to_vec()));
                    }
                },
                (),
            )
            .map_err(|e| anyhow::anyhow!("Failed to create virtual MIDI input port: {}", e))?;

        let midi_out = MidiOutput::new(&format!("{}-out", port_name))
            .context("Failed to create MIDI output")?;
        let out_conn = midi_out
            .create_virtual(port_name)
            .map_err(|e| anyhow::anyhow!("Failed to create virtual MIDI output port: {}", e))?;

        Ok(Arc::new(RoutePorts {
            out_conn: Arc::new(Mutex::new(out_conn)),
            midi_fwd,
            _in_conn: in_conn,
        }))
    }
}

/// A running route: owns its MIDI ports, Lua VM, and timer.
/// Dropping this stops everything cleanly.
pub struct Route {
    ports: Arc<RoutePorts>,
    _timer: Arc<Timer>,
    _thread: std::thread::JoinHandle<()>,
}

impl Route {
    /// Consume the route and return its ports so they can be reused on reload.
    pub fn take_ports(self) -> Arc<RoutePorts> {
        self.ports
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

        let port_name = format!("midi-daemon:{}", name);

        let (tx, rx) = mpsc::channel::<RouteEvent>(256);

        let route_cfg = config.route_config(&name).cloned();

        let ports = match existing_ports {
            Some(p) => {
                // Redirect the input callback to the new event channel.
                *p.midi_fwd.lock().unwrap() = Some(tx.clone());
                p
            }
            None => RoutePorts::create(&port_name, tx.clone())?,
        };

        // --- Timer ---
        let timer = Arc::new(Timer::new(config.default_bpm, config.default_ppqn));
        let _timer_thread = timer.spawn(tx.clone(), RouteEvent::Timer);

        // --- Lua script ---
        let script = std::fs::read_to_string(lua_path)
            .with_context(|| format!("Failed to read {}", lua_path.display()))?;

        // --- Spawn event loop thread ---
        let out_for_thread = Arc::clone(&ports.out_conn);
        let timer_for_thread = Arc::clone(&timer);
        let name_for_thread = name.clone();

        let thread = std::thread::spawn(move || {
            if let Err(e) = run_lua_event_loop(
                &name_for_thread,
                &script,
                rx,
                out_for_thread,
                timer_for_thread,
                route_cfg,
            ) {
                error!("Route '{}' event loop error: {}", name_for_thread, e);
            }
        });

        info!("Started route '{}' on port '{}'", name, port_name);

        Ok(Route {
            ports,
            _timer: timer,
            _thread: thread,
        })
    }
}

fn run_lua_event_loop(
    name: &str,
    script: &str,
    mut rx: mpsc::Receiver<RouteEvent>,
    out_conn: Arc<Mutex<MidiOutputConnection>>,
    timer: Arc<Timer>,
    route_cfg: Option<toml::Table>,
) -> Result<()> {
    let lua = Lua::new();

    // --- Expose `send(msg)` to Lua ---
    {
        let out = Arc::clone(&out_conn);
        let send_fn = lua.create_function(move |_lua, msg: LuaTable| {
            match lua_to_midi_bytes(&msg) {
                Ok(bytes) => {
                    let mut conn = out.lock().unwrap();
                    if let Err(e) = conn.send(&bytes) {
                        warn!("MIDI send error: {}", e);
                    }
                }
                Err(e) => warn!("lua_to_midi_bytes error: {}", e),
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
                .map_err(|e| anyhow::anyhow!("Failed to convert route config to Lua table: {}", e))?,
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
            None => break, // channel closed
        };

        match event {
            RouteEvent::Midi(bytes) => {
                if let Some(ref on_midi) = on_midi_fn {
                    match midi_bytes_to_lua(&lua, &bytes) {
                        Ok(msg) => {
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
