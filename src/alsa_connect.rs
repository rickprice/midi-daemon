use alsa::seq::{Addr, ClientIter, EventType, PortCap, PortIter, PortSubscribe, PortType, Seq};
use anyhow::Result;
use regex::Regex;
use std::ffi::CString;
use std::sync::{Arc, Mutex};
use tracing::{debug, info, warn};

use crate::route::{ConnectDecl, PortDecl};

#[derive(Clone, Copy, PartialEq, Eq)]
enum ConnDir {
    Input,  // external READ port -> our WRITE input
    Output, // our READ output -> external WRITE port
}

struct PortSpec {
    our_client_name: String,
    pattern: Regex,
    dir: ConnDir,
}

pub struct ConnectionManager {
    specs: Arc<Mutex<Vec<PortSpec>>>,
}

impl ConnectionManager {
    pub fn new() -> Self {
        ConnectionManager { specs: Arc::new(Mutex::new(Vec::new())) }
    }

    /// Register (or replace) all auto-connect specs for one route.
    pub fn register_route(&self, route_name: &str, decl: &PortDecl, connect: &ConnectDecl) {
        let prefix = format!("midi-daemon:{}", route_name);
        let is_default = decl.is_default();
        let base = prefix.clone();

        let mut specs = self.specs.lock().unwrap();
        specs.retain(|s| !s.our_client_name.starts_with(&prefix));

        for port_name in &decl.inputs {
            if let Some(pat_str) = connect.inputs.get(port_name) {
                match Regex::new(pat_str) {
                    Ok(pattern) => {
                        let client_name = if is_default {
                            format!("{}-in", base)
                        } else {
                            format!("{}/{}-in", base, port_name)
                        };
                        specs.push(PortSpec { our_client_name: client_name, pattern, dir: ConnDir::Input });
                    }
                    Err(e) => warn!("[{}] invalid connect_input regex '{}': {}", route_name, pat_str, e),
                }
            }
        }

        for port_name in &decl.outputs {
            if let Some(pat_str) = connect.outputs.get(port_name) {
                match Regex::new(pat_str) {
                    Ok(pattern) => {
                        let client_name = if is_default {
                            format!("{}-out", base)
                        } else {
                            format!("{}/{}-out", base, port_name)
                        };
                        specs.push(PortSpec { our_client_name: client_name, pattern, dir: ConnDir::Output });
                    }
                    Err(e) => warn!("[{}] invalid connect_output regex '{}': {}", route_name, pat_str, e),
                }
            }
        }
    }

    /// Remove all specs for a route (called when it is deleted).
    pub fn unregister_route(&self, route_name: &str) {
        let prefix = format!("midi-daemon:{}", route_name);
        self.specs.lock().unwrap().retain(|s| !s.our_client_name.starts_with(&prefix));
    }

    /// Scan all current ALSA ports and apply matching connections.
    pub fn apply_all(&self) {
        match open_seq() {
            Ok(seq) => {
                let specs = self.specs.lock().unwrap();
                apply_connections(&seq, &specs);
            }
            Err(e) => warn!("auto-connect: failed to open ALSA seq: {}", e),
        }
    }

    /// Spawn a background thread that watches for new ALSA ports and connects them.
    pub fn spawn_watcher(self: Arc<Self>) {
        std::thread::Builder::new()
            .name("alsa-port-watcher".into())
            .spawn(move || {
                if let Err(e) = watch_loop(&self) {
                    warn!("ALSA port watcher stopped: {}", e);
                }
            })
            .ok();
    }
}

// ── ALSA helpers ──────────────────────────────────────────────────────────────

fn open_seq() -> Result<Seq> {
    Ok(Seq::open(None, None, false)?)
}

/// Find the ALSA address of one of our virtual ports by client name.
fn find_our_port(seq: &Seq, client_name: &str) -> Option<Addr> {
    for ci in ClientIter::new(seq) {
        if ci.get_name().ok() == Some(client_name) {
            let cid = ci.get_client();
            if let Some(pi) = PortIter::new(seq, cid).next() {
                return Some(Addr { client: cid, port: pi.get_port() });
            }
        }
    }
    None
}

/// Search all non-daemon ALSA ports for ones matching `pattern` with the right capability.
fn find_matching_external(seq: &Seq, pattern: &Regex, dir: ConnDir) -> Vec<(Addr, String)> {
    let mut result = Vec::new();
    for ci in ClientIter::new(seq) {
        let cname = match ci.get_name() {
            Ok(n) => n.to_string(),
            Err(_) => continue,
        };
        if cname.starts_with("midi-daemon:") { continue; }
        let cid = ci.get_client();
        for pi in PortIter::new(seq, cid) {
            let pname = match pi.get_name() {
                Ok(n) => n.to_string(),
                Err(_) => continue,
            };
            let cap = pi.get_capability();
            let required = match dir {
                ConnDir::Input  => PortCap::READ | PortCap::SUBS_READ,
                ConnDir::Output => PortCap::WRITE | PortCap::SUBS_WRITE,
            };
            if !cap.contains(required) { continue; }
            let full = format!("{}:{}", cname, pname);
            if pattern.is_match(&full) {
                result.push((Addr { client: cid, port: pi.get_port() }, full));
            }
        }
    }
    result
}

/// Try to make an ALSA subscription; silently ignore "already subscribed".
fn try_subscribe(seq: &Seq, sender: Addr, dest: Addr, label: &str) {
    let sub = match PortSubscribe::empty() {
        Ok(s) => s,
        Err(e) => { warn!("PortSubscribe::new: {}", e); return; }
    };
    sub.set_sender(sender);
    sub.set_dest(dest);
    match seq.subscribe_port(&sub) {
        Ok(_) => info!("auto-connected: {}", label),
        Err(e) => {
            // EBUSY (-16) means already connected — not an error.
            let msg = e.to_string();
            if !msg.contains("16") && !msg.contains("busy") {
                warn!("auto-connect failed ({}): {}", label, e);
            }
        }
    }
}

fn apply_connections(seq: &Seq, specs: &[PortSpec]) {
    for spec in specs {
        let our_addr = match find_our_port(seq, &spec.our_client_name) {
            Some(a) => a,
            None => {
                debug!("auto-connect: our port '{}' not yet visible in ALSA", spec.our_client_name);
                continue;
            }
        };
        for (ext_addr, name) in find_matching_external(seq, &spec.pattern, spec.dir) {
            let (sender, dest, label) = match spec.dir {
                ConnDir::Input  => (ext_addr, our_addr, format!("{} -> {}", name, spec.our_client_name)),
                ConnDir::Output => (our_addr, ext_addr, format!("{} -> {}", spec.our_client_name, name)),
            };
            try_subscribe(seq, sender, dest, &label);
        }
    }
}

/// Connect a newly-appeared ALSA port to any matching specs.
fn connect_new_port(seq: &Seq, specs: &[PortSpec], new_addr: Addr) {
    let ci = match seq.get_any_client_info(new_addr.client) {
        Ok(c) => c,
        Err(_) => return,
    };
    let pi = match seq.get_any_port_info(new_addr) {
        Ok(p) => p,
        Err(_) => return,
    };

    let cname = ci.get_name().unwrap_or("").to_string();
    if cname.starts_with("midi-daemon:") { return; }
    let pname = pi.get_name().unwrap_or("").to_string();
    let cap = pi.get_capability();
    let full = format!("{}:{}", cname, pname);

    for spec in specs {
        let required = match spec.dir {
            ConnDir::Input  => PortCap::READ | PortCap::SUBS_READ,
            ConnDir::Output => PortCap::WRITE | PortCap::SUBS_WRITE,
        };
        if !cap.contains(required) { continue; }
        if !spec.pattern.is_match(&full) { continue; }

        let our_addr = match find_our_port(seq, &spec.our_client_name) {
            Some(a) => a,
            None => continue,
        };

        let (sender, dest, label) = match spec.dir {
            ConnDir::Input  => (new_addr, our_addr, format!("{} -> {}", full, spec.our_client_name)),
            ConnDir::Output => (our_addr, new_addr, format!("{} -> {}", spec.our_client_name, full)),
        };
        try_subscribe(seq, sender, dest, &label);
    }
}

// ── Port announcement watcher ─────────────────────────────────────────────────

fn watch_loop(mgr: &Arc<ConnectionManager>) -> Result<()> {
    let watch_seq = open_seq()?;
    watch_seq.set_client_name(&CString::new("midi-daemon-connect")?)?;

    let announce_port = watch_seq.create_simple_port(
        &CString::new("announce-listener")?,
        PortCap::WRITE | PortCap::SUBS_WRITE,
        PortType::APPLICATION,
    )?;

    // Subscribe to ALSA system announce port so we receive PORT_START events.
    let sub = PortSubscribe::empty()?;
    sub.set_sender(Addr::system_announce());
    sub.set_dest(Addr { client: watch_seq.client_id()?, port: announce_port });
    watch_seq.subscribe_port(&sub)?;

    // A separate seq handle for making subscriptions (avoids borrow conflict with Input).
    let conn_seq = open_seq()?;

    loop {
        let (ev_type, addr) = {
            let mut input = watch_seq.input();
            let ev = input.event_input()?;
            (ev.get_type(), ev.get_data::<Addr>())
        };

        if ev_type == EventType::PortStart {
            if let Some(addr) = addr {
                debug!("new ALSA port: {}:{}", addr.client, addr.port);
                // Small delay so the port is fully registered before we query it.
                std::thread::sleep(std::time::Duration::from_millis(100));
                let specs = mgr.specs.lock().unwrap();
                connect_new_port(&conn_seq, &specs, addr);
            }
        }
    }
}

