-- metronome.lua
-- A simple configurable metronome.
-- Plays a high click on beat 1, low click on other beats.
-- Virtual ports created: midi-daemon:metronome (in + out)

local BEAT_1_NOTE  = config.beat_1_note  or 37   -- Side Stick (GM)
local BEAT_N_NOTE  = config.beat_n_note  or 56   -- Cowbell (GM)
local CHANNEL      = config.channel      or 10   -- GM percussion channel
local VELOCITY     = config.velocity     or 100
local BEATS_PER_BAR = config.beats_per_bar or 4

-- Incoming message that controls BPM: cc_type / cc_channel / cc_controller
local CC_TYPE       = config.cc_type       or "cc"
local CC_CHANNEL    = config.cc_channel    or 1
local CC_CONTROLLER = config.cc_controller or 21

set_bpm(config.bpm   or 120)
set_ppqn(config.ppqn or 24)

local beat = 0
local note_off_at = {}  -- tick -> {note, channel}

function on_tick(tick, bpm, ppqn)
    -- Handle pending note-offs
    if note_off_at[tick] then
        for _, ev in ipairs(note_off_at[tick]) do
            send({ type = "note_off", channel = ev.channel, note = ev.note, velocity = 0 })
        end
        note_off_at[tick] = nil
    end

    -- Fire on quarter-note boundaries
    if tick % ppqn == 0 then
        beat = (beat % BEATS_PER_BAR) + 1
        local note = (beat == 1) and BEAT_1_NOTE or BEAT_N_NOTE

        send({ type = "note_on", channel = CHANNEL, note = note, velocity = VELOCITY })

        -- Schedule note-off ~20ms later (approximate: 1 tick at 120bpm/24ppqn ≈ 20.8ms)
        local off_tick = tick + 1
        note_off_at[off_tick] = note_off_at[off_tick] or {}
        table.insert(note_off_at[off_tick], { note = note, channel = CHANNEL })

        log(string.format("Beat %d/%d  BPM: %.1f", beat, BEATS_PER_BAR, bpm))
    end
end

-- React to incoming MIDI CC to change BPM (CC maps 0–127 → 20–200 BPM)
function on_midi(msg)
    if msg.type == CC_TYPE and msg.channel == CC_CHANNEL and msg.controller == CC_CONTROLLER then
        local new_bpm = 20 + (msg.value / 127.0) * 180
        set_bpm(new_bpm)
        log(string.format("BPM changed to %.1f", new_bpm))
    end
end
