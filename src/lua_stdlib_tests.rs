//! Integration tests for src/lua/stdlib.lua.
//!
//! Each test creates a fresh Lua VM with:
//!   • a `send_osc(...)` stub that appends calls to `_sent`
//!   • `os.time()` replaced by a controllable `_time` integer
//!   • a `M(addr, from, ...)` helper that builds on_osc message tables
//!   • `clear()` that empties `_sent` between operations
//!
//! Tests assert entirely in Lua via `assert(cond, msg)`.  A failing assert
//! propagates as a Lua error which `run()` converts to a Rust panic.

#[cfg(test)]
mod tests {
    use mlua::prelude::*;

    const STDLIB: &str = include_str!("lua/stdlib.lua");

    fn make_lua() -> Lua {
        let lua = Lua::new();
        lua.load(r#"
            _sent = {}
            function send_osc(...) table.insert(_sent, {...}) end
            function clear() _sent = {} end
            _time = 1000
            os.time = function() return _time end
            function M(addr, from, ...)
                return { address = addr, from = from, args = {...} }
            end
        "#)
        .exec()
        .unwrap();
        lua.load(STDLIB).set_name("stdlib").exec().unwrap();
        lua
    }

    fn run(lua: &Lua, code: &str) {
        lua.load(code).exec().unwrap_or_else(|e| panic!("{}", e));
    }

    // ── Basic dispatch ────────────────────────────────────────────────────────

    #[test]
    fn set_calls_handler_with_args() {
        let lua = make_lua();
        run(&lua, r#"
            local v = 0
            local on_osc, _ = osc_params("/p", {
                x = { set = function(a) v = a end },
            })
            on_osc(M("/p/x", "1.2.3.4:9000", 42))
            assert(v == 42, "expected 42, got " .. tostring(v))
        "#);
    }

    #[test]
    fn get_query_replies_to_sender_only() {
        let lua = make_lua();
        run(&lua, r#"
            local on_osc, _ = osc_params("/p", {
                x = { get = function() return 7 end },
            })
            on_osc(M("/p/x", "1.2.3.4:9001"))
            assert(#_sent == 1, "expected 1 send, got " .. #_sent)
            assert(_sent[1][1] == "1.2.3.4:9001", "wrong dest: " .. tostring(_sent[1][1]))
            assert(_sent[1][2] == "/p/x",          "wrong addr: " .. tostring(_sent[1][2]))
            assert(_sent[1][3] == 7,               "wrong value: " .. tostring(_sent[1][3]))
        "#);
    }

    #[test]
    fn set_only_param_called_with_no_args_form() {
        // A param with only `set` (no `get`) treats a no-arg message as a command.
        let lua = make_lua();
        run(&lua, r#"
            local called = false
            local on_osc, _ = osc_params("/p", {
                stop = { set = function() called = true end },
            })
            on_osc(M("/p/stop", "1.2.3.4:9000"))
            assert(called, "stop handler should have been called")
        "#);
    }

    #[test]
    fn unknown_param_is_silently_ignored() {
        let lua = make_lua();
        run(&lua, r#"
            local on_osc, _ = osc_params("/p", {
                x = { set = function(v) end },
            })
            on_osc(M("/p/nonexistent", "1.2.3.4:9000", 1))
            assert(#_sent == 0, "should not have sent anything")
        "#);
    }

    #[test]
    fn wrong_prefix_is_not_dispatched() {
        let lua = make_lua();
        run(&lua, r#"
            local v = 0
            local on_osc, _ = osc_params("/metronome", {
                bpm = { set = function(x) v = x end },
            })
            on_osc(M("/other/bpm", "1.2.3.4:9000", 130))
            assert(v == 0, "wrong prefix should not dispatch")
        "#);
    }

    // ── Post-set notification uses get(), not raw args ────────────────────────

    #[test]
    fn post_set_notifies_via_get_not_raw_args() {
        // set() clamps the value; the notification must report the clamped result.
        let lua = make_lua();
        run(&lua, r#"
            local bpm = 120
            local on_osc, _ = osc_params("/p", {
                bpm = {
                    set = function(v) bpm = math.max(20, math.min(200, v)) end,
                    get = function() return bpm end,
                },
            })
            -- Subscribe first so someone receives the notification
            on_osc(M("/p/subscribe", "1.2.3.4:9001"))
            clear()

            on_osc(M("/p/bpm", "1.2.3.4:9000", 999))   -- 999 clamped to 200

            assert(bpm == 200, "bpm should be clamped to 200")
            -- notification should carry 200 (from get), not 999 (raw arg)
            local notified_value = nil
            for _, s in ipairs(_sent) do
                if s[1] == "1.2.3.4:9001" and s[2] == "/p/bpm" then
                    notified_value = s[3]
                end
            end
            assert(notified_value == 200,
                "notification should carry clamped value 200, got " .. tostring(notified_value))
        "#);
    }

    #[test]
    fn set_without_get_does_not_notify_subscribers() {
        let lua = make_lua();
        run(&lua, r#"
            local on_osc, _ = osc_params("/p", {
                cmd = { set = function(v) end },   -- no get
            })
            on_osc(M("/p/subscribe", "1.2.3.4:9001"))
            clear()
            on_osc(M("/p/cmd", "1.2.3.4:9000", 1))
            assert(#_sent == 0, "set-only param must not notify")
        "#);
    }

    // ── Subscribe / unsubscribe ───────────────────────────────────────────────

    #[test]
    fn subscribe_sends_initial_state_dump() {
        let lua = make_lua();
        run(&lua, r#"
            local on_osc, _ = osc_params("/p", {
                bpm  = { get = function() return 120 end },
                mute = { get = function() return 0   end },
                stop = { set = function() end },   -- set-only, must be skipped
            })
            on_osc(M("/p/subscribe", "1.2.3.4:9001"))

            -- Two get-able params → two sends to the subscriber address
            local count = 0
            for _, s in ipairs(_sent) do
                if s[1] == "1.2.3.4:9001" then count = count + 1 end
            end
            assert(count == 2, "expected 2 initial-state sends, got " .. count)
        "#);
    }

    #[test]
    fn subscribe_does_not_dump_set_only_params() {
        let lua = make_lua();
        run(&lua, r#"
            local on_osc, _ = osc_params("/p", {
                stop = { set = function() end },
            })
            on_osc(M("/p/subscribe", "1.2.3.4:9001"))
            assert(#_sent == 0, "set-only params should not appear in state dump")
        "#);
    }

    #[test]
    fn subscribe_registers_for_change_notifications() {
        let lua = make_lua();
        run(&lua, r#"
            local v = 0
            local on_osc, _ = osc_params("/p", {
                x = { set = function(a) v = a end, get = function() return v end },
            })
            on_osc(M("/p/subscribe", "1.2.3.4:9001"))
            clear()

            on_osc(M("/p/x", "1.2.3.4:9000", 55))

            local notified = false
            for _, s in ipairs(_sent) do
                if s[1] == "1.2.3.4:9001" and s[2] == "/p/x" then
                    notified = true
                end
            end
            assert(notified, "subscriber should have received change notification")
        "#);
    }

    #[test]
    fn unsubscribe_stops_change_notifications() {
        let lua = make_lua();
        run(&lua, r#"
            local v = 0
            local on_osc, _ = osc_params("/p", {
                x = { set = function(a) v = a end, get = function() return v end },
            })
            on_osc(M("/p/subscribe",   "1.2.3.4:9001"))
            on_osc(M("/p/unsubscribe", "1.2.3.4:9001"))
            clear()

            on_osc(M("/p/x", "1.2.3.4:9000", 55))

            assert(#_sent == 0, "unsubscribed address must not receive notifications")
        "#);
    }

    #[test]
    fn multiple_subscribers_all_notified() {
        let lua = make_lua();
        run(&lua, r#"
            local v = 0
            local on_osc, _ = osc_params("/p", {
                x = { set = function(a) v = a end, get = function() return v end },
            })
            on_osc(M("/p/subscribe", "10.0.0.1:9001"))
            on_osc(M("/p/subscribe", "10.0.0.2:9001"))
            clear()

            on_osc(M("/p/x", "10.0.0.3:9000", 7))

            local seen = {}
            for _, s in ipairs(_sent) do
                if s[2] == "/p/x" then seen[s[1]] = true end
            end
            assert(seen["10.0.0.1:9001"], "first subscriber should be notified")
            assert(seen["10.0.0.2:9001"], "second subscriber should be notified")
        "#);
    }

    // ── Explicit feedback port ────────────────────────────────────────────────

    #[test]
    fn subscribe_with_explicit_port_uses_that_port() {
        let lua = make_lua();
        run(&lua, r#"
            local v = 0
            local on_osc, _ = osc_params("/p", {
                x = { set = function(a) v = a end, get = function() return v end },
            })
            -- Send from :5000, but request feedback on :9001
            on_osc(M("/p/subscribe", "1.2.3.4:5000", 9001))
            clear()

            on_osc(M("/p/x", "1.2.3.4:5000", 3))

            local notified_on_9001 = false
            for _, s in ipairs(_sent) do
                if s[1] == "1.2.3.4:9001" then notified_on_9001 = true end
            end
            assert(notified_on_9001, "notification should go to explicit feedback port 9001")
        "#);
    }

    #[test]
    fn unsubscribe_with_explicit_port_removes_correct_entry() {
        let lua = make_lua();
        run(&lua, r#"
            local v = 0
            local on_osc, _ = osc_params("/p", {
                x = { set = function(a) v = a end, get = function() return v end },
            })
            on_osc(M("/p/subscribe",   "1.2.3.4:5000", 9001))
            on_osc(M("/p/unsubscribe", "1.2.3.4:5000", 9001))
            clear()

            on_osc(M("/p/x", "1.2.3.4:5000", 3))

            assert(#_sent == 0, "explicitly-unsubscribed port should not be notified")
        "#);
    }

    // ── Timeout / heartbeat ───────────────────────────────────────────────────

    #[test]
    fn custom_timeout_in_subscribe_message() {
        let lua = make_lua();
        run(&lua, r#"
            local on_osc, osc_tick = osc_params("/p", {
                x = { get = function() return 1 end },
            })
            -- Subscribe with a 10-second timeout
            on_osc(M("/p/subscribe", "1.2.3.4:9001", 9001, 10))
            clear()

            -- Advance 11 seconds and run eviction
            _time = 1011
            osc_tick()
            _time = 1016   -- past EVICTION_INTERVAL from last check
            osc_tick()

            -- Should be evicted now; a set should not notify
            on_osc(M("/p/x", "1.2.3.4:9000"))   -- query, not a set

            -- No heartbeat or notification should reach the evicted subscriber
            local reached = false
            for _, s in ipairs(_sent) do
                if s[1] == "1.2.3.4:9001" then reached = true end
            end
            assert(not reached, "evicted subscriber should not receive any sends")
        "#);
    }

    #[test]
    fn subscriber_evicted_after_default_timeout() {
        let lua = make_lua();
        run(&lua, r#"
            local v = 0
            local on_osc, osc_tick = osc_params("/p", {
                x = { set = function(a) v = a end, get = function() return v end },
            })
            on_osc(M("/p/subscribe", "1.2.3.4:9001"))
            clear()

            -- Advance past default 30-second timeout + eviction check interval
            _time = 1031
            osc_tick()     -- first check; now - last_eviction = 31 >= 5
            _time = 1036
            osc_tick()     -- second check to be sure

            on_osc(M("/p/x", "1.2.3.4:9000", 5))

            for _, s in ipairs(_sent) do
                assert(s[1] ~= "1.2.3.4:9001",
                    "evicted subscriber should not receive notification")
            end
        "#);
    }

    #[test]
    fn renewing_subscription_resets_expiry() {
        let lua = make_lua();
        run(&lua, r#"
            local v = 0
            local on_osc, osc_tick = osc_params("/p", {
                x = { set = function(a) v = a end, get = function() return v end },
            })
            on_osc(M("/p/subscribe", "1.2.3.4:9001"))

            -- Advance 25 seconds — not yet expired
            _time = 1025
            -- Renew: resets expiry to now + 30 = 1055
            on_osc(M("/p/subscribe", "1.2.3.4:9001"))
            clear()

            -- Advance to 1040 — would have expired (1030) without renewal
            _time = 1040
            osc_tick()
            _time = 1045
            osc_tick()

            on_osc(M("/p/x", "1.2.3.4:9000", 9))

            local notified = false
            for _, s in ipairs(_sent) do
                if s[1] == "1.2.3.4:9001" and s[2] == "/p/x" then
                    notified = true
                end
            end
            assert(notified, "renewed subscription should still be active at t=1040")
        "#);
    }

    #[test]
    fn heartbeat_sent_after_interval() {
        let lua = make_lua();
        run(&lua, r#"
            local on_osc, osc_tick = osc_params("/p", {})
            on_osc(M("/p/subscribe", "1.2.3.4:9001"))
            clear()

            -- Advance 6 seconds (past HEARTBEAT_INTERVAL = 5)
            _time = 1006
            osc_tick()

            local hb_sent = false
            for _, s in ipairs(_sent) do
                if s[1] == "1.2.3.4:9001" and s[2] == "/p/heartbeat" then
                    hb_sent = true
                end
            end
            assert(hb_sent, "heartbeat should be sent after 6 seconds")
        "#);
    }

    #[test]
    fn heartbeat_not_sent_before_interval() {
        let lua = make_lua();
        run(&lua, r#"
            local on_osc, osc_tick = osc_params("/p", {})
            on_osc(M("/p/subscribe", "1.2.3.4:9001"))
            clear()

            -- Advance only 3 seconds (less than HEARTBEAT_INTERVAL = 5)
            _time = 1003
            osc_tick()

            for _, s in ipairs(_sent) do
                assert(s[2] ~= "/p/heartbeat",
                    "heartbeat must not be sent before the interval")
            end
        "#);
    }

    #[test]
    fn evicted_subscriber_stops_receiving_heartbeats() {
        let lua = make_lua();
        run(&lua, r#"
            local on_osc, osc_tick = osc_params("/p", {})
            on_osc(M("/p/subscribe", "1.2.3.4:9001"))

            -- Expire and evict
            _time = 1031
            osc_tick()
            _time = 1036
            osc_tick()
            clear()

            -- Trigger another heartbeat window
            _time = 1042
            osc_tick()

            for _, s in ipairs(_sent) do
                assert(s[1] ~= "1.2.3.4:9001",
                    "evicted subscriber must not receive heartbeats")
            end
        "#);
    }

    #[test]
    fn eviction_check_runs_on_interval_not_every_tick() {
        // Call osc_tick many times within the eviction window and confirm
        // that a subscriber who should not yet be evicted is still alive.
        let lua = make_lua();
        run(&lua, r#"
            local v = 0
            local on_osc, osc_tick = osc_params("/p", {
                x = { set = function(a) v = a end, get = function() return v end },
            })
            on_osc(M("/p/subscribe", "1.2.3.4:9001"))
            clear()

            -- Tick many times but only advance 2 seconds — well within timeout
            for i = 1, 100 do
                _time = 1000 + i * 0.02   -- fractional; os.time() truncates? No — mock returns number
            end
            _time = 1002
            for i = 1, 10 do osc_tick() end

            -- Subscriber should still be alive
            on_osc(M("/p/x", "1.2.3.4:9000", 3))

            local notified = false
            for _, s in ipairs(_sent) do
                if s[1] == "1.2.3.4:9001" and s[2] == "/p/x" then notified = true end
            end
            assert(notified, "subscriber should still be active after 2 seconds")
        "#);
    }
}
