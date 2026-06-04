use anyhow::Result;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use libc;

/// Public config — fully resolved (no Option fields).
#[derive(Debug, Clone)]
pub struct Config {
    pub routes_dir: PathBuf,
    pub default_bpm: f64,
    pub default_ppqn: u32,
    /// Per-route config sections, e.g. `[metronome]` in config.toml
    pub route_configs: HashMap<String, toml::Value>,
    /// Path to the config file that was loaded, or None if using built-in defaults.
    pub config_path: Option<PathBuf>,
    /// Regex applied to all route inputs when no per-route pattern is set.
    pub default_connect_input: Option<String>,
    /// Regex applied to all route outputs when no per-route pattern is set.
    pub default_connect_output: Option<String>,
    /// Single global UDP port to receive all OSC messages on. Incoming packets
    /// are dispatched to routes by address prefix: `/route-name/...`.
    pub osc_receive_port: Option<u16>,
    /// Global OSC send destination (`"host:port"`). Used by every route that
    /// does not declare its own `osc.send` target in `init()`.
    pub osc_send_addr: Option<String>,
    /// How often (in seconds) the daemon sends `/route/heartbeat` to OSC subscribers.
    pub osc_heartbeat_interval: f64,
}

/// Internal deserialization target. `routes_dir` is optional so the caller
/// can supply the right default after knowing which config file was loaded.
#[derive(Deserialize)]
struct RawConfig {
    routes_dir: Option<PathBuf>,
    #[serde(default = "default_bpm")]
    default_bpm: f64,
    #[serde(default = "default_ppqn")]
    default_ppqn: u32,
    default_connect_input: Option<String>,
    default_connect_output: Option<String>,
    osc_receive_port: Option<u16>,
    osc_send_addr: Option<String>,
    #[serde(default = "default_osc_heartbeat_interval")]
    osc_heartbeat_interval: f64,
    #[serde(flatten)]
    route_configs: HashMap<String, toml::Value>,
}

impl RawConfig {
    fn into_config(self, default_routes_dir: PathBuf, config_path: Option<PathBuf>) -> Config {
        Config {
            routes_dir: self.routes_dir.unwrap_or(default_routes_dir),
            default_bpm: self.default_bpm,
            default_ppqn: self.default_ppqn,
            route_configs: self.route_configs,
            config_path,
            default_connect_input: self.default_connect_input,
            default_connect_output: self.default_connect_output,
            osc_receive_port: self.osc_receive_port,
            osc_send_addr: self.osc_send_addr,
            osc_heartbeat_interval: self.osc_heartbeat_interval,
        }
    }
}

// ── Path helpers ─────────────────────────────────────────────────────────────

fn default_bpm() -> f64 {
    120.0
}

fn default_ppqn() -> u32 {
    24
}

fn default_osc_heartbeat_interval() -> f64 {
    5.0
}

fn default_config(routes_dir: PathBuf) -> Config {
    Config {
        routes_dir,
        default_bpm: default_bpm(),
        default_ppqn: default_ppqn(),
        route_configs: HashMap::new(),
        config_path: None,
        default_connect_input: None,
        default_connect_output: None,
        osc_receive_port: None,
        osc_send_addr: None,
        osc_heartbeat_interval: default_osc_heartbeat_interval(),
    }
}

fn user_config_dir() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    // System services run with home=/var/empty or /nonexistent; skip user dirs.
    if !home.exists() || home == std::path::Path::new("/var/empty") {
        return None;
    }
    dirs::config_dir().map(|d| d.join("midi-daemon"))
}

fn system_config_dir() -> PathBuf {
    PathBuf::from("/etc/midi-daemon")
}

fn system_cache_dir() -> PathBuf {
    PathBuf::from("/var/cache/midi-daemon")
}

#[allow(dead_code)]
pub fn default_config_path() -> PathBuf {
    user_config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config/midi-daemon"))
        .join("config.toml")
}

/// Path to the Unix control socket.
/// Root uses `/run/midi-daemon/control.sock`; non-root uses `$XDG_RUNTIME_DIR`.
pub fn control_socket_path() -> PathBuf {
    if unsafe { libc::getuid() } == 0 {
        PathBuf::from("/run/midi-daemon/control.sock")
    } else {
        std::env::var("XDG_RUNTIME_DIR")
            .map(|d| PathBuf::from(d).join("midi-daemon/control.sock"))
            .unwrap_or_else(|_| {
                dirs::cache_dir()
                    .map(|d| d.join("midi-daemon/control.sock"))
                    .unwrap_or_else(|| PathBuf::from("/tmp/midi-daemon.sock"))
            })
    }
}

// ── Config impl ───────────────────────────────────────────────────────────────

impl Config {
    /// Returns the cache directory appropriate for this process:
    /// `/var/cache/midi-daemon` when running as root, user cache otherwise.
    pub fn cache_dir(&self) -> PathBuf {
        if unsafe { libc::getuid() } == 0 {
            return system_cache_dir();
        }
        dirs::cache_dir()
            .map(|d| d.join("midi-daemon"))
            .unwrap_or_else(|| {
                dirs::home_dir()
                    .map(|h| h.join(".cache/midi-daemon"))
                    .unwrap_or_else(system_cache_dir)
            })
    }

    /// Returns the `[route_name]` section from config.toml, if present.
    pub fn route_config(&self, name: &str) -> Option<&toml::Table> {
        self.route_configs.get(name)?.as_table()
    }

    /// Search for a config file and load it. Lookup order:
    ///   1. `$MIDI_DAEMON_CONFIG` environment variable
    ///   2. `~/.config/midi-daemon/config.toml`  (user)
    ///   3. `/etc/midi-daemon/config.toml`        (system)
    ///   4. Built-in defaults
    pub fn find_and_load() -> Result<Self> {
        // 1. Explicit override
        if let Ok(val) = std::env::var("MIDI_DAEMON_CONFIG") {
            let path = PathBuf::from(&val);
            let default_routes = path
                .parent()
                .unwrap_or(path.as_path())
                .join("routes.d");
            tracing::info!("Loading config from $MIDI_DAEMON_CONFIG: {}", path.display());
            return Self::load_file(&path, default_routes);
        }

        // 2. User config
        if let Some(dir) = user_config_dir() {
            let path = dir.join("config.toml");
            if path.exists() {
                tracing::info!("Loading user config: {}", path.display());
                return Self::load_file(&path, dir.join("routes.d"));
            }
        }

        // 3. System config
        {
            let dir = system_config_dir();
            let path = dir.join("config.toml");
            if path.exists() {
                tracing::info!("Loading system config: {}", path.display());
                return Self::load_file(&path, dir.join("routes.d"));
            }
        }

        // 4. No config found — fall back to built-in defaults
        let (routes_dir, scope) = user_config_dir()
            .map(|d| (d.join("routes.d"), "user"))
            .unwrap_or_else(|| (system_config_dir().join("routes.d"), "system"));
        tracing::info!("No config file found, using {} defaults", scope);
        Ok(default_config(routes_dir))
    }

    /// Load from an explicit path. Useful for testing or when the caller
    /// already knows which file to use.
    #[allow(dead_code)]
    /// Falls back to built-in defaults with `default_routes_dir` if the file
    /// is absent.
    pub fn load(path: &Path) -> Result<Self> {
        let default_routes = path.parent().unwrap_or(path).join("routes.d");
        if path.exists() {
            Self::load_file(path, default_routes)
        } else {
            tracing::info!("No config found at {}, using defaults", path.display());
            let routes_dir = user_config_dir()
                .unwrap_or_else(|| PathBuf::from("~/.config/midi-daemon"))
                .join("routes.d");
            Ok(default_config(routes_dir))
        }
    }

    /// Re-read config from the same file it was originally loaded from.
    /// Falls back to returning a clone of self if no file path is known.
    pub fn reload(&self) -> Result<Self> {
        match &self.config_path {
            Some(path) => Self::load_file(path, self.routes_dir.clone()),
            None => Ok(self.clone()),
        }
    }

    fn load_file(path: &Path, default_routes_dir: PathBuf) -> Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let raw: RawConfig = toml::from_str(&text)?;
        Ok(raw.into_config(default_routes_dir, Some(path.to_path_buf())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_tmp(name: &str, content: &str) -> PathBuf {
        let path = std::env::temp_dir().join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        write!(f, "{}", content).unwrap();
        path
    }

    #[test]
    fn default_bpm_is_120() {
        assert_eq!(default_bpm(), 120.0);
    }

    #[test]
    fn default_ppqn_is_24() {
        assert_eq!(default_ppqn(), 24);
    }

    #[test]
    fn missing_file_returns_defaults() {
        let path = PathBuf::from("/tmp/midi_daemon_nonexistent_config_abc123.toml");
        let _ = std::fs::remove_file(&path);
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.default_bpm, 120.0);
        assert_eq!(cfg.default_ppqn, 24);
    }

    #[test]
    fn load_full_config() {
        let path = write_tmp(
            "midi_daemon_test_full.toml",
            "default_bpm = 140.0\ndefault_ppqn = 48\n",
        );
        let cfg = Config::load(&path).unwrap();
        assert!((cfg.default_bpm - 140.0).abs() < 1e-9);
        assert_eq!(cfg.default_ppqn, 48);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_partial_config_fills_defaults() {
        let path = write_tmp("midi_daemon_test_partial.toml", "default_bpm = 90.0\n");
        let cfg = Config::load(&path).unwrap();
        assert!((cfg.default_bpm - 90.0).abs() < 1e-9);
        assert_eq!(cfg.default_ppqn, 24);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_ppqn_only() {
        let path = write_tmp("midi_daemon_test_ppqn.toml", "default_ppqn = 96\n");
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.default_bpm, 120.0);
        assert_eq!(cfg.default_ppqn, 96);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_custom_routes_dir() {
        let path = write_tmp(
            "midi_daemon_test_routes_dir.toml",
            "routes_dir = \"/custom/routes\"\n",
        );
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.routes_dir, PathBuf::from("/custom/routes"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn routes_dir_defaults_to_parent_of_config() {
        let path = write_tmp("midi_daemon_test_routes_default.toml", "");
        let cfg = Config::load(&path).unwrap();
        let expected = path.parent().unwrap().join("routes.d");
        assert_eq!(cfg.routes_dir, expected);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn invalid_toml_returns_error() {
        let path = write_tmp("midi_daemon_test_invalid.toml", "not valid toml ][[\n");
        let result = Config::load(&path);
        assert!(result.is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn default_config_path_ends_with_config_toml() {
        let p = default_config_path();
        assert_eq!(p.file_name().unwrap(), "config.toml");
    }

    // ── default_connect_input / default_connect_output ────────────────────────

    #[test]
    fn connect_fields_absent_returns_none() {
        let path = write_tmp("midi_daemon_test_no_connect.toml", "default_bpm = 120.0\n");
        let cfg = Config::load(&path).unwrap();
        assert!(cfg.default_connect_input.is_none());
        assert!(cfg.default_connect_output.is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn connect_input_only_parses() {
        let path = write_tmp(
            "midi_daemon_test_connect_in.toml",
            "default_connect_input = \".*KeyLab.*\"\n",
        );
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.default_connect_input.as_deref(), Some(".*KeyLab.*"));
        assert!(cfg.default_connect_output.is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn connect_output_only_parses() {
        let path = write_tmp(
            "midi_daemon_test_connect_out.toml",
            "default_connect_output = \".*Surge.*\"\n",
        );
        let cfg = Config::load(&path).unwrap();
        assert!(cfg.default_connect_input.is_none());
        assert_eq!(cfg.default_connect_output.as_deref(), Some(".*Surge.*"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn connect_both_fields_parse() {
        let path = write_tmp(
            "midi_daemon_test_connect_both.toml",
            "default_connect_input = \".*KeyLab.*\"\ndefault_connect_output = \".*Surge.*\"\n",
        );
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.default_connect_input.as_deref(), Some(".*KeyLab.*"));
        assert_eq!(cfg.default_connect_output.as_deref(), Some(".*Surge.*"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn connect_fields_survive_missing_file_fallback() {
        let path = PathBuf::from("/tmp/midi_daemon_nonexistent_connect_xyz.toml");
        let _ = std::fs::remove_file(&path);
        let cfg = Config::load(&path).unwrap();
        assert!(cfg.default_connect_input.is_none());
        assert!(cfg.default_connect_output.is_none());
    }
}
