use anyhow::{Context, Result};
use midir::os::unix::{VirtualInput, VirtualOutput};
use midir::{MidiInput, MidiOutput, MidiOutputConnection};
use mlua::prelude::*;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::config::Config;
use crate::lua_api::{lua_to_midi_bytes, midi_bytes_to_lua};
use crate::timer::{Timer, TimerEvent};

/// Events dispatched to a route's Lua event loop
enum RouteEvent {
    Midi(Vec<u8>),
    Timer(TimerEvent),
}

/// A running route: owns its MIDI ports, Lua VM, and timer.
/// Dropping this stops everything cleanly.
pub struct Route {
    // Kept alive to hold the MIDI output connection open
    _output: Arc<Mutex<MidiOutputConnection>>,
    // Kept alive to stop the timer on drop
    _timer: Arc<Timer>,
    // Thread handle for the event loop
    _thread: std::thread::JoinHandle<()>,
}

impl Route {
    pub fn start(lua_path: &PathBuf, config: Arc<Config>) -> Result<Self> {
        let name = lua_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();

        let port_name = format!("midi-daemon:{}", name);

        // --- MIDI Input ---
        let midi_in = MidiInput::new(&format!("{}-in", port_name))
            .context("Failed to create MIDI input")?;

        // --- MIDI Output ---
        let midi_out = MidiOutput::new(&format!("{}-out", port_name))
            .context("Failed to create MIDI output")?;

        let out_port_name = port_name.clone();
        let out_conn = midi_out
            .create_virtual(&out_port_name)
            .map_err(|e| anyhow::anyhow!("Failed to create virtual MIDI output port: {}", e))?;

        let out_conn = Arc::new(Mutex::new(out_conn));

        // --- Event channel ---
        let (tx, rx) = mpsc::channel::<RouteEvent>(256);

        // --- MIDI input callback ---
        let midi_tx = tx.clone();
        let _in_conn = midi_in
            .create_virtual(
                &format!("{}-in", port_name),
                move |_stamp, message, _| {
                    let _ = midi_tx.blocking_send(RouteEvent::Midi(message.to_vec()));
                },
                (),
            )
            .map_err(|e| anyhow::anyhow!("Failed to create virtual MIDI input port: {}", e))?;

        // --- Timer ---
        let timer = Arc::new(Timer::new(config.default_bpm, config.default_ppqn));
        let timer_tx = tx.clone();
        let _timer_thread = timer.spawn(
            // wrap TimerEvent in RouteEvent
            {
                let (ttx, mut trx) = mpsc::channel::<TimerEvent>(256);
                tokio::spawn(async move {
                    while let Some(ev) = trx.recv().await {
                        if timer_tx.send(RouteEvent::Timer(ev)).await.is_err() {
                            break;
                        }
                    }
                });
                ttx
            },
        );

        // --- Lua script ---
        let script = std::fs::read_to_string(lua_path)
            .with_context(|| format!("Failed to read {}", lua_path.display()))?;

        // --- Spawn event loop thread ---
        let out_for_thread = Arc::clone(&out_conn);
        let timer_for_thread = Arc::clone(&timer);
        let name_for_thread = name.clone();

        let thread = std::thread::spawn(move || {
            if let Err(e) = run_lua_event_loop(
                &name_for_thread,
                &script,
                rx,
                out_for_thread,
                timer_for_thread,
            ) {
                error!("Route '{}' event loop error: {}", name_for_thread, e);
            }
        });

        info!("Started route '{}' on port '{}'", name, port_name);

        Ok(Route {
            _output: out_conn,
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

    // --- Load the user script ---
    anyhow::Context::with_context(lua.load(script).set_name(name).exec(), || {
        format!("Lua load error in '{}'", name)
    })?;

    // --- Event loop ---
    loop {
        let event = match rx.blocking_recv() {
            Some(e) => e,
            None => break, // channel closed
        };

        match event {
            RouteEvent::Midi(bytes) => {
                if let Ok(on_midi) = lua.globals().get::<LuaFunction>("on_midi") {
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
                if let Ok(on_tick) = lua.globals().get::<LuaFunction>("on_tick") {
                    if let Err(e) = on_tick.call::<()>((tick, bpm, ppqn)) {
                        warn!("[{}] on_tick error: {}", name, e);
                    }
                }
            }
        }
    }

    Ok(())
}
