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
-- Optional: declare named ports and auto-connect patterns (see below).
-- Called once at startup before on_midi/on_tick.
function init() end

-- Called on every timer tick
-- tick:  monotonically increasing tick counter
-- bpm:   current BPM (float)
-- ppqn:  current pulses per quarter note
function on_tick(tick, bpm, ppqn) end

-- Called on every incoming MIDI message on this route's input port.
-- msg.port holds the input port name when the route has multiple inputs.
-- Other msg fields vary by type (see below).
function on_midi(msg) end
```

### Named ports via `init()`

By default each route gets one input and one output port. Return a table from
`init()` to declare multiple named ports:

```lua
function init()
    return {
        inputs  = {"keyboard", "pad"},
        outputs = {"synth", "drums"},
    }
end
```

ALSA ports created (visible in `aconnect -l`):

```
midi-daemon:my-route/keyboard-in
midi-daemon:my-route/pad-in
midi-daemon:my-route/synth-out
midi-daemon:my-route/drums-out
```

In `on_midi`, `msg.port` tells you which input fired. In `send`, the first
argument selects the output:

```lua
function on_midi(msg)
    if msg.port == "keyboard" then
        send("synth", msg)
    elseif msg.port == "pad" then
        send("drums", msg)
    end
end
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
send(msg)              -- Send msg to the first/only output port
send(port_name, msg)   -- Send msg to a named output port (multi-port routes)
set_bpm(bpm)           -- Set timer BPM (float)
get_bpm()              -- Get current BPM (float)
set_ppqn(ppqn)         -- Set pulses per quarter note (integer)
get_ppqn()             -- Get current PPQN (integer)
log(message)           -- Log a string to the systemd journal / stdout
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

# Auto-connect: regex matched against "ClientName:PortName" of ALSA ports.
# Applied to every route input/output that has no per-route pattern.
# default_connect_input  = ".*My Keyboard.*"
# default_connect_output = ".*My Synth.*"
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

## Auto-connect

The daemon can automatically wire its virtual ALSA ports to physical or
software devices when it starts, and also when a device is plugged in later.
Patterns are regular expressions matched against the full ALSA address string
`"ClientName:PortName"` (the same strings shown by `aconnect -l`).

There are three levels of configuration, applied from highest to lowest
priority:

### 1. Per-port — `init()` in Lua

The most specific level. Use the `connect` field in the table returned by
`init()`:

```lua
function init()
    return {
        inputs  = {"keyboard", "pad"},
        outputs = {"synth", "drums"},
        connect = {
            -- per named port (highest priority)
            inputs  = { keyboard = ".*KeyLab.*", pad = ".*LinnStrument.*" },
            outputs = { synth = ".*Surge.*", drums = ".*DrumMachine.*" },

            -- OR: one pattern for all inputs / all outputs (singular form)
            -- input  = ".*My Keyboard.*",
            -- output = ".*My Synth.*",
        },
    }
end
```

`connect.inputs` / `connect.outputs` are tables of `port_name = "pattern"`.
`connect.input` / `connect.output` (singular) apply to every input or output
of that route.

### 2. Per-route — `config.toml`

Patterns set here override the global default and are overridden by Lua `init()`.

**All ports of a route** — one pattern for every input, one for every output:

```toml
[transpose]
connect_input  = ".*KeyLab.*"
connect_output = ".*Surge.*"
```

**Individual named ports** — use `connect_{portname}-in` / `connect_{portname}-out`
(where `portname` matches the port name declared in `init()`):

```toml
[timing-trainer]
connect_keyboard-in   = ".*A-PRO 2.*"
connect_metronome-in  = ".*metronome-out.*"
connect_pan-out       = ".*MyPlugin.*"
```

When any per-port or per-route connect pattern is present for a route, the
global `default_connect_input` / `default_connect_output` is not applied to
that route at all.

### 3. Global default — `config.toml`

Applies to every port of every route that has no higher-priority pattern.
The simplest option when a single controller drives all routes.

```toml
default_connect_input  = ".*KeyLab Essential.*"
default_connect_output = ".*Surge XT.*"
```

### Hot-plug

A background thread subscribes to ALSA sequencer announcements. When a
device appears after the daemon has started, any matching route ports are
connected to it automatically — no restart needed.

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
metronome, value < 64 stops it.

MIDI Transport messages are also honoured:

| Message    | Behaviour                                                  |
|------------|------------------------------------------------------------|
| `start`    | Reset to beat 1 and begin playing (re-syncs if running)    |
| `continue` | Resume from the current beat position without resetting    |
| `stop`     | Stop and reset beat position to beat 1                     |

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

## Example: Keyboard Split

See `routes.d/keyboard-split.lua`. Splits a single keyboard at a configurable
note: notes below go to a "bass" output, notes at or above go to a "lead"
output. Non-note messages (CC, pitch bend, …) are broadcast to both. Demonstrates
per-port auto-connect driven from config.toml.

```toml
[keyboard-split]
split_note    = 60              # split at middle C (C4)
connect_input = ".*KeyLab.*"   # auto-connect keyboard input on startup
connect_bass  = ".*ZynAddSubFX.*"
connect_lead  = ".*Surge.*"
```

The `connect_*` keys are all optional — omit any you don't need and connect
that port manually with `aconnect`, or rely on `default_connect_input` /
`default_connect_output` in the top-level config.

## Example: Transpose

See `routes.d/transpose.lua`. Shifts all notes up by a configurable interval,
passes everything else through unchanged.

## Example: Timing Trainer

See `routes.d/timing-trainer.lua`. Gives real-time feedback on how well you
are keeping time with a metronome by outputting a pan CC that shifts left when
you play early and right when you play late.

Connect the `metronome-in` port to any source that sends a `note_on` on each
beat (e.g. the `metronome.lua` output), and connect `keyboard-in` to your
keyboard. Route `pan-out` to a panning plugin in your audio chain. The
output is CC #10 (standard MIDI pan) on channel 1:

- **0 (hard left)** — playing ahead of the beat
- **64 (center)** — on time
- **127 (hard right)** — playing behind the beat

The CC value reflects a rolling average of recent hits, so the pan homes in
on your overall tendency to rush or drag rather than reacting to every
individual note. After a configurable period of silence the average resets
and pan returns to center.

Configurable via `[timing-trainer]` in `config.toml`:

| Key                      | Default | Description                                               |
|--------------------------|---------|-----------------------------------------------------------|
| `pan_channel`            | 1       | MIDI channel for the pan CC output                        |
| `pan_controller`         | 10      | CC number (10 = standard MIDI pan)                        |
| `max_error_ms`           | 200     | ±ms of timing error that maps to fully left or right      |
| `history_size`           | 8       | Number of recent hits included in the running average     |
| `idle_seconds`           | 3       | Seconds of silence before resetting the average to center |
| `start_stop_channel`     | 1       | MIDI channel for the enable/disable CC                    |
| `start_stop_controller`  | 22      | CC number that enables (≥ 64) or disables (< 64) training |
| `start_running`          | `true`  | Whether training is active immediately on launch          |

MIDI Transport messages (`start`, `continue`, `stop`) are also honoured and
will enable or disable training regardless of which input they arrive on.

```toml
[timing-trainer]
max_error_ms  = 150   # tighter window for more sensitive feedback
history_size  = 4     # react faster to changes in timing
idle_seconds  = 5
start_running = false # start disabled; enable via CC or transport start
```
