//! `smtpmaster service ...`: generate and install a systemd unit so the relay
//! starts automatically.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::error::{AppError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    System,
    User,
}

impl Scope {
    pub fn is_user(self) -> bool {
        self == Self::User
    }

    fn wanted_by(self) -> &'static str {
        if self.is_user() {
            "default.target"
        } else {
            "multi-user.target"
        }
    }
}

#[derive(Debug, Clone)]
pub struct UnitOptions {
    pub name: String,
    pub exec: PathBuf,
    pub config: Option<PathBuf>,
    pub environment: Vec<(String, String)>,
    pub scope: Scope,
}

impl UnitOptions {
    pub fn new(
        name: &str,
        exec: PathBuf,
        config: Option<PathBuf>,
        environment: Vec<(String, String)>,
        scope: Scope,
    ) -> Result<Self> {
        validate_name(name)?;
        for (key, value) in &environment {
            validate_env(key, value)?;
        }
        Ok(Self {
            name: name.to_string(),
            exec,
            config,
            environment,
            scope,
        })
    }

    /// systemd unit file contents.
    pub fn render(&self) -> String {
        let mut unit = String::new();
        let _ = writeln!(unit, "[Unit]");
        let _ = writeln!(unit, "Description=smtpmaster SMTP router ({})", self.name);
        let _ = writeln!(unit, "Documentation=https://github.com/manhavn/smtpmaster");
        let _ = writeln!(unit, "Wants=network-online.target");
        let _ = writeln!(unit, "After=network-online.target");
        let _ = writeln!(unit);
        let _ = writeln!(unit, "[Service]");
        let _ = writeln!(unit, "Type=simple");
        let _ = writeln!(unit, "ExecStart={}", self.exec_start());
        let _ = writeln!(unit, "Restart=on-failure");
        let _ = writeln!(unit, "RestartSec=3");
        let _ = writeln!(unit, "TimeoutStopSec=30");
        let _ = writeln!(unit, "KillSignal=SIGTERM");
        let _ = writeln!(unit, "LimitNOFILE=65536");
        let _ = writeln!(unit, "NoNewPrivileges=true");
        let _ = writeln!(unit, "PrivateTmp=true");
        if !self.scope.is_user() {
            // Reading the config still works; the relay only needs network.
            let _ = writeln!(unit, "ProtectSystem=full");
            let _ = writeln!(unit, "ProtectHome=read-only");
        }
        for (key, value) in &self.environment {
            let _ = writeln!(unit, "Environment={}", quote(&format!("{key}={value}")));
        }
        let _ = writeln!(unit);
        let _ = writeln!(unit, "[Install]");
        let _ = writeln!(unit, "WantedBy={}", self.scope.wanted_by());
        unit
    }

    fn exec_start(&self) -> String {
        let mut command = quote(&self.exec.to_string_lossy());
        command.push_str(" serve");
        if let Some(config) = &self.config {
            command.push_str(" --config ");
            command.push_str(&quote(&config.to_string_lossy()));
        }
        command
    }
}

/// Path of the unit file for `name`.
pub fn unit_path(scope: Scope, name: &str) -> Result<PathBuf> {
    validate_name(name)?;
    let directory = match scope {
        Scope::System => PathBuf::from("/etc/systemd/system"),
        Scope::User => match std::env::var_os("XDG_CONFIG_HOME") {
            Some(value) if !value.is_empty() => PathBuf::from(value).join("systemd").join("user"),
            _ => {
                let home = std::env::var_os("HOME").ok_or_else(|| {
                    AppError::Config("HOME is not set; cannot locate user units".into())
                })?;
                PathBuf::from(home)
                    .join(".config")
                    .join("systemd")
                    .join("user")
            }
        },
    };
    Ok(directory.join(format!("{name}.service")))
}

/// Write the unit and enable it (`--now` unless `no_start`).
pub fn install(options: &UnitOptions, force: bool, no_start: bool, dry_run: bool) -> Result<()> {
    let path = unit_path(options.scope, &options.name)?;
    if path.exists() && !force {
        return Err(AppError::Conflict(format!(
            "{} already exists (use --force to overwrite)",
            path.display()
        )));
    }
    if dry_run {
        println!("# {}", path.display());
        print!("{}", options.render());
        println!();
        println!("+ {}", systemctl_display(options.scope, &["daemon-reload"]));
        if no_start {
            println!(
                "+ {}",
                systemctl_display(options.scope, &["enable", &options.name])
            );
        } else {
            println!(
                "+ {}",
                systemctl_display(options.scope, &["enable", "--now", &options.name])
            );
        }
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(write_error)?;
    }
    std::fs::write(&path, options.render()).map_err(write_error)?;
    println!("wrote {}", path.display());

    systemctl(options.scope, &["daemon-reload"])?;
    if no_start {
        systemctl(options.scope, &["enable", &options.name])?;
        println!("enabled {} (not started)", options.name);
    } else {
        systemctl(options.scope, &["enable", "--now", &options.name])?;
        println!("enabled and started {}", options.name);
    }
    if options.scope.is_user() {
        println!(
            "tip: run `loginctl enable-linger $USER` so the user service starts at boot without a login session"
        );
    }
    Ok(())
}

pub fn uninstall(name: &str, scope: Scope, dry_run: bool) -> Result<()> {
    let path = unit_path(scope, name)?;
    if !path.exists() {
        return Err(AppError::NotFound(format!("{}", path.display())));
    }
    if dry_run {
        println!("+ {}", systemctl_display(scope, &["stop", name]));
        println!("+ {}", systemctl_display(scope, &["disable", name]));
        println!("- {}", path.display());
        println!("+ {}", systemctl_display(scope, &["daemon-reload"]));
        return Ok(());
    }
    best_effort(scope, &["stop", name]);
    best_effort(scope, &["disable", name]);
    std::fs::remove_file(&path).map_err(write_error)?;
    systemctl(scope, &["daemon-reload"])?;
    println!("removed {} and disabled {}", path.display(), name);
    Ok(())
}

pub fn status(name: &str, scope: Scope) -> Result<()> {
    validate_name(name)?;
    let mut command = Command::new("systemctl");
    if scope.is_user() {
        command.arg("--user");
    }
    command.args(["status", name, "--no-pager"]);
    let status = command
        .status()
        .map_err(|error| AppError::Internal(format!("cannot run systemctl: {error}")))?;
    if !status.success() {
        println!("service `{name}` is not active (systemctl exited with {status})");
    }
    Ok(())
}

fn systemctl(scope: Scope, arguments: &[&str]) -> Result<()> {
    let mut command = Command::new("systemctl");
    if scope.is_user() {
        command.arg("--user");
    }
    command.args(arguments).stdin(Stdio::null());
    let status = command
        .status()
        .map_err(|error| AppError::Internal(format!("cannot run systemctl: {error}")))?;
    if !status.success() {
        return Err(AppError::Internal(format!(
            "systemctl {} failed (exit {status})",
            arguments.join(" ")
        )));
    }
    Ok(())
}

fn best_effort(scope: Scope, arguments: &[&str]) {
    let mut command = Command::new("systemctl");
    if scope.is_user() {
        command.arg("--user");
    }
    let _ = command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn systemctl_display(scope: Scope, arguments: &[&str]) -> String {
    let mut out = String::from("systemctl");
    if scope.is_user() {
        out.push_str(" --user");
    }
    for argument in arguments {
        out.push(' ');
        out.push_str(argument);
    }
    out
}

fn write_error(error: std::io::Error) -> AppError {
    if error.kind() == std::io::ErrorKind::PermissionDenied {
        AppError::Invalid("permission denied writing the unit; run as root or pass --user".into())
    } else {
        AppError::Io(error)
    }
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name == "." || name == ".." {
        return Err(AppError::Invalid("service name is empty or invalid".into()));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@' | ':'))
    {
        return Err(AppError::Invalid(format!(
            "service name `{name}` may only contain letters, digits, . _ - @ :"
        )));
    }
    Ok(())
}

fn validate_env(key: &str, value: &str) -> Result<()> {
    let mut chars = key.chars();
    let valid_start = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    if !valid_start || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(AppError::Invalid(format!(
            "invalid environment variable name `{key}`"
        )));
    }
    if value.contains(['\n', '\r']) {
        return Err(AppError::Invalid(format!(
            "environment variable `{key}` must not contain newlines"
        )));
    }
    Ok(())
}

/// Escape a value for a systemd unit file (specifiers and quoting).
fn quote(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    format!("\"{escaped}\"")
}

/// Parse `--env KEY=VALUE` pairs, defaulting `SMTPMASTER_LOG` to `info`.
pub fn parse_environment(values: &[String]) -> Result<Vec<(String, String)>> {
    let mut map: BTreeMap<String, String> = BTreeMap::new();
    map.insert("SMTPMASTER_LOG".to_string(), "info".to_string());
    for raw in values {
        let (key, value) = raw
            .split_once('=')
            .ok_or_else(|| AppError::Invalid(format!("--env `{raw}` must be KEY=VALUE")))?;
        validate_env(key.trim(), value)?;
        map.insert(key.trim().to_string(), value.to_string());
    }
    Ok(map.into_iter().collect())
}

/// Make `path` absolute and resolve symlinks when possible.
pub fn absolute(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    Ok(absolute.canonicalize().unwrap_or(absolute))
}

/// Locate the config file to embed in the unit: explicit `--config`, then
/// `./smtpmaster.toml`.
pub fn resolve_config(explicit: Option<&Path>) -> Result<Option<PathBuf>> {
    if let Some(path) = explicit {
        return Ok(Some(absolute(path)?));
    }
    let candidate = Path::new("smtpmaster.toml");
    if candidate.exists() {
        return Ok(Some(absolute(candidate)?));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(scope: Scope) -> UnitOptions {
        UnitOptions::new(
            "smtpmaster",
            PathBuf::from("/usr/local/bin/smtpmaster"),
            Some(PathBuf::from("/etc/smtpmaster.toml")),
            parse_environment(&[]).unwrap(),
            scope,
        )
        .unwrap()
    }

    #[test]
    fn renders_system_unit() {
        let unit = options(Scope::System).render();
        assert!(unit.contains(
            "ExecStart=\"/usr/local/bin/smtpmaster\" serve --config \"/etc/smtpmaster.toml\""
        ));
        assert!(unit.contains("WantedBy=multi-user.target"));
        assert!(unit.contains("Environment=\"SMTPMASTER_LOG=info\""));
        assert!(unit.contains("ProtectSystem=full"));
        assert!(unit.contains("Restart=on-failure"));
    }

    #[test]
    fn renders_user_unit() {
        let unit = options(Scope::User).render();
        assert!(unit.contains("WantedBy=default.target"));
        assert!(!unit.contains("ProtectSystem="));
    }

    #[test]
    fn omits_config_when_unset() {
        let unit = UnitOptions::new(
            "smtpmaster",
            PathBuf::from("/usr/bin/smtpmaster"),
            None,
            Vec::new(),
            Scope::System,
        )
        .unwrap()
        .render();
        assert!(unit.contains("ExecStart=\"/usr/bin/smtpmaster\" serve\n"));
    }

    #[test]
    fn quotes_and_escapes_specifiers() {
        assert_eq!(quote("/opt/a b"), "\"/opt/a b\"");
        assert_eq!(quote("50%"), "\"50%%\"");
        assert_eq!(quote("a\"b"), "\"a\\\"b\"");
    }

    #[test]
    fn rejects_bad_names_and_env() {
        assert!(
            UnitOptions::new(
                "../x",
                PathBuf::from("/bin/true"),
                None,
                Vec::new(),
                Scope::System
            )
            .is_err()
        );
        assert!(
            UnitOptions::new(
                "ok-name",
                PathBuf::from("/bin/true"),
                None,
                Vec::new(),
                Scope::System
            )
            .is_ok()
        );
        assert!(parse_environment(&["1BAD=x".to_string()]).is_err());
        assert!(parse_environment(&["GOOD".to_string()]).is_err());
        assert!(parse_environment(&["GOOD=a\nb".to_string()]).is_err());
    }

    #[test]
    fn environment_overrides_default_log_level() {
        let env = parse_environment(&[
            "SMTPMASTER_LOG=debug".to_string(),
            "RUST_BACKTRACE=1".to_string(),
        ])
        .unwrap();
        assert_eq!(
            env,
            vec![
                ("RUST_BACKTRACE".to_string(), "1".to_string()),
                ("SMTPMASTER_LOG".to_string(), "debug".to_string())
            ]
        );
    }

    #[test]
    fn system_unit_path_is_stable() {
        assert_eq!(
            unit_path(Scope::System, "smtpmaster").unwrap(),
            PathBuf::from("/etc/systemd/system/smtpmaster.service")
        );
    }
}
