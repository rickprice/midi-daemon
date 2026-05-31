-- midi-daemon Lua stdlib — loaded into every route before its script.
-- All symbols defined here are globals available without any require/dofile.

--- Build on_osc and on_tick handlers from a declarative parameter table,
--- following Ardour's subscribe/notify/heartbeat pattern.
--
-- Returns TWO values: on_osc_fn, on_tick_fn.
-- Assign both so that subscriber eviction and /heartbeat pings work:
--
--   local osc_tick
--   on_osc, osc_tick = osc_params("/" .. ROUTE_NAME, { ... })
--
--   function on_tick(tick, bpm, ppqn)
--       osc_tick()         -- evict timed-out subscribers; send /heartbeat
--       -- ...route logic...
--   end
--
-- ── Subscribe/unsubscribe ─────────────────────────────────────────────────────
--
--   /prefix/subscribe [feedback_port [timeout_secs]]
--       Register (or renew) the sender as a subscriber.
--       feedback_port  — port on the sender's host to send notifications to;
--                        defaults to the source port of the message.
--       timeout_secs   — how long before the subscription expires without renewal;
--                        defaults to default_timeout (30 s, matching Ardour).
--       On subscribe, the current value of every get-able param is sent
--       immediately to the feedback address.
--
--   /prefix/unsubscribe [feedback_port]
--       Remove the subscriber immediately.
--
-- ── Heartbeat (Ardour-compatible) ────────────────────────────────────────────
--
--   The daemon sends /prefix/heartbeat to every active subscriber every
--   HEARTBEAT_INTERVAL seconds (default 5 s) so clients know it is alive.
--   Clients are expected to re-send /subscribe before their timeout expires
--   to renew their subscription.
--
-- ── Normal param dispatch ─────────────────────────────────────────────────────
--
--   /prefix/<param>  (no args)
--       Query: get() → reply to msg.from only.
--   /prefix/<param> v…
--       Set: set(v…), then notify all subscribers via get().
--       Post-set notification uses get() so clamped/coerced values are reported.
--
-- ── Parameters ───────────────────────────────────────────────────────────────
--
-- prefix           string   OSC address prefix, e.g. "/" .. ROUTE_NAME
-- params           table    param_name -> { set = fn, get = fn }
-- default_timeout  number   seconds before a subscription expires (default 30)
--
-- Example:
--   local osc_tick
--   on_osc, osc_tick = osc_params("/" .. ROUTE_NAME, {
--       bpm  = { set = function(v) set_bpm(v) end, get = get_bpm },
--       mute = { set = function(v) muted = (v ~= 0) end,
--                get = function() return muted and 1 or 0 end },
--       stop = { set = function() running = false end },
--   })

local HEARTBEAT_INTERVAL = 5   -- send /heartbeat to subscribers every N seconds
local EVICTION_INTERVAL  = 5   -- check for expired subscriptions every N seconds

function osc_params(prefix, params, default_timeout)
    default_timeout = default_timeout or 30

    local slash_prefix     = prefix .. "/"
    local prefix_len       = #slash_prefix
    local subscribe_addr   = prefix .. "/subscribe"
    local unsubscribe_addr = prefix .. "/unsubscribe"
    local heartbeat_addr   = prefix .. "/heartbeat"

    -- subscribers[feedback_addr] = { expiry = <unix time>, timeout = <secs> }
    local subscribers    = {}
    local last_eviction  = os.time()
    local last_heartbeat = os.time()

    -- Compute feedback address and timeout from a subscribe/unsubscribe message.
    -- msg.args[1]: feedback port (optional, overrides source port)
    -- msg.args[2]: timeout in seconds (optional, subscribe only)
    local function parse_feedback(msg)
        local fb = msg.from
        if #msg.args >= 1 then
            local ip = msg.from:match("^([^:]+):")
            if ip then fb = ip .. ":" .. tostring(msg.args[1]) end
        end
        local timeout = default_timeout
        if #msg.args >= 2 then
            timeout = tonumber(msg.args[2]) or default_timeout
        end
        return fb, timeout
    end

    local function notify_subscribers(addr, ...)
        for fb in pairs(subscribers) do
            send_osc(fb, addr, ...)
        end
    end

    -- on_osc handler ──────────────────────────────────────────────────────────
    local on_osc_fn = function(msg)
        local addr = msg.address
        local from = msg.from

        -- /prefix/subscribe [port [timeout]]
        if addr == subscribe_addr then
            local fb, timeout = parse_feedback(msg)
            subscribers[fb] = { expiry = os.time() + timeout, timeout = timeout }
            -- Initial state dump to the new (or renewed) subscriber.
            for pname, p in pairs(params) do
                if p.get then
                    send_osc(fb, slash_prefix .. pname, p.get())
                end
            end
            return
        end

        -- /prefix/unsubscribe [port]
        if addr == unsubscribe_addr then
            subscribers[(parse_feedback(msg))] = nil   -- () truncates to 1 return value
            return
        end

        -- Normal param dispatch
        if addr:sub(1, prefix_len) ~= slash_prefix then return end
        local p = params[addr:sub(prefix_len + 1)]
        if not p then return end

        if #msg.args == 0 then
            if p.get then
                send_osc(from, addr, p.get())   -- query → reply to sender only
            elseif p.set then
                p.set()
            end
        elseif p.set then
            p.set(table.unpack(msg.args))
            if p.get then
                notify_subscribers(addr, p.get())
            end
        end
    end

    -- on_tick handler ─────────────────────────────────────────────────────────
    -- Call this from your route's on_tick (before any early-return guards).
    local on_tick_fn = function()
        local now = os.time()

        -- Evict timed-out subscribers.
        if now - last_eviction >= EVICTION_INTERVAL then
            last_eviction = now
            for fb, sub in pairs(subscribers) do
                if now >= sub.expiry then
                    subscribers[fb] = nil
                end
            end
        end

        -- Send /heartbeat to every active subscriber so clients know we are alive.
        if now - last_heartbeat >= HEARTBEAT_INTERVAL then
            last_heartbeat = now
            for fb in pairs(subscribers) do
                send_osc(fb, heartbeat_addr)
            end
        end
    end

    return on_osc_fn, on_tick_fn
end
