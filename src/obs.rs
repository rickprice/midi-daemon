use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::Value;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::config::ObsConnCfg;

/// Result of an `ObsRequest`, delivered through `reply` (if present) or logged.
pub struct ObsReply {
    pub ok: bool,
    pub result: Value,
    pub error: Option<String>,
}

/// One outbound call queued onto a connection's request channel.
///
/// `request` names an entry in [`dispatch_call`]'s match (e.g. `"scenes.set_current"`);
/// `args` is a JSON object carrying its parameters. `reply` is `None` for
/// fire-and-forget calls (`obs_call`) and `Some` for calls that block the
/// calling route thread on a plain `std::sync::mpsc` channel (`obs_call_sync`)
/// — the sync side never touches the tokio runtime.
pub struct ObsRequest {
    pub request: String,
    pub args: Value,
    pub reply: Option<std::sync::mpsc::Sender<ObsReply>>,
}

/// A route's handler for events from one or more OBS connections.
/// Takes the originating connection name plus the serialized `obws` event.
pub type EventInjector = Arc<dyn Fn(&str, Value) + Send + Sync>;

struct ObsConnState {
    cfg: ObsConnCfg,
    sender: mpsc::Sender<ObsRequest>,
}

/// Owns one outbound request queue and one reconnecting background task per
/// named OBS connection declared in config.toml. Routes never talk to
/// `obws::Client` directly — they queue a request by connection name and
/// (optionally) wait on their own reply channel.
pub struct ObsManager {
    connections: Mutex<HashMap<String, ObsConnState>>,
    dispatch: Arc<Mutex<HashMap<String, HashMap<String, EventInjector>>>>,
}

impl ObsManager {
    pub fn new(cfgs: &HashMap<String, ObsConnCfg>) -> Arc<Self> {
        let mgr = Arc::new(ObsManager {
            connections: Mutex::new(HashMap::new()),
            dispatch: Arc::new(Mutex::new(HashMap::new())),
        });
        mgr.sync(cfgs);
        mgr
    }

    /// Ensure exactly the connections in `cfgs` are running: starts
    /// new/changed ones and stops removed ones. Connections whose config is
    /// unchanged are left running untouched.
    pub fn sync(&self, cfgs: &HashMap<String, ObsConnCfg>) {
        let mut conns = self.connections.lock().unwrap_or_else(std::sync::PoisonError::into_inner);

        for (name, cfg) in cfgs {
            if conns.get(name).is_some_and(|s| &s.cfg == cfg) {
                continue;
            }
            let sender = spawn_connection(name.clone(), cfg.clone(), Arc::clone(&self.dispatch));
            self.dispatch
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(name.clone(), HashMap::new());
            conns.insert(name.clone(), ObsConnState { cfg: cfg.clone(), sender });
        }

        let removed: Vec<String> = conns
            .keys()
            .filter(|n| !cfgs.contains_key(n.as_str()))
            .cloned()
            .collect();
        for name in removed {
            conns.remove(&name);
            self.dispatch.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&name);
        }
    }

    /// All request-queue senders by connection name, handed to each `Route`
    /// so Lua's `obs_call`/`obs_call_sync` can address any configured connection.
    pub fn sender_map(&self) -> HashMap<String, mpsc::Sender<ObsRequest>> {
        self.connections
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(name, state)| (name.clone(), state.sender.clone()))
            .collect()
    }

    /// Register `injector` to receive events from every connection name in
    /// `connections` (as declared by the route's `init()`). Replaces any
    /// prior registration for this route name. Unknown connection names are
    /// warned about and otherwise ignored.
    pub fn register_route(&self, route_name: &str, connections: &[String], injector: &EventInjector) {
        let mut guard = self.dispatch.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        for conn in connections {
            if let Some(routes) = guard.get_mut(conn) {
                routes.insert(route_name.to_string(), Arc::clone(injector));
            } else {
                warn!("obs: route '{}' declared unknown connection '{}'", route_name, conn);
            }
        }
    }

    /// Remove a route's event registration from every connection.
    pub fn unregister_route(&self, route_name: &str) {
        let mut guard = self.dispatch.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        for routes in guard.values_mut() {
            routes.remove(route_name);
        }
    }
}

fn spawn_connection(
    name: String,
    cfg: ObsConnCfg,
    dispatch: Arc<Mutex<HashMap<String, HashMap<String, EventInjector>>>>,
) -> mpsc::Sender<ObsRequest> {
    let (tx, rx) = mpsc::channel::<ObsRequest>(64);
    tokio::spawn(run_connection(name, cfg, rx, dispatch));
    tx
}

/// Owns the `obws::Client` for one connection for as long as the daemon runs.
/// Reconnects with a fixed backoff on any disconnect; `obws` has no built-in
/// reconnect, so the daemon owns this loop the same way `alsa_connect.rs`
/// owns ALSA port reconnection.
async fn run_connection(
    name: String,
    cfg: ObsConnCfg,
    mut rx: mpsc::Receiver<ObsRequest>,
    dispatch: Arc<Mutex<HashMap<String, HashMap<String, EventInjector>>>>,
) {
    const RECONNECT_DELAY: Duration = Duration::from_secs(2);

    loop {
        let client = match obws::Client::connect(cfg.host.as_str(), cfg.port, cfg.password.as_deref()).await {
            Ok(c) => {
                info!("obs '{}': connected to {}:{}", name, cfg.host, cfg.port);
                c
            }
            Err(e) => {
                warn!("obs '{}': connect failed: {}; retrying in {:?}", name, e, RECONNECT_DELAY);
                tokio::time::sleep(RECONNECT_DELAY).await;
                continue;
            }
        };

        let mut events = match client.events() {
            Ok(s) => s,
            Err(e) => {
                warn!("obs '{}': failed to subscribe to events: {}; retrying in {:?}", name, e, RECONNECT_DELAY);
                tokio::time::sleep(RECONNECT_DELAY).await;
                continue;
            }
        };

        loop {
            tokio::select! {
                maybe_req = rx.recv() => {
                    if let Some(req) = maybe_req {
                        handle_request(&name, &client, req).await;
                    } else {
                        debug!("obs '{}': request queue closed — shutting down connection task", name);
                        return;
                    }
                }
                ev = events.next() => {
                    if let Some(event) = ev {
                        dispatch_event(&name, &dispatch, &event);
                    } else {
                        warn!("obs '{}': disconnected — reconnecting", name);
                        break;
                    }
                }
            }
        }
    }
}

async fn handle_request(name: &str, client: &obws::Client, req: ObsRequest) {
    match dispatch_call(client, &req.request, &req.args).await {
        Ok(result) => {
            if let Some(reply) = req.reply {
                let _ = reply.send(ObsReply { ok: true, result, error: None });
            }
        }
        Err(e) => {
            warn!("obs '{}': request '{}' failed: {}", name, req.request, e);
            if let Some(reply) = req.reply {
                let _ = reply.send(ObsReply { ok: false, result: Value::Null, error: Some(e.to_string()) });
            }
        }
    }
}

/// The extensible set of OBS operations routes can call by name. Grows as
/// routes need more of the `obws` surface — not meant to cover all of it
/// up front.
async fn dispatch_call(client: &obws::Client, request: &str, args: &Value) -> anyhow::Result<Value> {
    use obws::requests::inputs::InputId;
    use obws::requests::scenes::SceneId;

    fn str_arg<'a>(args: &'a Value, key: &str, request: &str) -> anyhow::Result<&'a str> {
        args.get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("{request}: missing '{key}' string argument"))
    }

    match request {
        "scenes.set_current" => {
            let scene = str_arg(args, "name", request)?;
            client.scenes().set_current_program_scene(SceneId::Name(scene)).await?;
            Ok(Value::Null)
        }
        "scenes.current" => {
            let current = client.scenes().current_program_scene().await?;
            Ok(serde_json::to_value(current)?)
        }
        "scenes.list" => {
            let list = client.scenes().list().await?;
            Ok(serde_json::to_value(list)?)
        }
        "inputs.set_mute" => {
            let input = str_arg(args, "name", request)?;
            let muted = args
                .get("muted")
                .and_then(Value::as_bool)
                .ok_or_else(|| anyhow::anyhow!("{request}: missing 'muted' bool argument"))?;
            client.inputs().set_muted(InputId::Name(input), muted).await?;
            Ok(Value::Null)
        }
        "inputs.toggle_mute" => {
            let input = str_arg(args, "name", request)?;
            let new_state = client.inputs().toggle_mute(InputId::Name(input)).await?;
            Ok(serde_json::json!({ "muted": new_state }))
        }
        other => Err(anyhow::anyhow!("obs: unknown request '{other}'")),
    }
}

fn dispatch_event(
    conn_name: &str,
    dispatch: &Arc<Mutex<HashMap<String, HashMap<String, EventInjector>>>>,
    event: &obws::events::Event,
) {
    let value = match serde_json::to_value(event) {
        Ok(v) => v,
        Err(e) => {
            warn!("obs '{}': failed to serialize event: {}", conn_name, e);
            return;
        }
    };
    let guard = dispatch.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(routes) = guard.get(conn_name) {
        for injector in routes.values() {
            injector(conn_name, value.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(host: &str, port: u16) -> ObsConnCfg {
        ObsConnCfg { host: host.to_string(), port, password: None }
    }

    fn noop_injector() -> EventInjector {
        Arc::new(|_conn: &str, _ev: Value| {})
    }

    // ── register_route / unregister_route ───────────────────────────────────

    #[test]
    fn register_route_unknown_connection_is_ignored() {
        let mgr = ObsManager::new(&HashMap::new());
        // Should not panic — just warns.
        mgr.register_route("r", &["nonexistent".to_string()], &noop_injector());
    }

    #[tokio::test]
    async fn sync_adds_and_removes_connections() {
        let mgr = ObsManager::new(&HashMap::new());
        let mut cfgs = HashMap::new();
        cfgs.insert("main".to_string(), cfg("127.0.0.1", 4455));
        mgr.sync(&cfgs);
        assert!(mgr.sender_map().contains_key("main"));

        cfgs.clear();
        mgr.sync(&cfgs);
        assert!(!mgr.sender_map().contains_key("main"));
    }

    #[tokio::test]
    async fn sync_leaves_unchanged_connection_running() {
        let mgr = ObsManager::new(&HashMap::new());
        let mut cfgs = HashMap::new();
        cfgs.insert("main".to_string(), cfg("127.0.0.1", 4455));
        mgr.sync(&cfgs);
        let first_sender = mgr.sender_map().get("main").unwrap().clone();

        // Re-sync with an identical config: the sender should be the same
        // instance (connection not torn down and restarted).
        mgr.sync(&cfgs);
        let second_sender = mgr.sender_map().get("main").unwrap().clone();
        assert!(first_sender.same_channel(&second_sender));
    }

    #[tokio::test]
    async fn sync_restarts_connection_on_config_change() {
        let mgr = ObsManager::new(&HashMap::new());
        let mut cfgs = HashMap::new();
        cfgs.insert("main".to_string(), cfg("127.0.0.1", 4455));
        mgr.sync(&cfgs);
        let first_sender = mgr.sender_map().get("main").unwrap().clone();

        cfgs.insert("main".to_string(), cfg("127.0.0.1", 4456));
        mgr.sync(&cfgs);
        let second_sender = mgr.sender_map().get("main").unwrap().clone();
        assert!(!first_sender.same_channel(&second_sender));
    }

    #[tokio::test]
    async fn unregister_route_removes_from_all_connections() {
        let mgr = ObsManager::new(&HashMap::new());
        let mut cfgs = HashMap::new();
        cfgs.insert("a".to_string(), cfg("127.0.0.1", 1));
        cfgs.insert("b".to_string(), cfg("127.0.0.1", 2));
        mgr.sync(&cfgs);

        mgr.register_route("r", &["a".to_string(), "b".to_string()], &noop_injector());
        mgr.unregister_route("r");

        let guard = mgr.dispatch.lock().unwrap();
        assert!(guard.get("a").unwrap().is_empty());
        assert!(guard.get("b").unwrap().is_empty());
    }
}
