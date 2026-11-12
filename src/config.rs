//! Configuration: TOML file plus `SMTPMASTER_*` environment overrides.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{AppError, Result};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub mongodb: MongoConfig,
    pub server: ServerConfig,
    pub maintenance: MaintenanceConfig,
    pub log: LogConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct MongoConfig {
    pub uri: String,
    pub database: String,
}

impl Default for MongoConfig {
    fn default() -> Self {
        Self {
            uri: "mongodb://127.0.0.1:27017".to_string(),
            database: "smtpmaster".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub listen: String,
    pub hostname: String,
    pub max_message_size: usize,
    pub max_recipients: usize,
    pub max_connections: usize,
    pub mode: SelectionMode,
    pub concurrency: usize,
    /// Deliver one message at a time, ignoring `concurrency`.
    pub sequential: bool,
    /// Accounts to try per message. `0` tries every eligible account.
    pub max_attempts: usize,
    /// Disable an account after this many consecutive send failures
    /// (`0` disables the feature).
    pub fail_threshold: u32,
    pub send_timeout_secs: u64,
    pub auth_required: bool,
    /// Rewrite the message `From:` header to the selected account identity.
    pub rewrite_from_header: bool,
    pub users: Vec<Credential>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:2525".to_string(),
            hostname: "smtpmaster".to_string(),
            max_message_size: 25 * 1024 * 1024,
            max_recipients: 100,
            max_connections: 256,
            mode: SelectionMode::Priority,
            concurrency: 8,
            sequential: false,
            max_attempts: 0,
            fail_threshold: 5,
            send_timeout_secs: 60,
            auth_required: false,
            rewrite_from_header: false,
            users: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Credential {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SelectionMode {
    /// Lowest `priority` first, then least recently used.
    #[default]
    Priority,
    /// Least recently used account first (priority breaks ties).
    #[serde(alias = "round_robin")]
    RoundRobin,
    /// Like `priority`, but per-account schedules decide eligibility and may
    /// override the priority inside a window.
    #[serde(alias = "time_window")]
    TimeWindow,
}

impl std::str::FromStr for SelectionMode {
    type Err = AppError;
    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "priority" => Ok(Self::Priority),
            "round-robin" => Ok(Self::RoundRobin),
            "time-window" => Ok(Self::TimeWindow),
            other => Err(AppError::Invalid(format!(
                "unknown mode `{other}` (use priority, round-robin or time-window)"
            ))),
        }
    }
}

impl std::fmt::Display for SelectionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Priority => "priority",
            Self::RoundRobin => "round-robin",
            Self::TimeWindow => "time-window",
        })
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct MaintenanceConfig {
    /// Seconds between background quota resets / expiry sweeps while serving.
    pub interval_secs: u64,
}

impl Default for MaintenanceConfig {
    fn default() -> Self {
        Self { interval_secs: 30 }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LogConfig {
    pub retention_days: u32,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self { retention_days: 14 }
    }
}

impl Config {
    /// Load `path` when given (or `smtpmaster.toml` in the working directory
    /// when present), then apply environment overrides.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let explicit = path.is_some();
        let path = path.map(Path::to_path_buf).or_else(|| {
            let default = Path::new("smtpmaster.toml");
            default.exists().then(|| default.to_path_buf())
        });
        let mut config = match path {
            Some(path) => {
                let text = std::fs::read_to_string(&path).map_err(|e| {
                    AppError::Config(format!("cannot read {}: {e}", path.display()))
                })?;
                toml::from_str(&text)
                    .map_err(|e| AppError::Config(format!("invalid {}: {e}", path.display())))?
            }
            None if explicit => {
                return Err(AppError::Config("config file not found".into()));
            }
            None => Self::default(),
        };
        config.apply_env()?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.mongodb.uri.trim().is_empty() {
            return Err(AppError::Config("mongodb.uri must not be empty".into()));
        }
        if self.mongodb.database.trim().is_empty() {
            return Err(AppError::Config(
                "mongodb.database must not be empty".into(),
            ));
        }
        if self.server.hostname.trim().is_empty() {
            return Err(AppError::Config("server.hostname must not be empty".into()));
        }
        if self.server.concurrency == 0 {
            return Err(AppError::Config(
                "server.concurrency must be at least 1".into(),
            ));
        }
        if self.server.max_message_size == 0 {
            return Err(AppError::Config(
                "server.max_message_size must be positive".into(),
            ));
        }
        for user in &self.server.users {
            if user.username.is_empty() || user.password.is_empty() {
                return Err(AppError::Config(
                    "server.users entries need both username and password".into(),
                ));
            }
        }
        Ok(())
    }

    /// Effective number of concurrent upstream sends.
    pub fn send_workers(&self) -> usize {
        if self.server.sequential {
            1
        } else {
            self.server.concurrency.max(1)
        }
    }

    fn apply_env(&mut self) -> Result<()> {
        if let Ok(v) = std::env::var("SMTPMASTER_MONGODB_URI") {
            self.mongodb.uri = v;
        }
        if let Ok(v) = std::env::var("SMTPMASTER_MONGODB_DB") {
            self.mongodb.database = v;
        }
        if let Ok(v) = std::env::var("SMTPMASTER_LISTEN") {
            self.server.listen = v;
        }
        if let Ok(v) = std::env::var("SMTPMASTER_HOSTNAME") {
            self.server.hostname = v;
        }
        if let Ok(v) = std::env::var("SMTPMASTER_MODE") {
            self.server.mode = v.parse()?;
        }
        if let Ok(v) = std::env::var("SMTPMASTER_CONCURRENCY") {
            self.server.concurrency = parse_env(&v, "SMTPMASTER_CONCURRENCY")?;
        }
        if let Ok(v) = std::env::var("SMTPMASTER_SEQUENTIAL") {
            self.server.sequential = parse_env(&v, "SMTPMASTER_SEQUENTIAL")?;
        }
        if let Ok(v) = std::env::var("SMTPMASTER_AUTH_REQUIRED") {
            self.server.auth_required = parse_env(&v, "SMTPMASTER_AUTH_REQUIRED")?;
        }
        if let Ok(v) = std::env::var("SMTPMASTER_REWRITE_FROM_HEADER") {
            self.server.rewrite_from_header = parse_env(&v, "SMTPMASTER_REWRITE_FROM_HEADER")?;
        }
        if let Ok(v) = std::env::var("SMTPMASTER_USERS") {
            self.server.users = parse_users(&v)?;
        }
        if let Ok(v) = std::env::var("SMTPMASTER_MAX_MESSAGE_SIZE") {
            self.server.max_message_size = parse_env(&v, "SMTPMASTER_MAX_MESSAGE_SIZE")?;
        }
        if let Ok(v) = std::env::var("SMTPMASTER_MAX_CONNECTIONS") {
            self.server.max_connections = parse_env(&v, "SMTPMASTER_MAX_CONNECTIONS")?;
        }
        if let Ok(v) = std::env::var("SMTPMASTER_MAX_ATTEMPTS") {
            self.server.max_attempts = parse_env(&v, "SMTPMASTER_MAX_ATTEMPTS")?;
        }
        if let Ok(v) = std::env::var("SMTPMASTER_FAIL_THRESHOLD") {
            self.server.fail_threshold = parse_env(&v, "SMTPMASTER_FAIL_THRESHOLD")?;
        }
        if let Ok(v) = std::env::var("SMTPMASTER_SEND_TIMEOUT_SECS") {
            self.server.send_timeout_secs = parse_env(&v, "SMTPMASTER_SEND_TIMEOUT_SECS")?;
        }
        if let Ok(v) = std::env::var("SMTPMASTER_LOG_RETENTION_DAYS") {
            self.log.retention_days = parse_env(&v, "SMTPMASTER_LOG_RETENTION_DAYS")?;
        }
        if let Ok(v) = std::env::var("SMTPMASTER_MAINTENANCE_INTERVAL_SECS") {
            self.maintenance.interval_secs = parse_env(&v, "SMTPMASTER_MAINTENANCE_INTERVAL_SECS")?;
        }
        Ok(())
    }
}

fn parse_env<T: std::str::FromStr>(value: &str, name: &str) -> Result<T> {
    value
        .trim()
        .parse()
        .map_err(|_| AppError::Config(format!("{name} has an invalid value `{value}`")))
}

fn parse_users(value: &str) -> Result<Vec<Credential>> {
    let mut users = Vec::new();
    for entry in value.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let (username, password) = entry.split_once(':').ok_or_else(|| {
            AppError::Config("SMTPMASTER_USERS entries must be user:password".into())
        })?;
        users.push(Credential {
            username: username.trim().to_string(),
            password: password.to_string(),
        });
    }
    Ok(users)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        Config::default().validate().unwrap();
    }

    #[test]
    fn mode_parsing() {
        assert_eq!(
            "priority".parse::<SelectionMode>().unwrap(),
            SelectionMode::Priority
        );
        assert_eq!(
            "round_robin".parse::<SelectionMode>().unwrap(),
            SelectionMode::RoundRobin
        );
        assert_eq!(
            "time-window".parse::<SelectionMode>().unwrap(),
            SelectionMode::TimeWindow
        );
        assert!("bogus".parse::<SelectionMode>().is_err());
    }

    #[test]
    fn parses_users() {
        let users = parse_users("a:b,c:d").unwrap();
        assert_eq!(users.len(), 2);
        assert_eq!(users[1].username, "c");
        assert_eq!(users[1].password, "d");
        assert!(parse_users("no-colon").is_err());
    }

    #[test]
    fn auth_without_users_is_valid_at_config_layer() {
        let mut config = Config::default();
        config.server.auth_required = true;
        // Routes stored in MongoDB may provide the credentials at runtime.
        config.validate().unwrap();
        config.server.users.push(Credential {
            username: "u".into(),
            password: String::new(),
        });
        assert!(config.validate().is_err());
    }
}
