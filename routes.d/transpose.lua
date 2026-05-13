-- transpose.lua
-- Transposes all incoming Note On/Off by a configurable number of semitones.
-- Virtual ports created: midi-daemon:transpose (in + out)

local SEMITONES = 7  -- transpose up a fifth

function on_midi(msg)
    if msg.type == "note_on" or msg.type == "note_off" then
        local new_note = math.max(0, math.min(127, msg.note + SEMITONES))
        send({
            type     = msg.type,
            channel  = msg.channel,
            note     = new_note,
            velocity = msg.velocity,
        })
    else
        -- Pass all other messages through unchanged
        send(msg)
    end
end
