-- metronome.lua
-- A simple configurable metronome.
-- Plays a high click on beat 1, low click on other beats.
-- Virtual ports created: midi-daemon:metronome (in + out)

-- MIDI note numbers for the clicks
local BEAT_1_NOTE = 37  -- Side Stick (GM)
local BEAT_N_NOTE = 56  -- Cowbell (GM)
local CHANNEL     = 10  -- GM percussion channel
local VELOCITY    = 100
local NOTE_LEN_MS = 20  -- milliseconds (approximate, via tick count)

-- Time signature
local BEATS_PER_BAR = 4

set_bpm(120)
set_ppqn(24)  -- 24 ticks per quarter note

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

-- Optionally react to incoming MIDI CC to change BPM
-- CC 21 on channel 1 maps 0–127 → 60–187 BPM
function on_midi(msg)
    if msg.type == "cc" and msg.channel == 1 and msg.controller == 21 then
        local new_bpm = 60 + (msg.value / 127.0) * 127
        set_bpm(new_bpm)
        log(string.format("BPM changed to %.1f", new_bpm))
    end
end
