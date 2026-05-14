use anyhow::Result;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Public config — fully resolved (no Option fields).
#[derive(Debug, Clone)]
pub struct Config {
    pub routes_dir: PathBuf,
    pub default_bpm: f64,
    pub default_ppqn: u32,
    /// Per-route config sections, e.g. `[metronome]` in config.toml
    pub route_configs: HashMap<String, toml::Value>,
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
    #[serde(flatten)]
    route_configs: HashMap<String, toml::Value>,
}

impl RawConfig {
    fn into_config(self, default_routes_dir: PathBuf) -> Config {
        Config {
            routes_dir: self.routes_dir.unwrap_or(default_routes_dir),
            default_bpm: self.default_bpm,
            default_ppqn: self.default_ppqn,
            route_configs: self.route_configs,
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

fn user_config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("midi-daemon"))
}

fn system_config_dir() -> PathBuf {
    PathBuf::from("/etc/midi-daemon")
}

#[allow(dead_code)]
pub fn default_config_path() -> PathBuf {
    user_config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config/midi-daemon"))
        .join("config.toml")
}

// ── Config impl ───────────────────────────────────────────────────────────────

impl Config {
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
        Ok(Config {
            routes_dir,
            default_bpm: default_bpm(),
            default_ppqn: default_ppqn(),
            route_configs: HashMap::new(),
        })
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
            Ok(Config {
                routes_dir: user_config_dir()
                    .unwrap_or_else(|| PathBuf::from("~/.config/midi-daemon"))
                    .join("routes.d"),
                default_bpm: default_bpm(),
                default_ppqn: default_ppqn(),
                route_configs: HashMap::new(),
            })
        }
    }

    fn load_file(path: &Path, default_routes_dir: PathBuf) -> Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let raw: RawConfig = toml::from_str(&text)?;
        Ok(raw.into_config(default_routes_dir))
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
}
