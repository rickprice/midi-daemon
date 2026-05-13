# midi-daemon

A Lua-scriptable MIDI routing daemon for Linux. Each `.lua` file in `routes.d/`
gets its own pair of virtual ALSA MIDI ports and a configurable BPM timer.
Drop in or edit a `.lua` file and the daemon hot-reloads it automatically.

## Requirements

- Rust (stable)
- ALSA development headers: `sudo pacman -S alsa-lib`

## Build

```bash
cargo build --release
```

## Install

```bash
# Install binary
cargo install --path .

# Create config directory
mkdir -p ~/.config/midi-daemon/routes.d

# Copy example config
cp config.toml ~/.config/midi-daemon/config.toml

# Copy example routes (optional)
cp routes.d/*.lua ~/.config/midi-daemon/routes.d/

# Install and enable systemd user service
mkdir -p ~/.config/systemd/user
cp systemd/midi-daemon.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now midi-daemon
```

## Usage

Check status:
```bash
systemctl --user status midi-daemon
journalctl --user -u midi-daemon -f
```

Add a route — just drop a `.lua` file in `routes.d/` and it hot-reloads:
```bash
cp my-route.lua ~/.config/midi-daemon/routes.d/
```

Remove a route:
```bash
rm ~/.config/midi-daemon/routes.d/my-route.lua
```

## Lua API

Each script can define these callback functions:

```lua
-- Called on every timer tick
-- tick:  monotonically increasing tick counter
-- bpm:   current BPM (float)
-- ppqn:  current pulses per quarter note
function on_tick(tick, bpm, ppqn) end

-- Called on every incoming MIDI message on this route's input port
-- msg fields vary by type (see below)
function on_midi(msg) end
```

### `msg` table fields by type

| type           | fields                                      |
|----------------|---------------------------------------------|
| `note_on`      | channel, note, velocity                     |
| `note_off`     | channel, note, velocity                     |
| `cc`           | channel, controller, value                  |
| `program_change` | channel, program                          |
| `pitch_bend`   | channel, value (-8192..8191)                |
| `clock`        | *(no extra fields)*                         |
| `start`        | *(no extra fields)*                         |
| `stop`         | *(no extra fields)*                         |
| `continue`     | *(no extra fields)*                         |
| `raw`          | data (1-indexed byte array)                 |

### Global functions available in Lua

```lua
send(msg)         -- Send a MIDI message table to this route's output port
set_bpm(bpm)      -- Set timer BPM (float)
get_bpm()         -- Get current BPM (float)
set_ppqn(ppqn)    -- Set pulses per quarter note (integer)
get_ppqn()        -- Get current PPQN (integer)
log(message)      -- Log a string to the systemd journal / stdout
```

## config.toml

```toml
# Path to routes directory (default: ~/.config/midi-daemon/routes.d)
# routes_dir = "/custom/path"

default_bpm  = 120.0
default_ppqn = 24
```

## Example: Simple Metronome

See `routes.d/metronome.lua`. Plays GM percussion clicks and accepts CC 21
on channel 1 to change BPM in real time.

## Example: Transpose

See `routes.d/transpose.lua`. Shifts all notes up by a configurable interval,
passes everything else through unchanged.
