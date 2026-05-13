use anyhow::Result;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    /// Directory containing .lua route files
    #[serde(default = "default_routes_dir")]
    pub routes_dir: PathBuf,

    /// Default BPM for timers
    #[serde(default = "default_bpm")]
    pub default_bpm: f64,

    /// Default pulses per quarter note
    #[serde(default = "default_ppqn")]
    pub default_ppqn: u32,
}

fn default_routes_dir() -> PathBuf {
    default_config_dir().join("routes.d")
}

fn default_bpm() -> f64 {
    120.0
}

fn default_ppqn() -> u32 {
    24
}

fn default_config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("midi-daemon")
}

pub fn default_config_path() -> PathBuf {
    default_config_dir().join("config.toml")
}

impl Config {
    pub fn load(path: &PathBuf) -> Result<Self> {
        if path.exists() {
            let text = std::fs::read_to_string(path)?;
            Ok(toml::from_str(&text)?)
        } else {
            tracing::info!(
                "No config found at {}, using defaults",
                path.display()
            );
            Ok(Config {
                routes_dir: default_routes_dir(),
                default_bpm: default_bpm(),
                default_ppqn: default_ppqn(),
            })
        }
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
        let _ = std::fs::remove_file(&path); // ensure absent
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
        assert_eq!(cfg.default_ppqn, 24); // default applied
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_ppqn_only() {
        let path = write_tmp("midi_daemon_test_ppqn.toml", "default_ppqn = 96\n");
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.default_bpm, 120.0); // default applied
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
