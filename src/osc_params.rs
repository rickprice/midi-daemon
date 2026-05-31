use mlua::prelude::*;
use std::collections::HashMap;
use tracing::warn;

const HEARTBEAT_INTERVAL: f64 = 5.0;
const EVICTION_INTERVAL: f64 = 5.0;

struct Param {
    get: Option<LuaRegistryKey>,
    set: Option<LuaRegistryKey>,
}

struct Subscriber {
    expiry: f64,
}

pub struct OscParamSet {
    slash_prefix: String,
    subscribe_addr: String,
    unsubscribe_addr: String,
    heartbeat_addr: String,
    default_timeout: f64,
    subscribers: HashMap<String, Subscriber>,
    params: HashMap<String, Param>,
    last_eviction: f64,
    last_heartbeat: f64,
}

fn lua_now(lua: &Lua) -> LuaResult<f64> {
    let os: LuaTable = lua.globals().get("os")?;
    let time_fn: LuaFunction = os.get("time")?;
    time_fn.call(())
}

fn parse_feedback(from: &str, args: &LuaTable, default_timeout: f64) -> LuaResult<(String, f64)> {
    let fb = match args.get::<LuaValue>(1)? {
        LuaValue::Integer(port) => match from.rsplit_once(':') {
            Some((ip, _)) => format!("{}:{}", ip, port),
            None => from.to_string(),
        },
        LuaValue::Number(port) => match from.rsplit_once(':') {
            Some((ip, _)) => format!("{}:{}", ip, port as i64),
            None => from.to_string(),
        },
        _ => from.to_string(),
    };
    let timeout = match args.get::<LuaValue>(2)? {
        LuaValue::Integer(n) => n as f64,
        LuaValue::Number(f) => f,
        _ => default_timeout,
    };
    Ok((fb, timeout))
}

impl OscParamSet {
    fn new(prefix: &str, default_timeout: f64, now: f64) -> Self {
        OscParamSet {
            slash_prefix: format!("{}/", prefix),
            subscribe_addr: format!("{}/subscribe", prefix),
            unsubscribe_addr: format!("{}/unsubscribe", prefix),
            heartbeat_addr: format!("{}/heartbeat", prefix),
            default_timeout,
            subscribers: HashMap::new(),
            params: HashMap::new(),
            last_eviction: now,
            last_heartbeat: now,
        }
    }

    fn add_param(
        &mut self,
        lua: &Lua,
        name: String,
        get: Option<LuaFunction>,
        set: Option<LuaFunction>,
    ) -> LuaResult<()> {
        let get_key = get.map(|f| lua.create_registry_value(f)).transpose()?;
        let set_key = set.map(|f| lua.create_registry_value(f)).transpose()?;
        self.params.insert(name, Param { get: get_key, set: set_key });
        Ok(())
    }

    pub fn dispatch(&mut self, lua: &Lua, msg: &LuaTable) -> LuaResult<()> {
        let addr: String = msg.get("address")?;
        let from: String = msg.get::<Option<String>>("from")?.unwrap_or_default();
        let args: LuaTable = msg.get("args")?;

        // /prefix/subscribe [port [timeout]]
        if addr == self.subscribe_addr {
            let (fb, timeout) = parse_feedback(&from, &args, self.default_timeout)?;
            let now = lua_now(lua)?;
            self.subscribers.insert(fb.clone(), Subscriber { expiry: now + timeout });

            let param_names: Vec<String> = self.params.iter()
                .filter(|(_, p)| p.get.is_some())
                .map(|(n, _)| n.clone())
                .collect();
            let send_osc: LuaFunction = lua.globals().get("send_osc")?;
            for pname in param_names {
                let get_fn: Option<LuaFunction> = self.params.get(&pname)
                    .and_then(|p| p.get.as_ref())
                    .map(|k| lua.registry_value(k))
                    .transpose()?;
                if let Some(f) = get_fn {
                    let value: LuaValue = f.call(())?;
                    let param_addr = format!("{}{}", self.slash_prefix, pname);
                    if let Err(e) = send_osc.call::<()>((fb.as_str(), param_addr.as_str(), value)) {
                        warn!("OscParamSet subscribe state dump: {}", e);
                    }
                }
            }
            return Ok(());
        }

        // /prefix/unsubscribe [port]
        if addr == self.unsubscribe_addr {
            let (fb, _) = parse_feedback(&from, &args, self.default_timeout)?;
            self.subscribers.remove(&fb);
            return Ok(());
        }

        // Normal param: /prefix/<param>
        if !addr.starts_with(&self.slash_prefix) {
            return Ok(());
        }
        let param_name = addr[self.slash_prefix.len()..].to_string();
        let n_args = args.len()?;

        if n_args == 0 {
            // No args: query (get → reply to sender) or no-arg command (set only).
            let get_fn: Option<LuaFunction> = self.params.get(&param_name)
                .and_then(|p| p.get.as_ref())
                .map(|k| lua.registry_value(k))
                .transpose()?;
            let set_fn: Option<LuaFunction> = if get_fn.is_none() {
                self.params.get(&param_name)
                    .and_then(|p| p.set.as_ref())
                    .map(|k| lua.registry_value(k))
                    .transpose()?
            } else {
                None
            };
            if let Some(f) = get_fn {
                let value: LuaValue = f.call(())?;
                let send_osc: LuaFunction = lua.globals().get("send_osc")?;
                if let Err(e) = send_osc.call::<()>((from.as_str(), addr.as_str(), value)) {
                    warn!("OscParamSet query reply: {}", e);
                }
            } else if let Some(f) = set_fn {
                if let Err(e) = f.call::<()>(()) {
                    warn!("OscParamSet no-arg trigger: {}", e);
                }
            }
        } else {
            // Args present: set(args...), then notify all subscribers via get().
            let set_fn: Option<LuaFunction> = self.params.get(&param_name)
                .and_then(|p| p.set.as_ref())
                .map(|k| lua.registry_value(k))
                .transpose()?;
            let get_fn: Option<LuaFunction> = self.params.get(&param_name)
                .and_then(|p| p.get.as_ref())
                .map(|k| lua.registry_value(k))
                .transpose()?;
            if let Some(f) = set_fn {
                let unpack: LuaFunction = lua.globals()
                    .get::<LuaTable>("table")?
                    .get("unpack")?;
                let call_args: LuaMultiValue = unpack.call(args)?;
                if let Err(e) = f.call::<()>(call_args) {
                    warn!("OscParamSet param set: {}", e);
                }
                if let Some(g) = get_fn {
                    let value: LuaValue = g.call(())?;
                    let subscribers: Vec<String> =
                        self.subscribers.keys().cloned().collect();
                    if !subscribers.is_empty() {
                        let send_osc: LuaFunction = lua.globals().get("send_osc")?;
                        for fb in subscribers {
                            if let Err(e) = send_osc.call::<()>((
                                fb.as_str(),
                                addr.as_str(),
                                value.clone(),
                            )) {
                                warn!("OscParamSet notify subscriber: {}", e);
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub fn tick(&mut self, lua: &Lua) -> LuaResult<()> {
        let now = lua_now(lua)?;
        if now - self.last_eviction >= EVICTION_INTERVAL {
            self.last_eviction = now;
            self.subscribers.retain(|_, sub| now < sub.expiry);
        }
        if now - self.last_heartbeat >= HEARTBEAT_INTERVAL {
            self.last_heartbeat = now;
            let subs: Vec<String> = self.subscribers.keys().cloned().collect();
            if !subs.is_empty() {
                let send_osc: LuaFunction = lua.globals().get("send_osc")?;
                for fb in subs {
                    if let Err(e) =
                        send_osc.call::<()>((fb.as_str(), self.heartbeat_addr.as_str()))
                    {
                        warn!("OscParamSet heartbeat: {}", e);
                    }
                }
            }
        }
        Ok(())
    }
}

/// Try to build an `OscParamSet` from the `osc.params` key of an `init()` return table.
///
/// Returns `None` if the table has no `osc.params` subtable.
pub fn from_init_table(
    lua: &Lua,
    prefix: &str,
    init_tbl: &LuaTable,
) -> LuaResult<Option<OscParamSet>> {
    let osc_tbl = match init_tbl.get::<LuaValue>("osc")? {
        LuaValue::Table(t) => t,
        _ => return Ok(None),
    };
    let params_tbl = match osc_tbl.get::<LuaValue>("params")? {
        LuaValue::Table(t) => t,
        _ => return Ok(None),
    };
    let default_timeout = match osc_tbl.get::<LuaValue>("subscribe_timeout")? {
        LuaValue::Integer(n) => n as f64,
        LuaValue::Number(f) => f,
        _ => 30.0,
    };
    let now = lua_now(lua)?;
    let mut ps = OscParamSet::new(prefix, default_timeout, now);
    for pair in params_tbl.pairs::<String, LuaTable>() {
        let (name, desc) = pair?;
        let get: Option<LuaFunction> = desc.get("get").ok().flatten();
        let set: Option<LuaFunction> = desc.get("set").ok().flatten();
        ps.add_param(lua, name, get, set)?;
    }
    Ok(Some(ps))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_lua() -> Lua {
        let lua = Lua::new();
        lua.load(
            r#"
            _sent = {}
            function send_osc(...) table.insert(_sent, {...}) end
            function clear() _sent = {} end
            _time = 1000
            os.time = function() return _time end
            "#,
        )
        .exec()
        .unwrap();
        lua
    }

    fn make_ps(lua: &Lua, prefix: &str) -> OscParamSet {
        OscParamSet::new(prefix, 30.0, lua_now(lua).unwrap())
    }

    fn make_msg(lua: &Lua, addr: &str, from: &str, args_lua: &str) -> LuaTable {
        lua.load(&format!(
            r#"{{ address = "{}", from = "{}", args = {{{}}} }}"#,
            addr, from, args_lua
        ))
        .eval()
        .unwrap()
    }

    fn assert_lua(lua: &Lua, code: &str) {
        lua.load(code)
            .exec()
            .unwrap_or_else(|e| panic!("{}", e));
    }

    // ── Basic dispatch ────────────────────────────────────────────────────────

    #[test]
    fn set_calls_handler_with_args() {
        let lua = make_lua();
        lua.globals().set("v", 0i64).unwrap();
        let mut ps = make_ps(&lua, "/p");
        let set_fn: LuaFunction = lua.load("function(a) v = a end").eval().unwrap();
        ps.add_param(&lua, "x".to_string(), None, Some(set_fn)).unwrap();

        let msg = make_msg(&lua, "/p/x", "1.2.3.4:9000", "42");
        ps.dispatch(&lua, &msg).unwrap();

        assert_lua(&lua, r#"assert(v == 42, "expected 42, got " .. tostring(v))"#);
    }

    #[test]
    fn get_query_replies_to_sender_only() {
        let lua = make_lua();
        let mut ps = make_ps(&lua, "/p");
        let get_fn: LuaFunction = lua.load("function() return 7 end").eval().unwrap();
        ps.add_param(&lua, "x".to_string(), Some(get_fn), None).unwrap();

        let msg = make_msg(&lua, "/p/x", "1.2.3.4:9001", "");
        ps.dispatch(&lua, &msg).unwrap();

        assert_lua(
            &lua,
            r#"
            assert(#_sent == 1, "expected 1 send, got " .. #_sent)
            assert(_sent[1][1] == "1.2.3.4:9001", "wrong dest: " .. tostring(_sent[1][1]))
            assert(_sent[1][2] == "/p/x",          "wrong addr: " .. tostring(_sent[1][2]))
            assert(_sent[1][3] == 7,               "wrong value: " .. tostring(_sent[1][3]))
            "#,
        );
    }

    #[test]
    fn set_only_param_called_with_no_args_form() {
        let lua = make_lua();
        lua.globals().set("called", false).unwrap();
        let mut ps = make_ps(&lua, "/p");
        let set_fn: LuaFunction =
            lua.load("function() called = true end").eval().unwrap();
        ps.add_param(&lua, "stop".to_string(), None, Some(set_fn)).unwrap();

        let msg = make_msg(&lua, "/p/stop", "1.2.3.4:9000", "");
        ps.dispatch(&lua, &msg).unwrap();

        assert_lua(&lua, r#"assert(called, "stop handler should have been called")"#);
    }

    #[test]
    fn unknown_param_is_silently_ignored() {
        let lua = make_lua();
        let mut ps = make_ps(&lua, "/p");
        let set_fn: LuaFunction = lua.load("function(v) end").eval().unwrap();
        ps.add_param(&lua, "x".to_string(), None, Some(set_fn)).unwrap();

        let msg = make_msg(&lua, "/p/nonexistent", "1.2.3.4:9000", "1");
        ps.dispatch(&lua, &msg).unwrap();

        assert_lua(&lua, r#"assert(#_sent == 0, "should not have sent anything")"#);
    }

    #[test]
    fn wrong_prefix_is_not_dispatched() {
        let lua = make_lua();
        lua.globals().set("v", 0i64).unwrap();
        let mut ps = make_ps(&lua, "/metronome");
        let set_fn: LuaFunction = lua.load("function(x) v = x end").eval().unwrap();
        ps.add_param(&lua, "bpm".to_string(), None, Some(set_fn)).unwrap();

        let msg = make_msg(&lua, "/other/bpm", "1.2.3.4:9000", "130");
        ps.dispatch(&lua, &msg).unwrap();

        assert_lua(&lua, r#"assert(v == 0, "wrong prefix should not dispatch")"#);
    }

    // ── Post-set notification uses get(), not raw args ────────────────────────

    #[test]
    fn post_set_notifies_via_get_not_raw_args() {
        let lua = make_lua();
        lua.globals().set("bpm", 120i64).unwrap();
        let mut ps = make_ps(&lua, "/p");
        let set_fn: LuaFunction = lua
            .load("function(v) bpm = math.max(20, math.min(200, v)) end")
            .eval()
            .unwrap();
        let get_fn: LuaFunction = lua.load("function() return bpm end").eval().unwrap();
        ps.add_param(&lua, "bpm".to_string(), Some(get_fn), Some(set_fn)).unwrap();

        // Subscribe first
        let sub = make_msg(&lua, "/p/subscribe", "1.2.3.4:9001", "");
        ps.dispatch(&lua, &sub).unwrap();
        assert_lua(&lua, "clear()");

        // Set 999 — should be clamped to 200
        let msg = make_msg(&lua, "/p/bpm", "1.2.3.4:9000", "999");
        ps.dispatch(&lua, &msg).unwrap();

        assert_lua(
            &lua,
            r#"
            assert(bpm == 200, "bpm should be clamped to 200")
            local notified_value = nil
            for _, s in ipairs(_sent) do
                if s[1] == "1.2.3.4:9001" and s[2] == "/p/bpm" then
                    notified_value = s[3]
                end
            end
            assert(notified_value == 200,
                "notification should carry clamped value 200, got " .. tostring(notified_value))
            "#,
        );
    }

    #[test]
    fn set_without_get_does_not_notify_subscribers() {
        let lua = make_lua();
        let mut ps = make_ps(&lua, "/p");
        let set_fn: LuaFunction = lua.load("function(v) end").eval().unwrap();
        ps.add_param(&lua, "cmd".to_string(), None, Some(set_fn)).unwrap();

        let sub = make_msg(&lua, "/p/subscribe", "1.2.3.4:9001", "");
        ps.dispatch(&lua, &sub).unwrap();
        assert_lua(&lua, "clear()");

        let msg = make_msg(&lua, "/p/cmd", "1.2.3.4:9000", "1");
        ps.dispatch(&lua, &msg).unwrap();

        assert_lua(&lua, r#"assert(#_sent == 0, "set-only param must not notify")"#);
    }

    // ── Subscribe / unsubscribe ───────────────────────────────────────────────

    #[test]
    fn subscribe_sends_initial_state_dump() {
        let lua = make_lua();
        let mut ps = make_ps(&lua, "/p");
        let get_bpm: LuaFunction = lua.load("function() return 120 end").eval().unwrap();
        let get_mute: LuaFunction = lua.load("function() return 0 end").eval().unwrap();
        let set_stop: LuaFunction = lua.load("function() end").eval().unwrap();
        ps.add_param(&lua, "bpm".to_string(), Some(get_bpm), None).unwrap();
        ps.add_param(&lua, "mute".to_string(), Some(get_mute), None).unwrap();
        ps.add_param(&lua, "stop".to_string(), None, Some(set_stop)).unwrap();

        let sub = make_msg(&lua, "/p/subscribe", "1.2.3.4:9001", "");
        ps.dispatch(&lua, &sub).unwrap();

        assert_lua(
            &lua,
            r#"
            local count = 0
            for _, s in ipairs(_sent) do
                if s[1] == "1.2.3.4:9001" then count = count + 1 end
            end
            assert(count == 2, "expected 2 initial-state sends, got " .. count)
            "#,
        );
    }

    #[test]
    fn subscribe_does_not_dump_set_only_params() {
        let lua = make_lua();
        let mut ps = make_ps(&lua, "/p");
        let set_fn: LuaFunction = lua.load("function() end").eval().unwrap();
        ps.add_param(&lua, "stop".to_string(), None, Some(set_fn)).unwrap();

        let sub = make_msg(&lua, "/p/subscribe", "1.2.3.4:9001", "");
        ps.dispatch(&lua, &sub).unwrap();

        assert_lua(
            &lua,
            r#"assert(#_sent == 0, "set-only params should not appear in state dump")"#,
        );
    }

    #[test]
    fn subscribe_registers_for_change_notifications() {
        let lua = make_lua();
        lua.globals().set("v", 0i64).unwrap();
        let mut ps = make_ps(&lua, "/p");
        let set_fn: LuaFunction = lua.load("function(a) v = a end").eval().unwrap();
        let get_fn: LuaFunction = lua.load("function() return v end").eval().unwrap();
        ps.add_param(&lua, "x".to_string(), Some(get_fn), Some(set_fn)).unwrap();

        let sub = make_msg(&lua, "/p/subscribe", "1.2.3.4:9001", "");
        ps.dispatch(&lua, &sub).unwrap();
        assert_lua(&lua, "clear()");

        let msg = make_msg(&lua, "/p/x", "1.2.3.4:9000", "55");
        ps.dispatch(&lua, &msg).unwrap();

        assert_lua(
            &lua,
            r#"
            local notified = false
            for _, s in ipairs(_sent) do
                if s[1] == "1.2.3.4:9001" and s[2] == "/p/x" then notified = true end
            end
            assert(notified, "subscriber should have received change notification")
            "#,
        );
    }

    #[test]
    fn unsubscribe_stops_change_notifications() {
        let lua = make_lua();
        lua.globals().set("v", 0i64).unwrap();
        let mut ps = make_ps(&lua, "/p");
        let set_fn: LuaFunction = lua.load("function(a) v = a end").eval().unwrap();
        let get_fn: LuaFunction = lua.load("function() return v end").eval().unwrap();
        ps.add_param(&lua, "x".to_string(), Some(get_fn), Some(set_fn)).unwrap();

        let sub = make_msg(&lua, "/p/subscribe", "1.2.3.4:9001", "");
        ps.dispatch(&lua, &sub).unwrap();
        let unsub = make_msg(&lua, "/p/unsubscribe", "1.2.3.4:9001", "");
        ps.dispatch(&lua, &unsub).unwrap();
        assert_lua(&lua, "clear()");

        let msg = make_msg(&lua, "/p/x", "1.2.3.4:9000", "55");
        ps.dispatch(&lua, &msg).unwrap();

        assert_lua(
            &lua,
            r#"assert(#_sent == 0, "unsubscribed address must not receive notifications")"#,
        );
    }

    #[test]
    fn multiple_subscribers_all_notified() {
        let lua = make_lua();
        lua.globals().set("v", 0i64).unwrap();
        let mut ps = make_ps(&lua, "/p");
        let set_fn: LuaFunction = lua.load("function(a) v = a end").eval().unwrap();
        let get_fn: LuaFunction = lua.load("function() return v end").eval().unwrap();
        ps.add_param(&lua, "x".to_string(), Some(get_fn), Some(set_fn)).unwrap();

        ps.dispatch(&lua, &make_msg(&lua, "/p/subscribe", "10.0.0.1:9001", "")).unwrap();
        ps.dispatch(&lua, &make_msg(&lua, "/p/subscribe", "10.0.0.2:9001", "")).unwrap();
        assert_lua(&lua, "clear()");

        let msg = make_msg(&lua, "/p/x", "10.0.0.3:9000", "7");
        ps.dispatch(&lua, &msg).unwrap();

        assert_lua(
            &lua,
            r#"
            local seen = {}
            for _, s in ipairs(_sent) do
                if s[2] == "/p/x" then seen[s[1]] = true end
            end
            assert(seen["10.0.0.1:9001"], "first subscriber should be notified")
            assert(seen["10.0.0.2:9001"], "second subscriber should be notified")
            "#,
        );
    }

    // ── Explicit feedback port ────────────────────────────────────────────────

    #[test]
    fn subscribe_with_explicit_port_uses_that_port() {
        let lua = make_lua();
        lua.globals().set("v", 0i64).unwrap();
        let mut ps = make_ps(&lua, "/p");
        let set_fn: LuaFunction = lua.load("function(a) v = a end").eval().unwrap();
        let get_fn: LuaFunction = lua.load("function() return v end").eval().unwrap();
        ps.add_param(&lua, "x".to_string(), Some(get_fn), Some(set_fn)).unwrap();

        // Subscribe from :5000 but request feedback on :9001
        let sub = make_msg(&lua, "/p/subscribe", "1.2.3.4:5000", "9001");
        ps.dispatch(&lua, &sub).unwrap();
        assert_lua(&lua, "clear()");

        let msg = make_msg(&lua, "/p/x", "1.2.3.4:5000", "3");
        ps.dispatch(&lua, &msg).unwrap();

        assert_lua(
            &lua,
            r#"
            local notified_on_9001 = false
            for _, s in ipairs(_sent) do
                if s[1] == "1.2.3.4:9001" then notified_on_9001 = true end
            end
            assert(notified_on_9001, "notification should go to explicit feedback port 9001")
            "#,
        );
    }

    #[test]
    fn unsubscribe_with_explicit_port_removes_correct_entry() {
        let lua = make_lua();
        lua.globals().set("v", 0i64).unwrap();
        let mut ps = make_ps(&lua, "/p");
        let set_fn: LuaFunction = lua.load("function(a) v = a end").eval().unwrap();
        let get_fn: LuaFunction = lua.load("function() return v end").eval().unwrap();
        ps.add_param(&lua, "x".to_string(), Some(get_fn), Some(set_fn)).unwrap();

        ps.dispatch(&lua, &make_msg(&lua, "/p/subscribe", "1.2.3.4:5000", "9001")).unwrap();
        ps.dispatch(&lua, &make_msg(&lua, "/p/unsubscribe", "1.2.3.4:5000", "9001")).unwrap();
        assert_lua(&lua, "clear()");

        let msg = make_msg(&lua, "/p/x", "1.2.3.4:5000", "3");
        ps.dispatch(&lua, &msg).unwrap();

        assert_lua(
            &lua,
            r#"assert(#_sent == 0, "explicitly-unsubscribed port should not be notified")"#,
        );
    }

    // ── Timeout / heartbeat ───────────────────────────────────────────────────

    #[test]
    fn custom_timeout_in_subscribe_message() {
        let lua = make_lua();
        let mut ps = make_ps(&lua, "/p");
        let get_fn: LuaFunction = lua.load("function() return 1 end").eval().unwrap();
        ps.add_param(&lua, "x".to_string(), Some(get_fn), None).unwrap();

        // Subscribe with 10-second timeout via args: port=9001, timeout=10
        let sub = make_msg(&lua, "/p/subscribe", "1.2.3.4:9001", "9001, 10");
        ps.dispatch(&lua, &sub).unwrap();
        assert_lua(&lua, "clear()");

        // Advance 11 s → past 10-s timeout; run two eviction windows
        assert_lua(&lua, "_time = 1011");
        ps.tick(&lua).unwrap();
        assert_lua(&lua, "_time = 1016");
        ps.tick(&lua).unwrap();

        // Query — should not reach the evicted subscriber
        let msg = make_msg(&lua, "/p/x", "1.2.3.4:9000", "");
        ps.dispatch(&lua, &msg).unwrap();

        assert_lua(
            &lua,
            r#"
            local reached = false
            for _, s in ipairs(_sent) do
                if s[1] == "1.2.3.4:9001" then reached = true end
            end
            assert(not reached, "evicted subscriber should not receive any sends")
            "#,
        );
    }

    #[test]
    fn subscriber_evicted_after_default_timeout() {
        let lua = make_lua();
        lua.globals().set("v", 0i64).unwrap();
        let mut ps = make_ps(&lua, "/p");
        let set_fn: LuaFunction = lua.load("function(a) v = a end").eval().unwrap();
        let get_fn: LuaFunction = lua.load("function() return v end").eval().unwrap();
        ps.add_param(&lua, "x".to_string(), Some(get_fn), Some(set_fn)).unwrap();

        let sub = make_msg(&lua, "/p/subscribe", "1.2.3.4:9001", "");
        ps.dispatch(&lua, &sub).unwrap();
        assert_lua(&lua, "clear()");

        assert_lua(&lua, "_time = 1031");
        ps.tick(&lua).unwrap();
        assert_lua(&lua, "_time = 1036");
        ps.tick(&lua).unwrap();

        let msg = make_msg(&lua, "/p/x", "1.2.3.4:9000", "5");
        ps.dispatch(&lua, &msg).unwrap();

        assert_lua(
            &lua,
            r#"
            for _, s in ipairs(_sent) do
                assert(s[1] ~= "1.2.3.4:9001",
                    "evicted subscriber should not receive notification")
            end
            "#,
        );
    }

    #[test]
    fn renewing_subscription_resets_expiry() {
        let lua = make_lua();
        lua.globals().set("v", 0i64).unwrap();
        let mut ps = make_ps(&lua, "/p");
        let set_fn: LuaFunction = lua.load("function(a) v = a end").eval().unwrap();
        let get_fn: LuaFunction = lua.load("function() return v end").eval().unwrap();
        ps.add_param(&lua, "x".to_string(), Some(get_fn), Some(set_fn)).unwrap();

        let sub = make_msg(&lua, "/p/subscribe", "1.2.3.4:9001", "");
        ps.dispatch(&lua, &sub).unwrap();

        // Advance 25 s; re-subscribe → expiry resets to 1025+30=1055
        assert_lua(&lua, "_time = 1025");
        ps.dispatch(&lua, &sub).unwrap();
        assert_lua(&lua, "clear()");

        // 1040 would have expired (1030) without renewal
        assert_lua(&lua, "_time = 1040");
        ps.tick(&lua).unwrap();
        assert_lua(&lua, "_time = 1045");
        ps.tick(&lua).unwrap();

        let msg = make_msg(&lua, "/p/x", "1.2.3.4:9000", "9");
        ps.dispatch(&lua, &msg).unwrap();

        assert_lua(
            &lua,
            r#"
            local notified = false
            for _, s in ipairs(_sent) do
                if s[1] == "1.2.3.4:9001" and s[2] == "/p/x" then notified = true end
            end
            assert(notified, "renewed subscription should still be active at t=1040")
            "#,
        );
    }

    #[test]
    fn heartbeat_sent_after_interval() {
        let lua = make_lua();
        let mut ps = make_ps(&lua, "/p");

        let sub = make_msg(&lua, "/p/subscribe", "1.2.3.4:9001", "");
        ps.dispatch(&lua, &sub).unwrap();
        assert_lua(&lua, "clear()");

        assert_lua(&lua, "_time = 1006");
        ps.tick(&lua).unwrap();

        assert_lua(
            &lua,
            r#"
            local hb_sent = false
            for _, s in ipairs(_sent) do
                if s[1] == "1.2.3.4:9001" and s[2] == "/p/heartbeat" then hb_sent = true end
            end
            assert(hb_sent, "heartbeat should be sent after 6 seconds")
            "#,
        );
    }

    #[test]
    fn heartbeat_not_sent_before_interval() {
        let lua = make_lua();
        let mut ps = make_ps(&lua, "/p");

        let sub = make_msg(&lua, "/p/subscribe", "1.2.3.4:9001", "");
        ps.dispatch(&lua, &sub).unwrap();
        assert_lua(&lua, "clear()");

        assert_lua(&lua, "_time = 1003");
        ps.tick(&lua).unwrap();

        assert_lua(
            &lua,
            r#"
            for _, s in ipairs(_sent) do
                assert(s[2] ~= "/p/heartbeat",
                    "heartbeat must not be sent before the interval")
            end
            "#,
        );
    }

    #[test]
    fn evicted_subscriber_stops_receiving_heartbeats() {
        let lua = make_lua();
        let mut ps = make_ps(&lua, "/p");

        let sub = make_msg(&lua, "/p/subscribe", "1.2.3.4:9001", "");
        ps.dispatch(&lua, &sub).unwrap();

        assert_lua(&lua, "_time = 1031");
        ps.tick(&lua).unwrap();
        assert_lua(&lua, "_time = 1036");
        ps.tick(&lua).unwrap();
        assert_lua(&lua, "clear()");

        assert_lua(&lua, "_time = 1042");
        ps.tick(&lua).unwrap();

        assert_lua(
            &lua,
            r#"
            for _, s in ipairs(_sent) do
                assert(s[1] ~= "1.2.3.4:9001",
                    "evicted subscriber must not receive heartbeats")
            end
            "#,
        );
    }

    #[test]
    fn eviction_check_runs_on_interval_not_every_tick() {
        let lua = make_lua();
        lua.globals().set("v", 0i64).unwrap();
        let mut ps = make_ps(&lua, "/p");
        let set_fn: LuaFunction = lua.load("function(a) v = a end").eval().unwrap();
        let get_fn: LuaFunction = lua.load("function() return v end").eval().unwrap();
        ps.add_param(&lua, "x".to_string(), Some(get_fn), Some(set_fn)).unwrap();

        let sub = make_msg(&lua, "/p/subscribe", "1.2.3.4:9001", "");
        ps.dispatch(&lua, &sub).unwrap();
        assert_lua(&lua, "clear()");

        assert_lua(&lua, "_time = 1002");
        for _ in 0..10 {
            ps.tick(&lua).unwrap();
        }

        let msg = make_msg(&lua, "/p/x", "1.2.3.4:9000", "3");
        ps.dispatch(&lua, &msg).unwrap();

        assert_lua(
            &lua,
            r#"
            local notified = false
            for _, s in ipairs(_sent) do
                if s[1] == "1.2.3.4:9001" and s[2] == "/p/x" then notified = true end
            end
            assert(notified, "subscriber should still be active after 2 seconds")
            "#,
        );
    }
}
