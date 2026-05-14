-- keyboard-split.lua
--
-- Splits a single keyboard at a configurable note boundary.
-- Notes below the split point go to "bass"; notes at or above go to "lead".
-- Non-note messages (CC, pitch bend, program change, …) are broadcast to both.
--
-- ALSA ports created:
--   input:   midi-daemon:keyboard-split/keyboard-in
--   outputs: midi-daemon:keyboard-split/bass-out
--            midi-daemon:keyboard-split/lead-out
--
-- config.toml example:
--
--   [keyboard-split]
--   split_note    = 60              -- split at middle C (C4); default 60
--   connect_input = ".*KeyLab.*"    -- auto-connect keyboard input on startup
--   connect_bass  = ".*ZynAddSubFX.*"
--   connect_lead  = ".*Surge.*"
--
-- All three connect keys are optional. When absent no auto-connection is
-- attempted for that port; use aconnect manually or rely on the global
-- default_connect_input / default_connect_output in config.toml.

local SPLIT = config.split_note or 60  -- notes below this → bass, >= this → lead

function init()
    return {
        inputs  = {"keyboard"},
        outputs = {"bass", "lead"},
        -- connect patterns are read from config.toml; nil entries are ignored
        connect = {
            inputs  = { keyboard = config.connect_input },
            outputs = { bass = config.connect_bass, lead = config.connect_lead },
        },
    }
end

function on_midi(msg)
    if msg.type == "note_on" or msg.type == "note_off" then
        if msg.note < SPLIT then
            send("bass", msg)
        else
            send("lead", msg)
        end
    else
        -- CC, pitch bend, program change, etc. go to both voices
        send("bass", msg)
        send("lead", msg)
    end
end
