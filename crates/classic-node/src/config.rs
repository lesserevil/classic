use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Top-level config file format. See plans/01-skeleton-transport.md
/// § "TOML config schema" for the canonical example.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub node: NodeConfig,
    #[serde(default)]
    pub log: LogConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NodeConfig {
    /// Address the daemon binds to, e.g. `"0.0.0.0:7421"`.
    pub listen_addr: String,
    /// Directory holding `node_id` and other long-lived state.
    pub state_dir: PathBuf,
    /// Static peer list. Empty by default; dynamic discovery is out of scope
    /// for plan 01.
    #[serde(default)]
    pub peers: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct LogConfig {
    /// Optional log-level override. `None` defers to `RUST_LOG`.
    #[serde(default)]
    pub level: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config file not found at any of the searched paths")]
    NotFound,
    #[error("explicit --config path {0} does not exist")]
    ExplicitMissing(PathBuf),
    #[error("io error reading {path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("parse error in {path}: {source}")]
    Parse { path: PathBuf, source: toml::de::Error },
}

const SYSTEM_CONFIG_PATH: &str = "/etc/classicd/config.toml";

/// Resolve and load the daemon config.
///
/// Order:
/// 1. `cli_path` (highest priority). If supplied but missing, that is fatal —
///    we do NOT fall back. Operators expect explicit flags to be honoured.
/// 2. `/etc/classicd/config.toml`.
/// 3. `$XDG_CONFIG_HOME/classicd/config.toml` (typically
///    `~/.config/classicd/config.toml`).
pub fn load_config(cli_path: Option<PathBuf>) -> Result<Config, ConfigError> {
    if let Some(path) = cli_path {
        if !path.exists() {
            return Err(ConfigError::ExplicitMissing(path));
        }
        return parse_at(&path);
    }
    let system = PathBuf::from(SYSTEM_CONFIG_PATH);
    if system.exists() {
        return parse_at(&system);
    }
    if let Some(xdg) = xdg_config_path() {
        if xdg.exists() {
            return parse_at(&xdg);
        }
    }
    Err(ConfigError::NotFound)
}

fn xdg_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|p| p.join("classicd").join("config.toml"))
}

fn parse_at(path: &Path) -> Result<Config, ConfigError> {
    let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    toml::from_str(&raw).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    const EXAMPLE: &str = r#"
[node]
listen_addr = "0.0.0.0:7421"
state_dir   = "/var/lib/classicd"
peers       = ["10.0.0.2:7421", "10.0.0.3:7421"]

[log]
level = "info"
"#;

    fn write_file(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let p = dir.join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        p
    }

    #[test]
    fn roundtrip_example() {
        let cfg: Config = toml::from_str(EXAMPLE).unwrap();
        assert_eq!(cfg.node.listen_addr, "0.0.0.0:7421");
        assert_eq!(cfg.node.state_dir, PathBuf::from("/var/lib/classicd"));
        assert_eq!(cfg.node.peers, vec!["10.0.0.2:7421", "10.0.0.3:7421"]);
        assert_eq!(cfg.log.level.as_deref(), Some("info"));
    }

    #[test]
    fn omitting_log_section_uses_defaults() {
        let no_log = r#"
[node]
listen_addr = "127.0.0.1:1"
state_dir   = "/tmp/x"
"#;
        let cfg: Config = toml::from_str(no_log).unwrap();
        assert!(cfg.log.level.is_none());
        assert!(cfg.node.peers.is_empty());
    }

    #[test]
    fn missing_required_key_errors() {
        let bad = r#"
[node]
listen_addr = "127.0.0.1:1"
"#;
        let err = toml::from_str::<Config>(bad).unwrap_err();
        assert!(err.to_string().contains("state_dir"));
    }

    #[test]
    fn cli_overrides_default() {
        let dir = TempDir::new().unwrap();
        let path = write_file(dir.path(), "custom.toml", EXAMPLE);
        let cfg = load_config(Some(path)).unwrap();
        assert_eq!(cfg.node.listen_addr, "0.0.0.0:7421");
    }

    #[test]
    fn explicit_cli_missing_is_fatal() {
        let bogus = PathBuf::from("/nonexistent/classicd-test/foo.toml");
        let err = load_config(Some(bogus.clone())).unwrap_err();
        match err {
            ConfigError::ExplicitMissing(p) => assert_eq!(p, bogus),
            other => panic!("expected ExplicitMissing, got {other:?}"),
        }
    }

    #[test]
    fn malformed_toml_is_fatal() {
        let dir = TempDir::new().unwrap();
        let path = write_file(dir.path(), "bad.toml", "this is = not toml [[[");
        let err = load_config(Some(path.clone())).unwrap_err();
        match err {
            ConfigError::Parse { path: p, .. } => assert_eq!(p, path),
            other => panic!("expected Parse error, got {other:?}"),
        }
    }
}
