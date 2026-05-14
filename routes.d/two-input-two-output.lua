-- two-input-two-output.lua
--
-- Receives from two inputs ("keyboard", "pad") and routes to two outputs
-- ("synth", "drums"). Notes from the keyboard are transposed up an octave
-- before being sent to the synth. Pad notes go straight to drums.
--
-- ALSA ports created:
--   inputs:  midi-daemon:two-input-two-output/keyboard-in
--            midi-daemon:two-input-two-output/pad-in
--   outputs: midi-daemon:two-input-two-output/synth
--            midi-daemon:two-input-two-output/drums

function init()
    return {
        inputs  = {"keyboard", "pad"},
        outputs = {"synth", "drums"},
    }
end

function on_midi(msg)
    if msg.port == "keyboard" then
        if msg.type == "note_on" or msg.type == "note_off" then
            msg.note = math.min(127, msg.note + 12)  -- transpose up one octave
            send("synth", msg)
        else
            send("synth", msg)  -- pass CC, pitch bend, etc. through unchanged
        end

    elseif msg.port == "pad" then
        send("drums", msg)
    end
end
