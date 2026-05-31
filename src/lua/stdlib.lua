-- midi-daemon Lua stdlib — loaded into every route before its script.
-- All symbols defined here are globals available without any require/dofile.

--- Build an on_osc handler from a declarative parameter table.
--
-- Follows Ardour's subscribe/notify pattern:
--
--   /prefix/subscribe [feedback_port]
--       Register msg.from (or src_ip:feedback_port) for change notifications.
--       Immediately sends the current value of every get-able param.
--
--   /prefix/unsubscribe [feedback_port]
--       Remove the address from the subscriber list.
--
--   /prefix/<param>  (no args)
--       Query: reply with current value via get() to msg.from directly.
--
--   /prefix/<param> v1 v2 ...
--       Set: call set(v1, v2, ...), then notify all subscribers with get().
--
-- prefix   string   OSC address prefix, e.g. "/" .. ROUTE_NAME
-- params   table    param_name -> { set = fn, get = fn }
--
--   get()           Called on a no-argument message (query or post-set notify).
--                   Return value(s) are sent back on the originating address.
--   set(v, ...)     Called with the message arguments when args are present.
--                   If only set is defined, a no-argument message calls set().
--
-- Example:
--   on_osc = osc_params("/" .. ROUTE_NAME, {
--       bpm  = { set = function(v) set_bpm(v) end, get = get_bpm },
--       mute = { set = function(v) muted = (v ~= 0) end,
--                get = function() return muted and 1 or 0 end },
--       stop = { set = function() running = false end },
--   })
function osc_params(prefix, params)
    local slash_prefix     = prefix .. "/"
    local prefix_len       = #slash_prefix
    local subscribe_addr   = prefix .. "/subscribe"
    local unsubscribe_addr = prefix .. "/unsubscribe"
    local subscribers      = {}  -- "ip:port" -> true

    -- Compute the feedback address for subscribe/unsubscribe messages:
    -- if the message carries an explicit port number, combine sender IP with that port;
    -- otherwise use msg.from directly (same address the sender came from).
    local function feedback_addr(msg)
        if #msg.args >= 1 then
            local ip = msg.from:match("^([^:]+):")
            if ip then return ip .. ":" .. tostring(msg.args[1]) end
        end
        return msg.from
    end

    local function notify_subscribers(addr, ...)
        for sub in pairs(subscribers) do
            send_osc(sub, addr, ...)
        end
    end

    return function(msg)
        local addr = msg.address
        local from = msg.from

        -- /prefix/subscribe [feedback_port]
        if addr == subscribe_addr then
            local fb = feedback_addr(msg)
            subscribers[fb] = true
            -- Initial state dump: send every queryable param to the new subscriber.
            for pname, p in pairs(params) do
                if p.get then
                    send_osc(fb, slash_prefix .. pname, p.get())
                end
            end
            return
        end

        -- /prefix/unsubscribe [feedback_port]
        if addr == unsubscribe_addr then
            subscribers[feedback_addr(msg)] = nil
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
                p.set()                          -- imperative with no args
            end
        elseif p.set then
            p.set(table.unpack(msg.args))
            -- Post-set notification: tell all subscribers (including the sender
            -- if subscribed) about the new value, using the canonical get() result
            -- rather than echoing the raw args (handles clamping, coercion, etc.).
            if p.get then
                notify_subscribers(addr, p.get())
            end
        end
    end
end
