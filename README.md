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

### Per-user install

Config and routes live in `~/.config/midi-daemon/`. The daemon runs under
your own account as a systemd user service.

```bash
# Install binary
cargo install --path .

# Create config and routes directories
mkdir -p ~/.config/midi-daemon/routes.d

# Copy example config and routes
cp config.toml ~/.config/midi-daemon/config.toml
cp routes.d/*.lua ~/.config/midi-daemon/routes.d/

# Install and enable systemd user service
mkdir -p ~/.config/systemd/user
cp systemd/midi-daemon.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now midi-daemon
```

### System-wide install

Config and routes live in `/etc/midi-daemon/`. The daemon runs as a
dedicated `midi-daemon` system user.

```bash
# Install binary
sudo cargo install --path . --root /usr/local

# Create a dedicated system user
sudo useradd --system --no-create-home --shell /usr/sbin/nologin midi-daemon
# Add it to the audio group so it can access ALSA/PipeWire
sudo usermod -aG audio midi-daemon

# Create config and routes directories
sudo mkdir -p /etc/midi-daemon/routes.d
sudo chown -R midi-daemon:midi-daemon /etc/midi-daemon

# Copy example config and routes
sudo cp config.toml /etc/midi-daemon/config.toml
sudo cp routes.d/*.lua /etc/midi-daemon/routes.d/
sudo chown midi-daemon:midi-daemon /etc/midi-daemon/config.toml \
    /etc/midi-daemon/routes.d/*.lua

# Install and enable systemd system service
sudo cp systemd/midi-daemon-system.service /etc/systemd/system/midi-daemon.service
sudo systemctl daemon-reload
sudo systemctl enable --now midi-daemon
```

## Usage

### Per-user

```bash
systemctl --user status midi-daemon
journalctl --user -u midi-daemon -f
```

Add or remove a route — the daemon hot-reloads automatically:
```bash
cp my-route.lua ~/.config/midi-daemon/routes.d/
rm ~/.config/midi-daemon/routes.d/my-route.lua
```

Edit `config.toml` — the daemon detects the change and reloads all routes
with the updated configuration automatically (no restart needed).

### System-wide

```bash
systemctl status midi-daemon
journalctl -u midi-daemon -f
```

```bash
sudo cp my-route.lua /etc/midi-daemon/routes.d/
sudo rm /etc/midi-daemon/routes.d/my-route.lua
```

Edit `config.toml` — the daemon detects the change and reloads all routes
with the updated configuration automatically (no restart needed).

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

The daemon searches for a config file in this order:

1. `$MIDI_DAEMON_CONFIG` — explicit path via environment variable
2. `~/.config/midi-daemon/config.toml` — per-user
3. `/etc/midi-daemon/config.toml` — system-wide
4. Built-in defaults (routes dir inferred from whichever scope applies)

```toml
# Path to routes directory.
# Default: <config-dir>/routes.d  (user or system, whichever was loaded)
# routes_dir = "/custom/path"

default_bpm  = 120.0
default_ppqn = 24
```

Changes to `config.toml` are picked up automatically and all routes are
reloaded with the new values. The one exception is `routes_dir` — changing
it requires a daemon restart.

### Per-route configuration

Add a TOML section named after the route file (without `.lua`) to pass
configuration into that route's `config` global table:

```toml
[my-route]
some_key = "value"
some_number = 42
```

In `my-route.lua`:

```lua
local value = config.some_key    or "default"
local num   = config.some_number or 0
```

Any TOML type is supported: strings, integers, floats, booleans, arrays, and
nested tables.

## Example: Simple Metronome

See `routes.d/metronome.lua`. Plays GM percussion clicks and accepts
configurable MIDI control signals to start/stop playback and change BPM in
real time.

Configurable via `[metronome]` in `config.toml`:

| Key             | Default | Description                                      |
|-----------------|---------|--------------------------------------------------|
| `bpm`           | 120.0   | Initial BPM                                      |
| `ppqn`          | 24      | Pulses per quarter note                          |
| `beat_1_note`   | 37      | MIDI note for beat 1 (GM: Side Stick)            |
| `beat_n_note`   | 56      | MIDI note for other beats (GM: Cowbell)          |
| `channel`       | 10      | MIDI output channel (GM: percussion)             |
| `velocity`      | 100     | Note velocity                                    |
| `beats_per_bar` | 4       | Beats per bar                                    |
| `note_len_ms`   | 20      | Note duration in ms (fixed, independent of BPM)  |
| `cc_type`            | `"cc"`  | Incoming message type that controls BPM                    |
| `cc_channel`         | 1       | Incoming MIDI channel that controls BPM                    |
| `cc_controller`      | 21      | CC controller number that controls BPM                     |
| `start_stop_channel`    | 1    | MIDI channel for the start/stop CC                         |
| `start_stop_controller` | 22   | CC controller that starts/stops the metronome              |
| `start_running`      | `true`  | Whether the metronome starts playing immediately on launch  |

BPM is clamped to the range 20–200 regardless of source.

The start/stop CC uses the value to determine state: value ≥ 64 starts the
metronome, value < 64 stops it. MIDI Transport messages (`start`, `stop`,
`continue`) are also honoured and override the CC.

## Example: Invert Controllers

See `routes.d/invert_controllers.lua`. Forwards all MIDI, inverting the
value (`0–127 → 127–0`) for a configured set of controllers.

Configurable via `[invert_controllers]` in `config.toml`:

| Key           | Default | Description                              |
|---------------|---------|------------------------------------------|
| `type`        | `"cc"`  | Message type to match                    |
| `channel`     | 1       | MIDI channel to match                    |
| `controllers` | `[]`    | List of controller numbers to invert     |

```toml
[invert_controllers]
type        = "cc"
channel     = 1
controllers = [7, 11]   # volume, expression
```

## Example: Transpose

See `routes.d/transpose.lua`. Shifts all notes up by a configurable interval,
passes everything else through unchanged.
