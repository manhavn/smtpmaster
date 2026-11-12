//! Runtime configuration: built-in defaults, `SMTPMASTER_*` environment
//! variables, then global CLI flags. There is no configuration file.

use std::fmt;
use std::str::FromStr;

use crate::error::{AppError, Result};

#[derive(Debug, Clone, Default)]
pub struct Config {
    pub mongodb: MongoConfig,
    pub server: ServerConfig,
    pub maintenance: MaintenanceConfig,
    pub log: LogConfig,
}

#[derive(Debug, Clone)]
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

#[derive(Debug, Clone)]
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

#[derive(Debug, Clone, Default)]
pub struct Credential {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SelectionMode {
    /// Lowest `priority` first, then least recently used.
    #[default]
    Priority,
    /// Least recently used account first (priority breaks ties).
    RoundRobin,
    /// Like `priority`, but per-account schedules decide eligibility and may
    /// override the priority inside a window.
    TimeWindow,
}

impl FromStr for SelectionMode {
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

impl fmt::Display for SelectionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Priority => "priority",
            Self::RoundRobin => "round-robin",
            Self::TimeWindow => "time-window",
        })
    }
}

#[derive(Debug, Clone)]
pub struct MaintenanceConfig {
    /// Seconds between background quota resets / expiry sweeps while serving.
    pub interval_secs: u64,
}

impl Default for MaintenanceConfig {
    fn default() -> Self {
        Self { interval_secs: 30 }
    }
}

#[derive(Debug, Clone)]
pub struct LogConfig {
    pub retention_days: u32,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self { retention_days: 14 }
    }
}

/// Values supplied by global CLI flags; `None` keeps the environment/default.
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    pub mongo_uri: Option<String>,
    pub mongo_db: Option<String>,
    pub listen: Option<String>,
    pub hostname: Option<String>,
    pub mode: Option<SelectionMode>,
    pub concurrency: Option<usize>,
    pub sequential: Option<bool>,
    pub max_attempts: Option<usize>,
    pub fail_threshold: Option<u32>,
    pub send_timeout_secs: Option<u64>,
    pub max_message_size: Option<usize>,
    pub max_recipients: Option<usize>,
    pub max_connections: Option<usize>,
    pub auth_required: Option<bool>,
    pub rewrite_from_header: Option<bool>,
    pub users: Vec<Credential>,
    pub maintenance_interval_secs: Option<u64>,
    pub log_retention_days: Option<u32>,
}

impl Config {
    /// Defaults plus `SMTPMASTER_*` environment overrides.
    pub fn from_env() -> Result<Self> {
        let mut config = Self::default();
        config.apply_env()?;
        config.validate()?;
        Ok(config)
    }

    /// Apply CLI flags (highest precedence).
    pub fn apply_overrides(&mut self, overrides: &Overrides) -> Result<()> {
        if let Some(uri) = &overrides.mongo_uri {
            let uri = uri.trim();
            if uri.is_empty() {
                return Err(AppError::Config("--mongo-uri must not be empty".into()));
            }
            self.mongodb.uri = uri.to_string();
        }
        if let Some(database) = &overrides.mongo_db {
            validate_database_name(database)?;
            self.mongodb.database = database.trim().to_string();
        }
        if let Some(listen) = &overrides.listen {
            if listen.trim().is_empty() {
                return Err(AppError::Config("--listen must not be empty".into()));
            }
            self.server.listen = listen.trim().to_string();
        }
        if let Some(hostname) = &overrides.hostname {
            if hostname.trim().is_empty() {
                return Err(AppError::Config("--hostname must not be empty".into()));
            }
            self.server.hostname = hostname.trim().to_string();
        }
        if let Some(mode) = overrides.mode {
            self.server.mode = mode;
        }
        if let Some(value) = overrides.concurrency {
            self.server.concurrency = value;
        }
        if let Some(value) = overrides.sequential {
            self.server.sequential = value;
        }
        if let Some(value) = overrides.max_attempts {
            self.server.max_attempts = value;
        }
        if let Some(value) = overrides.fail_threshold {
            self.server.fail_threshold = value;
        }
        if let Some(value) = overrides.send_timeout_secs {
            self.server.send_timeout_secs = value;
        }
        if let Some(value) = overrides.max_message_size {
            self.server.max_message_size = value;
        }
        if let Some(value) = overrides.max_recipients {
            self.server.max_recipients = value;
        }
        if let Some(value) = overrides.max_connections {
            self.server.max_connections = value;
        }
        if let Some(value) = overrides.auth_required {
            self.server.auth_required = value;
        }
        if let Some(value) = overrides.rewrite_from_header {
            self.server.rewrite_from_header = value;
        }
        if !overrides.users.is_empty() {
            self.server.users = overrides.users.clone();
        }
        if let Some(value) = overrides.maintenance_interval_secs {
            self.maintenance.interval_secs = value;
        }
        if let Some(value) = overrides.log_retention_days {
            self.log.retention_days = value;
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        if self.mongodb.uri.trim().is_empty() {
            return Err(AppError::Config("mongodb uri must not be empty".into()));
        }
        validate_database_name(&self.mongodb.database)?;
        if self.server.hostname.trim().is_empty() {
            return Err(AppError::Config("server hostname must not be empty".into()));
        }
        if self.server.listen.trim().is_empty() {
            return Err(AppError::Config("listen address must not be empty".into()));
        }
        if self.server.concurrency == 0 {
            return Err(AppError::Config("concurrency must be at least 1".into()));
        }
        if self.server.max_message_size == 0 {
            return Err(AppError::Config("max message size must be positive".into()));
        }
        if self.server.max_recipients == 0 {
            return Err(AppError::Config("max recipients must be positive".into()));
        }
        if self.server.max_connections == 0 {
            return Err(AppError::Config("max connections must be positive".into()));
        }
        if self.server.send_timeout_secs == 0 {
            return Err(AppError::Config(
                "send timeout must be at least 1 second".into(),
            ));
        }
        if self.maintenance.interval_secs == 0 {
            return Err(AppError::Config(
                "maintenance interval must be at least 1 second".into(),
            ));
        }
        if self.log.retention_days == 0 {
            return Err(AppError::Config(
                "log retention must be at least 1 day".into(),
            ));
        }
        for user in &self.server.users {
            if user.username.is_empty() || user.password.is_empty() {
                return Err(AppError::Config(
                    "relay users need both username and password".into(),
                ));
            }
            if user.username.contains([':', ',']) || user.password.contains(',') {
                return Err(AppError::Config(format!(
                    "relay user `{}` may not contain ':' in the username or ',' in either field",
                    user.username
                )));
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
        if let Ok(v) = std::env::var("SMTPMASTER_MAX_RECIPIENTS") {
            self.server.max_recipients = parse_env(&v, "SMTPMASTER_MAX_RECIPIENTS")?;
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

fn validate_database_name(database: &str) -> Result<()> {
    let database = database.trim();
    if database.is_empty()
        || database
            .chars()
            .any(|c| matches!(c, '/' | '\\' | ' ' | '$' | '"'))
    {
        return Err(AppError::Config(format!(
            "`{database}` is not a valid MongoDB database name"
        )));
    }
    Ok(())
}

fn parse_env<T: FromStr>(value: &str, name: &str) -> Result<T> {
    value
        .trim()
        .parse()
        .map_err(|_| AppError::Config(format!("{name} has an invalid value `{value}`")))
}

/// Parse a single `username:password` pair.
pub fn parse_user_entry(entry: &str) -> Result<Credential> {
    let entry = entry.trim();
    let (username, password) = entry
        .split_once(':')
        .ok_or_else(|| AppError::Config(format!("relay user `{entry}` must be user:password")))?;
    if username.is_empty() || password.is_empty() {
        return Err(AppError::Config(
            "relay users need both username and password".into(),
        ));
    }
    Ok(Credential {
        username: username.to_string(),
        password: password.to_string(),
    })
}

fn parse_users(value: &str) -> Result<Vec<Credential>> {
    let mut users = Vec::new();
    for entry in value.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        users.push(parse_user_entry(entry)?);
        let user = users.last().expect("just pushed");
        if user.username.contains(':') || user.password.contains(',') {
            return Err(AppError::Config(format!(
                "relay user `{}` may not contain ':' in the username or ',' in either field",
                user.username
            )));
        }
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
        assert!(parse_user_entry(":x").is_err());
    }

    #[test]
    fn auth_without_users_is_valid_at_config_layer() {
        let mut config = Config::default();
        config.server.auth_required = true;
        // Routes stored in MongoDB may provide credentials at runtime.
        config.validate().unwrap();
        config.server.users.push(Credential {
            username: "u".into(),
            password: String::new(),
        });
        assert!(config.validate().is_err());
    }

    #[test]
    fn cli_overrides_win() {
        let mut config = Config::default();
        config
            .apply_overrides(&Overrides {
                mongo_uri: Some("mongodb://db:27017".into()),
                mongo_db: Some("production".into()),
                listen: Some("127.0.0.1:2626".into()),
                mode: Some(SelectionMode::RoundRobin),
                concurrency: Some(3),
                sequential: Some(true),
                max_attempts: Some(0),
                users: vec![Credential {
                    username: "app".into(),
                    password: "secret".into(),
                }],
                ..Overrides::default()
            })
            .unwrap();
        assert_eq!(config.mongodb.uri, "mongodb://db:27017");
        assert_eq!(config.mongodb.database, "production");
        assert_eq!(config.server.listen, "127.0.0.1:2626");
        assert_eq!(config.server.mode, SelectionMode::RoundRobin);
        assert_eq!(config.server.concurrency, 3);
        assert!(config.server.sequential);
        assert_eq!(config.send_workers(), 1);
        assert_eq!(config.server.users.len(), 1);
        config.validate().unwrap();

        assert!(
            config
                .apply_overrides(&Overrides {
                    mongo_uri: Some("  ".into()),
                    ..Overrides::default()
                })
                .is_err()
        );
        assert!(
            config
                .apply_overrides(&Overrides {
                    mongo_db: Some("bad/name".into()),
                    ..Overrides::default()
                })
                .is_err()
        );
        let mut invalid = Config::default();
        invalid
            .apply_overrides(&Overrides {
                concurrency: Some(0),
                ..Overrides::default()
            })
            .unwrap();
        assert!(invalid.validate().is_err());
    }
}
