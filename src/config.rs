//! `~/.config/rex-relay/config.toml`, and where the relay keeps its state.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

pub const DEFAULT_MAX_MESSAGE_BYTES: usize = 32 * 1024;
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30 * 60);
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(10);
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
/// Names a config file to use instead of the default location.
pub const CONFIG_ENV: &str = "REX_RELAY_CONFIG";
/// The endpoint variable that a Rex server sets in its terminals.
pub const SERVER_ENV: &str = "REX_SERVER";

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The endpoint to use when neither `--endpoint` nor `REX_SERVER` names one.
    pub endpoint: Option<String>,
    /// Every endpoint the relay may connect to. Empty refuses all of them.
    #[serde(default)]
    pub allowed_endpoints: Vec<String>,
    /// The largest message `send` accepts, in bytes.
    pub max_message_bytes: Option<usize>,
    /// How long `send` and `wait` wait by default, such as `30m`.
    pub timeout: Option<String>,
    /// How long one call to the server may take before the connection counts as stalled,
    /// such as `10s`. `send` and `wait` then reconnect.
    pub call_timeout: Option<String>,
    /// How long a command keeps retrying its first connection, such as `60s`. `0s` tries once.
    pub connect_timeout: Option<String>,
    /// Where the audit log goes. Defaults to `$XDG_STATE_HOME/rex-relay/audit.jsonl`.
    pub audit_log: Option<PathBuf>,
    /// Where the registry of sessions `new` created goes. Defaults to
    /// `$XDG_STATE_HOME/rex-relay/owned.json`.
    pub registry: Option<PathBuf>,
}

impl Config {
    /// Loads the config at `path`; a missing file is an empty config.
    pub fn load(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let config: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if self.max_message_bytes == Some(0) {
            bail!("max_message_bytes must be positive");
        }
        if let Some(timeout) = &self.timeout {
            humantime::parse_duration(timeout)
                .with_context(|| format!("invalid timeout {timeout:?}"))?;
        }
        if let Some(timeout) = &self.call_timeout {
            let parsed = humantime::parse_duration(timeout)
                .with_context(|| format!("invalid call_timeout {timeout:?}"))?;
            if parsed.is_zero() {
                bail!("call_timeout must be positive");
            }
        }
        if let Some(timeout) = &self.connect_timeout {
            humantime::parse_duration(timeout)
                .with_context(|| format!("invalid connect_timeout {timeout:?}"))?;
        }
        Ok(())
    }

    pub fn max_message_bytes(&self) -> usize {
        self.max_message_bytes.unwrap_or(DEFAULT_MAX_MESSAGE_BYTES)
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
            .as_deref()
            .and_then(|t| humantime::parse_duration(t).ok())
            .unwrap_or(DEFAULT_TIMEOUT)
    }

    pub fn call_timeout(&self) -> Duration {
        self.call_timeout
            .as_deref()
            .and_then(|t| humantime::parse_duration(t).ok())
            .unwrap_or(DEFAULT_CALL_TIMEOUT)
    }

    pub fn connect_timeout(&self) -> Duration {
        self.connect_timeout
            .as_deref()
            .and_then(|t| humantime::parse_duration(t).ok())
            .unwrap_or(DEFAULT_CONNECT_TIMEOUT)
    }

    pub fn audit_log(&self) -> Result<PathBuf> {
        match &self.audit_log {
            Some(path) => Ok(path.clone()),
            None => Ok(state_dir()?.join("audit.jsonl")),
        }
    }

    pub fn registry(&self) -> Result<PathBuf> {
        match &self.registry {
            Some(path) => Ok(path.clone()),
            None => Ok(state_dir()?.join("owned.json")),
        }
    }
}

/// `--config`, else `REX_RELAY_CONFIG`, else `$XDG_CONFIG_HOME/rex-relay/config.toml`.
pub fn config_path(flag: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = flag {
        return Ok(path.to_owned());
    }
    if let Some(path) = env_path(CONFIG_ENV) {
        return Ok(path);
    }
    let base = match env_path("XDG_CONFIG_HOME") {
        Some(dir) => dir,
        None => home()?.join(".config"),
    };
    Ok(base.join("rex-relay").join("config.toml"))
}

fn state_dir() -> Result<PathBuf> {
    let base = match env_path("XDG_STATE_HOME") {
        Some(dir) => dir,
        None => home()?.join(".local").join("state"),
    };
    Ok(base.join("rex-relay"))
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

fn home() -> Result<PathBuf> {
    env_path("HOME").context("HOME is not set")
}

/// Where the endpoint came from, for error messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointSource {
    Flag,
    Env,
    Config,
}

/// `--endpoint`, else `REX_SERVER`, else the config's `endpoint`. Never a built-in default.
pub fn choose_endpoint(
    flag: Option<&str>,
    env: Option<&str>,
    config: &Config,
) -> Result<(String, EndpointSource)> {
    let pick = |v: Option<&str>| {
        v.map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    };
    if let Some(v) = pick(flag) {
        return Ok((v, EndpointSource::Flag));
    }
    if let Some(v) = pick(env) {
        return Ok((v, EndpointSource::Env));
    }
    if let Some(v) = pick(config.endpoint.as_deref()) {
        return Ok((v, EndpointSource::Config));
    }
    bail!("no Rex endpoint: pass --endpoint, set {SERVER_ENV}, or set `endpoint` in the config")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_config() {
        let config: Config = toml::from_str(
            r#"
            endpoint = "https://rex-host.example.ts.net"
            allowed_endpoints = ["https://rex-host.example.ts.net"]
            max_message_bytes = 1024
            timeout = "5m"
            audit_log = "/tmp/audit.jsonl"
            "#,
        )
        .unwrap();
        config.validate().unwrap();
        assert_eq!(config.max_message_bytes(), 1024);
        assert_eq!(config.timeout(), Duration::from_secs(300));
        assert_eq!(
            config.audit_log().unwrap(),
            PathBuf::from("/tmp/audit.jsonl")
        );
    }

    #[test]
    fn example_config_parses() {
        let config: Config = toml::from_str(include_str!("../config.example.toml")).unwrap();
        config.validate().unwrap();
        assert_eq!(
            config.allowed_endpoints,
            ["https://rex-host.example.ts.net"]
        );
    }

    #[test]
    fn defaults() {
        let config = Config::default();
        assert_eq!(config.max_message_bytes(), 32 * 1024);
        assert_eq!(config.timeout(), Duration::from_secs(1800));
        assert_eq!(config.call_timeout(), Duration::from_secs(10));
        assert_eq!(config.connect_timeout(), Duration::from_secs(60));
        assert!(config.allowed_endpoints.is_empty());
    }

    #[test]
    fn rejects_unknown_keys_and_bad_values() {
        assert!(toml::from_str::<Config>("endpont = \"x\"").is_err());
        for bad in [
            "max_message_bytes = 0",
            "timeout = \"soon\"",
            "call_timeout = \"0s\"",
            "connect_timeout = \"soon\"",
        ] {
            let config: Config = toml::from_str(bad).unwrap();
            assert!(config.validate().is_err(), "{bad}");
        }
    }

    #[test]
    fn missing_file_is_empty_config() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::load(&dir.path().join("none.toml")).unwrap();
        assert!(config.endpoint.is_none());
    }

    #[test]
    fn endpoint_precedence() {
        let config = Config {
            endpoint: Some("https://c.example.ts.net".into()),
            ..Config::default()
        };
        let (v, s) = choose_endpoint(Some("https://f.example.ts.net"), Some("x"), &config).unwrap();
        assert_eq!(
            (v.as_str(), s),
            ("https://f.example.ts.net", EndpointSource::Flag)
        );
        let (v, s) = choose_endpoint(None, Some("https://e.example.ts.net"), &config).unwrap();
        assert_eq!(
            (v.as_str(), s),
            ("https://e.example.ts.net", EndpointSource::Env)
        );
        let (v, s) = choose_endpoint(None, Some(""), &config).unwrap();
        assert_eq!(
            (v.as_str(), s),
            ("https://c.example.ts.net", EndpointSource::Config)
        );
        assert!(choose_endpoint(None, None, &Config::default()).is_err());
    }
}
