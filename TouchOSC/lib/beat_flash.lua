-- Beat flash indicator for TouchOSC
-- Receives OSC on /metronom/beat with arguments: int32 beat, int32 ..., float ...
-- Place this script on a Box or Button control.
-- In the editor, add an OSC message to the control:
--   Path: /metronom/beat
--   Receive: ON, Send: OFF

local flashUntil = 0
local FLASH_MS   = 120

-- Resting color (dark grey, visible but unobtrusive)
local REST_R, REST_G, REST_B = 0.12, 0.12, 0.12

function onReceiveOSC(message, connections)
    local beat = 0

    -- message[2] is the argument list; each entry has .tag and .value
    -- First argument is int32 beat number
    if message[2] and message[2][1] and message[2][1].tag == 'i' then
        beat = message[2][1].value
    end

    if beat == 1 then
        -- Downbeat: bright amber/orange
        self.color = Color(1.0, 0.55, 0.0)
    elseif beat == 2 or beat == 4 then
        -- Beats 2 & 4 (backbeats): cool cyan
        self.color = Color(0.0, 0.75, 0.9)
    else
        -- Beat 3 (and any unexpected value): softer green
        self.color = Color(0.2, 0.85, 0.3)
    end

    flashUntil = getMillis() + FLASH_MS
end

function update()
    if getMillis() > flashUntil then
        self.color = Color(REST_R, REST_G, REST_B)
    end
end