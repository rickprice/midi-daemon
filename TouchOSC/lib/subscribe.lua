local subPath = ""
local heartbeatPath = ""
local discPath = ""

local heartbeatDelay = 5000
local lastHeartbeat = 0
local heartbeatActive = false

function init()
  
  
  port = self.tag
  subPath = "/" .. self.name .. "/subscribe"
  heartbeatPath = "/" .. self.name .. "/heartbeat"
  discPath = "/" .. self.name .. "/disconnect"

  -- Send initial subscription
  sendOSC({ subPath, { { tag = 'i', value = self.tag }, { tag = 'i', value = 30 } } })

  -- Arm the heartbeat
  lastHeartbeat = getMillis()
  heartbeatActive = true
end

function update()
  if not heartbeatActive then return end
  local now = getMillis()
  if (now - lastHeartbeat >= heartbeatDelay) then
    lastHeartbeat = now
    sendOSC({ heartbeatPath, { { "f", 1.0 } } })
  end
end

function onReceiveNotify(message, argument)
  if message == "LayoutClosing" then
    -- Stop the heartbeat
    heartbeatActive = false

    -- Send disconnect
    sendOSC({ discPath, { { "f", 1.0 } } })
  end
end