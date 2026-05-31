-- midi-daemon Lua stdlib — loaded into every route before its script.
-- All symbols defined here are globals available without any require/dofile.

--- Build an on_osc handler from a declarative parameter table.
--
-- prefix   string   OSC address prefix, e.g. "/" .. ROUTE_NAME
-- params   table    param_name -> { set = fn, get = fn }
--
--   get()           Called when a no-argument message arrives and get is defined.
--                   Return value(s) are sent back on the same address.
--   set(v, ...)     Called with the message arguments when args are present.
--                   If only set is defined, a no-argument message also calls set().
--
-- Example:
--   on_osc = osc_params("/" .. ROUTE_NAME, {
--       bpm  = { set = function(v) set_bpm(v) end, get = get_bpm },
--       mute = { set = function(v) muted = (v ~= 0) end,
--                get = function() return muted and 1 or 0 end },
--       stop = { set = function() running = false end },
--   })
function osc_params(prefix, params)
    local slash_prefix = prefix .. "/"
    local prefix_len   = #slash_prefix
    return function(msg)
        local addr = msg.address
        if addr:sub(1, prefix_len) ~= slash_prefix then return end
        local p = params[addr:sub(prefix_len + 1)]
        if not p then return end
        if #msg.args == 0 then
            if p.get then
                send_osc(addr, p.get())   -- query: reply in-place
            elseif p.set then
                p.set()                   -- imperative with no args
            end
        elseif p.set then
            p.set(table.unpack(msg.args))
        end
    end
end
