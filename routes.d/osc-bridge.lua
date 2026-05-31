-- OSC bridge example: receive OSC messages and forward to MIDI,
-- and send OSC when MIDI note events arrive.
--
-- Run a loopback test with netcat:
--   nc -lu 9001 &
--   oscsend osc.udp://localhost:9000 /hello s "world"

function init()
    return {
        inputs  = {"midi"},
        outputs = {"midi"},
        osc = {
            receive = 9000,
            send = {
                default = "127.0.0.1:9001",
            },
        },
    }
end

-- Called for every incoming OSC message.
-- msg.address  - OSC address pattern (e.g. "/note/on")
-- msg.args     - 1-indexed array of typed arguments
function on_osc(msg)
    log("OSC " .. msg.address .. " (" .. #msg.args .. " args)")

    -- Map /note/on <note> <vel> to a MIDI note_on on channel 1
    if msg.address == "/note/on" and #msg.args >= 2 then
        send({
            type     = "note_on",
            channel  = 1,
            note     = msg.args[1],
            velocity = msg.args[2],
        })
    elseif msg.address == "/note/off" and #msg.args >= 1 then
        send({
            type     = "note_off",
            channel  = 1,
            note     = msg.args[1],
            velocity = 0,
        })
    end
end

-- Called for every incoming MIDI message.
-- Forwards note events as OSC to the configured send target.
function on_midi(msg)
    if msg.type == "note_on" then
        send_osc("/midi/note_on", msg.note, msg.velocity)
    elseif msg.type == "note_off" then
        send_osc("/midi/note_off", msg.note)
    end
    send(msg)
end
